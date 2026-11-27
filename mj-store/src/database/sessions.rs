use super::*;

/// Persist one operational session without rewriting unrelated controller
/// state. Dashboard lifecycle jobs use this path so independent jobs can
/// commit concurrently without restoring stale copies of other sessions.
pub fn save_imported_session(
    session: &SessionRecord,
    materialized: &MaterializedSession,
) -> Result<()> {
    let session = session.clone();
    let materialized = materialized.clone();
    submit_database_write("save_imported_session", move |connection| {
        validate_session_record(&session)?;
        materialized.validate()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            !session_exists(&tx, &session.id)?,
            "imported session already exists"
        );
        insert_session(&tx, &session, &super::state_io::configured_sessions_root())?;
        write_materialized_session(&tx, &materialized)?;
        tx.commit()?;
        Ok(())
    })
}

pub fn save_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_session", move |_| {
        save_session_to(&database_path(), &session)
    })
}

pub fn save_new_session(
    session: &SessionRecord,
    container_size: Option<(String, HostContainerSize)>,
    remember_launch_choices: bool,
    remember_container_size: bool,
) -> Result<()> {
    save_new_session_with_preferences(
        session,
        container_size,
        NewSessionPreferences {
            remember_launch_choices,
            remember_container_size,
            remember_subagent_policy: remember_launch_choices,
        },
    )
}

pub struct NewSessionPreferences {
    pub remember_launch_choices: bool,
    pub remember_container_size: bool,
    pub remember_subagent_policy: bool,
}

pub fn save_new_session_with_preferences(
    session: &SessionRecord,
    container_size: Option<(String, HostContainerSize)>,
    preferences: NewSessionPreferences,
) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_new_session", move |_| {
        save_new_session_with_preferences_to(
            &database_path(),
            &session,
            container_size,
            preferences,
        )
    })
}

#[cfg(test)]
pub(super) fn save_new_session_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(String, HostContainerSize)>,
    remember_launch_choices: bool,
    remember_container_size: bool,
) -> Result<()> {
    save_new_session_with_preferences_to(
        path,
        session,
        container_size,
        NewSessionPreferences {
            remember_launch_choices,
            remember_container_size,
            remember_subagent_policy: remember_launch_choices,
        },
    )
}

fn save_new_session_with_preferences_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(String, HostContainerSize)>,
    preferences: NewSessionPreferences,
) -> Result<()> {
    let NewSessionPreferences {
        remember_launch_choices,
        remember_container_size,
        remember_subagent_policy,
    } = preferences;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    validate_session_record(session)?;
    ensure!(
        !session.last_profile.is_empty(),
        "a session profile id cannot use the reserved legacy preference key"
    );
    insert_session(&tx, session, &super::state_io::sessions_root_for(path))?;
    super::session_create::insert_create_intent(&tx, session, remember_launch_choices)?;
    if let Some((host, size)) = container_size.filter(|_| remember_container_size) {
        write_host_container_size(&tx, &host, size)?;
    }
    let policy = session.subagents.clone().unwrap_or_default();
    if remember_subagent_policy && policy.updates_remembered_choice(session.harness_kind) {
        save_subagent_preference(&tx, &session.last_profile, policy)?;
    }
    if remember_launch_choices {
        tx.execute(
            "INSERT INTO oracle_preference(profile_id, enabled) VALUES(?1, ?2)
             ON CONFLICT(profile_id) DO UPDATE SET enabled = excluded.enabled",
            params![session.last_profile, session.oracle],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn save_subagent_preference(
    tx: &Transaction<'_>,
    profile: &str,
    policy: mj_core::subagent::SubagentPolicy,
) -> Result<()> {
    tx.execute(
        "INSERT INTO subagent_preference(profile_id, policy) VALUES(?1, ?2)
         ON CONFLICT(profile_id) DO UPDATE SET policy = excluded.policy",
        params![profile, serde_json::to_string(&policy)?],
    )?;
    Ok(())
}

/// Update publication evidence only while the exact stopped checkpoint is
/// still current. An age scan may race a user's Resume or a new checkpoint.
pub fn set_publication_assessment_if_current(
    session_id: &str,
    assessment: &mj_core::state::PublicationAssessment,
) -> Result<bool> {
    let session_id = session_id.to_owned();
    let assessment = assessment.clone();
    submit_database_write("set_publication_assessment_if_current", move |_| {
        let connection = open(&database_path())?;
        let updated = connection.execute(
            "UPDATE sessions SET publication_json = ?2
             WHERE session_id = ?1 AND state = 'stopped'
               AND EXISTS (SELECT 1 FROM session_checkpoints c
                           WHERE c.session_id = ?1 AND c.sha256 = ?3)",
            params![
                session_id,
                serde_json::to_string(&assessment)?,
                assessment.checkpoint_sha256
            ],
        )?;
        Ok(updated == 1)
    })
}

/// A primary may have one live oracle; registration refused another.
#[derive(Debug)]
pub struct OracleAlreadyLive;

impl std::fmt::Display for OracleAlreadyLive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("this session already has a live oracle")
    }
}

impl std::error::Error for OracleAlreadyLive {}

/// Persist a borrowed-target child and its parent relationship atomically.
pub fn save_subagent_session(
    session: &SessionRecord,
    subagent: &mj_core::subagent::SubagentRecord,
) -> Result<()> {
    register_subagent_with_startup(session, subagent, Vec::new(), None, None, false)
}

/// Publish the child and its complete initial input in the same transaction.
/// Recovery must never find a newly registered worker without its first brief.
pub fn register_subagent_with_startup(
    session: &SessionRecord,
    subagent: &mj_core::subagent::SubagentRecord,
    startup: Vec<super::startup::StartupDelivery>,
    report_dir: Option<&str>,
    category: Option<&str>,
    own_environment: bool,
) -> Result<()> {
    register_subagent_with_startup_admitted(
        session,
        subagent,
        startup,
        report_dir,
        category,
        own_environment,
        |_, _| Ok(()),
    )
}

/// Run the controller's admission policy against the latest committed state
/// in the same serialized write that registers the child.
pub fn register_subagent_with_startup_admitted(
    session: &SessionRecord,
    subagent: &mj_core::subagent::SubagentRecord,
    startup: Vec<super::startup::StartupDelivery>,
    report_dir: Option<&str>,
    category: Option<&str>,
    own_environment: bool,
    admit: impl FnOnce(&State, &mj_core::subagent::SubagentRecord) -> Result<()> + Send + 'static,
) -> Result<()> {
    let session = session.clone();
    let subagent = subagent.clone();
    let report_dir = report_dir.map(str::to_owned);
    let category = category.map(str::to_owned);
    submit_database_write("save_subagent_session", move |_| {
        let mut connection = open(&database_path())?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let committed =
            committed_state()?.context("sub-agent admission requires the database writer")?;
        admit(&committed.state, &subagent)?;
        let parent_state = tx.query_row(
            "SELECT state FROM sessions WHERE session_id=?1",
            [&subagent.parent_session_id],
            |row| row.get::<_, String>(0),
        )?;
        ensure!(
            super::values::stored_session_state(&parent_state).can_receive_child_work(),
            "parent is no longer accepting child work"
        );
        if !own_environment {
            super::session_restart::ensure_container_parent_available(
                &tx,
                &subagent.parent_session_id,
            )?;
        }
        if subagent.kind == mj_core::subagent::SubagentKind::Oracle {
            let mut statement = tx.prepare(
                "SELECT s.state FROM subagent_sessions c
                 JOIN sessions s ON s.session_id = c.child_session_id
                 WHERE c.parent_session_id = ?1 AND c.request_key LIKE 'oracle:%'",
            )?;
            let live = statement
                .query_map([&subagent.parent_session_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .filter_map(|state| mj_core::state::SessionState::from_stored(state))
                .any(mj_core::state::SessionState::can_receive_child_work);
            drop(statement);
            if live {
                return Err(OracleAlreadyLive.into());
            }
        }
        super::state_io::insert_session_with_parent(
            &tx,
            &session,
            (!own_environment).then_some(subagent.parent_session_id.as_str()),
            &super::state_io::configured_sessions_root(),
        )?;
        super::session_create::insert_create_intent(&tx, &session, false)?;
        super::usage::record_child_accounting(&tx, &subagent, category.as_deref())?;
        tx.execute(
            "INSERT INTO subagent_sessions(
                 child_session_id, parent_session_id, request_key, record_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                subagent.child_session_id,
                subagent.parent_session_id,
                subagent.kind.request_key(&subagent.request_key),
                serde_json::to_string(&subagent)?,
            ],
        )?;
        super::startup::enqueue_startup_deliveries_with(&tx, startup)?;
        // The report directory commits with the registration, so no later
        // step can leave a registered child without it.
        if let Some(report_dir) = &report_dir {
            record_subagent_report_dir_with(&tx, &subagent.child_session_id, report_dir)?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// Record the child turn whose completion notice the parent already has.
pub fn mark_subagent_turn_noticed(child_session_id: &str, turn: u64) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    submit_database_write("mark_subagent_turn_noticed", move |_| {
        let mut relation = load_subagent(&child_session_id)?
            .with_context(|| format!("unknown sub-agent session {child_session_id}"))?;
        relation.noticed_turn = Some(turn);
        let json = serde_json::to_string(&relation)?;
        let connection = open(&database_path())?;
        connection.execute(
            "UPDATE subagent_sessions SET record_json = ?2 WHERE child_session_id = ?1",
            params![child_session_id, json],
        )?;
        Ok(())
    })
}

pub fn load_subagent(child_session_id: &str) -> Result<Option<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row(
            "SELECT record_json FROM subagent_sessions WHERE child_session_id = ?1",
            [child_session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).context("decode sub-agent record"))
        .transpose()
}

pub fn set_subagent_resume_with_parent(
    child_session_id: &str,
    resume_with_parent: bool,
) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    submit_database_write("set_subagent_resume_with_parent", move |connection| {
        let changed = connection.execute(
            "UPDATE subagent_sessions
             SET record_json = CASE WHEN ?2
                 THEN json_set(record_json, '$.resume_with_parent', json('true'))
                 ELSE json_remove(record_json, '$.resume_with_parent')
             END WHERE child_session_id = ?1",
            params![child_session_id, resume_with_parent],
        )?;
        ensure!(changed == 1, "unknown sub-agent session {child_session_id}");
        Ok(())
    })
}

/// Whether a sub-agent child has handed back its report for its parent's
/// newest task, from what the store holds; see
/// [`mj_core::subagent::has_handed_back`]. A session that is not a child, or
/// that has never finished a turn, has not.
pub fn subagent_has_handed_back(
    child_session_id: &str,
    liveness: &mj_core::subagent::Liveness,
) -> Result<bool> {
    let Some(relation) = load_subagent(child_session_id)? else {
        return Ok(false);
    };
    // A child is parked only once its turn ended and its parent was told, so
    // whatever the parent was going to get from it, it has.
    if load_session_state(child_session_id)? == Some(SessionState::Parked) {
        return Ok(true);
    }
    let Some((execution, active_turn, last_turn)) =
        load_materialized_turn_outcome(child_session_id)?
    else {
        return Ok(false);
    };
    let report = load_subagent_report(child_session_id)?;
    let working = active_turn.is_some() || !matches!(execution, MaterializedExecutionState::Idle);
    Ok(mj_core::subagent::has_handed_back(
        relation.handback_tool,
        &report,
        working,
        last_turn.as_ref(),
        liveness,
        mj_core::clock::epoch_millis(),
    ))
}

pub fn list_subagents(parent_session_id: &str) -> Result<Vec<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT record_json FROM subagent_sessions
         WHERE parent_session_id = ?1 ORDER BY rowid",
    )?;
    statement
        .query_map([parent_session_id], |row| row.get::<_, String>(0))?
        .map(|row| serde_json::from_str(&row?).context("decode sub-agent record"))
        .collect()
}

/// A child's durable role and latest turn selection for the parent's listing.
pub struct SubagentListingDetails {
    pub relation: mj_core::subagent::SubagentRecord,
    pub category: Option<String>,
    pub selection: mj_core::storage::UsageSelection,
}

pub fn list_subagent_details(parent: &str) -> Result<Vec<SubagentListingDetails>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT c.record_json, a.category, t.model, t.effort
         FROM subagent_sessions c
         LEFT JOIN subagent_accounting a USING (child_session_id)
         LEFT JOIN sessions s ON s.session_id = c.child_session_id
         LEFT JOIN session_turn_selections t ON t.rowid = (
             SELECT rowid FROM session_turn_selections
             WHERE session_id = c.child_session_id ORDER BY rowid DESC LIMIT 1
         ) AND (t.profile IS NULL OR t.profile = s.last_profile)
         WHERE c.parent_session_id = ?1 ORDER BY c.rowid",
    )?;
    statement
        .query_map([parent], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get(1)?,
                mj_core::storage::UsageSelection {
                    model: row.get(2)?,
                    effort: row.get(3)?,
                },
            ))
        })?
        .map(|row| {
            let (record, category, selection) = row?;
            Ok(SubagentListingDetails {
                relation: serde_json::from_str(&record).context("decode sub-agent record")?,
                category,
                selection,
            })
        })
        .collect()
}

