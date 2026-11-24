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
/// Both reads share one SQLite snapshot so a lagging `CommittedState` cache
/// cannot pair a settled intent with a stale `Provisioning` record.
pub enum CreateEnd {
    Unfinished,
    Finished,
    Failed(String),
}

pub fn session_create_end(session_id: &str) -> Result<CreateEnd> {
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    if has_create_intent_with(&snapshot, session_id)? {
        return Ok(CreateEnd::Unfinished);
    }
    let Some(record) = state_io::load_session_with(&snapshot, session_id)? else {
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

pub fn has_session_create_intent(session_id: &str) -> Result<bool> {
    let connection = open_reader(&database_path())?;
    has_create_intent_with(&connection, session_id)
}

pub(super) fn has_create_intent_with(connection: &Connection, session_id: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_create_intents WHERE session_id = ?1)",
        [session_id],
        |row| row.get(0),
    )?)
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
        let current = state_io::load_session_with(&tx, &session.id)?
            .context("cannot reset Create for a missing session")?;
        anyhow::ensure!(
            !create_intent_is_stale(&current) && has_create_intent_with(&tx, &session.id)?,
            "session {} no longer has an unfinished Create",
            session.id
        );
        // The lifecycle owner fences relay attachment and confirms worker stop
        // before calling this, so no page from the retired worker can arrive.
        tx.execute(
            "DELETE FROM worker_restart_intents WHERE session_id = ?1",
            [&session.id],
        )?;
        tx.execute(
            "DELETE FROM materialized_sessions WHERE session_id = ?1",
            [&session.id],
        )?;
        tx.execute(
            "INSERT INTO materialized_sessions(session_id) VALUES (?1)",
            [&session.id],
        )?;
        tx.execute(
            "UPDATE sessions SET acp_session_title = NULL, native_session_id = NULL WHERE session_id = ?1",
            [&session.id],
        )?;
        update_lifecycle_fields(&tx, &session)?;
        tx.commit()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_isolation::{IsolatedTest, test_name};
    use std::sync::mpsc::channel;

    // Hard-won: #1675: interrupted Create reused its retired worker projection
    #[test]
    fn create_retry_resets_only_the_retired_worker_projection() {
        const CHILD: &str = "MJ_TEST_CREATE_PROJECTION_RESET";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "create_retry_resets_only_the_retired_worker_projection",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "create-projection-reset")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let writer = install_isolated_test_writer();
        let mut session = super::super::tests::session("retry", "project");
        session.state = SessionState::Disconnected;
        session.acp_session_title = Some("Retired title".into());
        session.native_session_id = Some("retired-native".into());
        save_new_session(&session, None, false, false).unwrap();
        let intents = load_session_create_intents().unwrap();
        assert_eq!(intents.len(), 1);

        submit_database_write("seed legacy Create projection", |connection| {
            connection.execute_batch(
                "UPDATE materialized_sessions SET applied_event_ordinal=42,
                    applied_event_digest=replace(applied_event_digest,'0','a'),
                    session_title='Retired title', last_activity_at_ms=7,
                    input_required_event_frontier=42, operator_away_event_frontier=41
                    WHERE session_id='retry';
                 INSERT INTO materialized_transcript_items(session_id,stable_id,position,
                    created_at_ms,last_changed_at_ms,body_json)
                    VALUES ('retry','first',1,1,1,'{}');
                 INSERT INTO materialized_queued_prompts(session_id,ordinal,command_id,
                    content_json,queued_at_ms) VALUES ('retry',0,'queued','[]',1);
                 INSERT INTO api_config_results VALUES ('retry','config',NULL);
                 INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase)
                    VALUES ('retry','worker-start','{}','old','prepared');
                 INSERT INTO startup_steps(session_id,command_id,step_json)
                    VALUES ('retry','startup','{}');
                 INSERT INTO mailbox_outbox(event_key,target_session_id,event_json,wake,created_at)
                    VALUES ('mail','retry','{}',0,'now');
                 INSERT INTO detached_drafts(draft_id,workspace_id,session_id,source,saved_at,text)
                    SELECT 'draft',workspace_id,'retry','terminal','now','unsent'
                    FROM session_contexts WHERE session_id='retry';
                 INSERT INTO session_turn_usage VALUES ('retry','usage',2,1,'{}');
                 INSERT INTO session_provider_cost VALUES ('retry','{}');",
            )?;
            Ok(())
        }).unwrap();

        session.state = SessionState::Provisioning;
        reset_session_for_create_retry(&session).unwrap();

        assert_eq!(
            materialized_event_frontier("retry").unwrap(),
            Some((0, mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into()))
        );
        let connection = open_reader(&database_path()).unwrap();
        let cleared: bool = connection
            .query_row(
                "SELECT session_title IS NULL AND last_activity_at_ms IS NULL
                    AND input_required_event_frontier=0 AND operator_away_event_frontier=0
             FROM materialized_sessions WHERE session_id='retry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cleared);
        for (table, expected) in [
            ("materialized_transcript_items", 0),
            ("materialized_queued_prompts", 0),
            ("api_config_results", 0),
            ("worker_restart_intents", 0),
            ("startup_steps", 1),
            ("mailbox_outbox", 1),
            ("detached_drafts", 1),
            ("session_turn_usage", 1),
            ("session_provider_cost", 1),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, expected, "{table}");
        }
        let reset = load_session_record("retry").unwrap().unwrap();
        assert_eq!(reset.state, SessionState::Provisioning);
        assert_eq!(reset.acp_session_title, None);
        assert_eq!(reset.native_session_id, None);
        assert_eq!(load_session_create_intents().unwrap(), intents);

        discard_session_create_intent("retry", &intents[0].operation_id).unwrap();
        assert!(reset_session_for_create_retry(&session).is_err());
        let mut running = reset;
        running.state = SessionState::Running;
        running.native_session_id = Some("new-native".into());
        save_session(&running).unwrap();
        assert!(reset_session_for_create_retry(&session).is_err());
        let committed = load_session_record("retry").unwrap().unwrap();
        assert_eq!(committed.state, SessionState::Running);
        assert_eq!(committed.native_session_id.as_deref(), Some("new-native"));
        writer.shutdown().unwrap();
    }

    // Hard-won: 4e82c9302e: session_create_end mixed an SQL intent check with a stale cached session record
    #[test]
    fn session_create_end_reads_failure_before_cache_publication() {
        const CHILD: &str = "MJ_TEST_CREATE_END_SNAPSHOT";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "session_create_end_reads_failure_before_cache_publication",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "create-end-snapshot")
            .isolated_store(directory.path())
            .run();
            return;
        }

        let owner = install_isolated_test_writer();
        let mut session = super::super::tests::session("cancelled-create", "project");
        session.state = SessionState::Provisioning;
        save_new_session(&session, None, false, false).unwrap();

        let id = session.id.clone();
        session.state = SessionState::StartupCleanup;
        session.last_error = Some("creation cancelled".into());
        let (committed, commit) = channel();
        let (publish, publication) = channel();
        let (cached, outcome) = std::thread::scope(|scope| {
            let write = scope.spawn(move || {
                submit_database_write("settle cancelled create", move |connection| {
                    let tx = connection.transaction()?;
                    update_lifecycle_fields(&tx, &session)?;
                    tx.commit()?;
                    committed.send(())?;
                    publication.recv()?;
                    Ok(())
                })
            });
            commit.recv().unwrap();
            let cached = load_session_record(&id);
            let outcome = session_create_end(&id);
            publish.send(()).unwrap();
            write.join().unwrap().unwrap();
            (cached, outcome)
        });

        assert_eq!(cached.unwrap().unwrap().state, SessionState::Provisioning);
        assert!(
            matches!(outcome.unwrap(), CreateEnd::Failed(reason) if reason == "creation cancelled")
        );
        owner.shutdown().unwrap();
    }
}
