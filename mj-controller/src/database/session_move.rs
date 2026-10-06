//! Move intent uses the same guarded writer as session lifecycle transitions.

use super::*;
use mj_core::state::MoveOperation;

pub fn retain_move_source(operation: &MoveOperation) -> Result<()> {
    let operation = operation.clone();
    submit_database_write("retain_move_source", move |connection| {
        let transfer = operation
            .workspace_transfer
            .as_ref()
            .context("Move transfer missing")?;
        connection.execute(
            "INSERT INTO retained_move_sources(operation_id, session_id, source_json, exclusions_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(operation_id) DO NOTHING",
            params![operation.operation_id, operation.selection.session_id,
                serde_json::to_string(&transfer.source)?, serde_json::to_string(&operation.selection.workspace.exclusions)?, operation.created_at],
        )?;
        Ok(())
    })
}

pub fn retained_move_sources(
    session_id: &str,
) -> Result<Vec<mj_core::move_workspace::RetainedMoveSource>> {
    let connection = open_reader(&database_path())?;
    let mut query = connection.prepare("SELECT operation_id, source_json, exclusions_json, created_at FROM retained_move_sources WHERE session_id=?1 ORDER BY created_at")?;
    let rows = query.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (operation_id, source, exclusions, created_at) = row?;
        Ok(mj_core::move_workspace::RetainedMoveSource {
            operation_id,
            session_id: session_id.into(),
            source: serde_json::from_str(&source)?,
            exclusions: serde_json::from_str(&exclusions)?,
            created_at,
        })
    })
    .collect()
}

pub fn forget_retained_move_source(operation_id: &str) -> Result<()> {
    let operation_id = operation_id.to_owned();
    submit_database_write("forget_retained_move_source", move |connection| {
        connection.execute(
            "DELETE FROM retained_move_sources WHERE operation_id=?1",
            [operation_id],
        )?;
        Ok(())
    })
}

pub fn save_move_operation(operation: &MoveOperation) -> Result<()> {
    let operation = operation.clone();
    submit_database_write("save_move_operation", move |connection| {
        save_move_operation_with(connection, &operation)
    })
}

/// Publish destination adoption and ownership together, without changing client fields.
pub fn adopt_move_destination(operation: &MoveOperation, session: &SessionRecord) -> Result<()> {
    let operation = operation.clone();
    let session = session.clone();
    submit_database_write("adopt_move_destination", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        update_lifecycle_fields(&tx, &session)?;
        save_move_operation_with(&tx, &operation)?;
        tx.commit()?;
        Ok(())
    })
}

/// Publish a Move result and its session message as one committed outcome.
/// Update only the message fields; other owners may have changed the session.
pub fn save_move_outcome(operation: &MoveOperation, last_error: Option<&str>) -> Result<()> {
    let operation = operation.clone();
    let last_error = last_error.map(str::to_owned);
    submit_database_write("save_move_outcome", move |connection| {
        save_move_outcome_with(connection, &operation, last_error.as_deref())
    })
}

fn save_move_outcome_with(
    connection: &mut Connection,
    operation: &MoveOperation,
    last_error: Option<&str>,
) -> Result<()> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    save_move_operation_with(&tx, operation)?;
    let updated = tx.execute(
        "UPDATE sessions SET last_error=?2, updated_at=?3 WHERE session_id=?1",
        params![
            operation.selection.session_id,
            last_error,
            operation.updated_at
        ],
    )?;
    anyhow::ensure!(updated == 1, "Move outcome session is missing");
    tx.commit()?;
    Ok(())
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
    if let Some(committed) = committed_state()? {
        return Ok(committed.moves.get(session_id).cloned());
    }
    let connection = open_reader(&database_path())?;
    load_move_operation_with(&connection, session_id)
}

/// Whether a durable in-place Move currently owns sub-agent admission closed.
/// A worker actor uses this before repairing a closed gate on reconnect: an
/// in-flight Move persists its phase before closing admission, so the durable
/// row keeps reconnect repair from reopening it prematurely.
pub fn has_active_in_place_move(session_id: &str) -> Result<bool> {
    Ok(load_move_operation(session_id)?
        .as_ref()
        .is_some_and(move_holds_subagent_gate))
}