/// A parent's child session ids in registration order, read from the index
/// without decoding any record.
pub fn list_subagent_ids(parent_session_id: &str) -> Result<Vec<String>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT child_session_id FROM subagent_sessions
         WHERE parent_session_id = ?1 ORDER BY rowid",
    )?;
    statement
        .query_map([parent_session_id], |row| row.get(0))?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// The parent's records for the given children, in registration order. A
/// parent can hold thousands of children; a caller that needs a few decodes
/// only those.
pub fn list_subagents_among(
    parent_session_id: &str,
    child_session_ids: &[String],
) -> Result<Vec<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT record_json FROM subagent_sessions
         WHERE parent_session_id = ?1
           AND child_session_id IN (SELECT value FROM json_each(?2))
         ORDER BY rowid",
    )?;
    let ids = serde_json::to_string(child_session_ids)?;
    statement
        .query_map(rusqlite::params![parent_session_id, ids], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| serde_json::from_str(&row?).context("decode sub-agent record"))
        .collect()
}

/// Record, on the parent's session, the sub-agents its suspend is about to
/// stop. A child already listed keeps its place and takes the newer details,
/// so a suspend that runs again after a restart lists each child once.
pub fn record_stopped_subagents(
    parent_session_id: &str,
    stopped: &[mj_core::subagent::StoppedSubagent],
) -> Result<()> {
    let parent_session_id = parent_session_id.to_owned();
    let stopped = stopped.to_vec();
    submit_database_write("record_stopped_subagents", move |_| {
        record_stopped_subagents_to(&database_path(), &parent_session_id, &stopped)
    })
}

