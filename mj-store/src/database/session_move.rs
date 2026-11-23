//! Move intent uses the same guarded writer as session lifecycle transitions.

use std::borrow::Cow;

use super::*;
use mj_core::state::{
    GrantedMoveKey, LegacyMoveOperation, MoveOperation, MovePhase, MoveSelection,
    StoredMoveOperation,
};

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

/// Atomically remove revoked ephemeral SSH credentials from whichever
/// `MoveOperation` row is currently persisted for `session_id`, without
/// overwriting any other field (so background cleanup can never clobber a
/// newer Move admitted concurrently for the same session).
pub fn remove_revoked_move_granted_keys(
    session_id: &str,
    revoked: &[mj_core::state::GrantedMoveKey],
    updated_at: &str,
) -> Result<()> {
    if revoked.is_empty() {
        return Ok(());
    }
    let session_id = session_id.to_owned();
    let revoked = revoked.to_vec();
    let updated_at = updated_at.to_owned();
    submit_database_write("remove_revoked_move_granted_keys", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((operation_id, mut current)) = load_move_granted_keys_with(&tx, &session_id)?
        else {
            return Ok(());
        };
        let before = current.len();
        current.retain(|key| !revoked.contains(key));
        if current.len() != before {
            tx.execute(
                "UPDATE session_moves
                 SET operation_json=json_set(operation_json, '$.granted_keys', json(?2), '$.updated_at', ?3)
                 WHERE session_id=?1 AND operation_id=?4",
                params![session_id, serde_json::to_string(&current)?, updated_at, operation_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub fn save_move_operation(operation: &MoveOperation) -> Result<()> {
    let operation = operation.clone();
    submit_database_write("save_move_operation", move |connection| {
        save_move_operation_with(connection, &operation)
    })
}

/// Record the person's requests atomically with acceptance of the Move intent.
pub fn accept_move_operation(operation: &MoveOperation) -> Result<()> {
    let operation = operation.clone();
    submit_database_write("accept Move", move |connection| {
        let tx = connection.transaction()?;
        update_move_requests(&tx, &operation.selection)?;
        save_move_operation_with(&tx, &operation)?;
        tx.commit()?;
        Ok(())
    })
}

/// An unchanged Move still accepts the person's explicit next-creation choices.
pub fn accept_unchanged_move(selection: &MoveSelection) -> Result<()> {
    let selection = selection.clone();
    submit_database_write("accept unchanged Move", move |connection| {
        let tx = connection.transaction()?;
        update_move_requests(&tx, &selection)?;
        tx.commit()?;
        Ok(())
    })
}

fn update_move_requests(tx: &Transaction<'_>, selection: &MoveSelection) -> Result<()> {
    super::state_io::update_container_requests(
        tx,
        &selection.session_id,
        if selection.clear_resource_allocation {
            Some(None)
        } else {
            selection.resource_allocation.as_ref().map(Some)
        },
        selection.additional_mounts.as_deref(),
    )
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
/// A failed rollback can settle its own Closing record as retryable Error.
/// Other session fields retain the values written by their own owners.
pub fn save_move_outcome(
    operation: &MoveOperation,
    last_error: Option<&str>,
    fail_closing_source: bool,
) -> Result<()> {
    let operation = operation.clone();
    let last_error = last_error.map(str::to_owned);
    submit_database_write("save_move_outcome", move |connection| {
        save_move_outcome_with(
            connection,
            &operation,
            last_error.as_deref(),
            fail_closing_source,
        )
    })
}

fn save_move_outcome_with(
    connection: &mut Connection,
    operation: &MoveOperation,
    last_error: Option<&str>,
    fail_closing_source: bool,
) -> Result<()> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    save_move_operation_with(&tx, operation)?;
    let updated = tx.execute(
        "UPDATE sessions SET last_error=?2, updated_at=?3 WHERE session_id=?1",
        params![
            operation.selection.session_id,
            last_error,
            operation.updated_at,
        ],
    )?;
    anyhow::ensure!(updated == 1, "Move outcome session is missing");
    if fail_closing_source {
        tx.execute(
            "UPDATE sessions SET state='error' WHERE session_id=?1 AND state='closing'",
            [&operation.selection.session_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn save_move_operation_with(
    connection: &Connection,
    operation: &MoveOperation,
) -> Result<()> {
    if connection.is_autocommit() {
        let tx = connection.unchecked_transaction()?;
        save_move_operation_with(&tx, operation)?;
        tx.commit()?;
        return Ok(());
    }
    super::environments::record_prepared_move_environment(connection, operation)?;
    connection.execute(
        "INSERT INTO session_moves(session_id, operation_id, operation_json,source_environment_id)
         VALUES (?1, ?2, ?3,(SELECT environment_id FROM environment_bindings WHERE session_id=?1))
         ON CONFLICT(session_id) DO UPDATE SET source_environment_id=CASE WHEN session_moves.operation_id=excluded.operation_id THEN session_moves.source_environment_id ELSE excluded.source_environment_id END,
             operation_id=excluded.operation_id,
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
    super::startup::reject_failed_move_startup_prompts_with(
        connection,
        &operation.selection.session_id,
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

/// Read credential ownership even when a Move's recovery snapshot names an
/// unsupported harness. Legacy rows without grants have no credentials here.
pub fn load_move_granted_keys(session_id: &str) -> Result<Option<(String, Vec<GrantedMoveKey>)>> {
    let connection = open_reader(&database_path())?;
    load_move_granted_keys_with(&connection, session_id)
}

fn load_move_granted_keys_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(String, Vec<GrantedMoveKey>)>> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT operation_id, json_extract(operation_json, '$.granted_keys')
             FROM session_moves WHERE session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(id, keys)| {
        let keys = keys
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        Ok((id, keys))
    })
    .transpose()
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
    operation.in_place && operation.is_active()
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
    Ok(json.and_then(|json| decode_move_operation(connection, session_id, &json)))
}

/// Release only the cancelled Move's restore authority. Placement ownership
/// has already been settled; do not adopt or recreate it while resolving.
pub(super) fn resolve_cancelled_move_for_close_with(
    connection: &Connection,
    session_id: &str,
    operation_id: &str,
    updated_at: &str,
) -> Result<()> {
    let mut operation = load_move_operation_with(connection, session_id)?
        .context("superseded Move disappeared before close publication")?;
    ensure!(
        operation.operation_id == operation_id
            && operation.phase == mj_core::state::MovePhase::Cancelled
            && !operation.queue_admission_started,
        "superseded Move no longer permits close settlement"
    );
    operation.queue_admission_finished = true;
    operation.updated_at = updated_at.to_owned();
    connection.execute(
        "UPDATE session_moves SET operation_json=?2, close_requested_operation_id=NULL WHERE session_id=?1",
        params![session_id, serde_json::to_string(&operation)?],
    )?;
    Ok(())
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
fn decode_move_operation(
    connection: &Connection,
    session_id: &str,
    json: &str,
) -> Option<MoveOperation> {
    match normalize_stored_move(connection, session_id, json)
        .and_then(|json| Ok(serde_json::from_str::<MoveOperation>(&json)?))
    {
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

/// Older Move intents can hold the opt-in in the sealed selection, its
/// preparation receipt, and the rollback snapshot. Translate those stored
/// fields at the reader boundary so strict public requests keep one policy.
fn normalize_stored_move<'a>(
    connection: &Connection,
    session_id: &str,
    json: &'a str,
) -> Result<Cow<'a, str>> {
    if !json.contains(r#""supervisors""#) {
        return Ok(Cow::Borrowed(json));
    }

    fn fold_orchestrators(
        record: &mut serde_json::Value,
        source_policy: &serde_json::Value,
    ) -> Result<()> {
        let Some(record) = record.as_object_mut() else {
            return Ok(());
        };
        let Some(enabled) = record
            .remove("supervisors")
            .filter(|value| !value.is_null())
        else {
            return Ok(());
        };
        let enabled = serde_json::from_value::<bool>(enabled)?;
        let policy = record
            .get("subagents")
            .filter(|policy| !policy.is_null())
            .unwrap_or(source_policy);
        if policy.is_null() {
            return Ok(());
        }
        let mut policy = mj_core::subagent::parse_stored_policy(&policy.to_string())?;
        if let mj_core::subagent::SubagentPolicy::Mjolnir { orchestrators, .. } = &mut policy {
            *orchestrators = enabled;
            record.insert("subagents".into(), serde_json::to_value(policy)?);
        }
        Ok(())
    }

    let mut intent: serde_json::Value = serde_json::from_str(json)?;
    if let Some(snapshot) = intent.get_mut("recovery_session") {
        fold_orchestrators(snapshot, &serde_json::Value::Null)?;
    }
    let source_policy = match intent.pointer("/recovery_session/subagents") {
        Some(policy) if !policy.is_null() => policy.clone(),
        _ => {
            let policy: Option<String> = connection
                .query_row(
                    "SELECT subagents FROM sessions WHERE session_id=?1",
                    [session_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            policy.map_or(Ok(serde_json::Value::Null), |policy| {
                serde_json::from_str(&policy)
            })?
        }
    };
    for path in ["/selection", "/accepted_preparation/selection"] {
        if let Some(selection) = intent.pointer_mut(path) {
            fold_orchestrators(selection, &source_policy)?;
        }
    }
    Ok(Cow::Owned(serde_json::to_string(&intent)?))
}

pub fn load_move_operations() -> Result<Vec<MoveOperation>> {
    if let Some(committed) = committed_state()? {
        return Ok(committed.moves.values().cloned().collect());
    }
    let connection = open_reader(&database_path())?;
    load_move_operations_with(&connection)
}

/// Load any pre-148 `LegacyMoveOperation` rows still present in `session_moves`
/// so startup recovery can roll back source-owned moves or finish forward
/// destination-owned moves.
pub fn load_legacy_move_operations() -> Result<Vec<(String, LegacyMoveOperation)>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection
        .prepare("SELECT session_id, operation_json FROM session_moves ORDER BY session_id")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut legacy = Vec::new();
    for row in rows {
        let (session_id, json) = row?;
        let stored = normalize_stored_move(&connection, &session_id, &json)
            .and_then(|json| StoredMoveOperation::decode_json(&json));
        if let Ok(StoredMoveOperation::Legacy(op)) = stored {
            legacy.push((session_id, *op));
        }
    }
    Ok(legacy)
}

/// Delete a settled legacy `MoveOperation` row after startup recovery.
pub fn delete_move_operation(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("delete_move_operation", move |connection| {
        connection.execute(
            "DELETE FROM session_moves WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    })
}

/// Single-transaction commit point for a cross-target Move: moves the operation
/// to `DestinationOwns` and updates the session's placement to the destination.
pub fn commit_move_destination_owns(
    operation: &MoveOperation,
    destination_session: &SessionRecord,
) -> Result<()> {
    ensure!(
        operation.phase == MovePhase::DestinationOwns,
        "commit_move_destination_owns requires MovePhase::DestinationOwns"
    );
    let operation = operation.clone();
    let destination_session = destination_session.clone();
    submit_database_write("commit_move_destination_owns", move |connection| {
        let tx = connection.transaction()?;
        super::state_io::update_lifecycle_fields(&tx, &destination_session)?;
        super::state_io::replace_targets(&tx, &destination_session)?;
        save_move_operation_with(&tx, &operation)?;
        let purged = if !operation.in_place {
            let destination_environment: String = tx.query_row(
                "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                [&destination_session.id],
                |row| row.get(0),
            )?;
            super::sessions::purge_borrowed_descendants_in_tx(
                &tx,
                &destination_session.id,
                Some(&destination_environment),
            )?
        } else {
            Vec::new()
        };
        tx.commit()?;
        super::tool_details::unlink_deleted_sessions(&database_path(), &purged);
        Ok(())
    })
}

/// Single-transaction durable transition for an in-place same-target Move
/// recording `DispatchReleased` before telling the worker to release its hold.
pub fn commit_in_place_dispatch_released(operation: &MoveOperation) -> Result<()> {
    ensure!(
        operation.phase == MovePhase::DispatchReleased,
        "commit_in_place_dispatch_released requires MovePhase::DispatchReleased"
    );
    let operation = operation.clone();
    submit_database_write("commit_in_place_dispatch_released", move |connection| {
        let tx = connection.transaction()?;
        save_move_operation_with(&tx, &operation)?;
        tx.commit()?;
        Ok(())
    })
}

/// Read Move and close intent together; an unmatched/stale close ID is inert.
pub(super) fn exact_move_close_intent(
    decoded_id: &str,
    row_id: &str,
    close_id: Option<&str>,
) -> bool {
    decoded_id == row_id && close_id == Some(row_id)
}

/// The durable owner of an interrupted Move, shared by startup and intent GC.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MoveRecoveryOwner {
    Move,
    Close,
}

pub fn move_recovery_owner(
    state: Option<SessionState>,
    exact_close_intent: bool,
) -> Option<MoveRecoveryOwner> {
    if exact_close_intent {
        Some(MoveRecoveryOwner::Close)
    } else if matches!(
        state,
        Some(SessionState::Closing | SessionState::Destroying)
    ) {
        Some(MoveRecoveryOwner::Move)
    } else {
        None
    }
}

pub fn load_move_recovery() -> Result<(Vec<MoveOperation>, BTreeSet<String>)> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare("SELECT session_id, operation_id, operation_json, close_requested_operation_id FROM session_moves ORDER BY session_id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut operations = Vec::new();
    let mut closes = BTreeSet::new();
    for row in rows {
        let (session_id, operation_id, json, close_id) = row?;
        if let Some(operation) = decode_move_operation(&connection, &session_id, &json) {
            if exact_move_close_intent(&operation.operation_id, &operation_id, close_id.as_deref())
            {
                closes.insert(operation_id);
            }
            operations.push(operation);
        }
    }
    Ok((operations, closes))
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
        if let Some(operation) = decode_move_operation(connection, &session_id, &json) {
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
        let (state, close_id): (Option<String>, Option<String>) = connection.query_row(
            "SELECT s.state, m.close_requested_operation_id FROM session_moves m LEFT JOIN sessions s ON s.session_id=m.session_id WHERE m.session_id=?1 AND m.operation_id=?2",
            params![operation.selection.session_id, operation.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if move_recovery_owner(
            state.as_deref().map(stored_session_state),
            exact_move_close_intent(
                &operation.operation_id,
                &operation.operation_id,
                close_id.as_deref(),
            ),
        )
        .is_some()
        {
            continue;
        }
        if (operation.phase != mj_core::state::MovePhase::Completed
            && operation
                .prepared_destination
                .as_ref()
                .is_some_and(|destination| destination.owns_resource()))
            || operation.retains_checkpoint()
            || !operation.granted_keys.is_empty()
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
             WHERE session_id=?1 AND json_extract(operation_json, '$.phase') IN (
                 'capturing','transferring','restoring','destination_starting',
                 'destination_owns','source_retired','queue_admitted','dispatch_released',
                 'preparing','closing_source','resuming_destination','starting_queue'
             )",
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

/// Cleanup uses the accepted source owner, independent of the current binding.
pub fn move_source_environment(operation_id: &str) -> Result<Option<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let id: Option<String> = connection
        .query_row(
            "SELECT source_environment_id FROM session_moves WHERE operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    id.map(|id| super::environments::read_environment(&connection, &id))
        .transpose()
        .map(Option::flatten)
}

pub fn retained_move_environment(operation_id: &str) -> Result<Option<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let id: Option<String> = connection
        .query_row(
            "SELECT environment_id FROM retained_move_sources WHERE operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    id.map(|id| super::environments::read_environment(&connection, &id))
        .transpose()
        .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MovePhase, MoveSelection, ResumeQueueDisposition};

    #[test]
    fn schema_preserves_legacy_move_orchestrator_choices() {
        use mj_core::subagent::SubagentPolicy;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-move.sqlite");
        let mut source = super::super::tests::session("source", "project");
        source.subagents = Some(SubagentPolicy::Mjolnir {
            straightforward: Some(Box::new(mj_core::config::SubagentChoice {
                model: "my-worker-model".into(),
                effort: None,
            })),
            subtle: None,
            orchestrators: true,
        });
        let mut connection = open(&path).unwrap();
        let tx = connection.transaction().unwrap();
        insert_session(&tx, &source).unwrap();
        tx.commit().unwrap();

        let mut intent = serde_json::to_value(operation(&source)).unwrap();
        intent["selection"]["supervisors"] = false.into();
        intent["recovery_session"]["supervisors"] = true.into();
        intent["recovery_session"]["subagents"]
            .as_object_mut()
            .unwrap()
            .remove("orchestrators");
        connection.execute(
            "INSERT INTO session_moves(session_id, operation_id, operation_json) VALUES (?1, ?2, ?3)",
            params![source.id, intent["operation_id"].as_str().unwrap(), intent.to_string()],
        ).unwrap();
        let restored = load_move_operation_with(&connection, &source.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.recovery_session.unwrap().subagents,
            source.subagents
        );
        assert!(matches!(restored.selection.subagents.as_ref(),
            Some(SubagentPolicy::Mjolnir { orchestrators: false, straightforward: Some(choice), .. })
                if choice.model == "my-worker-model"
        ));
        let canonical = serde_json::to_string(&restored.selection).unwrap();
        assert!(!canonical.contains("supervisors"));

        intent["recovery_session"] = serde_json::Value::Null;
        connection
            .execute(
                "UPDATE session_moves SET operation_json=?2 WHERE session_id=?1",
                params![source.id, intent.to_string()],
            )
            .unwrap();
        let without_snapshot = load_move_operation_with(&connection, &source.id)
            .unwrap()
            .unwrap();
        assert_eq!(without_snapshot.selection, restored.selection);
    }

    fn operation(session: &SessionRecord) -> MoveOperation {
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
            operation_id: "move-one".into(),
            selection: MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: false,
                session_id: session.id.clone(),
                profile_id: Some("destination".into()),
                target_template_id: Some("local".into()),
                additional_mounts: Some(Vec::new()),
                resource_allocation: None,
            },
            source_profile_id: session.last_profile.clone(),
            source_target_template_id: session.target_template_id.clone(),
            source_target: session.target.clone().map(|v| *v),
            source_native_session_id: session.native_session_id.clone(),
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: session.additional_mounts.clone(),
            source_resource_allocation: session.resource_allocation.clone(),
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: "fingerprint".into(),
            checkpoint: session.checkpoint.clone().map(|v| *v),
            recovery_session: Some(session.clone()),
            queue: ResumeQueueDisposition::Start,
            phase: MovePhase::Capturing,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            error: None,
        }
    }

    #[test]
    fn move_destination_fence_purges_only_source_and_historical_closed_borrowers() {
        const NAME: &str =
            "move_destination_fence_purges_only_source_and_historical_closed_borrowers";
        const CHILD: &str = "MJ_TEST_MOVE_BORROWED_TRANSCRIPTS";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
                module_path!(),
                NAME,
            ))
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        use super::super::tests::{session, test_subagent};
        use mj_core::state::TargetLocator;
        let _writer = install_isolated_test_writer();
        let mut parent = session("parent", "project");
        parent.state = SessionState::Running;
        let place = |parent: &mut SessionRecord, generation| {
            parent.target = Some(Box::new(TargetLocator::LocalBare {
                worker_root: PathBuf::from(format!("/fixture/{generation}/parent")),
            }));
            save_session(parent).unwrap();
        };
        let child = |id: &str, parent: &str, state, own| {
            let mut record = session(id, "project");
            record.state = state;
            let mut relation = test_subagent(parent, id);
            relation.orchestrator = matches!(id, "old" | "orchestrator");
            if id == "independent-reviewer" {
                relation.kind = mj_core::subagent::SubagentKind::Reviewer;
            }
            register_subagent_with_startup(&record, &relation, Vec::new(), None, None, own)
                .unwrap();
            let body = serde_json::to_string(&mj_core::transcript::TranscriptBody::System {
                text: id.into(),
            })
            .unwrap();
            tool_details::append_tool_detail(&mj_core::config::data_dir(), id, "detail", &body)
                .unwrap();
        };
        let exists = |id: &str| {
            let row = load_session_record(id).unwrap().is_some();
            assert_eq!(
                row,
                mj_core::config::data_dir()
                    .join("tool-details")
                    .join(format!("{id}.mjtd"))
                    .is_file()
            );
            row
        };
        place(&mut parent, "historical");
        child("old", "parent", SessionState::Running, false);
        child("nested", "old", SessionState::Stopped, false);
        child("orchestrator", "parent", SessionState::Running, true);
        child(
            "orchestrator-child",
            "orchestrator",
            SessionState::Stopped,
            false,
        );
        child("ancestor", "parent", SessionState::Running, false);
        child(
            "independent-reviewer",
            "ancestor",
            SessionState::Stopped,
            true,
        );
        for (id, state) in [
            ("old", SessionState::Error),
            ("orchestrator", SessionState::Stopped),
            ("ancestor", SessionState::Stopped),
        ] {
            let mut owner = load_session_record(id).unwrap().unwrap();
            owner.state = state;
            save_lifecycle_session(&owner).unwrap();
        }
        place(&mut parent, "source");
        child("source", "parent", SessionState::Stopped, false);
        let mut intent = operation(&parent);
        intent.in_place = true;
        intent.phase = MovePhase::DispatchReleased;
        commit_in_place_dispatch_released(&intent).unwrap();
        assert!(exists("source") && exists("old"));
        intent.in_place = false;
        intent.phase = MovePhase::Cancelled;
        save_move_operation(&intent).unwrap();
        assert!(exists("source") && exists("old"));
        intent.phase = MovePhase::DestinationStarting;
        save_move_operation(&intent).unwrap();

        place(&mut parent, "destination");
        child("destination", "parent", SessionState::Stopped, false);
        child("live", "parent", SessionState::Running, false);
        intent.phase = MovePhase::DestinationOwns;
        intent.destination_target = parent.target.as_deref().cloned();
        intent.queue_admission_started = true;
        let connection = open(&database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_purge BEFORE DELETE ON sessions
            WHEN OLD.session_id='source' BEGIN SELECT RAISE(ABORT, 'injected purge failure'); END;",
            )
            .unwrap();
        let error = commit_move_destination_owns(&intent, &parent).unwrap_err();
        assert!(format!("{error:#}").contains("injected purge failure"));
        assert_eq!(
            load_move_operation("parent").unwrap().unwrap().phase,
            MovePhase::DestinationStarting
        );
        assert!(exists("nested") && exists("old") && exists("source"));

        connection
            .execute_batch("DROP TRIGGER reject_purge")
            .unwrap();
        commit_move_destination_owns(&intent, &parent).unwrap();
        assert_eq!(
            load_move_operation("parent").unwrap().unwrap().phase,
            MovePhase::DestinationOwns
        );
        for id in ["old", "nested", "source"] {
            assert!(!exists(id), "{id} survived the ownership fence");
        }
        for id in [
            "destination",
            "live",
            "orchestrator",
            "orchestrator-child",
            "ancestor",
            "independent-reviewer",
        ] {
            assert!(exists(id), "{id} was purged");
        }
    }

    #[test]
    fn move_outcome_commits_the_message_with_the_result_and_rolls_back_both_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let mut session = super::super::tests::session("moving", "project");
        session.state = SessionState::Closing;
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
        assert!(
            save_move_outcome_with(&mut connection, &intent, Some("public message"), true).is_err()
        );
        assert_eq!(
            load_move_operation_with(&connection, &session.id)
                .unwrap()
                .unwrap()
                .phase,
            MovePhase::Capturing
        );
        let failed_write = load_state_from(&path).unwrap();
        assert_eq!(failed_write.sessions[&session.id].last_error, None);
        assert_eq!(
            failed_write.sessions[&session.id].state,
            SessionState::Closing
        );
        connection
            .execute_batch("DROP TRIGGER refuse_move_message")
            .unwrap();
        save_move_outcome_with(&mut connection, &intent, Some("public message"), true).unwrap();
        drop(connection);
        let restored = load_state_from(&path).unwrap();
        assert_eq!(restored.sessions[&session.id].state, SessionState::Error);
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
        assert_eq!(held.moves["moving"].phase, MovePhase::Capturing);
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
        session.target = Some(Box::new(mj_core::state::TargetLocator::SshPodman {
            host: "original.test".into(),
            container_id: "source-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        }));
        save_session_to(&path, &session).unwrap();
        let mut intent = operation(&session);
        for phase in [
            MovePhase::Capturing,
            MovePhase::Transferring,
            MovePhase::Restoring,
            MovePhase::DestinationOwns,
            MovePhase::Failed,
            MovePhase::Completed,
        ] {
            intent.phase = phase;
            if phase == MovePhase::DestinationOwns {
                intent.queue_admission_started = true;
                intent.destination_target = session.target.clone().map(|v| *v);
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
            assert_eq!(restored.source_target, session.target.as_deref().cloned());
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
        intent.quota_stop_reason = Some("Weekly quota exhausted".into());
        intent.quota_continuation = Some(mj_core::state::QuotaMoveContinuation::ResumeGoal);
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
        let mut legacy_json: serde_json::Value = serde_json::from_str(&stored).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("quota_stop_reason");
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("quota_continuation");
        let legacy_intent: MoveOperation = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy_intent.quota_stop_reason.is_none());
        assert!(legacy_intent.quota_continuation.is_none());
        assert!(
            !serde_json::to_string(&legacy_intent)
                .unwrap()
                .contains("quota_stop_reason")
        );
        assert!(
            !serde_json::to_string(&legacy_intent)
                .unwrap()
                .contains("quota_continuation")
        );
        // Unknown fields from a newer daemon are skipped by every store reader.
        let mut newer_json: serde_json::Value = serde_json::from_str(&stored).unwrap();
        newer_json["future_quota_field"] = serde_json::json!("new intent");
        connection
            .execute(
                "UPDATE session_moves SET operation_json=?2 WHERE session_id=?1",
                params![session.id, serde_json::to_string(&newer_json).unwrap()],
            )
            .unwrap();
        assert!(
            load_move_operation_with(&connection, &session.id)
                .unwrap()
                .is_none()
        );
        assert!(load_move_operations_with(&connection).unwrap().is_empty());
        save_move_operation_with(&connection, &intent).unwrap();
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
            ("running-gone", MovePhase::Restoring, false, false, true),
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
        intent.phase = MovePhase::Transferring;
        save_move_operation_with(&connection, &intent).unwrap();
        let restored = load_move_operation_with(&connection, &session.id)
            .unwrap()
            .unwrap();
        assert!(restored.cancellation_requested);
        assert_eq!(restored.phase, MovePhase::Transferring);
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
            MovePhase::Capturing,
            MovePhase::Transferring,
            MovePhase::Restoring,
            MovePhase::DestinationOwns,
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
        intent.phase = MovePhase::Transferring;
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

        // A no-op Move's omitted choices cannot overwrite a settings edit.
        let allocation = SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        };
        set_session_container_settings_to(
            &path,
            &stale.id,
            Some(&allocation),
            &current.additional_mounts,
            &current.updated_at,
        )
        .unwrap();
        let mut selection = operation(&stale).selection;
        selection.additional_mounts = None;
        let mut connection = open(&path).unwrap();
        let tx = connection.transaction().unwrap();
        update_move_requests(&tx, &selection).unwrap();
        tx.commit().unwrap();
        let retained = load_session_with(&connection, &stale.id).unwrap().unwrap();
        assert_eq!(retained.resource_allocation, Some(allocation));
        assert_eq!(retained.additional_mounts, current.additional_mounts);

        // An explicit choice still applies when no environment change is needed.
        selection.clear_resource_allocation = true;
        selection.additional_mounts = Some(Vec::new());
        let tx = connection.transaction().unwrap();
        update_move_requests(&tx, &selection).unwrap();
        tx.commit().unwrap();
        let changed = load_session_with(&connection, &stale.id).unwrap().unwrap();
        assert_eq!(changed.resource_allocation, None);
        assert!(changed.additional_mounts.is_empty());
    }
}
