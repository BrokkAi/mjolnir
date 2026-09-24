//! Move intent uses the same guarded writer as session lifecycle transitions.

use super::*;
use mj_core::state::MoveOperation;

pub fn save_move_operation(operation: &MoveOperation) -> Result<()> {
    let operation = operation.clone();
    submit_database_write("save_move_operation", move |connection| {
        save_move_operation_with(connection, &operation)
    })
}

pub(super) fn save_move_operation_with(
    connection: &Connection,
    operation: &MoveOperation,
) -> Result<()> {
    connection.execute(
        "INSERT INTO session_moves(session_id, operation_id, operation_json) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET operation_id=excluded.operation_id,
             operation_json=CASE WHEN session_moves.operation_id=excluded.operation_id
                 AND json_extract(session_moves.operation_json, '$.cancellation_requested')=1
                 THEN json_set(excluded.operation_json, '$.cancellation_requested', json('true'))
                 ELSE excluded.operation_json END",
        params![
            operation.selection.session_id,
            operation.operation_id,
            serde_json::to_string(operation)?
        ],
    )?;
    Ok(())
}

pub fn load_move_operation(session_id: &str) -> Result<Option<MoveOperation>> {
    let connection = open_reader(&database_path())?;
    load_move_operation_with(&connection, session_id)
}

pub(super) fn load_move_operation_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<MoveOperation>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT operation_json FROM session_moves WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json.and_then(|json| decode_move_operation(session_id, &json)))
}

/// Decode one stored move intent, or `None` when it no longer decodes.
///
/// A move intent recorded before a harness was removed keeps that harness in
/// its recovery snapshot, so it fails to decode under a binary that no longer
/// knows the harness. Nothing deletes a completed intent, so such rows linger
/// indefinitely (BrokkAi/mjolnir#1026). Every reader asks the same question,
/// "is there a move to act on for this session?", and a move whose snapshot
/// names a removed harness cannot be acted on, so all readers treat the row as
/// absent with a warning rather than failing the operation that asked, whether
/// that is daemon startup, a stop, a checkpoint, or a new move.
fn decode_move_operation(session_id: &str, json: &str) -> Option<MoveOperation> {
    match serde_json::from_str::<MoveOperation>(json) {
        Ok(operation) => Some(operation),
        Err(error) => {
            tracing::warn!(
                session_id,
                %error,
                "durable move intent no longer decodes; treating it as absent (its harness may have been removed)"
            );
            None
        }
    }
}

pub fn load_move_operations() -> Result<Vec<MoveOperation>> {
    let connection = open_reader(&database_path())?;
    load_move_operations_with(&connection)
}

pub(super) fn load_move_operations_with(connection: &Connection) -> Result<Vec<MoveOperation>> {
    let mut statement = connection
        .prepare("SELECT session_id, operation_json FROM session_moves ORDER BY session_id")?;
    // The daemon loads every intent at startup, so one undecodable row would
    // otherwise stop the daemon from starting; `decode_move_operation` says why
    // such a row is skipped rather than fatal.
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut operations = Vec::new();
    for row in rows {
        let (session_id, json) = row?;
        if let Some(operation) = decode_move_operation(&session_id, &json) {
            operations.push(operation);
        }
    }
    Ok(operations)
}

pub fn move_checkpoint_is_retained(path: &Path) -> Result<bool> {
    Ok(load_move_operations()?.iter().any(|operation| {
        operation.retains_checkpoint()
            && operation
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.archive_path == path)
    }))
}

/// Delete the rows of finished moves that can no longer act, and report how
/// many went away.
///
/// A finished move keeps its row only for what still reads it: the resume
/// dialog's "move needs recovery" offer and the viewer's retry projection, both
/// of which restore a destination from the move's own checkpoint archive. Once
/// that archive is gone the offer cannot do anything, so the row is litter every
/// reader has to skip, and a lingering row is what wedged daemon startup once
/// its harness was removed (BrokkAi/mjolnir#1026).
///
/// [`MoveOperation::retains_checkpoint`] already draws the line: it is false
/// only for a completed or cancelled move that is not mid queue admission. A
/// move that is preparing, closing, resuming, starting a queue, failed, or
/// holding a partly admitted queue keeps its row however its archive looks.
/// Deleting rows needs no migration.
pub fn reap_finished_move_intents() -> Result<usize> {
    submit_database_write("reap_finished_move_intents", |connection| {
        reap_finished_move_intents_with(connection)
    })
}

