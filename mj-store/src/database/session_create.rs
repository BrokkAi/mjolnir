//! Durable record that a new session's creation is unfinished.
//!
//! The session record cannot say this by itself: `Disconnected` also
//! describes every ordinary session whose worker connection dropped. One row
//! per session, written in the transaction that registers the session and
//! removed by every write that moves the session out of the creating states,
//! is the single owner of "creation unfinished". A daemon that stops while a
//! create runs leaves the row; the next daemon finishes the create from it.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreateIntent {
    pub session_id: String,
    pub operation_id: String,
    /// Whether the person launched interactively; the post-success step that
    /// remembers the project directory needs it after a restart.
    pub remember_launch_choices: bool,
}

/// The states a session's create intent can exist in: the storage rule that
/// registration and every session write keep. It says nothing about whether a
/// given session's creation is unfinished; the intent row is the owner of that.
pub fn holds_create_intent(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Provisioning | SessionState::Disconnected
    )
}

/// Whether a create intent outlived its creation: its session was archived, or
/// finished or failed by something that did not settle the intent (an older
/// daemon).
pub fn create_intent_is_stale(session: &SessionRecord) -> bool {
    session.archived || !holds_create_intent(session.state)
}

/// How a session's creation stands, read from durable state alone: the
/// intent row says whether it is unfinished, the record says how it ended.
/// The row is read first. It is removed in the transaction that writes the
/// final state, so a missing row means the record already shows the end.
pub enum CreateEnd {
    Unfinished,
    Finished,
    Failed(String),
}

pub fn session_create_end(session_id: &str) -> Result<CreateEnd> {
    let connection = open_reader(&database_path())?;
    let unfinished: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_create_intents WHERE session_id = ?1)",
        [session_id],
        |row| row.get(0),
    )?;
    drop(connection);
    if unfinished {
        return Ok(CreateEnd::Unfinished);
    }
    let Some(record) = load_session_record(session_id)? else {
        return Ok(CreateEnd::Failed(format!(
            "session {session_id} no longer exists"
        )));
    };
    // A failed create passes through StartupCleanup (intent settled) on its
    // way to Error; that is already its failure.
    Ok(match record.state {
        SessionState::Error | SessionState::StartupCleanup => CreateEnd::Failed(
            record
                .last_error
                .unwrap_or_else(|| "session creation failed".to_owned()),
        ),
        _ => CreateEnd::Finished,
    })
}

pub(super) fn insert_create_intent(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    remember_launch_choices: bool,
) -> Result<Option<String>> {
    // Sessions registered already past creation (fixtures, imports) own none.
    if !holds_create_intent(session.state) {
        return Ok(None);
    }
    let operation_id = mj_client::session::new_command_id("create")?;
    tx.query_row(
        "INSERT INTO session_create_intents(session_id, operation_id, remember_launch_choices, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING RETURNING operation_id",
        params![
            session.id,
            operation_id,
            remember_launch_choices,
            Utc::now().to_rfc3339()
        ],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Every session write calls this with the state it writes: a state outside
/// the creating ones means the create committed or failed for good.
pub(super) fn settle_create_intent(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    settle_create_intent_for_state(tx, &session.id, session.state)
}

pub(super) fn settle_create_intent_for_state(
    tx: &Transaction<'_>,
    session_id: &str,
    state: SessionState,
) -> Result<()> {
    if !holds_create_intent(state) {
        tx.execute(
            "DELETE FROM session_create_intents WHERE session_id = ?1",
            [session_id],
        )?;
    }
    Ok(())
}

pub fn load_session_create_intents() -> Result<Vec<SessionCreateIntent>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT session_id, operation_id, remember_launch_choices
         FROM session_create_intents ORDER BY created_at, session_id",
    )?;
    statement
        .query_map([], |row| {
            Ok(SessionCreateIntent {
                session_id: row.get(0)?,
                operation_id: row.get(1)?,
                remember_launch_choices: row.get(2)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// Drop an intent whose session already finished or failed elsewhere.
pub fn discard_session_create_intent(session_id: &str, operation_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("discard_session_create_intent", move |connection| {
        connection.execute(
            "DELETE FROM session_create_intents WHERE session_id = ?1 AND operation_id = ?2",
            params![session_id, operation_id],
        )?;
        Ok(())
    })
}

/// A sub-agent's failed worker start that is retried stays the same create:
/// the cleanup between the attempts moves the record out of the creating
/// states and settles its intent, so the retry puts the intent back.
pub(super) fn restore_create_intent_for_retry(
    tx: &Transaction<'_>,
    session: &SessionRecord,
) -> Result<()> {
    insert_create_intent(tx, session, false).map(|_| ())
}

/// The only classification of a failed create that Resume may launch again.
pub fn failed_first_launch(
    session: &SessionRecord,
    is_subagent: bool,
    movement: Option<&mj_core::state::MoveOperation>,
) -> bool {
    !is_subagent
        && !mj_core::native_agent::is_view_id(&session.id)
        && !session.archived
        && session.checkpoint.is_none()
        && session.native_session_id.is_none()
        && (session.state == SessionState::StartupCleanup
            || (session.state == SessionState::Error && session.target.is_none()))
        && !movement.is_some_and(|movement| {
            movement.is_active()
                || movement.holds_source_environment()
                || (movement.queue_admission_started && !movement.queue_admission_finished)
        })
}

fn failed_launch_record_with(connection: &Connection, id: &str) -> Result<Option<SessionRecord>> {
    let Some(record) = state_io::load_session_with(connection, id)? else {
        return Ok(None);
    };
    let is_subagent: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1)",
        [id],
        |row| row.get(0),
    )?;
    let movement = session_move::load_move_operation_with(connection, id)?;
    Ok(failed_first_launch(&record, is_subagent, movement.as_ref()).then_some(record))
}

pub fn read_failed_launch_record(id: &str) -> Result<Option<SessionRecord>> {
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    failed_launch_record_with(&snapshot, id)
}

/// Accept the retry and publish a recoverable create in the same transaction.
pub fn begin_failed_launch_retry(expected: SessionRecord) -> Result<Option<String>> {
    submit_database_write("begin failed launch retry", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(mut record) = failed_launch_record_with(&tx, &expected.id)? else {
            return Ok(None);
        };
        if record.workspace_id != expected.workspace_id
            || record.last_profile != expected.last_profile
            || record.target_template_id != expected.target_template_id
            || record.resource_allocation != expected.resource_allocation
            || record.additional_mounts != expected.additional_mounts
        {
            return Ok(None);
        }
        record.state = SessionState::Provisioning;
        record.last_error = None;
        record.updated_at = Utc::now().to_rfc3339();
        update_lifecycle_fields(&tx, &record)?;
        let operation_id = insert_create_intent(&tx, &record, false)?
            .context("failed session still has a create intent")?;
        startup::dismiss_failed_startup_groups_with(&tx, &record.id)?;
        tx.commit()?;
        Ok(Some(operation_id))
    })
}

/// Reset a session to its registered form so its create can run again: the
/// failed attempt's environment is retired by the caller first, so nothing
/// the worker restart intent describes exists any more.
pub fn reset_session_for_create_retry(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("reset_session_for_create_retry", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM worker_restart_intents WHERE session_id = ?1",
            [&session.id],
        )?;
        update_lifecycle_fields(&tx, &session)?;
        tx.commit()?;
        Ok(())
    })
}
