//! Purge legacy untracked checkpoint payloads.
//!
//! Each selected session takes its own lifecycle admission, and only for the
//! rewrite of that one archive. The admission moves into the blocking task
//! that rewrites the archive, so it is released when that task returns,
//! panics, or is dropped. That admission is the recovery gate. It does not
//! hold daemon-handoff admission. The blocking task's cancellation probe
//! stops verification and rewriting when the client disconnects or the
//! upgrade gate closes, and the commit step takes handoff admission only
//! for the install, the checkpoint swap, and the removal of the old archive.
//! A session another lifecycle operation already owns is skipped (`--all`)
//! or refused (`--session`) without waiting. Reported bytes are the archive
//! file's on-disk delta, or for a dry run the compressed size of the removed
//! untracked ZIP entries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::RuntimeState;

use mj_controller::controller::checkpoint::{PurgeSessionOutcome, purge_session_untracked_archive};
use mj_controller::recovery_gate::{LifecycleOrigin, RefusalCause, SessionAdmission};

use crate::server::api::{
    PurgeUntrackedFailure, PurgeUntrackedRequest, PurgeUntrackedResponse, PurgeUntrackedSession,
    PurgeUntrackedStashNote, PurgeUntrackedStatus,
};

const LIFECYCLE_BUSY: &str = "lifecycle operation in progress";
const SHUTTING_DOWN: &str = "daemon is shutting down";
const HANDOFF_MESSAGE: &str = "the Mjolnir daemon is being replaced by an upgrade; ask again";
const CANCELLED: &str = "purge untracked was cancelled";