pub(super) fn reap_finished_move_intents_with(connection: &Connection) -> Result<usize> {
    let mut reaped = 0;
    for operation in load_move_operations_with(connection)? {
        if operation.retains_checkpoint()
            || operation
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.archive_path.exists())
        {
            continue;
        }
        let removed = connection.execute(
            "DELETE FROM session_moves WHERE session_id=?1 AND operation_id=?2",
            params![operation.selection.session_id, operation.operation_id],
        )?;
        if removed > 0 {
            tracing::debug!(
                session_id = operation.selection.session_id,
                operation_id = operation.operation_id,
                phase = ?operation.phase,
                "reaped the durable row of a finished move whose checkpoint is gone"
            );
            reaped += removed;
        }
    }
    Ok(reaped)
}

pub fn request_move_cancellation(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("request_move_cancellation", move |connection| {
        connection.execute(
            "UPDATE session_moves SET operation_json=json_set(operation_json, '$.cancellation_requested', json('true'))
             WHERE session_id=?1 AND json_extract(operation_json, '$.phase') IN ('preparing','closing_source','resuming_destination','starting_queue')",
            [session_id],
        )?;
        Ok(())
    })
}

pub fn clear_move_cancellation_for_retry(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("clear_move_cancellation_for_retry", move |connection| {
        connection.execute(
            "UPDATE session_moves SET operation_json=json_set(operation_json, '$.cancellation_requested', json('false')) WHERE session_id=?1",
            [session_id],
        )?;
        Ok(())
    })
}