fn move_holds_subagent_gate(operation: &MoveOperation) -> bool {
    operation.in_place
        && matches!(
            operation.phase,
            mj_core::state::MovePhase::Preparing
                | mj_core::state::MovePhase::ClosingSource
                | mj_core::state::MovePhase::ResumingDestination
                | mj_core::state::MovePhase::StartingQueue
        )
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
    if let Some(committed) = committed_state()? {
        return Ok(committed.moves.values().cloned().collect());
    }
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
        operation
            .retained_archives()
            .any(|checkpoint| checkpoint.archive_path == path)
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
        if (operation.phase != mj_core::state::MovePhase::Completed
            && operation
                .prepared_destination
                .as_ref()
                .is_some_and(|destination| destination.owns_resource()))
            || operation.retains_checkpoint()
            || operation
                .restore_artifact()
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
            prepared_destination: None,
            accepted_preparation: None,
            acknowledge_interruption: false,
            workspace_transfer: None,
            handoff: None,
            in_place: false,
            source_checkpoint_only: false,
            operation_id: "move-one".into(),
            selection: MoveSelection {
                subagents: None,
                workspace: Default::default(),
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
    fn move_outcome_commits_the_message_with_the_result_and_rolls_back_both_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let mut session = super::super::tests::session("moving", "project");
        session.draft_input = "a concurrent draft".into();
        save_session_to(&path, &session).unwrap();
        let mut connection = open(&path).unwrap();
        let mut intent = operation(&session);
        save_move_operation_with(&connection, &intent).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER refuse_move_message BEFORE UPDATE OF last_error ON sessions
             BEGIN SELECT RAISE(ABORT, 'message write failed'); END;",
            )
            .unwrap();
        intent.phase = MovePhase::Failed;
        intent.error = Some("private diagnostic".into());
        assert!(save_move_outcome_with(&mut connection, &intent, Some("public message")).is_err());
        assert_eq!(
            load_move_operation_with(&connection, &session.id)
                .unwrap()
                .unwrap()
                .phase,
            MovePhase::Preparing
        );
        assert_eq!(
            load_state_from(&path).unwrap().sessions[&session.id].last_error,
            None
        );
        connection
            .execute_batch("DROP TRIGGER refuse_move_message")
            .unwrap();
        save_move_outcome_with(&mut connection, &intent, Some("public message")).unwrap();
        drop(connection);
        let restored = load_state_from(&path).unwrap();
        assert_eq!(
            restored.sessions[&session.id].last_error.as_deref(),
            Some("public message")
        );
        assert_eq!(
            restored.sessions[&session.id].draft_input,
            "a concurrent draft"
        );
        assert_eq!(
            load_move_operation_with(&open_reader(&path).unwrap(), &session.id)
                .unwrap()
                .unwrap(),
            intent
        );
    }

    #[test]
    fn committed_moves_follow_phase_changes_and_cascading_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let session = super::super::tests::session("moving", "project");
        save_session_to(&path, &session).unwrap();
        let intent = operation(&session);
        save_move_operation_with(&open(&path).unwrap(), &intent).unwrap();
        let writer = start_database_writer_at(&path, false).unwrap();
        let held = writer.writer.committed_state().unwrap();
        assert_eq!(held.moves["moving"], intent);
        let mut failed = intent.clone();
        failed.phase = MovePhase::Failed;
        writer
            .writer
            .execute("fail move", move |connection| {
                save_move_operation_with(connection, &failed)
            })
            .unwrap();
        assert_eq!(
            writer.writer.committed_state().unwrap().moves["moving"].phase,
            MovePhase::Failed
        );
        assert_eq!(held.moves["moving"].phase, MovePhase::Preparing);
        writer
            .writer
            .execute("delete moving session", |connection| {
                connection.execute("DELETE FROM sessions WHERE session_id='moving'", [])?;
                Ok(())
            })
            .unwrap();
        assert!(writer.writer.committed_state().unwrap().moves.is_empty());
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

    // Hard-won: 04d4b7f0: a completed move naming a removed harness stopped daemon startup.
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

    // Hard-won: 61db26ba: close failed on a completed move row after its harness was removed.
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

    // Hard-won: c5f1cbad: completed removed-harness move rows wedged daemon startup.
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
    fn reconnect_repair_preserves_admission_for_active_in_place_moves_only() {
        let session = super::super::tests::session("move-admission-repair", "project");
        let mut intent = operation(&session);
        intent.in_place = true;
        for phase in [
            MovePhase::Preparing,
            MovePhase::ClosingSource,
            MovePhase::ResumingDestination,
            MovePhase::StartingQueue,
        ] {
            intent.phase = phase;
            assert!(move_holds_subagent_gate(&intent), "{phase:?}");
        }
        for phase in [
            MovePhase::Completed,
            MovePhase::Failed,
            MovePhase::Cancelled,
        ] {
            intent.phase = phase;
            assert!(!move_holds_subagent_gate(&intent), "{phase:?}");
        }
        intent.in_place = false;
        intent.phase = MovePhase::ClosingSource;
        assert!(!move_holds_subagent_gate(&intent));
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