impl RuntimeState {
    pub(crate) async fn purge_untracked(
        self: &Arc<Self>,
        request: PurgeUntrackedRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<PurgeUntrackedResponse, PurgeUntrackedFailure> {
        let dry_run = request.dry_run;
        let single = !request.all;
        let ids = purge_session_ids(&request).await?;
        let mut sessions = Vec::with_capacity(ids.len());
        for (index, session_id) in ids.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                return cancelled_report(&sessions, dry_run);
            }
            // Observe a handoff without taking admission. A hold here would
            // cover the rewrite and stop the daemon from closing the gate.
            if mj_controller::upgrade::is_draining() {
                tracing::debug!("purge untracked deferred for daemon handoff");
                return handoff_report(&sessions, &ids[index..], dry_run);
            }
            if self.session_lifecycle_busy(session_id).is_some() {
                match busy_row(single, session_id) {
                    Ok(row) => {
                        sessions.push(row);
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            let admission = match self.session_gate.try_enter_foreground(
                session_id,
                "purge untracked",
                LifecycleOrigin::Person,
            ) {
                Ok(admission) => admission,
                Err(refusal) => {
                    if matches!(
                        refusal.cause,
                        RefusalCause::Closed | RefusalCause::DaemonDraining
                    ) {
                        tracing::debug!(
                            session_id,
                            holders = ?refusal.holders,
                            "purge untracked stopped because the daemon is shutting down"
                        );
                        return handoff_report(&sessions, &ids[index..], dry_run);
                    }
                    tracing::debug!(
                        session_id,
                        holders = ?refusal.holders,
                        "purge untracked admission refused"
                    );
                    match busy_row(single, session_id) {
                        Ok(row) => {
                            sessions.push(row);
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
            };
            let id = session_id.clone();
            let cancel_flag = Arc::clone(&cancel);
            let joined = mj_core::runtime::spawn_blocking(move || {
                let admitted = SessionAdmission::Foreground(admission);
                let cancelled: mj_checkpoint::archive::ArchivePurgeCancel = Arc::new(move || {
                    cancel_flag.load(Ordering::Acquire) || !mj_controller::upgrade::gate().is_open()
                });
                purge_session_untracked_archive(&admitted, &id, dry_run, &cancelled)
            })
            .await;
            let outcome = match joined {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    let reason = format!("{error:#}");
                    tracing::warn!(session_id, error = %reason, "purge untracked failed");
                    if single {
                        return Err(PurgeUntrackedFailure::Internal(reason));
                    }
                    sessions.push(failed(session_id, &reason));
                    continue;
                }
                Err(error) => {
                    return Err(PurgeUntrackedFailure::Internal(format!(
                        "purge untracked task failed: {error}"
                    )));
                }
            };
            if matches!(outcome, PurgeSessionOutcome::Aborted) {
                if cancel.load(Ordering::Acquire) {
                    return cancelled_report(&sessions, dry_run);
                }
                return handoff_report(&sessions, &ids[index..], dry_run);
            }
            sessions.push(session_row(session_id, outcome));
        }
        Ok(report(sessions, dry_run))
    }
}

async fn purge_session_ids(
    request: &PurgeUntrackedRequest,
) -> Result<Vec<String>, PurgeUntrackedFailure> {
    let selected = match (&request.session_id, request.all) {
        (Some(session_id), false) if !session_id.is_empty() => {
            mj_core::targets::validate_session_id(session_id)
                .map_err(|error| PurgeUntrackedFailure::BadRequest(format!("{error:#}")))?;
            Some(session_id.clone())
        }
        (None, true) => None,
        _ => {
            return Err(PurgeUntrackedFailure::BadRequest(
                "name exactly one of session_id or all".to_owned(),
            ));
        }
    };
    let state = mj_core::runtime::spawn_blocking(mj_controller::database::load_state)
        .await
        .map_err(|error| {
            PurgeUntrackedFailure::Internal(format!("purge untracked task failed: {error}"))
        })?
        .map_err(|error| PurgeUntrackedFailure::Internal(format!("{error:#}")))?;
    match selected {
        Some(session_id) => {
            if !state.sessions.contains_key(&session_id) {
                return Err(PurgeUntrackedFailure::NotFound(format!(
                    "unknown session {session_id}"
                )));
            }
            Ok(vec![session_id])
        }
        None => Ok(state.sessions.keys().cloned().collect()),
    }
}

fn busy_row(
    single: bool,
    session_id: &str,
) -> Result<PurgeUntrackedSession, PurgeUntrackedFailure> {
    if single {
        Err(PurgeUntrackedFailure::Conflict(format!(
            "{LIFECYCLE_BUSY} for session {session_id}"
        )))
    } else {
        Ok(skipped(session_id, LIFECYCLE_BUSY))
    }
}

fn cancelled_report(
    sessions: &[PurgeUntrackedSession],
    dry_run: bool,
) -> Result<PurgeUntrackedResponse, PurgeUntrackedFailure> {
    if sessions.is_empty() {
        Err(PurgeUntrackedFailure::Unavailable {
            message: CANCELLED.to_owned(),
            handoff: false,
        })
    } else {
        Ok(report(sessions.to_vec(), dry_run))
    }
}

fn handoff_report(
    sessions: &[PurgeUntrackedSession],
    remaining: &[String],
    dry_run: bool,
) -> Result<PurgeUntrackedResponse, PurgeUntrackedFailure> {
    if sessions.is_empty() {
        return Err(PurgeUntrackedFailure::Unavailable {
            message: HANDOFF_MESSAGE.to_owned(),
            handoff: true,
        });
    }
    let mut sessions = sessions.to_vec();
    for session_id in remaining {
        sessions.push(skipped(session_id, SHUTTING_DOWN));
    }
    Ok(report(sessions, dry_run))
}

fn report(sessions: Vec<PurgeUntrackedSession>, dry_run: bool) -> PurgeUntrackedResponse {
    let total_bytes_reclaimed = sessions.iter().map(|session| session.bytes_reclaimed).sum();
    PurgeUntrackedResponse {
        sessions,
        total_bytes_reclaimed,
        dry_run,
    }
}

fn skipped(session_id: &str, reason: &str) -> PurgeUntrackedSession {
    PurgeUntrackedSession {
        session_id: session_id.to_owned(),
        status: PurgeUntrackedStatus::Skipped,
        bytes_reclaimed: 0,
        reason: Some(reason.to_owned()),
        stash_untracked_notes: Vec::new(),
        warnings: Vec::new(),
    }
}

fn failed(session_id: &str, reason: &str) -> PurgeUntrackedSession {
    PurgeUntrackedSession {
        session_id: session_id.to_owned(),
        status: PurgeUntrackedStatus::Failed,
        bytes_reclaimed: 0,
        reason: Some(reason.to_owned()),
        stash_untracked_notes: Vec::new(),
        warnings: Vec::new(),
    }
}

fn session_row(session_id: &str, outcome: PurgeSessionOutcome) -> PurgeUntrackedSession {
    match outcome {
        PurgeSessionOutcome::Purged {
            bytes_reclaimed,
            stash_untracked_notes,
            warnings,
        } => PurgeUntrackedSession {
            session_id: session_id.to_owned(),
            status: PurgeUntrackedStatus::Purged,
            bytes_reclaimed,
            reason: None,
            stash_untracked_notes: wire_notes(stash_untracked_notes),
            warnings,
        },
        PurgeSessionOutcome::DryRunWouldPurge {
            bytes_reclaimed,
            stash_untracked_notes,
            warnings,
        } => PurgeUntrackedSession {
            session_id: session_id.to_owned(),
            status: PurgeUntrackedStatus::DryRun,
            bytes_reclaimed,
            reason: None,
            stash_untracked_notes: wire_notes(stash_untracked_notes),
            warnings,
        },
        PurgeSessionOutcome::Skipped { reason } => skipped(session_id, &reason),
        // The loop returns a handoff or cancel report before rendering a row.
        PurgeSessionOutcome::Aborted => skipped(session_id, SHUTTING_DOWN),
    }
}

fn wire_notes(
    notes: Vec<mj_checkpoint::archive::StashUntrackedNote>,
) -> Vec<PurgeUntrackedStashNote> {
    notes
        .into_iter()
        .map(|note| PurgeUntrackedStashNote {
            repository_id: note.repository_id,
            commit: note.commit,
            message: note.message,
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use mj_checkpoint::archive::{RepositoryMetadata, RepositorySnapshot, write_archive_atomic};
    use mj_core::state::{CheckpointMetadata, SessionState};

    const CHILD: &str = "MJ_PURGE_UNTRACKED_DAEMON_TEST";
    const BUSY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PURGE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const UNKNOWN: &str = "cccccccccccccccccccccccccccccccc";

    #[tokio::test]
    async fn purge_untracked_skips_a_busy_lifecycle_and_rewrites_a_stopped_legacy_archive() {
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "purge_untracked_skips_a_busy_lifecycle_and_rewrites_a_stopped_legacy_archive",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "purge-untracked-daemon")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let (busy_archive, busy_sha) = save_legacy_session(BUSY);
        let (purge_archive, purge_sha) = save_legacy_session(PURGE);
        let busy_bytes = std::fs::read(&busy_archive).unwrap();
        let purge_bytes = std::fs::read(&purge_archive).unwrap();
        // Keep the worker-upgrade coordinator. Dropping it closes the shared
        // recovery gate, and a closed gate refuses the purge admission.
        let (state, _recovery, _upgrades) =
            crate::daemon::tests::test_runtime_state_loading_the_store_with_live_recovery();
        let release = Arc::new(tokio::sync::Notify::new());
        let (started, ready) = tokio::sync::oneshot::channel();
        let watch = state
            .start_or_join_lifecycle(BUSY.to_owned(), crate::daemon::LifecycleKind::Resume, {
                let release = release.clone();
                move |_state, _session_id, _cancelled, _admission| async move {
                    started.send(()).unwrap();
                    release.notified().await;
                    Ok(crate::daemon::DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        ready.await.unwrap();

        let dry = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: None,
                    all: true,
                    dry_run: true,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.sessions.len(), 2);
        assert_eq!(dry.sessions[0].session_id, BUSY);
        assert_eq!(dry.sessions[0].status, PurgeUntrackedStatus::Skipped);
        assert_eq!(
            dry.sessions[0].reason.as_deref(),
            Some("lifecycle operation in progress")
        );
        assert_eq!(dry.sessions[1].session_id, PURGE);
        assert_eq!(
            dry.sessions[1].status,
            PurgeUntrackedStatus::DryRun,
            "{}",
            dry.sessions[1].reason.as_deref().unwrap_or("")
        );
        let purge_len = purge_bytes.len() as u64;
        let inspected = mj_checkpoint::archive::verify_archive_streaming(&purge_archive).unwrap();
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
        assert!(dry.sessions[1].bytes_reclaimed > 0);
        assert!(
            dry.sessions[1].bytes_reclaimed < uncompressed,
            "dry run reported {}, the uncompressed excess is {uncompressed}",
            dry.sessions[1].bytes_reclaimed
        );
        assert_eq!(dry.total_bytes_reclaimed, dry.sessions[1].bytes_reclaimed);
        assert_eq!(std::fs::read(&purge_archive).unwrap(), purge_bytes);
        assert_eq!(std::fs::read(&busy_archive).unwrap(), busy_bytes);

        let conflict = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: Some(BUSY.to_owned()),
                    all: false,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
        match conflict {
            PurgeUntrackedFailure::Conflict(message) => {
                assert!(
                    message.contains("lifecycle operation in progress"),
                    "{message}"
                );
                assert!(message.contains(BUSY), "{message}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        let missing = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: Some(UNKNOWN.to_owned()),
                    all: false,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
        match missing {
            PurgeUntrackedFailure::NotFound(message) => {
                assert!(message.contains(UNKNOWN), "{message}");
            }
            other => panic!("expected not found, got {other:?}"),
        }

        let live = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: None,
                    all: true,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert!(!live.dry_run);
        assert_eq!(live.sessions[0].status, PurgeUntrackedStatus::Skipped);
        assert_eq!(
            live.sessions[0].reason.as_deref(),
            Some("lifecycle operation in progress")
        );
        assert_eq!(live.sessions[1].status, PurgeUntrackedStatus::Purged);
        assert!(live.sessions[1].bytes_reclaimed > 0);
        assert!(live.sessions[1].bytes_reclaimed < uncompressed);
        assert!(!purge_archive.exists());
        assert_eq!(std::fs::read(&busy_archive).unwrap(), busy_bytes);
        let loaded = mj_controller::database::load_state().unwrap();
        let rewritten = loaded.sessions[PURGE].checkpoint.as_ref().unwrap();
        assert_ne!(rewritten.archive_path, purge_archive);
        assert_ne!(rewritten.sha256, purge_sha);
        assert!(rewritten.archive_path.is_file());
        assert_eq!(
            live.sessions[1].bytes_reclaimed,
            purge_len.saturating_sub(rewritten.archive_path.metadata().unwrap().len())
        );
        let verified =
            mj_checkpoint::archive::verify_archive_streaming(&rewritten.archive_path).unwrap();
        assert_eq!(verified.archive_sha256, rewritten.sha256);
        let busy_checkpoint = loaded.sessions[BUSY].checkpoint.as_ref().unwrap();
        assert_eq!(busy_checkpoint.archive_path, busy_archive);
        assert_eq!(busy_checkpoint.sha256, busy_sha);

        release.notify_one();
        crate::daemon::RuntimeState::wait_lifecycle_result(watch)
            .await
            .unwrap();
        let released = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: Some(BUSY.to_owned()),
                    all: false,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(released.sessions.len(), 1);
        assert_eq!(released.sessions[0].status, PurgeUntrackedStatus::Purged);
        assert!(released.sessions[0].bytes_reclaimed > 0);
        assert!(!busy_archive.exists());
    }

    #[tokio::test]
    async fn purge_untracked_all_reports_a_missing_archive_and_still_purges_the_rest() {
        const CHILD: &str = "MJ_PURGE_UNTRACKED_MISSING_ARCHIVE";
        const FIRST: &str = "11111111111111111111111111111111";
        const MISSING: &str = "22222222222222222222222222222222";
        const NEXT: &str = "33333333333333333333333333333333";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "purge_untracked_all_reports_a_missing_archive_and_still_purges_the_rest",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "purge-untracked-missing")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let (first_archive, first_sha) = save_legacy_session(FIRST);
        let (missing_archive, missing_sha) = save_legacy_session(MISSING);
        std::fs::remove_file(&missing_archive).unwrap();
        let (next_archive, next_sha) = save_legacy_session(NEXT);
        let (state, _recovery, _upgrades) =
            crate::daemon::tests::test_runtime_state_loading_the_store_with_live_recovery();

        let single = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: Some(MISSING.to_owned()),
                    all: false,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
        match single {
            PurgeUntrackedFailure::Internal(message) => {
                assert!(message.contains("missing"), "{message}");
                assert!(message.contains(MISSING), "{message}");
            }
            other => panic!("expected an error for one session, got {other:?}"),
        }
        assert!(first_archive.is_file());
        assert!(next_archive.is_file());

        let report = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: None,
                    all: true,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(report.sessions.len(), 3);
        assert_eq!(report.sessions[0].session_id, FIRST);
        assert_eq!(report.sessions[0].status, PurgeUntrackedStatus::Purged);
        assert!(report.sessions[0].bytes_reclaimed > 0);
        assert_eq!(report.sessions[1].session_id, MISSING);
        assert_eq!(report.sessions[1].status, PurgeUntrackedStatus::Failed);
        assert_eq!(report.sessions[1].bytes_reclaimed, 0);
        let reason = report.sessions[1].reason.as_deref().unwrap_or("");
        assert!(reason.contains("missing"), "{reason}");
        assert_eq!(report.sessions[2].session_id, NEXT);
        assert_eq!(report.sessions[2].status, PurgeUntrackedStatus::Purged);
        assert!(report.sessions[2].bytes_reclaimed > 0);
        assert_eq!(
            report.total_bytes_reclaimed,
            report.sessions[0].bytes_reclaimed + report.sessions[2].bytes_reclaimed
        );
        assert!(!first_archive.exists());
        assert!(!next_archive.exists());
        let loaded = mj_controller::database::load_state().unwrap();
        assert_ne!(
            loaded.sessions[FIRST].checkpoint.as_ref().unwrap().sha256,
            first_sha
        );
        assert_ne!(
            loaded.sessions[NEXT].checkpoint.as_ref().unwrap().sha256,
            next_sha
        );
        let missing_row = loaded.sessions[MISSING].checkpoint.as_ref().unwrap();
        assert_eq!(missing_row.archive_path, missing_archive);
        assert_eq!(missing_row.sha256, missing_sha);
        assert!(!missing_archive.exists());
    }

    #[tokio::test]
    async fn purge_untracked_handoff_closes_while_a_rewrite_waits_to_commit() {
        const CHILD: &str = "MJ_PURGE_UNTRACKED_HANDOFF_DURING_REWRITE";
        const SKIPPED: &str = "11111111111111111111111111111111";
        const REWRITE: &str = "22222222222222222222222222222222";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "purge_untracked_handoff_closes_while_a_rewrite_waits_to_commit",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "purge-untracked-handoff")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let mut skipped = mj_controller::controller::test_support::checkpoint_test_session(SKIPPED);
        skipped.state = mj_core::state::SessionState::Stopped;
        skipped.checkpoint = None;
        mj_controller::database::save_session(&skipped).unwrap();
        let (archive, sha) = save_legacy_session(REWRITE);
        let original = std::fs::read(&archive).unwrap();
        let (state, _recovery, _upgrades) =
            crate::daemon::tests::test_runtime_state_loading_the_store_with_live_recovery();
        let pause = mj_controller::controller::checkpoint::arm_purge_untracked_commit_pause();
        let observed = pause.clone();
        let during = state.session_gate.clone();
        let observer = std::thread::spawn(move || {
            if !observed.wait_until_blocked(std::time::Duration::from_secs(60)) {
                observed.release();
                return None;
            }
            // The session admission is still held here. It must not stop
            // handoff: only the commit step takes upgrade-gate activity,
            // and that step has not started.
            let held = during.foreground_reservation_labels(REWRITE);
            let closed = mj_controller::upgrade::gate().try_close();
            observed.release();
            Some((closed, held))
        });
        let report = state
            .purge_untracked(
                PurgeUntrackedRequest {
                    session_id: None,
                    all: true,
                    dry_run: false,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        let (closed, held_during_rewrite) = observer
            .join()
            .expect("handoff observer")
            .expect("rewrite did not reach the commit boundary");
        assert!(
            closed,
            "handoff was blocked while the rewrite waited to commit: {:?}",
            mj_controller::upgrade::active_labels()
        );
        assert_eq!(held_during_rewrite, ["purge untracked"]);
        assert_eq!(report.sessions.len(), 2);
        assert_eq!(report.sessions[0].session_id, SKIPPED);
        assert_eq!(report.sessions[0].status, PurgeUntrackedStatus::Skipped);
        assert_eq!(
            report.sessions[0].reason.as_deref(),
            Some("session has no checkpoint")
        );
        assert_eq!(report.sessions[1].session_id, REWRITE);
        assert_eq!(report.sessions[1].status, PurgeUntrackedStatus::Skipped);
        assert_eq!(
            report.sessions[1].reason.as_deref(),
            Some("daemon is shutting down")
        );
        assert_eq!(report.total_bytes_reclaimed, 0);
        assert_eq!(std::fs::read(&archive).unwrap(), original);
        let loaded = mj_controller::database::load_state().unwrap();
        let checkpoint = loaded.sessions[REWRITE].checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.archive_path, archive);
        assert_eq!(checkpoint.sha256, sha);
        let archives: Vec<_> = std::fs::read_dir(archive.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".hel.zip") || name.contains(".tmp"))
            .collect();
        assert_eq!(
            archives,
            vec![archive.file_name().unwrap().to_string_lossy().into_owned()]
        );
        // The upgrade gate is closed, so try_enter_foreground would refuse
        // with daemon handoff even after this session's admission is gone.
        assert!(
            state
                .session_gate
                .foreground_reservation_labels(REWRITE)
                .is_empty(),
            "foreground admission was still held after the rewrite joined"
        );
    }

    fn save_legacy_session(session_id: &str) -> (PathBuf, String) {
        let directory = mj_core::config::sessions_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let repository = RepositorySnapshot {
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
            untracked_tar: untracked_tar("pad.txt", &vec![b'a'; 8 * 1024]),
            operation_payloads: None,
            linked_worktrees: Vec::new(),
        };
        let input = mj_controller::controller::test_support::checkpoint_archive_input(
            session_id,
            7,
            vec![repository],
            Vec::new(),
        );
        let archive_path = directory.join(format!(
            "{session_id}-7-archive-0123456789abcdef0123456789abcdef.hel.zip"
        ));
        let verified = write_archive_atomic(&archive_path, &input).unwrap();
        let sha256 = verified.archive_sha256;
        let mut session =
            mj_controller::controller::test_support::checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.checkpoint = Some(Box::new(CheckpointMetadata {
            archive_path: archive_path.clone(),
            sha256: sha256.clone(),
            created_at: "2026-08-14T12:00:00Z".into(),
            event_frontier: 7,
        }));
        mj_controller::database::save_session(&session).unwrap();
        (archive_path, sha256)
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
}