pub(super) fn record_stopped_subagents_to(
    path: &Path,
    parent_session_id: &str,
    stopped: &[mj_core::subagent::StoppedSubagent],
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for child in stopped {
        tx.execute(
            "INSERT INTO stopped_subagents(parent_session_id, child_session_id, record_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(parent_session_id, child_session_id) DO UPDATE SET
                 record_json = excluded.record_json",
            params![
                parent_session_id,
                child.child_session_id,
                serde_json::to_string(child)?
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The sub-agents a suspend of this session stopped and its model has not
/// been told about yet, in the order they were recorded.
pub fn load_stopped_subagents(
    parent_session_id: &str,
) -> Result<Vec<mj_core::subagent::StoppedSubagent>> {
    load_stopped_subagents_from(&database_path(), parent_session_id)
}

pub(super) fn load_stopped_subagents_from(
    path: &Path,
    parent_session_id: &str,
) -> Result<Vec<mj_core::subagent::StoppedSubagent>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT record_json FROM stopped_subagents
         WHERE parent_session_id = ?1 ORDER BY rowid",
    )?;
    statement
        .query_map([parent_session_id], |row| row.get::<_, String>(0))?
        .map(|row| serde_json::from_str(&row?).context("decode stopped sub-agent record"))
        .collect()
}

/// Forget the stopped sub-agents whose note the parent's relay has taken.
/// Only the named children go, so a child listed after the note was built
/// waits for the next one.
pub fn clear_stopped_subagents(
    parent_session_id: &str,
    child_session_ids: &[String],
) -> Result<()> {
    let parent_session_id = parent_session_id.to_owned();
    let child_session_ids = child_session_ids.to_vec();
    submit_database_write("clear_stopped_subagents", move |_| {
        clear_stopped_subagents_from(&database_path(), &parent_session_id, &child_session_ids)
    })
}

pub(super) fn clear_stopped_subagents_from(
    path: &Path,
    parent_session_id: &str,
    child_session_ids: &[String],
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for child_session_id in child_session_ids {
        tx.execute(
            "DELETE FROM stopped_subagents
             WHERE parent_session_id = ?1 AND child_session_id = ?2",
            params![parent_session_id, child_session_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Everything recorded about one child's report. A child with nothing
/// recorded yet reads as the empty report.
/// One session's stored lifecycle state, or `None` when the store holds no
/// such session.
pub fn load_session_state(session_id: &str) -> Result<Option<SessionState>> {
    let connection = open_reader(&database_path())?;
    let stored = connection
        .query_row(
            "SELECT state FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(stored.as_deref().map(stored_session_state))
}

pub fn load_subagent_report(child_session_id: &str) -> Result<mj_core::subagent::SubagentReport> {
    load_subagent_report_from(&database_path(), child_session_id)
}

pub(super) fn load_subagent_report_from(
    path: &Path,
    child_session_id: &str,
) -> Result<mj_core::subagent::SubagentReport> {
    Ok(load_subagent_report_with(&*open_reader(path)?, child_session_id)?.unwrap_or_default())
}

/// A child's recorded report, or `None` when nothing is recorded for it.
pub(super) fn load_subagent_report_with(
    connection: &Connection,
    child_session_id: &str,
) -> Result<Option<mj_core::subagent::SubagentReport>> {
    let row = connection
        .prepare_cached(
            "SELECT handback_command_id, handback_message, handback_recorded_at_ms,
                    reminder_command_id, reminder_for_command_id, reminder_sent_at_ms,
                    reminder_failed_for_command_id, awaited_ordinal, report_dir
             FROM subagent_handbacks WHERE child_session_id = ?1",
        )?
        .query_row([child_session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .optional()?;
    let Some((
        handback_command,
        handback_message,
        handback_at,
        reminder_command,
        reminder_for,
        reminder_at,
        reminder_failed_for,
        awaited_ordinal,
        report_dir,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(mj_core::subagent::SubagentReport {
        handback: match (handback_command, handback_message, handback_at) {
            (Some(command_id), Some(message), Some(recorded_at_ms)) => {
                Some(mj_core::subagent::SubagentHandback {
                    command_id,
                    message,
                    recorded_at_ms,
                })
            }
            _ => None,
        },
        reminder: match (reminder_command, reminder_for, reminder_at) {
            (Some(command_id), Some(for_command_id), Some(sent_at_ms)) => {
                Some(mj_core::subagent::HandbackReminder {
                    command_id,
                    for_command_id,
                    sent_at_ms,
                })
            }
            _ => None,
        },
        reminder_failed_for,
        awaited_ordinal: awaited_ordinal.and_then(|ordinal| u64::try_from(ordinal).ok()),
        report_dir,
    }))
}

/// Record the directory Mjolnir created for a child's report files. It is
/// recorded only for a sub-agent child.
pub fn record_subagent_report_dir(child_session_id: &str, report_dir: &str) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    let report_dir = report_dir.to_owned();
    submit_database_write("record_subagent_report_dir", move |_| {
        record_subagent_report_dir_to(&database_path(), &child_session_id, &report_dir)
    })
}

pub(super) fn record_subagent_report_dir_to(
    path: &Path,
    child_session_id: &str,
    report_dir: &str,
) -> Result<()> {
    record_subagent_report_dir_with(&open(path)?, child_session_id, report_dir)
}

/// One writer owns both facts: the child's report directory and the
/// environment's obligation to capture and remove that directory. Recording
/// the same directory again is the same fact, so the environment row is
/// present already and is left alone; the check and both writes share one
/// transaction.
fn record_subagent_report_dir_with(
    connection: &rusqlite::Connection,
    child_session_id: &str,
    report_dir: &str,
) -> Result<()> {
    if connection.is_autocommit() {
        let transaction = connection.unchecked_transaction()?;
        record_subagent_report_dir_rows(&transaction, child_session_id, report_dir)?;
        transaction.commit()?;
    } else {
        record_subagent_report_dir_rows(connection, child_session_id, report_dir)?;
    }
    Ok(())
}

fn record_subagent_report_dir_rows(
    connection: &rusqlite::Connection,
    child_session_id: &str,
    report_dir: &str,
) -> Result<()> {
    connection.execute(
        "INSERT INTO subagent_handbacks(child_session_id, report_dir)
         SELECT ?1, ?2 WHERE EXISTS (
             SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1
         )
         ON CONFLICT(child_session_id) DO UPDATE SET report_dir = excluded.report_dir",
        params![child_session_id, report_dir],
    )?;
    // The environment ledger is keyed by exactly this fact. A report
    // directory recorded again under the same environment is the row that is
    // already there, so it is not inserted twice, and no conflict resolution
    // is left for a trigger to raise.
    connection.execute(
        "INSERT INTO environment_reports(environment_id, child_session_id, report_dir)
         SELECT b.environment_id, ?1, ?2
           FROM environment_bindings b
          WHERE b.session_id = ?1
            AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1)
            AND NOT EXISTS (
                SELECT 1 FROM environment_reports r
                 WHERE r.environment_id = b.environment_id
                   AND r.child_session_id = ?1
                   AND r.report_dir = ?2
            )",
        params![child_session_id, report_dir],
    )?;
    Ok(())
}

/// Record that the parent gave a child a prompt, accepted at `ordinal`. It is
/// recorded only for a sub-agent child, and never moves backwards.
pub fn record_subagent_prompt(child_session_id: &str, ordinal: u64) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    submit_database_write("record_subagent_prompt", move |_| {
        record_subagent_prompt_to(&database_path(), &child_session_id, ordinal)
    })
}

pub(super) fn record_subagent_prompt_to(
    path: &Path,
    child_session_id: &str,
    ordinal: u64,
) -> Result<()> {
    let ordinal = i64::try_from(ordinal).context("prompt ordinal exceeds the store's range")?;
    open(path)?.execute(
        "INSERT INTO subagent_handbacks(child_session_id, awaited_ordinal)
         SELECT ?1, ?2 WHERE EXISTS (
             SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1
         )
         ON CONFLICT(child_session_id) DO UPDATE SET
             awaited_ordinal = max(coalesce(awaited_ordinal, 0), excluded.awaited_ordinal)",
        params![child_session_id, ordinal],
    )?;
    Ok(())
}

/// Record a child's report for the turn it names, unless a report for that
/// turn is already recorded. Returns whether this one was recorded: a child
/// delivers one report per turn, and the check and the write are one
/// transaction so two racing calls cannot both be accepted.
pub fn record_subagent_handback(
    child_session_id: &str,
    handback: &mj_core::subagent::SubagentHandback,
) -> Result<bool> {
    let child_session_id = child_session_id.to_owned();
    let handback = handback.clone();
    submit_database_write("record_subagent_handback", move |_| {
        record_subagent_handback_to(&database_path(), &child_session_id, &handback)
    })
}

pub(super) fn record_subagent_handback_to(
    path: &Path,
    child_session_id: &str,
    handback: &mj_core::subagent::SubagentHandback,
) -> Result<bool> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let recorded_for: Option<String> = tx
        .query_row(
            "SELECT handback_command_id FROM subagent_handbacks WHERE child_session_id = ?1",
            [child_session_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    // The first report for a turn wins, including when a restart continuation
    // of that turn reports again under its own command ID.
    if recorded_for
        .is_some_and(|recorded| mj_core::subagent::report_answers(&recorded, &handback.command_id))
    {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO subagent_handbacks(
             child_session_id, handback_command_id, handback_message, handback_recorded_at_ms
         ) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(child_session_id) DO UPDATE SET
             handback_command_id = excluded.handback_command_id,
             handback_message = excluded.handback_message,
             handback_recorded_at_ms = excluded.handback_recorded_at_ms",
        params![
            child_session_id,
            handback.command_id,
            handback.message,
            handback.recorded_at_ms
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Record the reminder sent after a child's turn ended without a report.
pub fn record_handback_reminder(
    child_session_id: &str,
    reminder: &mj_core::subagent::HandbackReminder,
) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    let reminder = reminder.clone();
    submit_database_write("record_handback_reminder", move |_| {
        open(&database_path())?.execute(
            "INSERT INTO subagent_handbacks(
                 child_session_id, reminder_command_id, reminder_for_command_id, reminder_sent_at_ms
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 reminder_command_id = excluded.reminder_command_id,
                 reminder_for_command_id = excluded.reminder_for_command_id,
                 reminder_sent_at_ms = excluded.reminder_sent_at_ms",
            params![
                child_session_id,
                reminder.command_id,
                reminder.for_command_id,
                reminder.sent_at_ms
            ],
        )?;
        Ok(())
    })
}

/// Record that the reminder for a turn could not be sent, so the child's
/// last message stands as its report.
pub fn record_handback_reminder_failed(child_session_id: &str, for_command_id: &str) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    let for_command_id = for_command_id.to_owned();
    submit_database_write("record_handback_reminder_failed", move |_| {
        open(&database_path())?.execute(
            "INSERT INTO subagent_handbacks(child_session_id, reminder_failed_for_command_id)
             VALUES (?1, ?2)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 reminder_failed_for_command_id = excluded.reminder_failed_for_command_id",
            params![child_session_id, for_command_id],
        )?;
        Ok(())
    })
}

pub fn lookup_subagent_request(
    parent_session_id: &str,
    request_key: &str,
) -> Result<Option<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row(
            "SELECT record_json FROM subagent_sessions
             WHERE parent_session_id = ?1 AND request_key = ?2",
            params![parent_session_id, request_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).context("decode sub-agent record"))
        .transpose()
}

/// Persist a session and the container size it most recently launched on its
/// host in one transaction.
pub fn save_session_with_container_size(
    session: &SessionRecord,
    host: &str,
    size: HostContainerSize,
) -> Result<()> {
    let session = session.clone();
    let host = host.to_owned();
    submit_database_write("save_session_with_container_size", move |_| {
        save_session_with_container_size_to(&database_path(), &session, Some((&host, size)))
    })
}

/// Resume publication owns the target and checkout. Requested size and mounts,
/// client titles, drafts, archive visibility and worker titles keep their owners.
pub fn save_resumed_session(
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
) -> Result<()> {
    let session = session.clone();
    let container_size = container_size.map(|(host, size)| (host.to_owned(), size));
    submit_database_write("save_resumed_session", move |_| {
        save_resumed_session_to(
            &database_path(),
            &session,
            container_size
                .as_ref()
                .map(|(host, size)| (host.as_str(), *size)),
        )
    })
}

/// Commit explicit dialog choices together with the first provisioning publication.
pub fn accept_resumed_session(
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
    requests: &SessionResumeOptions,
) -> Result<()> {
    let session = session.clone();
    let requests = requests.clone();
    let container_size = container_size.map(|(host, size)| (host.to_owned(), size));
    submit_database_write("accept resumed session", move |_| {
        save_resumed_session_in_environment_to(
            &database_path(),
            &session,
            container_size
                .as_ref()
                .map(|(host, size)| (host.as_str(), *size)),
            None,
            Some(&requests),
            None,
        )
    })
}

pub(super) fn save_resumed_session_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
) -> Result<()> {
    save_resumed_session_in_environment_to(path, session, container_size, None, None, None)
}

pub fn restore_resumed_session(session: &SessionRecord, environment_id: &str) -> Result<()> {
    let session = session.clone();
    let environment_id = environment_id.to_owned();
    submit_database_write("restore resumed environment", move |_| {
        save_resumed_session_in_environment_to(
            &database_path(),
            &session,
            None,
            Some(&environment_id),
            None,
            None,
        )
    })
}

/// Restore the source and release Move's restore authority in one commit.
/// Read the latest intent here: destination retirement may have updated it.
pub fn settle_superseded_move_source(
    session: &SessionRecord,
    environment_id: Option<&str>,
    operation_id: &str,
    suspended: bool,
) -> Result<()> {
    let session = session.clone();
    let environment_id = environment_id.map(str::to_owned);
    let operation_id = operation_id.to_owned();
    submit_database_write("settle superseded Move source", move |_| {
        save_resumed_session_in_environment_to(
            &database_path(),
            &session,
            None,
            environment_id.as_deref(),
            None,
            Some((&operation_id, suspended)),
        )
    })
}

fn save_resumed_session_in_environment_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
    environment_id: Option<&str>,
    requests: Option<&SessionResumeOptions>,
    close_move: Option<(&str, bool)>,
) -> Result<()> {
    validate_session_record(session)?;
    let mut connection = open(path)?;
    let tx = connection.transaction()?;
    let (bundle, workspace): (String, String) = tx.query_row(
        "SELECT bundle_id, workspace_id FROM session_contexts WHERE session_id = ?1",
        [&session.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        bundle == session.bundle_id && workspace == session.workspace_id,
        "session {} context changed before resume publication",
        session.id
    );
    if let Some(environment_id) = environment_id {
        let owner: String = tx.query_row(
            "SELECT owner_session_id FROM environments WHERE environment_id=?1",
            [environment_id],
            |row| row.get(0),
        )?;
        ensure!(
            owner == session.id,
            "resume rollback does not own the selected environment"
        );
        tx.execute(
            "UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1",
            params![session.id, environment_id],
        )?;
    }
    update_lifecycle_fields(&tx, session)?;
    if let Some(requests) = requests {
        super::state_io::update_container_requests(
            &tx,
            &session.id,
            requests.resource_allocation.as_ref().map(Some),
            requests.additional_mounts.as_deref(),
        )?;
    }
    tx.execute(
        "UPDATE sessions SET native_session_id = ?2, container_workspace = ?3,
             create_managed_worktree = ?4, launch_base = ?5, launch_branch = ?6,
             checkout_json = ?7
         WHERE session_id = ?1",
        params![
            session.id,
            session.native_session_id,
            session
                .container_workspace
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            session.create_managed_worktree,
            session.launch_base,
            session.launch_branch,
            session
                .checkout
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    replace_checkpoint(&tx, session, &super::state_io::sessions_root_for(path))?;
    if let Some((host, size)) = container_size {
        write_host_container_size(&tx, host, size)?;
    }
    if let Some((operation_id, _)) = close_move {
        super::session_move::resolve_cancelled_move_for_close_with(
            &tx,
            &session.id,
            operation_id,
            &session.updated_at,
        )?;
    }
    let purged = if close_move.is_some_and(|(_, suspended)| suspended)
        && session.state == SessionState::Stopped
    {
        purge_borrowed_descendants_in_tx(&tx, &session.id, None)?
    } else {
        Vec::new()
    };
    tx.commit()?;
    tool_details::unlink_deleted_sessions(path, &purged);
    Ok(())
}

/// Update only the fields a lifecycle transition owns on a session that
/// already exists. Everything else — display titles, checkpoints, container
/// settings, and attached directories — stays with its own writer.
pub fn save_lifecycle_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_lifecycle_session", move |_| {
        save_lifecycle_session_to(&database_path(), &session)
    })
}

/// Claim the worker and install its optional cut as Disconnected
/// in one writer transaction. Ordinary attachment later owns all post-cut replay.
pub async fn enqueue_adopted_session<G: Send + 'static>(
    expected: SessionRecord,
    cut: Option<(MaterializedSession, mj_core::relay::LiveSessionBaseline)>,
    updated_at: String,
    guard: G,
) -> Result<AcceptedDatabaseWrite<SessionRecord, G>> {
    enqueue_database_write("install adopted session", guard, move |connection| {
        install_adopted_session_with(
            connection,
            expected,
            cut,
            updated_at,
            &super::state_io::configured_sessions_root(),
        )
    })
    .await
}

fn install_adopted_session_with(
    connection: &mut Connection,
    mut expected: SessionRecord,
    cut: Option<(MaterializedSession, mj_core::relay::LiveSessionBaseline)>,
    updated_at: String,
    sessions_root: &Path,
) -> Result<SessionRecord> {
    mj_core::test_hooks::reach_test_hook("adoption_install_write_accepted")?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [&expected.workspace_id],
        |row| row.get::<_, bool>(0),
    )? {
        expected.workspace_id =
            super::workspaces::create_or_get_workspace_with(&tx, "Recovered")?.id;
    }
    let locator = expected
        .target
        .as_deref()
        .context("adoption has no locator")?;
    for (id, target) in super::state_io::read_targets(&tx, None)? {
        ensure!(
            id == expected.id || !target.same_resource(locator),
            "worker is already named by session {id}"
        );
    }
    if let Some(current) = super::state_io::load_session_with(&tx, &expected.id, sessions_root)? {
        ensure!(
            current.state == SessionState::Disconnected
                && current.native_session_id.is_none()
                && current.target == expected.target,
            "session {} changed before adoption install",
            expected.id
        );
    } else {
        validate_session_record(&expected)?;
        insert_session(&tx, &expected, sessions_root)?;
    }
    if let Some((projection, baseline)) = &cut {
        ensure!(
            projection.session_id == expected.id,
            "adoption cut belongs to another session"
        );
        super::materialized::install_live_session_baseline_with(&tx, projection, baseline)?;
    }
    tx.execute("UPDATE sessions SET state='disconnected', native_session_id=NULL, updated_at=?2, last_error=NULL WHERE session_id=?1", params![expected.id, updated_at])?;
    let current = super::state_io::load_session_with(&tx, &expected.id, sessions_root)?
        .context("adopted session disappeared")?;
    tx.commit()?;
    if cut
        .as_ref()
        .is_some_and(|(projection, _)| !projection.pending_elicitations.is_empty())
    {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(current)
}

/// Publish only the worker that the atomic adoption install claimed. Ordinary
/// catch-up runs without an install holder and may race a destroy or another attach.
pub async fn enqueue_adoption_connected<G: Send + 'static>(
    expected: SessionRecord,
    native_session_id: Option<String>,
    updated_at: String,
    guard: G,
) -> Result<AcceptedDatabaseWrite<SessionRecord, G>> {
    enqueue_database_write("publish adopted connection", guard, move |connection| {
        publish_adoption_connected_with(
            connection,
            &expected,
            native_session_id.as_deref(),
            &updated_at,
            &super::state_io::configured_sessions_root(),
        )
    })
    .await
}

fn publish_adoption_connected_with(
    connection: &mut Connection,
    expected: &SessionRecord,
    native_session_id: Option<&str>,
    updated_at: &str,
    sessions_root: &Path,
) -> Result<SessionRecord> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current = super::state_io::load_session_with(&tx, &expected.id, sessions_root)?
        .context("adopted session disappeared before attachment publication")?;
    ensure!(
        current.target == expected.target
            && matches!(
                current.state,
                SessionState::Disconnected | SessionState::Running
            ),
        "adopted worker changed during attachment"
    );
    if current.state == SessionState::Disconnected {
        ensure!(
            current.native_session_id.is_none(),
            "adoption attachment ownership changed"
        );
        mark_session_worker_connected_with(&tx, &expected.id, native_session_id, updated_at)?;
    }
    let current = super::state_io::load_session_with(&tx, &expected.id, sessions_root)?
        .context("adopted session disappeared")?;
    tx.commit()?;
    Ok(current)
}

/// Refuse a legacy provisional adoption without restoring stale session edits.
pub fn refuse_adoption_if_current(
    expected: &SessionRecord,
    detail: &str,
    updated_at: &str,
) -> Result<SessionRecord> {
    let expected = expected.clone();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("refuse legacy adoption", move |connection| {
        refuse_adoption_with(
            connection,
            &expected,
            &detail,
            &updated_at,
            &super::state_io::configured_sessions_root(),
        )
    })
}

fn refuse_adoption_with(
    connection: &mut Connection,
    expected: &SessionRecord,
    detail: &str,
    updated_at: &str,
    sessions_root: &Path,
) -> Result<SessionRecord> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut current = super::state_io::load_session_with(&tx, &expected.id, sessions_root)?
        .context("provisional adoption record disappeared")?;
    ensure!(
        current.state == SessionState::Disconnected
            && current.native_session_id.is_none()
            && current.target == expected.target
            && current.target_template_id == expected.target_template_id
            && current.last_profile == expected.last_profile,
        "adoption refusal ownership changed"
    );
    let previous_error = super::events::previous_session_error(&tx, &current.id)?;
    current.target = None;
    current.state = SessionState::Lost;
    current.last_error = Some(match current.last_error {
        Some(previous) => format!("{previous}\n{detail}"),
        None => detail.to_owned(),
    });
    current.updated_at = updated_at.to_owned();
    tx.execute(
        "UPDATE sessions SET state=?2, last_error=?3, updated_at=?4 WHERE session_id=?1",
        params![
            current.id,
            current.state.as_str(),
            current.last_error,
            current.updated_at
        ],
    )?;
    super::events::record_session_fault_transition(&tx, &current, previous_error)?;
    super::session_create::settle_create_intent(&tx, &current)?;
    super::state_io::replace_targets(&tx, &current)?;
    let current = super::state_io::load_session_with(&tx, &expected.id, sessions_root)?
        .context("refused adoption record disappeared")?;
    tx.commit()?;
    Ok(current)
}

/// Record a failed adoption only while its disconnected resource owner is still
/// current. A successful concurrent adoption must never be demoted by a loser.
pub fn record_adoption_failure_if_current(
    expected: &SessionRecord,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionRecord>> {
    let expected = expected.clone();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("record adoption failure", move |_| {
        record_adoption_failure_if_current_to(&database_path(), &expected, &detail, &updated_at)
    })
}

pub(super) fn record_adoption_failure_if_current_to(
    path: &Path,
    expected: &SessionRecord,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionRecord>> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let Some(mut current) = super::state_io::load_session_with(
        &tx,
        &expected.id,
        &super::state_io::sessions_root_for(path),
    )?
    else {
        return Ok(None);
    };
    if current.state == SessionState::Disconnected
        && current.native_session_id.is_none()
        && current.target == expected.target
        && current.target_template_id == expected.target_template_id
        && current.last_profile == expected.last_profile
        && current.bundle_id == expected.bundle_id
    {
        current.last_error = Some(detail.to_owned());
        current.updated_at = updated_at.to_owned();
        update_lifecycle_fields(&tx, &current)?;
    }
    tx.commit()?;
    Ok(Some(current))
}

/// Apply either a live preparation failure or a dead worker's startup failure
/// only to the lifecycle state and revision that the caller observed.
pub fn fail_unready_session(
    session_id: &str,
    cause: &str,
    observed_state: SessionState,
    observed_updated_at: &str,
) -> Result<Option<SessionRecord>> {
    let session_id = session_id.to_owned();
    let cause = cause.to_owned();
    let observed_updated_at = observed_updated_at.to_owned();
    submit_database_write("fail_unready_session", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(mut record) = load_session_with(
            &tx,
            &session_id,
            &super::state_io::configured_sessions_root(),
        )?
        else {
            return Ok(None);
        };
        if !matches!(
            observed_state,
            SessionState::Running | SessionState::Disconnected
        ) || record.state != observed_state
            || record.updated_at != observed_updated_at
        {
            return Ok(None);
        }
        record.state = SessionState::Error;
        record.updated_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        record.last_error = Some(cause);
        update_lifecycle_fields(&tx, &record)?;
        tx.commit()?;
        Ok(Some(record))
    })
}

/// Acceptance of Suspend owns its intent, readiness and Closing publication.
pub fn save_suspension_intent(
    session: &SessionRecord,
    ready_move: Option<&mj_core::state::MoveOperation>,
    consent: Option<&mj_core::suspend_consent::SuspendConsent>,
) -> Result<()> {
    let mut session = session.clone();
    let ready_move = ready_move.cloned();
    if let Some(consent) = consent {
        consent.validate()?;
    }
    let consent_json = consent.map(serde_json::to_string).transpose()?;
    submit_database_write("save suspension intent", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut operation = super::session_move::load_move_operation_with(&tx, &session.id)?;
        if let Some(operation) = operation.as_mut() {
            if operation.is_resolved() {
                tx.execute("UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1", [&session.id])?;
            } else {
                if let Some(ready) = ready_move
                    .as_ref()
                    .filter(|ready| ready.operation_id == operation.operation_id)
                {
                    operation.destination_target = ready.destination_target.clone();
                    operation.destination_native_session_id =
                        ready.destination_native_session_id.clone();
                    operation.updated_at = session.updated_at.clone();
                    super::session_move::save_move_operation_with(&tx, operation)?;
                }
                // Only this accepted request grants close authority. A Move's
                // cancellation/phase alone never does, even for historical rows.
                if session.state != SessionState::Destroying {
                    session.state = SessionState::Closing;
                }
                tx.execute("UPDATE session_moves SET close_requested_operation_id=?2 WHERE session_id=?1 AND operation_id=?2", params![session.id, operation.operation_id])?;
            }
        }
        validate_session_record(&session)?;
        update_lifecycle_fields(&tx, &session)?;
        tx.execute(
            "UPDATE sessions SET closing_consent = ?2 WHERE session_id = ?1",
            params![session.id, consent_json],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// Read the persisted suspend consent for a session currently in `Closing`.
pub fn load_closing_consent(
    session_id: &str,
) -> Result<Option<mj_core::suspend_consent::SuspendConsent>> {
    let connection = open_reader(&database_path())?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT closing_consent FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    raw.as_deref()
        .map(|json| serde_json::from_str(json).context("decode sessions.closing_consent"))
        .transpose()
}

pub fn save_failed_close(
    session: &SessionRecord,
    preserve_close_intent: bool,
) -> Result<SessionRecord> {
    let mut session = session.clone();
    submit_database_write("save failed close", move |connection| {
        validate_session_record(&session)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: String = tx.query_row(
            "SELECT state FROM sessions WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )?;
        session.state = stored_session_state(&current);
        if !preserve_close_intent {
            let operation = super::session_move::load_move_operation_with(&tx, &session.id)?;
            let close_intent: Option<(String, Option<String>)> = tx.query_row(
                "SELECT operation_id, close_requested_operation_id FROM session_moves WHERE session_id=?1",
                [&session.id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let accepted_move_close = operation
                .as_ref()
                .filter(|operation| !operation.is_resolved())
                .zip(close_intent.as_ref())
                .is_some_and(|(operation, (row_id, close_id))| {
                    super::session_move::exact_move_close_intent(
                        &operation.operation_id,
                        row_id,
                        close_id.as_deref(),
                    )
                });
            let unresolved_move = operation
                .as_ref()
                .is_some_and(|operation| !operation.is_resolved());
            if (session.state == SessionState::Closing && (!unresolved_move || accepted_move_close))
                || (session.state == SessionState::Provisioning && accepted_move_close)
            {
                session.state = SessionState::Error;
            }
        }
        update_lifecycle_fields(&tx, &session)?;
        // Verified teardown and an accepted worker stop both remain owned
        // until cleanup or stop observation settles them.
        if !preserve_close_intent && session.state != SessionState::Destroying {
            tx.execute(
                "UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1",
                [&session.id],
            )?;
        }
        tx.commit()?;
        Ok(session)
    })
}

/// A verified close of a restored destination also ends its cancelled Move.
/// Check the old placement before publishing the terminal lifecycle state.
pub fn save_closed_session(session: &SessionRecord) -> Result<()> {
    save_closed_session_with(session, true)
}

pub fn save_temporary_close(session: &SessionRecord) -> Result<()> {
    save_closed_session_with(session, false)
}

fn save_closed_session_with(session: &SessionRecord, suspended: bool) -> Result<()> {
    let session = session.clone();
    submit_database_write("save closed session", move |connection| {
        validate_session_record(&session)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ready_move = if session.state == SessionState::Stopped {
            super::session_move::load_move_operation_with(&tx, &session.id)?.filter(|operation| {
                operation.phase == mj_core::state::MovePhase::Cancelled
                    && !operation.queue_admission_started
                    && operation.destination_target.is_some()
            })
        } else {
            None
        };
        let ready_move = match ready_move {
            Some(operation)
                if super::state_io::read_targets(&tx, Some(&session.id))?
                    .iter()
                    .any(|(_, target)| Some(target) == operation.destination_target.as_ref()) =>
            {
                Some(operation)
            }
            _ => None,
        };
        update_lifecycle_fields(&tx, &session)?;
        if let Some(operation) = ready_move {
            super::session_move::resolve_cancelled_move_for_close_with(
                &tx,
                &session.id,
                &operation.operation_id,
                &session.updated_at,
            )?;
        }
        if session.state == SessionState::Stopped {
            tx.execute(
                "UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=?1",
                [&session.id],
            )?;
        }
        let purged = if suspended && session.state == SessionState::Stopped {
            purge_borrowed_descendants_in_tx(&tx, &session.id, None)?
        } else {
            Vec::new()
        };
        tx.commit()?;
        tool_details::unlink_deleted_sessions(&database_path(), &purged);
        Ok(())
    })
}

/// Settle an explicit suspend without a fresh checkpoint.
pub fn save_suspended_session(session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;
    ensure!(
        session.state == SessionState::Stopped,
        "suspend has not settled"
    );
    let session = session.clone();
    submit_database_write("settle suspend", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        update_lifecycle_fields(&tx, &session)?;
        let purged = purge_borrowed_descendants_in_tx(&tx, &session.id, None)?;
        tx.commit()?;
        tool_details::unlink_deleted_sessions(&database_path(), &purged);
        Ok(())
    })
}

/// An already-stopped parent needs no record rewrite. In particular, deferred
/// cleanup can update its locator independently.
pub fn settle_stopped_suspend(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("settle already-stopped suspend", move |connection| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1 AND state='stopped')",
                [&session_id],
                |row| row.get::<_, bool>(0),
            )?,
            "suspend no longer names a stopped session"
        );
        let purged = purge_borrowed_descendants_in_tx(&tx, &session_id, None)?;
        tx.commit()?;
        tool_details::unlink_deleted_sessions(&database_path(), &purged);
        Ok(())
    })
}

/// Clear exactly the failure observed at turn start. The writer owns the
/// comparison and update together so a concurrent lifecycle result survives.
pub fn clear_recorded_lifecycle_failure(
    previous: &SessionRecord,
    updated_at: &str,
) -> Result<bool> {
    if previous.public_error().is_none() {
        return Ok(false);
    }
    let previous = previous.clone();
    let updated_at = updated_at.to_owned();
    submit_database_write("clear_recorded_lifecycle_failure", move |connection| {
        Ok(connection.execute(
            "UPDATE sessions SET last_error = NULL, updated_at = ?3
             WHERE session_id = ?1 AND last_error = ?2",
            params![previous.id, previous.last_error, updated_at],
        )? == 1)
    })
}

/// Install a lifecycle transition together with the checkpoint it just
/// verified and the harness session id that produced it.
/// Point one checkpoint row at a replacement archive when it still names
/// `expected_archive_path` and `expected_sha256`.
///
/// The update changes only `archive_path` and `sha256`, in one immediate
/// transaction. `created_at` and `event_frontier` stay as they are. A missing
/// row or a row that no longer matches returns `Ok(false)` so a concurrent
/// checkpoint write is left alone. An illegal digest is an error from the
/// table's check constraint.
pub fn replace_checkpoint_archive_if_current(
    session_id: &str,
    expected_archive_path: &Path,
    expected_sha256: &str,
    replacement: &CheckpointMetadata,
) -> Result<bool> {
    let session_id = session_id.to_owned();
    let expected_archive_path = expected_archive_path.to_path_buf();
    let expected_sha256 = expected_sha256.to_owned();
    let replacement = replacement.clone();
    submit_database_write("replace_checkpoint_archive_if_current", move |connection| {
        replace_checkpoint_archive_if_current_on(
            connection,
            &session_id,
            &expected_archive_path,
            &expected_sha256,
            &replacement,
            &super::state_io::configured_sessions_root(),
        )
    })
}

pub(super) fn replace_checkpoint_archive_if_current_on(
    connection: &mut Connection,
    session_id: &str,
    expected_archive_path: &Path,
    expected_sha256: &str,
    replacement: &CheckpointMetadata,
    sessions_root: &Path,
) -> Result<bool> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Every managed row is stored relative, and callers hold decoded paths.
    let expected_archive_path =
        super::state_io::checkpoint_archive_blob(expected_archive_path, sessions_root);
    let archive_path =
        super::state_io::checkpoint_archive_blob(&replacement.archive_path, sessions_root);
    let changed = tx.execute(
        "UPDATE session_checkpoints
             SET archive_path = ?3, sha256 = ?4
             WHERE session_id = ?1 AND archive_path = ?2 AND sha256 = ?5",
        params![
            session_id,
            expected_archive_path,
            archive_path,
            replacement.sha256,
            expected_sha256,
        ],
    )?;
    tx.commit()?;
    Ok(changed == 1)
}

pub fn save_checkpointed_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_checkpointed_session", move |_| {
        save_checkpointed_session_to(&database_path(), &session)
    })
}