/// Read the confirmation data without loading the conversation history.
pub fn move_pending_work(session_id: &str) -> Result<(bool, Vec<MaterializedQueuedPrompt>)> {
    let connection = open_reader(&database_path())?;
    let running: Option<String> = connection
        .query_row(
            "SELECT execution_state FROM materialized_sessions WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok((
        running.as_deref() == Some("running"),
        read_materialized_queued_prompts(&connection, session_id)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MovePhase, MoveSelection, ResumeQueueDisposition};

    fn operation(session: &SessionRecord) -> MoveOperation {
        MoveOperation {
            in_place: false,
            source_checkpoint_only: false,
            operation_id: "move-one".into(),
            selection: MoveSelection {
                clear_resource_allocation: false,
                session_id: session.id.clone(),
                profile_id: Some("destination".into()),
                target_template_id: Some("local".into()),
                additional_mounts: Some(Vec::new()),
                resource_allocation: None,
            },
            source_profile_id: session.last_profile.clone(),
            source_target_template_id: session.target_template_id.clone(),
            source_target: session.target.clone(),
            source_native_session_id: session.native_session_id.clone(),
            source_additional_mounts: session.additional_mounts.clone(),
            source_resource_allocation: session.resource_allocation.clone(),
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: "fingerprint".into(),
            checkpoint: session.checkpoint.clone(),
            recovery_session: Some(session.clone()),
            queue: ResumeQueueDisposition::Start,
            phase: MovePhase::Preparing,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            error: None,
        }
    }

    #[test]
    fn move_boundaries_survive_database_reopen_and_retain_the_source_locator() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let mut session = super::super::tests::session("move-reopen", "project");
        let template: mj_core::config::TargetTemplate = serde_json::from_str(
            r#"{"kind":"ssh-podman","host":"original.test","image":"test","user":"builder"}"#,
        )
        .unwrap();
        session.target_runtime = Some((&template).into());
        session.target = Some(mj_core::state::TargetLocator::SshPodman {
            host: "original.test".into(),
            container_id: "source-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        save_session_to(&path, &session).unwrap();
        let mut intent = operation(&session);
        for phase in [
            MovePhase::Preparing,
            MovePhase::ClosingSource,
            MovePhase::ResumingDestination,
            MovePhase::StartingQueue,
            MovePhase::Failed,
            MovePhase::Completed,
        ] {
            intent.phase = phase;
            if phase == MovePhase::StartingQueue {
                intent.queue_admission_started = true;
                intent.destination_target = session.target.clone();
                intent.destination_store_id = Some("durable-destination".into());
            }
            if phase == MovePhase::Completed {
                intent.queue_admission_finished = true;
            }
            let connection = open(&path).unwrap();
            save_move_operation_with(&connection, &intent).unwrap();
            drop(connection);
            let reopened = open_reader(&path).unwrap();
            let restored = load_move_operation_with(&reopened, &session.id)
                .unwrap()
                .unwrap();
            assert_eq!(restored, intent);
            assert_eq!(restored.source_target, session.target);
            assert_eq!(restored.retains_checkpoint(), phase != MovePhase::Completed);
        }
    }

    #[test]
    fn bulk_load_skips_a_move_intent_whose_harness_no_longer_decodes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let good = super::super::tests::session("move-good", "project");
        let stale_session = super::super::tests::session("move-removed-harness", "project");
        save_session_to(&path, &good).unwrap();
        save_session_to(&path, &stale_session).unwrap();
        let connection = open(&path).unwrap();
        save_move_operation_with(&connection, &operation(&good)).unwrap();
        let mut stale_operation = operation(&stale_session);
        stale_operation.operation_id = "move-two".into();
        save_move_operation_with(&connection, &stale_operation).unwrap();
        // Simulate a row recorded before a harness was removed: rewrite the
        // stored recovery snapshot to name a harness the current binary no
        // longer knows, so the row fails to decode.
        let rewritten = connection
            .execute(
                "UPDATE session_moves
                 SET operation_json = replace(operation_json, '\"harness_kind\":\"codex\"', '\"harness_kind\":\"zcode\"')
                 WHERE session_id = 'move-removed-harness'",
                [],
            )
            .unwrap();
        assert_eq!(
            rewritten, 1,
            "the test session must store a codex harness to rewrite"
        );
        let loaded = load_move_operations_with(&connection).unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "the undecodable row must be skipped, not fail the load"
        );
        assert_eq!(loaded[0].selection.session_id, good.id);
    }

    #[test]
    fn in_place_intent_round_trips_and_a_legacy_row_without_it_reads_as_a_fresh_environment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let session = super::super::tests::session("move-in-place", "project");
        save_session_to(&path, &session).unwrap();
        let connection = open(&path).unwrap();
        let mut intent = operation(&session);
        intent.in_place = true;
        save_move_operation_with(&connection, &intent).unwrap();
        let stored: String = connection
            .query_row(
                "SELECT operation_json FROM session_moves WHERE session_id=?1",
                [&session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored.contains("\"in_place\":true"),
            "the in-place choice must be durable: {stored}"
        );
        let restored = load_move_operation_with(&connection, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored, intent);
        // A row written before this field existed must still decode, as the
        // full fresh-environment move it was.
        let rewritten = connection
            .execute(
                "UPDATE session_moves
                 SET operation_json = replace(operation_json, '\"in_place\":true,', '')
                 WHERE session_id = ?1",
                [&session.id],
            )
            .unwrap();
        assert_eq!(rewritten, 1);
        let legacy = load_move_operation_with(&connection, &session.id)
            .unwrap()
            .expect("a row without in_place must still decode");
        assert!(!legacy.in_place);
    }

    #[test]
    fn per_session_load_treats_an_intent_whose_harness_no_longer_decodes_as_absent() {
        // The stop, checkpoint, recovery, and new-move paths each read the one
        // intent for their session. A completed move whose recovery snapshot
        // names a removed harness must not fail those operations.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let stale_session = super::super::tests::session("move-removed-harness", "project");
        save_session_to(&path, &stale_session).unwrap();
        let connection = open(&path).unwrap();
        let mut stale_operation = operation(&stale_session);
        stale_operation.phase = MovePhase::Completed;
        save_move_operation_with(&connection, &stale_operation).unwrap();
        let rewritten = connection
            .execute(
                "UPDATE session_moves
                 SET operation_json = replace(operation_json, '\"harness_kind\":\"codex\"', '\"harness_kind\":\"zcode\"')
                 WHERE session_id = 'move-removed-harness'",
                [],
            )
            .unwrap();
        assert_eq!(
            rewritten, 1,
            "the test session must store a codex harness to rewrite"
        );
        let loaded = load_move_operation_with(&connection, &stale_session.id)
            .expect("an undecodable intent must not fail the read");
        assert!(
            loaded.is_none(),
            "the undecodable intent is treated as absent"
        );
    }

    #[test]
    fn reaping_deletes_a_finished_move_only_once_its_checkpoint_archive_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let present = directory.path().join("present.hel.zip");
        std::fs::write(&present, b"archive").unwrap();
        let gone = directory.path().join("gone.hel.zip");
        // session id, phase, whether its archive is still on disk, whether its
        // queue admission is half done, and whether the row must survive.
        let cases = [
            (
                "completed-retained",
                MovePhase::Completed,
                true,
                false,
                true,
            ),
            ("completed-gone", MovePhase::Completed, false, false, false),
            (
                "completed-admitting",
                MovePhase::Completed,
                false,
                true,
                true,
            ),
            (
                "cancelled-retained",
                MovePhase::Cancelled,
                true,
                false,
                true,
            ),
            ("cancelled-gone", MovePhase::Cancelled, false, false, false),
            ("failed-gone", MovePhase::Failed, false, false, true),
            (
                "running-gone",
                MovePhase::ResumingDestination,
                false,
                false,
                true,
            ),
        ];
        for (session_id, phase, archive_present, mid_admission, _) in cases {
            let session = super::super::tests::session(session_id, "project");
            save_session_to(&path, &session).unwrap();
            let connection = open(&path).unwrap();
            let mut intent = operation(&session);
            intent.operation_id = format!("{session_id}-operation");
            intent.phase = phase;
            intent.queue_admission_started = mid_admission;
            intent.checkpoint = Some(CheckpointMetadata {
                archive_path: if archive_present {
                    present.clone()
                } else {
                    gone.clone()
                },
                sha256: "b".repeat(64),
                created_at: session.created_at.clone(),
                event_frontier: 6,
            });
            save_move_operation_with(&connection, &intent).unwrap();
        }
        let connection = open(&path).unwrap();
        let reaped = reap_finished_move_intents_with(&connection).unwrap();
        assert_eq!(
            reaped,
            cases.iter().filter(|case| !case.4).count(),
            "only the finished moves whose archive is gone are reaped"
        );
        for (session_id, _, _, _, survives) in cases {
            assert_eq!(
                load_move_operation_with(&connection, session_id)
                    .unwrap()
                    .is_some(),
                survives,
                "{session_id} row survival"
            );
        }
        assert_eq!(
            reap_finished_move_intents_with(&connection).unwrap(),
            0,
            "a second sweep finds nothing left to reap"
        );
    }

    #[test]
    fn concurrent_phase_save_cannot_erase_durable_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let session = super::super::tests::session("move-cancel", "project");
        save_session_to(&path, &session).unwrap();
        let connection = open(&path).unwrap();
        let mut intent = operation(&session);
        save_move_operation_with(&connection, &intent).unwrap();
        let mut cancelled = intent.clone();
        cancelled.cancellation_requested = true;
        save_move_operation_with(&connection, &cancelled).unwrap();
        intent.phase = MovePhase::ClosingSource;
        save_move_operation_with(&connection, &intent).unwrap();
        let restored = load_move_operation_with(&connection, &session.id)
            .unwrap()
            .unwrap();
        assert!(restored.cancellation_requested);
        assert_eq!(restored.phase, MovePhase::ClosingSource);
        intent.operation_id = "explicit-new-operation".into();
        save_move_operation_with(&connection, &intent).unwrap();
        assert!(
            !load_move_operation_with(&connection, &session.id)
                .unwrap()
                .unwrap()
                .cancellation_requested
        );
    }

    #[test]
    fn cancelling_partial_queue_admission_does_not_release_its_archive() {
        let session = super::super::tests::session("move-queue", "project");
        let mut intent = operation(&session);
        intent.phase = MovePhase::Cancelled;
        intent.queue_admission_started = true;
        assert!(intent.retains_checkpoint());
        intent.queue_admission_finished = true;
        assert!(!intent.retains_checkpoint());
    }

    #[test]
    fn destination_record_install_keeps_drafts_and_titles_edited_during_move() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let mut stale = super::super::tests::session("move-draft", "project");
        save_session_to(&path, &stale).unwrap();
        let connection = open(&path).unwrap();
        save_move_operation_with(&connection, &operation(&stale)).unwrap();
        connection.execute("UPDATE sessions SET draft_input='keep this draft', session_title_override='new title' WHERE session_id=?1", [&stale.id]).unwrap();
        stale.last_profile = "destination".into();
        stale.state = SessionState::Provisioning;
        save_session_to(&path, &stale).unwrap();
        let current = load_state_from(&path)
            .unwrap()
            .sessions
            .remove(&stale.id)
            .unwrap();
        assert_eq!(current.draft_input, "keep this draft");
        assert_eq!(current.session_title_override.as_deref(), Some("new title"));
        assert_eq!(current.last_profile, "destination");
    }
}
