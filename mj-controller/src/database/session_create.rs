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
pub(crate) struct SessionCreateIntent {
    pub session_id: String,
    pub operation_id: String,
    /// Whether the person launched interactively; the post-success step that
    /// remembers the project directory needs it after a restart.
    pub remember_launch_choices: bool,
}

/// States in which a session's creation is still unfinished.
fn is_creating(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Provisioning | SessionState::Disconnected
    )
}

pub(super) fn insert_create_intent(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    remember_launch_choices: bool,
) -> Result<()> {
    // Sessions registered already past creation (fixtures, imports) own none.
    if !is_creating(session.state) {
        return Ok(());
    }
    let operation_id = crate::session_manager::new_command_id("create")?;
    tx.execute(
        "INSERT OR IGNORE INTO session_create_intents(session_id, operation_id, remember_launch_choices, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            session.id,
            operation_id,
            remember_launch_choices,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
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
    if !is_creating(state) {
        tx.execute(
            "DELETE FROM session_create_intents WHERE session_id = ?1",
            [session_id],
        )?;
    }
    Ok(())
}

pub(crate) fn load_session_create_intents() -> Result<Vec<SessionCreateIntent>> {
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
pub(crate) fn discard_session_create_intent(session_id: &str, operation_id: &str) -> Result<()> {
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
    insert_create_intent(tx, session, false)
}

/// Forget the worker boot an interrupted create left, once its worker is stopped.
pub(crate) fn discard_worker_restart_intent(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("discard_worker_restart_intent", move |connection| {
        connection.execute(
            "DELETE FROM worker_restart_intents WHERE session_id = ?1",
            [&session_id],
        )?;
        Ok(())
    })
}

/// Reset a session to its registered form so its create can run again: the
/// failed attempt's environment is retired by the caller first, so nothing
/// the worker restart intent describes exists any more.
pub(crate) fn reset_session_for_create_retry(session: &SessionRecord) -> Result<()> {
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