/// Recover lifecycle rows stranded by a process exit during checkpoint
/// creation. This must be called once by the top-level controller process
/// while it owns the controller-store guard, not by per-operation reloads.
pub fn recover_interrupted_checkpointing_sessions(updated_at: &str) -> Result<usize> {
    let updated_at = updated_at.to_owned();
    submit_database_write("recover_interrupted_checkpointing_sessions", move |_| {
        recover_interrupted_checkpointing_sessions_to(&database_path(), &updated_at)
    })
}

/// Change only the user-owned display name. This avoids writing a stale
/// SessionRecord over independently committed checkpoint or relay metadata.
pub fn set_session_title_override(session_id: &str, title: &str, updated_at: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let title = title.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_title_override", move |_| {
        set_session_title_override_to(&database_path(), &session_id, &title, &updated_at)
    })
}

/// Rewrite a configured profile id in every persisted session and recorded
/// turn in one SQLite transaction. Configuration is stored separately, so the controller owns
/// coordinating this update with the matching config-map rename.
pub fn rename_profile_references(old_id: &str, new_id: &str) -> Result<usize> {
    let old_id = old_id.to_owned();
    let new_id = new_id.to_owned();
    submit_database_write("rename_profile_references", move |_| {
        rename_profile_references_at(&database_path(), &old_id, &new_id)
    })
}

pub(super) fn rename_profile_references_at(
    path: &Path,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    ensure!(
        !old_id.is_empty() && !new_id.is_empty(),
        "profile ids cannot use the reserved legacy preference key"
    );
    if old_id == new_id {
        return Ok(0);
    }
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions SET last_profile = ?2 WHERE last_profile = ?1",
        params![old_id, new_id],
    )?;
    tx.execute(
        "UPDATE session_contexts SET profile_id = ?2 WHERE profile_id = ?1",
        params![old_id, new_id],
    )?;
    tx.execute(
        "UPDATE session_turn_selections SET profile = ?2 WHERE profile = ?1",
        params![old_id, new_id],
    )?;
    // The source wins on a rename. A retry after commit finds no source row.
    tx.execute(
        "INSERT INTO subagent_preference(profile_id, policy)
         SELECT ?2, policy FROM subagent_preference WHERE profile_id = ?1
         ON CONFLICT(profile_id) DO UPDATE SET policy = excluded.policy",
        params![old_id, new_id],
    )?;
    tx.execute(
        "DELETE FROM subagent_preference WHERE profile_id = ?1",
        [old_id],
    )?;
    tx.execute(
        "INSERT INTO oracle_preference(profile_id, enabled)
         SELECT ?2, enabled FROM oracle_preference WHERE profile_id = ?1
         ON CONFLICT(profile_id) DO UPDATE SET enabled = excluded.enabled",
        params![old_id, new_id],
    )?;
    tx.execute(
        "DELETE FROM oracle_preference WHERE profile_id = ?1",
        [old_id],
    )?;
    tx.commit()?;
    Ok(changed)
}

/// Rewrite a configured target id in every persisted session in one SQLite
/// transaction.
pub fn rename_target_references(old_id: &str, new_id: &str) -> Result<usize> {
    rename_session_reference("target_template_id", old_id, new_id)
}

pub(super) fn rename_session_reference(
    column: &'static str,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    ensure!(
        column == "target_template_id",
        "unsupported session reference column"
    );
    let old_id = old_id.to_owned();
    let new_id = new_id.to_owned();
    submit_database_write("rename_session_reference", move |_| {
        rename_session_reference_at(&database_path(), column, &old_id, &new_id)
    })
}

pub(super) fn rename_session_reference_at(
    path: &Path,
    column: &str,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        &format!("UPDATE sessions SET {column} = ?2 WHERE {column} = ?1"),
        params![old_id, new_id],
    )?;
    tx.commit()?;
    Ok(changed)
}

/// Change only whether the resume dialog hides this session. Archiving is a
/// display choice, so it has its own writer and never rewrites lifecycle,
/// checkpoint, or title columns another task owns.
pub fn set_session_archived(session_id: &str, archived: bool) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("set_session_archived", move |_| {
        set_session_archived_to(&database_path(), &session_id, archived)
    })
}

/// Record that the managed target of an otherwise live session is definitively
/// gone. A verified checkpoint keeps the session recoverable as an error on the
/// dashboard; without one, the session is lost. The state predicate keeps a
/// late poll result from overwriting a concurrent lifecycle transition.
pub fn mark_session_target_missing(
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("mark_session_target_missing", move |_| {
        mark_session_target_missing_to(&database_path(), &session_id, &detail, &updated_at)
    })
}

pub(super) fn mark_session_target_missing_to(
    path: &Path,
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    mark_session_target_missing_if_current_to(path, session_id, detail, updated_at, None)
}

/// Record a definitive worker failure only while the observed session record
/// is still current. A delayed background write must not invalidate a resume.
pub fn mark_session_target_missing_if_current(
    session_id: &str,
    detail: &str,
    updated_at: &str,
    observed_updated_at: &str,
) -> Result<Option<SessionState>> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    let observed_updated_at = observed_updated_at.to_owned();
    submit_database_write("mark_session_target_missing_if_current", move |_| {
        mark_session_target_missing_if_current_to(
            &database_path(),
            &session_id,
            &detail,
            &updated_at,
            Some(&observed_updated_at),
        )
    })
}

pub(super) fn mark_session_target_missing_if_current_to(
    path: &Path,
    session_id: &str,
    detail: &str,
    updated_at: &str,
    observed_updated_at: Option<&str>,
) -> Result<Option<SessionState>> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if super::session_restart::container_upgrade_owns_target_with(&tx, session_id)? {
        return Ok(None);
    }
    let previous_error = super::events::previous_session_error(&tx, session_id)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET state = CASE
                 WHEN EXISTS(
                     SELECT 1 FROM session_checkpoints
                     WHERE session_checkpoints.session_id = sessions.session_id
                 ) THEN 'error'
                 ELSE 'lost'
             END,
             last_error = ?2,
             updated_at = ?3
         WHERE session_id = ?1
           AND (?4 IS NULL OR updated_at = ?4)
           AND state IN ('running', 'disconnected')",
        params![session_id, detail, updated_at, observed_updated_at],
    )?;
    ensure!(changed <= 1, "updated {changed} sessions for {session_id}");
    let state = if changed == 1 {
        let stored: String = tx.query_row(
            "SELECT state FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        Some(stored_session_state(&stored))
    } else {
        None
    };
    if changed == 1 && previous_error.as_deref() != Some(detail) {
        super::events::insert_api_event(
            &tx,
            session_id,
            Utc::now().timestamp_millis(),
            &ApiEventData::SessionFault {
                reason: mj_core::event_outcome::OutcomeReason::RuntimeUnavailable,
                message: detail.into(),
                command_id: None,
            },
        )?;
    }
    tx.commit()?;
    Ok(state)
}

pub(super) fn set_session_archived_to(path: &Path, session_id: &str, archived: bool) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET archived = ?2 WHERE session_id = ?1",
        params![session_id, archived],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Change only whether this session raises notifications. Ignoring is a
/// display choice, so it has its own writer and never rewrites lifecycle,
/// checkpoint, or title columns another task owns.
pub fn set_session_notifications_ignored(session_id: &str, ignored: bool) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("set_session_notifications_ignored", move |_| {
        set_session_notifications_ignored_to(&database_path(), &session_id, ignored)
    })
}

pub(super) fn set_session_notifications_ignored_to(
    path: &Path,
    session_id: &str,
    ignored: bool,
) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET notifications_ignored = ?2 WHERE session_id = ?1",
        params![session_id, ignored],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Native sessions the resume dialog hides. Hel never writes into a harness
/// home, so the hidden set lives here instead of in the harness's own store.
pub fn hidden_native_sessions() -> Result<BTreeSet<(mj_core::config::HarnessKind, String)>> {
    hidden_native_sessions_from(&database_path())
}

pub(super) fn hidden_native_sessions_from(
    path: &Path,
) -> Result<BTreeSet<(mj_core::config::HarnessKind, String)>> {
    let connection = open_reader(path)?;
    let mut statement =
        connection.prepare("SELECT harness_kind, native_session_id FROM hidden_native_sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hidden = BTreeSet::new();
    for row in rows {
        let (harness, native_session_id) = row?;
        // Rows for a harness this release no longer supports are ignored, not
        // fatal; they simply hide nothing.
        match harness.parse::<mj_core::config::HarnessKind>() {
            Ok(harness) => {
                hidden.insert((harness, native_session_id));
            }
            Err(_) => tracing::warn!(
                harness = %harness,
                "ignoring a hidden native session for a harness that is no longer supported"
            ),
        }
    }
    Ok(hidden)
}

/// Hide or reveal one native session in the resume dialog.
pub fn set_native_session_hidden(
    harness: mj_core::config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    let native_session_id = native_session_id.to_owned();
    submit_database_write("set_native_session_hidden", move |_| {
        set_native_session_hidden_to(&database_path(), harness, &native_session_id, hidden)
    })
}

pub(super) fn set_native_session_hidden_to(
    path: &Path,
    harness: mj_core::config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    if native_session_id.trim().is_empty() {
        bail!("native session id is empty");
    }
    let connection = open(path)?;
    if hidden {
        connection.execute(
            "INSERT INTO hidden_native_sessions(harness_kind, native_session_id, hidden_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(harness_kind, native_session_id) DO NOTHING",
            params![harness.id(), native_session_id, Utc::now().to_rfc3339()],
        )?;
    } else {
        connection.execute(
            "DELETE FROM hidden_native_sessions
             WHERE harness_kind = ?1 AND native_session_id = ?2",
            params![harness.id(), native_session_id],
        )?;
    }
    Ok(())
}

/// Change only the per-session container provisioning inputs: the size
/// overrides and the attached directories. Everything else the session row
/// owns is left to its own writer.
pub fn set_session_container_settings(
    session_id: &str,
    allocation: Option<&SessionResourceAllocation>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let allocation = allocation.cloned();
    let mounts = mounts.to_vec();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_container_settings", move |_| {
        set_session_container_settings_to(
            &database_path(),
            &session_id,
            allocation.as_ref(),
            &mounts,
            &updated_at,
        )
    })
}

pub(super) fn set_session_container_settings_to(
    path: &Path,
    session_id: &str,
    allocation: Option<&SessionResourceAllocation>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    crate::targets::validate_additional_mounts(mounts)?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let restarting: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_restart_intents WHERE session_id = ?1)",
        [session_id],
        |row| row.get(0),
    )?;
    if restarting {
        return Err(anyhow::Error::new(mj_core::refusal::Refusal::precondition(
            "finish Restart or suspend the session before changing container settings",
        )));
    }
    write_container_settings(&tx, session_id, allocation, mounts, updated_at)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn write_container_settings(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    allocation: Option<&SessionResourceAllocation>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sessions
         SET resource_allocation = ?2, updated_at = ?3
         WHERE session_id = ?1",
        params![
            session_id,
            allocation.map(serde_json::to_string).transpose()?,
            updated_at
        ],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    replace_mounts(tx, session_id, mounts)
}

pub(super) fn set_session_title_override_to(
    path: &Path,
    session_id: &str,
    title: &str,
    updated_at: &str,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("session title is empty");
    }
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions
         SET session_title_override = ?2, updated_at = ?3
         WHERE session_id = ?1",
        params![session_id, title, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Persist a title resolved from a materialized relay view. The writer
/// checks the projection frontier in the same statement as the update, so
/// a delayed observation cannot overwrite a title from a newer view.
pub fn set_session_acp_title_at_projection(
    session_id: String,
    applied_ordinal: u64,
    applied_digest: String,
    title: Option<String>,
) -> Result<()> {
    submit_database_write("set_session_acp_title_at_projection", move |connection| {
        set_session_acp_title_at_projection_with(
            connection,
            &session_id,
            applied_ordinal,
            &applied_digest,
            title.as_deref(),
        )
    })
}

pub(super) fn set_session_acp_title_at_projection_with(
    connection: &Connection,
    session_id: &str,
    applied_ordinal: u64,
    applied_digest: &str,
    title: Option<&str>,
) -> Result<()> {
    let title = title.and_then(mj_core::state::normalize_session_title);
    connection.execute(
        "UPDATE sessions SET acp_session_title = ?2
         WHERE session_id = ?1 AND acp_session_title IS NOT ?2 AND EXISTS (
             SELECT 1 FROM materialized_sessions
             WHERE session_id = ?1 AND applied_event_ordinal = ?3 AND applied_event_digest = ?4
         )",
        params![session_id, title, applied_ordinal, applied_digest],
    )?;
    Ok(())
}

#[cfg(test)]
pub(super) fn set_session_acp_title_to(
    path: &Path,
    session_id: &str,
    title: Option<&str>,
) -> Result<()> {
    if title.is_some_and(|title| title.trim().is_empty()) {
        bail!("ACP session title is empty");
    }
    let title = title.and_then(mj_core::state::normalize_session_title);
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET acp_session_title = ?2 WHERE session_id = ?1",
        params![session_id, title],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Commit the successful handshake for a newly provisioned worker without
/// replacing checkpoint or display metadata owned by other controller tasks.
pub fn mark_session_worker_connected(
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<Option<String>> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.map(str::to_owned);
    let updated_at = updated_at.to_owned();
    let owns_worker = crate::worker_lifecycle::current(&session_id).is_some();
    submit_database_write("mark_session_worker_connected", move |_| {
        mark_session_worker_connected_to(
            &database_path(),
            &session_id,
            native_session_id.as_deref(),
            &updated_at,
            owns_worker,
        )
    })
}

/// Point a session at a native session its worker opened on its own. Only that
/// column moves: the session's lifecycle state belongs to whatever operation is
/// running.
pub fn adopt_native_session_id(session_id: &str, native_session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.to_owned();
    submit_database_write("adopt_native_session_id", move |_| {
        adopt_native_session_id_to(&database_path(), &session_id, &native_session_id)
    })
}

pub(super) fn adopt_native_session_id_to(
    path: &Path,
    session_id: &str,
    native_session_id: &str,
) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session_id, native_session_id],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

pub(super) fn mark_session_worker_connected_to(
    path: &Path,
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
    owns_worker: bool,
) -> Result<Option<String>> {
    if updated_at.trim().is_empty() {
        bail!("worker connection timestamp is empty");
    }
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    mark_session_worker_connected_with(&tx, session_id, native_session_id, updated_at)?;
    if owns_worker {
        let boot: Option<String> = tx.query_row(
            "SELECT operation_id FROM worker_restart_intents WHERE session_id=?1 AND phase != 'prepared'",
            [session_id],
            |row| row.get(0),
        ).optional()?;
        if let Some(operation_id) = boot {
            super::worker_restart::publish_worker_restart_aws_profile(
                &tx,
                session_id,
                &operation_id,
            )?;
        }
    }
    let staged = tx.query_row(
        "SELECT staged_aws_profile FROM sessions WHERE session_id=?1",
        [session_id],
        |row| row.get(0),
    )?;
    tx.commit()?;
    Ok(staged)
}

pub(super) fn mark_session_worker_connected_with(
    tx: &Transaction<'_>,
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sessions
         SET state = 'running',
             native_session_id = coalesce(?2, native_session_id),
             updated_at = ?3,
             last_error = NULL
         WHERE session_id = ?1",
        params![session_id, native_session_id, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    // The create commits in this write: nothing is left for a later daemon.
    super::session_create::settle_create_intent_for_state(tx, session_id, SessionState::Running)?;
    Ok(())
}

pub(super) fn recover_interrupted_checkpointing_sessions_to(
    path: &Path,
    updated_at: &str,
) -> Result<usize> {
    ensure!(
        !updated_at.trim().is_empty(),
        "checkpoint recovery timestamp is empty"
    );
    let mut connection = open(path)?;
    let tx = connection.transaction()?;
    let ids = {
        let mut query = tx.prepare("SELECT session_id FROM checkpoint_operations")?;
        query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in ids {
        super::events::finish_checkpoint_operation(
            &tx,
            &id,
            mj_core::event_outcome::CommandResultKind::Failed,
            Some(mj_core::event_outcome::OutcomeReason::ControllerRestarted),
            Some("Checkpoint operation interrupted by controller restart".into()),
        )?;
    }
    let changed = tx.execute("UPDATE sessions SET state = 'running', updated_at = ?1, last_checkpoint_error = ?2 WHERE state = 'checkpointing'",
        params![updated_at, "checkpointing was interrupted by a controller restart; the target was left running"])?;
    tx.commit()?;
    Ok(changed)
}

pub fn save_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    save_session_with_container_size_to(path, session, None)
}

pub(super) fn save_session_with_container_size_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(existing_bundle) = tx
        .query_row(
            "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
            [session.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && existing_bundle
            != super::projects::session_project_id(&tx, &session.id, &session.bundle_id)?
    {
        bail!(
            "session {} was already associated with bundle {}, not {}",
            session.id,
            existing_bundle,
            session.bundle_id
        );
    }
    let mut session = session.clone();
    let moving: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_moves WHERE session_id=?1
         AND json_extract(operation_json, '$.phase') IN (
             'capturing','transferring','restoring','destination_starting',
             'destination_owns','source_retired','queue_admitted','dispatch_released',
             'preparing','closing_source','resuming_destination','starting_queue'
         ))",
        [&session.id],
        |row| row.get(0),
    )?;
    if moving {
        // A Move may provision for minutes while clients keep editing drafts
        // and titles. Merge these independently owned fields in this same
        // transaction rather than restoring the lifecycle's earlier copy.
        let (draft, title, acp_title, viewed, archived) = tx.query_row(
            "SELECT draft_input, session_title_override, acp_session_title, viewed_through_event_ordinal, archived
             FROM sessions WHERE session_id=?1", [&session.id], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?,
                row.get::<_, u64>(3)?, row.get::<_, bool>(4)?,
            )),
        )?;
        session.draft_input = draft;
        session.session_title_override = title;
        session.acp_session_title = acp_title;
        session.viewed_through_event_ordinal = viewed;
        session.archived = archived;
    }
    insert_session(&tx, &session, &super::state_io::sessions_root_for(path))?;
    if let Some((host, size)) = container_size {
        write_host_container_size(&tx, host, size)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn save_lifecycle_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.commit()?;
    Ok(())
}

/// Settle teardown and archive failed children in the same owner transaction.
pub fn save_startup_cleanup_outcome(session: &SessionRecord, archive: bool) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_startup_cleanup_outcome", move |_| {
        validate_session_record(&session)?;
        let mut connection = open(&database_path())?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        update_lifecycle_fields(&tx, &session)?;
        if session.state == SessionState::Provisioning {
            // A retried child start continues its create.
            super::session_create::restore_create_intent_for_retry(&tx, &session)?;
        }
        if archive {
            tx.execute(
                "UPDATE sessions SET archived=1 WHERE session_id=?1",
                [&session.id],
            )?;
            // Only the non-retry cleanup outcome owns final startup failure.
            super::mailbox::settle_failed_parent_messages_with(&tx, &session)?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub(super) fn save_checkpointed_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session.id, session.native_session_id],
    )?;
    replace_checkpoint(&tx, session, &super::state_io::sessions_root_for(path))?;
    tx.commit()?;
    Ok(())
}

pub(super) fn validate_session_record(session: &SessionRecord) -> Result<()> {
    ensure!(
        session
            .staged_aws_profile
            .as_deref()
            .is_none_or(mj_core::aws_credentials::valid_profile_name),
        "invalid staged AWS profile name"
    );
    let mut validation = State::default();
    validation
        .sessions
        .insert(session.id.clone(), session.clone());
    validation.validate()
}

/// Remove one operational session while retaining its relational history
/// context. A database trigger removes its prompt history with it.
pub fn delete_session(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("delete_session", move |_| {
        delete_session_from(&database_path(), &session_id)
    })
}

pub(super) fn delete_session_from(path: &Path, session_id: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction()?;
    delete_session_in_tx(&tx, session_id)?;
    tx.commit()?;
    remove_deleted_session_tool_details(path, session_id);
    Ok(())
}

pub(super) fn remove_deleted_session_tool_details(path: &Path, session_id: &str) {
    tool_details::unlink_deleted_sessions(path, &[session_id]);
}

fn delete_session_in_tx(tx: &Transaction<'_>, session_id: &str) -> Result<()> {
    mj_core::config::validate_id("session", session_id)?;
    tx.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
    Ok(())
}

pub(super) fn purge_borrowed_descendants_in_tx(
    tx: &Transaction<'_>,
    root_session_id: &str,
    exclude_environment_id: Option<&str>,
) -> Result<Vec<String>> {
    // Walk borrowed ancestry only. A candidate with a retained descendant
    // must also survive, since subagent_sessions retains a parent reference.
    let mut statement = tx.prepare(
        "WITH RECURSIVE
         borrowed(id, depth, path, owner, state, environment_id) AS (
             SELECT ?1, 0, '/' || hex(?1) || '/', NULL, NULL, NULL
             UNION ALL
             SELECT a.child_session_id, b.depth + 1,
                    b.path || hex(a.child_session_id) || '/', e.owner_session_id, s.state, e.environment_id
             FROM borrowed b
             JOIN subagent_sessions a ON a.parent_session_id=b.id
             JOIN sessions s ON s.session_id=a.child_session_id
             JOIN environment_bindings eb ON eb.session_id=s.session_id
             JOIN environments e ON e.environment_id=eb.environment_id
             WHERE e.owner_session_id <> s.session_id
               AND instr(b.path, '/' || hex(a.child_session_id) || '/')=0
         ),
         candidates AS (
             SELECT id, depth FROM borrowed
             WHERE owner=?1 AND state IN ('stopped', 'error')
               AND (?2 IS NULL OR environment_id <> ?2)
         ),
         retained(id) AS (
             SELECT c.id FROM candidates c
             JOIN subagent_sessions a ON a.parent_session_id=c.id
             WHERE NOT EXISTS (SELECT 1 FROM candidates WHERE id=a.child_session_id)
             UNION
             SELECT c.id FROM retained r
             JOIN subagent_sessions a ON a.child_session_id=r.id
             JOIN candidates c ON c.id=a.parent_session_id
         )
         SELECT id FROM candidates
         WHERE id NOT IN (SELECT id FROM retained)
         ORDER BY depth DESC, id",
    )?;
    let ids = statement
        .query_map(params![root_session_id, exclude_environment_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in &ids {
        delete_session_in_tx(tx, id)?;
    }
    Ok(ids)
}

/// Retire a submitted shared draft without erasing a newer client's edit.
pub fn clear_session_draft_input_if_matches(session_id: &str, expected: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let expected = expected.to_owned();
    submit_database_write("clear_session_draft_input_if_matches", move |connection| {
        connection.execute(
            "UPDATE sessions SET draft_input = '' WHERE session_id = ?1 AND draft_input = ?2",
            params![session_id, expected],
        )?;
        Ok(())
    })
}

/// Re-associate a session with another project bundle.
///
/// A session's bundle is otherwise fixed, because prompt history is grouped by
/// it. Resume calls this when it converts a session between its raw and bundle
/// representations: the project is the same, so its history follows it, and
/// only the name Hel files it under changes.
/// Files the session under `bundle_id`'s project and returns the bundle id the
/// session context now holds: the canonical project when the catalog knows
/// `bundle_id` as an alias of another bundle. A caller keeping the session
/// record must adopt the returned id, because resume publication checks the
/// record's bundle against the context.
pub fn rebind_session_bundle(session_id: &str, bundle_id: &str) -> Result<String> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    submit_database_write("rebind_session_bundle", move |_| {
        rebind_session_bundle_to(&database_path(), &session_id, &bundle_id)
    })
}

pub(super) fn rebind_session_bundle_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
) -> Result<String> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let canonical_bundle = super::projects::canonical_project_id(&tx, bundle_id)?;
    tx.execute("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE session_id=?1",[session_id])?;
    let changed = tx.execute(
        "UPDATE session_contexts SET bundle_id = ?2 WHERE session_id = ?1",
        params![session_id, canonical_bundle],
    )?;
    if changed == 0 {
        tx.execute(
            "INSERT INTO session_contexts(session_id, bundle_id, created_at, profile_id)
             VALUES (?1, ?2, ?3, (SELECT last_profile FROM sessions WHERE session_id = ?1))",
            params![session_id, canonical_bundle, Utc::now().to_rfc3339()],
        )?;
    }
    tx.commit()?;
    Ok(canonical_bundle)
}

/// Compare and dismiss under the database writer, so a concurrent new failure
/// cannot inherit the receipt for the failure the person saw.
pub fn dismiss_recorded_failure(
    session_id: &str,
    failure: mj_core::attention::FailureIdentity,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let json = serde_json::to_string(&failure)?;
    submit_database_write("dismiss recorded failure", move |connection| {
        let changed = connection.execute(
            "UPDATE sessions SET dismissed_failure_json=?2 WHERE session_id=?1 AND state=?3 AND last_error IS ?4 AND failure_generation=?5",
            params![session_id, json, failure.state.as_str(), failure.last_error, failure.generation],
        )?;
        ensure!(changed == 1, "notification is no longer current");
        Ok(())
    })
}

/// What a resume may change about the session it restarts.
#[derive(Clone)]
pub struct SessionResumeOptions {
    pub additional_mounts: Option<Vec<crate::targets::AdditionalMount>>,
    pub resource_allocation: Option<mj_core::state::SessionResourceAllocation>,
    pub discard_queue: bool,
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    // The unguarded catch-up must not resurrect a lifecycle that won before publication.
    #[test]
    fn adoption_connection_publication_does_not_resurrect_a_stopped_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("adoption-race.sqlite3");
        let mut expected = super::super::tests::session("adoption-owner", "project");
        expected.state = SessionState::Disconnected;
        expected.native_session_id = None;
        expected.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: PathBuf::from(format!("/workers/{}", expected.id)),
        }));
        save_session_to(&path, &expected).unwrap();
        let mut stopped = expected.clone();
        stopped.state = SessionState::Stopped;
        stopped.target = None;
        save_session_to(&path, &stopped).unwrap();
        let stopped = super::super::state_io::load_session_with(
            &open(&path).unwrap(),
            &expected.id,
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap()
        .unwrap();
        let mut connection = open(&path).unwrap();
        assert!(
            publish_adoption_connected_with(
                &mut connection,
                &expected,
                Some("late-native"),
                "2030-01-01T00:00:00Z",
                &crate::database::state_io::configured_sessions_root()
            )
            .is_err()
        );
        assert_eq!(
            super::super::state_io::load_session_with(
                &connection,
                &expected.id,
                &crate::database::state_io::configured_sessions_root()
            )
            .unwrap()
            .unwrap(),
            stopped
        );
    }

    // Hard-won: b8acf76: a legacy refusal rewrote concurrent title/notification edits from a stale record.
    #[test]
    fn legacy_adoption_refusal_preserves_concurrent_edits_and_resource_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-adoption.sqlite3");
        let mut expected = super::super::tests::session("legacy-owner", "project");
        expected.state = SessionState::Disconnected;
        expected.native_session_id = None;
        expected.last_error = Some("previous diagnosis".into());
        expected.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: PathBuf::from(format!("/workers/{}", expected.id)),
        }));
        save_session_to(&path, &expected).unwrap();
        let mut connection = open(&path).unwrap();
        connection.execute("UPDATE sessions SET title='concurrent edit', notifications_ignored=1 WHERE session_id=?1", [&expected.id]).unwrap();
        let refused = refuse_adoption_with(
            &mut connection,
            &expected,
            "legacy refusal",
            "2030-01-01T00:00:00Z",
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap();
        assert_eq!(refused.title, "concurrent edit");
        assert!(refused.notifications_ignored);
        assert_eq!(refused.state, SessionState::Lost);
        assert!(refused.target.is_none());
        assert_eq!(
            refused.last_error.as_deref(),
            Some("previous diagnosis\nlegacy refusal")
        );
        // A later lifecycle owner cannot be detached by the same stale refusal.
        let mut stopped = refused;
        stopped.state = SessionState::Stopped;
        save_session_to(&path, &stopped).unwrap();
        let stopped = super::super::state_io::load_session_with(
            &connection,
            &expected.id,
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap()
        .unwrap();
        assert!(
            refuse_adoption_with(
                &mut connection,
                &expected,
                "late refusal",
                "2030-01-01T00:00:01Z",
                &crate::database::state_io::configured_sessions_root()
            )
            .is_err()
        );
        assert_eq!(
            super::super::state_io::load_session_with(
                &connection,
                &expected.id,
                &crate::database::state_io::configured_sessions_root()
            )
            .unwrap()
            .unwrap(),
            stopped
        );
    }

    // Hard-won: de056ae: a late adoption failure overwrote a successful worker connection.
    #[test]
    fn a_stale_adoption_failure_preserves_the_connected_owner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("adoption.sqlite3");
        let mut stale = super::super::tests::session("adoption-owner", "project");
        stale.state = SessionState::Disconnected;
        stale.native_session_id = None;
        save_session_to(&path, &stale).unwrap();
        mark_session_worker_connected_to(
            &path,
            &stale.id,
            Some("surviving-native"),
            "2030-01-01T00:00:00Z",
            false,
        )
        .unwrap();
        let current = record_adoption_failure_if_current_to(
            &path,
            &stale,
            "late failure",
            "2030-01-01T00:00:01Z",
        )
        .unwrap()
        .unwrap();
        assert_eq!(current.state, SessionState::Running);
        assert_eq!(
            current.native_session_id.as_deref(),
            Some("surviving-native")
        );
        assert!(current.last_error.is_none());
        let saved = load_state_from(&path).unwrap();
        assert_eq!(saved.sessions[&stale.id], current);
        // Hard-won: 3210bf3: a captured failure owner must not outlive identity changes.
        for field in ["profile", "target"] {
            let mut changed = stale.clone();
            changed.last_error = Some("new owner's diagnosis".into());
            match field {
                "profile" => changed.last_profile.push_str("-new"),
                "target" => changed.target_template_id.push_str("-new"),
                _ => unreachable!(),
            }
            save_session_to(&path, &changed).unwrap();
            let expected = load_state_from(&path).unwrap().sessions[&stale.id].clone();
            let actual = record_adoption_failure_if_current_to(
                &path,
                &stale,
                "stale refusal",
                "2030-01-01T00:00:02Z",
            )
            .unwrap()
            .unwrap();
            assert_eq!(actual, expected);
        }
    }

    // Hard-won: #1538: a resume save overwrote a Container settings edit made while it provisioned
    #[test]
    fn resume_and_rollback_preserve_edits_committed_after_their_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resume.sqlite3");
        let original = super::super::tests::session("resume-owner", "project");
        save_session_to(&path, &original).unwrap();
        let connection = open(&path).unwrap();
        connection
            .execute(
                "UPDATE sessions SET session_title_override = 'renamed during resume',
                 acp_session_title = 'new worker title', draft_input = 'new draft',
                 archived = 1, viewed_through_event_ordinal = 99 WHERE session_id = ?1",
                [&original.id],
            )
            .unwrap();
        let mut provisioning = original.clone();
        provisioning.state = SessionState::Provisioning;
        provisioning.target = None;
        provisioning.native_session_id = Some("resumed-native".into());
        provisioning.additional_mounts.clear();
        save_resumed_session_to(&path, &provisioning, None).unwrap();
        let current = load_session_with(
            &connection,
            &original.id,
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(current.state, SessionState::Provisioning);
        assert_eq!(current.native_session_id, provisioning.native_session_id);
        assert_eq!(current.additional_mounts, original.additional_mounts);
        assert_eq!(
            current.session_title_override.as_deref(),
            Some("renamed during resume")
        );
        assert_eq!(
            current.acp_session_title.as_deref(),
            Some("new worker title")
        );
        assert_eq!(current.draft_input, "new draft");
        assert!(current.archived);
        assert_eq!(current.viewed_through_event_ordinal, 99);

        let allocation = SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        };
        let mounts = vec![AdditionalMount {
            source: "/new/source".into(),
            destination: "/new/destination".into(),
            access: crate::targets::MountAccess::Ro,
        }];
        set_session_container_settings_to(
            &path,
            &original.id,
            Some(&allocation),
            &mounts,
            "2026-11-13T00:00:00Z",
        )
        .unwrap();
        // The converted-checkpoint publication still holds the provisioning snapshot.
        provisioning.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
            archive_path: "converted.hel.zip".into(),
            sha256: "c".repeat(64),
            created_at: "2026-11-13T00:01:00Z".into(),
            event_frontier: 1,
        }));
        save_resumed_session_to(&path, &provisioning, None).unwrap();
        let converted = load_session_with(
            &connection,
            &original.id,
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(converted.resource_allocation, Some(allocation.clone()));
        assert_eq!(converted.additional_mounts, mounts);

        // Failure restores a snapshot captured before those independent edits.
        save_resumed_session_to(&path, &original, None).unwrap();
        let rolled_back = load_session_with(
            &connection,
            &original.id,
            &crate::database::state_io::configured_sessions_root(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(rolled_back.state, original.state);
        assert_eq!(rolled_back.additional_mounts, mounts);
        assert_eq!(rolled_back.resource_allocation, Some(allocation));
        assert_eq!(
            rolled_back.session_title_override,
            current.session_title_override
        );
        assert_eq!(rolled_back.acp_session_title, current.acp_session_title);
        assert_eq!(rolled_back.draft_input, current.draft_input);
        assert_eq!(rolled_back.archived, current.archived);
        assert_eq!(rolled_back.viewed_through_event_ordinal, 99);
    }
}
