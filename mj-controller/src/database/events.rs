//! Durable, ordered events for the native subagent API.
use super::*;
use mj_core::event_outcome::{CommandOwner, CommandResult, CommandResultKind, OutcomeReason};

#[cfg(test)]
use mj_core::elicitation::ElicitationRequest;

pub fn load_api_events(
    filter: &ApiEventFilter,
    after_seq: Option<u64>,
    limit: usize,
) -> Result<ApiEventPage> {
    load_api_events_from(&database_path(), filter, after_seq, limit)
}

pub(super) fn load_api_events_from(
    path: &Path,
    filter: &ApiEventFilter,
    after_seq: Option<u64>,
    limit: usize,
) -> Result<ApiEventPage> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    // sqlite_sequence survives deletion of the last event; cursors never move back.
    let latest_seq: u64 = tx.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'api_events'), 0)",
        [],
        |r| r.get(0),
    )?;
    let after_seq = after_seq.unwrap_or(latest_seq);
    let mut statement = tx.prepare("SELECT e.seq, e.session_id, e.recorded_at_ms, e.body FROM api_events e JOIN session_contexts w ON w.session_id = e.session_id WHERE e.seq > ?1 AND (?2 IS NULL OR e.session_id = ?2) AND (?3 IS NULL OR w.workspace_id = ?3) ORDER BY e.seq LIMIT ?4")?;
    let events = statement
        .query_map(
            params![
                after_seq,
                filter.session_id,
                filter.workspace_id,
                limit.clamp(1, 1000) as i64
            ],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?
        .map(|r| {
            let (seq, session_id, recorded_at_ms, body) = r?;
            Ok(ApiEvent {
                seq,
                session_id,
                recorded_at_ms,
                event: serde_json::from_str(&body)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let next_after_seq = events
        .last()
        .map_or(latest_seq.max(after_seq), |event| event.seq);
    // Historical activity is disposable. Advance over it without replaying it,
    // including while the background cleanup is still removing old rows.
    let events = events
        .into_iter()
        .filter(|event| !matches!(event.event, ApiEventData::ActivityChanged { .. }))
        .collect();
    Ok(ApiEventPage {
        events,
        next_after_seq,
        latest_seq,
    })
}

pub(super) fn insert_api_event(
    tx: &Transaction<'_>,
    session_id: &str,
    recorded_at_ms: i64,
    event: &ApiEventData,
) -> Result<()> {
    tx.execute(
        "INSERT INTO api_events(session_id, recorded_at_ms, body) VALUES (?1, ?2, ?3)",
        params![session_id, recorded_at_ms, serde_json::to_string(event)?],
    )?;
    let seq = tx.last_insert_rowid() as u64;
    super::operator_away::project_event(tx, session_id, seq, recorded_at_ms, event)?;
    if let ApiEventData::InputRequired {
        request: Some(request),
        ..
    } = event
    {
        super::subagent_questions::forward_subagent_question_with(
            tx,
            session_id,
            seq,
            recorded_at_ms,
            request,
        )?;
    }
    Ok(())
}

/// Called on a blocking task; the shared writer serializes this with relay projection.
pub fn record_api_activities(
    activities: Vec<(String, ApiActivityState)>,
    recorded_at_ms: i64,
) -> Result<()> {
    submit_database_write("record_api_activities", move |connection| {
        record_api_activities_with(connection, activities, recorded_at_ms)
    })
}

pub(super) fn record_api_activities_with(
    connection: &mut Connection,
    activities: Vec<(String, ApiActivityState)>,
    _recorded_at_ms: i64,
) -> Result<()> {
    let tx = connection.transaction()?;
    for (session_id, activity) in activities {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            [&session_id],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let body = serde_json::to_string(&activity)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT body FROM api_session_activity WHERE session_id = ?1",
                [&session_id],
                |r| r.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(body.as_str()) {
            continue;
        }
        tx.execute("INSERT INTO api_session_activity(session_id, body) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET body = excluded.body", params![session_id, body])?;
    }
    tx.commit()?;
    Ok(())
}

/// Each batch bounds both the scan and the deletion, including stores with
/// millions of legacy activity rows. Never reset sqlite_sequence: old cursors
/// still name the same durable frontier after its last row has been removed.
pub(crate) fn prune_api_activity_events(after_seq: u64) -> Result<Option<(u64, usize)>> {
    submit_database_write("prune API activity history", move |connection| {
        prune_api_activity_events_with(connection, after_seq, 1_000)
    })
}

fn prune_api_activity_events_with(
    connection: &mut Connection,
    after_seq: u64,
    limit: usize,
) -> Result<Option<(u64, usize)>> {
    let tx = connection.transaction()?;
    let through: Option<u64> = tx.query_row(
        "SELECT max(seq) FROM (SELECT seq FROM api_events WHERE seq > ?1 ORDER BY seq LIMIT ?2)",
        params![after_seq, limit as u64],
        |row| row.get(0),
    )?;
    let Some(through) = through else {
        return Ok(None);
    };
    let removed = tx.execute(
        "DELETE FROM api_events WHERE seq > ?1 AND seq <= ?2
         AND json_extract(body, '$.type') = 'activity_changed'",
        params![after_seq, through],
    )?;
    tx.commit()?;
    Ok(Some((through, removed)))
}

pub fn record_startup_fault(session_id: String, message: String) -> Result<()> {
    submit_database_write("record_startup_fault", move |connection| {
        let tx = connection.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            [&session_id],
            |r| r.get(0),
        )?;
        if exists {
            insert_api_event(
                &tx,
                &session_id,
                chrono::Utc::now().timestamp_millis(),
                &ApiEventData::SessionFault {
                    reason: OutcomeReason::StartupFailed,
                    message,
                    command_id: None,
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// Upgrade bodies in place: sequence identity and the AUTOINCREMENT frontier stay unchanged.
pub(super) fn migrate_event_outcomes(tx: &Transaction<'_>) -> Result<()> {
    let mut query = tx.prepare("SELECT seq, body FROM api_events WHERE json_extract(body, '$.type') IN ('error', 'turn_ended')")?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, body) in rows {
        let mut value: serde_json::Value = serde_json::from_str(&body)?;
        if value["type"] == "error" {
            value["type"] = "legacy_notice".into();
            value["data"]["original_type"] = "error".into();
        } else {
            let turn: MaterializedTurnOutcome =
                serde_json::from_value(value["data"]["turn"].clone())?;
            value["data"]["turn"] =
                serde_json::to_value(mj_core::event_outcome::ApiTurnOutcome::from(&turn))?;
        }
        tx.execute(
            "UPDATE api_events SET body = ?2 WHERE seq = ?1",
            params![seq, serde_json::to_string(&value)?],
        )?;
    }
    Ok(())
}

pub(super) fn previous_session_error(
    tx: &Transaction<'_>,
    session_id: &str,
) -> Result<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT last_error FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Lifecycle writes own this comparison and event in the same database transaction.
pub(super) fn record_session_fault_transition(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    previous: Option<String>,
) -> Result<()> {
    if let Some(message) = &session.last_error
        && previous.as_ref() != Some(message)
    {
        insert_api_event(
            tx,
            &session.id,
            Utc::now().timestamp_millis(),
            &ApiEventData::SessionFault {
                reason: OutcomeReason::LifecycleFailed,
                message: message.clone(),
                command_id: None,
            },
        )?;
    }
    Ok(())
}

/// Reserve one requested checkpoint and save its lifecycle state in one writer decision.
pub fn begin_checkpoint_operation(session: &SessionRecord, command_id: &str) -> Result<()> {
    let session = session.clone();
    let command_id = command_id.to_owned();
    submit_database_write("begin_checkpoint_operation", move |connection| {
        let tx = connection.transaction()?;
        begin_checkpoint_operation_with(&tx, &session, &command_id)?;
        tx.commit()?;
        Ok(())
    })
}

pub(super) fn begin_checkpoint_operation_with(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    command_id: &str,
) -> Result<()> {
    super::sessions::validate_session_record(session)?;
    let state: String = tx.query_row(
        "SELECT state FROM sessions WHERE session_id = ?1",
        [&session.id],
        |row| row.get(0),
    )?;
    ensure!(
        !matches!(state.as_str(), "checkpointing" | "closing" | "destroying"),
        "session {} is already in a lifecycle operation",
        session.id
    );
    tx.execute(
        "INSERT INTO checkpoint_operations(session_id, command_id) VALUES (?1, ?2)",
        params![session.id, command_id],
    )?;
    super::state_io::update_lifecycle_fields(tx, session)?;
    Ok(())
}

pub fn save_requested_checkpoint(session: &SessionRecord, command_id: &str) -> Result<()> {
    let session = session.clone();
    let command_id = command_id.to_owned();
    submit_database_write("save_requested_checkpoint", move |connection| {
        let tx = connection.transaction()?;
        save_requested_checkpoint_with(&tx, &session, &command_id)?;
        tx.commit()?;
        Ok(())
    })
}

fn save_requested_checkpoint_with(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    command_id: &str,
) -> Result<()> {
    super::sessions::validate_session_record(session)?;
    let owns: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM checkpoint_operations WHERE session_id = ?1 AND command_id = ?2)", params![session.id, command_id], |row| row.get(0))?;
    ensure!(
        owns,
        "checkpoint operation no longer owns session {}",
        session.id
    );
    ensure!(
        session.checkpoint.is_some(),
        "successful checkpoint has no archive metadata"
    );
    super::state_io::update_lifecycle_fields(tx, session)?;
    tx.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session.id, session.native_session_id],
    )?;
    super::state_io::replace_checkpoint(tx, session)?;
    finish_checkpoint_operation(tx, &session.id, CommandResultKind::Succeeded, None, None)
}

/// Consume identity and publish the terminal fact atomically; repeated finishes do nothing.
pub(super) fn finish_checkpoint_operation(
    tx: &Transaction<'_>,
    session_id: &str,
    outcome: CommandResultKind,
    reason: Option<OutcomeReason>,
    message: Option<String>,
) -> Result<()> {
    let operation = tx.query_row("SELECT command_id, related_command_ids FROM checkpoint_operations WHERE session_id = ?1", [session_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).optional()?;
    if let Some((command_id, related)) = operation {
        insert_api_event(
            tx,
            session_id,
            Utc::now().timestamp_millis(),
            &ApiEventData::CommandEnded {
                result: CommandResult {
                    owner: CommandOwner::Daemon,
                    command_id,
                    command_kind: "checkpoint".into(),
                    outcome,
                    reason,
                    message,
                    related_command_ids: serde_json::from_str(&related)?,
                },
            },
        )?;
        tx.execute(
            "DELETE FROM checkpoint_operations WHERE session_id = ?1",
            [session_id],
        )?;
    }
    Ok(())
}

/// Called before submission, so even a lost acknowledgement retains barrier correlation.
pub fn correlate_checkpoint_barrier(
    session_id: &str,
    operation_id: &str,
    command_id: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let command_id = command_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("correlate_checkpoint_barrier", move |connection| {
        connection.execute("UPDATE checkpoint_operations SET related_command_ids = json_insert(related_command_ids, '$[#]', ?2) WHERE session_id = ?1 AND command_id = ?3", params![session_id, command_id, operation_id])?;
        Ok(())
    })
}

pub fn finish_failed_checkpoint(
    session: &SessionRecord,
    command_id: &str,
    deferred: bool,
    message: String,
) -> Result<()> {
    let session = session.clone();
    let command_id = command_id.to_owned();
    submit_database_write("finish_failed_checkpoint", move |connection| {
        let tx = connection.transaction()?;
        finish_failed_checkpoint_with(&tx, &session, &command_id, deferred, message)?;
        tx.commit()?;
        Ok(())
    })
}

fn finish_failed_checkpoint_with(
    tx: &Transaction<'_>,
    session: &SessionRecord,
    command_id: &str,
    deferred: bool,
    message: String,
) -> Result<()> {
    super::sessions::validate_session_record(session)?;
    let owns: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM checkpoint_operations WHERE session_id = ?1 AND command_id = ?2)", params![session.id, command_id], |row| row.get(0))?;
    if !owns {
        return Ok(());
    }
    super::state_io::update_lifecycle_fields(tx, session)?;
    finish_checkpoint_operation(
        tx,
        &session.id,
        if deferred {
            CommandResultKind::Rejected
        } else {
            CommandResultKind::Failed
        },
        Some(if deferred {
            OutcomeReason::CheckpointDeferred
        } else {
            OutcomeReason::CheckpointFailed
        }),
        Some(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::{
        RelayCommand, RelayCommandOutcome, RelayEvent, RelayObservation, relay_event_digest,
    };
    use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};

    fn page(path: &Path, observations: Vec<RelayObservation>, fail: bool) -> Result<()> {
        page_for(path, "session-1", observations, fail)
    }

    fn page_for(
        path: &Path,
        session: &str,
        observations: Vec<RelayObservation>,
        fail: bool,
    ) -> Result<()> {
        let mut current = load_materialized_session_from(path, session)?.unwrap();
        apply_projection_page_to(path, session, |page| {
            for observation in observations {
                let mut event = RelayEvent {
                    format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
                    ordinal: current.applied_event_ordinal + 1,
                    previous_digest: current.applied_event_digest.clone(),
                    digest: String::new(),
                    recorded_at_ms: 100,
                    command_id: None,
                    observation,
                };
                event.digest = relay_event_digest(&event)?;
                let mutation = project_relay_event(&current, &event)?.mutation;
                page.apply(
                    event.ordinal,
                    &event.previous_digest,
                    &event.digest,
                    &mutation,
                )?;
                apply_committed_projection_event(&mut current, &event, mutation)?;
            }
            if fail {
                bail!("injected rollback");
            }
            Ok(())
        })
    }

    fn turn() -> Vec<RelayObservation> {
        vec![
            RelayObservation::CommandQueued {
                command_id: "prompt-1".into(),
                command: RelayCommand::Prompt { prompt: vec![] },
                created_at_ms: 1,
            },
            RelayObservation::CommandStarted {
                command_id: "prompt-1".into(),
                started_at_ms: 2,
            },
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: None,
                command_id: "prompt-1".into(),
                outcome: RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            },
        ]
    }

    #[test]
    fn api_events_survive_coalescing_rollback_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        assert!(page(&path, turn(), true).is_err());
        assert!(
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events
                .is_empty()
        );
        page(&path, turn(), false).unwrap();
        let first = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 1).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].event.kind(), "turn_started");
        let rest = load_api_events_from(
            &path,
            &ApiEventFilter::default(),
            Some(first.next_after_seq),
            100,
        )
        .unwrap();
        assert_eq!(rest.events.len(), 1);
        assert_eq!(rest.events[0].event.kind(), "turn_ended");
        let current = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap();
        assert!(current.active_turn.is_none());
        // Re-delivery of the committed frontier must not insert a duplicate.
        apply_projection_event_to(
            &path,
            "session-1",
            current.applied_event_ordinal,
            "",
            &current.applied_event_digest,
            &MaterializedSessionMutation::default(),
        )
        .unwrap();
        assert_eq!(
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events
                .len(),
            2
        );
        assert!(
            load_api_events_from(&path, &ApiEventFilter::default(), None, 100)
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn api_events_filter_and_keep_cursors_after_forgetting_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        page(&path, turn(), false).unwrap();
        let filter = ApiEventFilter {
            session_id: Some("other".into()),
            workspace_id: None,
        };
        let empty = load_api_events_from(&path, &filter, Some(0), 100).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.next_after_seq, 2);
        let filter = ApiEventFilter {
            session_id: None,
            workspace_id: Some("default".into()),
        };
        assert_eq!(
            load_api_events_from(&path, &filter, Some(0), 100)
                .unwrap()
                .events
                .len(),
            2
        );
        delete_session_from(&path, "session-1").unwrap();
        let deleted =
            load_api_events_from(&path, &ApiEventFilter::default(), Some(2), 100).unwrap();
        assert!(deleted.events.is_empty());
        assert_eq!(deleted.latest_seq, 2);
    }

    #[test]
    fn operator_away_mail_is_atomic_and_return_notice_skips_later_blocking_requests() {
        use mj_core::elicitation::OPERATOR_AWAY_ACTION;
        use mj_core::mailbox::{MailboxEvent, MailboxEventBody};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("away.sqlite");
        for id in ["session-1", "session-2", "legacy", "late", "pre-away"] {
            let mut record = super::super::tests::session(id, "project");
            record.state = SessionState::Running;
            save_session_to(&path, &record).unwrap();
        }
        let question = ElicitationRequest::from_acp_params("reused-id", serde_json::json!({
            "mode":"form", "sessionId":"session-1", "message":"Which architecture?",
            "requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "enum":["thin","thick"]}}}
        })).unwrap();
        let requested = || RelayObservation::ElicitationRequested {
            request: question.clone(),
        };
        let resolved = || RelayObservation::ElicitationResolved {
            elicitation_id: question.id.clone(),
            action: OPERATOR_AWAY_ACTION.into(),
            // Even a historical worker carrying a reply must not manufacture a user answer.
            reply: Some("not a user answer".into()),
        };
        // A question already open when AFK starts must be captured into that AFK period
        // when the worker's away resolution arrives.
        page_for(&path, "pre-away", vec![requested()], false).unwrap();
        let mut connection = open(&path).unwrap();
        assert!(super::super::operator_away::set_with(&mut connection, true).unwrap());
        assert!(!super::super::operator_away::set_with(&mut connection, true).unwrap());
        assert_eq!(
            load_state_from(&path).unwrap().operator_presence,
            OperatorPresence {
                away: true,
                revision: 1
            }
        );
        connection.execute_batch("CREATE TRIGGER fail_away BEFORE INSERT ON mailbox_outbox BEGIN SELECT RAISE(ABORT,'away rollback'); END;").unwrap();
        assert!(page(&path, vec![requested(), resolved()], false).is_err());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM api_events", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1,
            "the pre-AFK request remains while the failed away resolution rolls back"
        );
        connection.execute_batch("DROP TRIGGER fail_away").unwrap();
        drop(connection);
        for id in ["session-1", "session-2", "legacy"] {
            // Capture and resolution cross a transaction/restart boundary.
            page_for(&path, id, vec![requested()], false).unwrap();
            page_for(&path, id, vec![resolved()], false).unwrap();
        }
        page_for(&path, "pre-away", vec![resolved()], false).unwrap();
        open(&path)
            .unwrap()
            .execute(
                "INSERT INTO operator_away_workers(session_id,protocol) VALUES('legacy',44)",
                [],
            )
            .unwrap();
        // A later request with the same runtime-local id waits for the operator.
        page(&path, vec![requested()], false).unwrap();
        // A later autoreply resets the blocking cutoff for the other session.
        page_for(&path, "session-2", vec![requested(), resolved()], false).unwrap();
        let current = load_materialized_session_from(&path, "session-2")
            .unwrap()
            .unwrap();
        assert!(current.pending_elicitations.is_empty());
        let transcript = current
            .transcript
            .iter()
            .map(|item| mj_transcript::transcript::transcript_item_text(item))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(transcript.contains("Not shown because you were away: Which architecture?"));
        assert!(!transcript.contains("not a user answer"));
        // This request was refused before AFK-off, but its resolution arrives later.
        page_for(&path, "late", vec![requested()], false).unwrap();
        let mut connection = open(&path).unwrap();
        connection
            .execute("DELETE FROM api_events WHERE session_id='late'", [])
            .unwrap();
        assert!(super::super::operator_away::set_with(&mut connection, false).unwrap());
        // Repeating the same write and rebuilding a projection emit nothing twice.
        assert!(!super::super::operator_away::set_with(&mut connection, false).unwrap());
        assert_eq!(
            load_state_from(&path).unwrap().operator_presence,
            OperatorPresence {
                away: false,
                revision: 2
            }
        );
        drop(connection);
        page_for(&path, "late", vec![resolved()], false).unwrap();
        save_materialized_session_to(&path, &MaterializedSession::empty("late")).unwrap();
        page_for(&path, "late", vec![requested(), resolved()], false).unwrap();
        save_materialized_session_to(&path, &MaterializedSession::empty("pre-away")).unwrap();
        page_for(&path, "pre-away", vec![requested(), resolved()], false).unwrap();
        save_materialized_session_to(&path, &MaterializedSession::empty("session-2")).unwrap();
        page_for(
            &path,
            "session-2",
            vec![requested(), resolved(), requested(), resolved()],
            false,
        )
        .unwrap();
        let connection = open(&path).unwrap();
        let messages = connection
            .prepare(
                "SELECT target_session_id,event_json,wake FROM mailbox_outbox ORDER BY event_key",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut away_count = 0;
        let mut back_count = 0;
        for (target, json, wake) in messages {
            assert!(wake);
            match serde_json::from_str::<MailboxEvent>(&json).unwrap().body {
                MailboxEventBody::OperatorAway { request } => {
                    away_count += 1;
                    assert!(request.contains("Which architecture?"));
                }
                MailboxEventBody::OperatorBack { requests } => {
                    back_count += 1;
                    assert!(target == "session-2" || target == "late" || target == "pre-away");
                    assert_eq!(requests.len(), if target == "session-2" { 2 } else { 1 });
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(away_count, 5);
        assert_eq!(back_count, 3);
    }

    #[test]
    fn child_question_outbox_survives_cancellation_replay_and_reused_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("questions.sqlite");
        for id in ["session-1", "parent"] {
            let mut record = super::super::tests::session(id, "project-1");
            record.state = SessionState::Running;
            save_session_to(&path, &record).unwrap();
        }
        let question = ElicitationRequest::from_acp_params("elicitation-1", serde_json::json!({
            "mode":"form", "sessionId":"session-1", "message":"Which architecture?",
            "requestedSchema":{"type":"object", "properties":{"choice":{"type":"string", "title":"Architecture", "enum":["Thin callers", "Thick callers"]}}}
        })).unwrap();
        let mut permission = question.clone();
        permission.message = "May I run this command?".into();
        permission.tool_permission = Some(mj_core::elicitation::ToolPermissionForm { skip: true });
        let observations = || {
            vec![
                RelayObservation::ElicitationRequested {
                    request: question.clone(),
                },
                RelayObservation::ElicitationResolved {
                    elicitation_id: question.id.clone(),
                    action: "cancel".into(),
                    reply: None,
                },
                // A restarted harness may reuse the same question id.
                RelayObservation::ElicitationRequested {
                    request: permission.clone(),
                },
                RelayObservation::ElicitationResolved {
                    elicitation_id: permission.id.clone(),
                    action: "cancel".into(),
                    reply: None,
                },
            ]
        };
        // A top-level form remains local.
        page(&path, observations(), false).unwrap();
        let connection = open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM mailbox_outbox", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        connection.execute("INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('session-1','parent','session-1','{}')", []).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_question_mail BEFORE INSERT ON mailbox_outbox BEGIN SELECT RAISE(ABORT, 'question mail failed'); END;").unwrap();
        assert!(
            page(&path, observations(), false)
                .unwrap_err()
                .to_string()
                .contains("question mail failed")
        );
        assert_eq!(
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events
                .len(),
            4,
            "outbox failure rolls back its API events too"
        );
        connection
            .execute_batch("DROP TRIGGER fail_question_mail")
            .unwrap();
        drop(connection);
        page(&path, observations(), false).unwrap();
        let current = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap();
        assert!(
            current.pending_elicitations.is_empty(),
            "both forms resolved in this same projection page"
        );
        apply_projection_event_to(
            &path,
            "session-1",
            current.applied_event_ordinal,
            "",
            &current.applied_event_digest,
            &MaterializedSessionMutation::default(),
        )
        .unwrap();
        let mut connection = open(&path).unwrap();
        let messages = connection
            .prepare(
                "SELECT target_session_id,event_json,wake FROM mailbox_outbox ORDER BY event_key",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(messages.len(), 2);
        let mut texts = Vec::new();
        for (target, body, wake) in messages {
            assert_eq!(
                target, "parent",
                "only the direct parent receives the question"
            );
            assert!(wake);
            let event: mj_core::mailbox::MailboxEvent = serde_json::from_str(&body).unwrap();
            let mj_core::mailbox::MailboxEventBody::SessionMessage {
                from: mj_core::mailbox::Sender::Session { id, .. },
                text,
            } = event.body
            else {
                panic!("session message expected")
            };
            assert_eq!(id, "session-1");
            assert!(text.contains("Architecture\n- Thin callers\n- Thick callers"));
            assert!(text.contains("send_message to session_id session-1"));
            texts.push(text);
        }
        assert!(
            texts
                .iter()
                .any(|text| text.contains("Which architecture?"))
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("May I run this command?"))
        );
        assert_eq!(
            super::super::subagent_questions::reconcile_pending_subagent_questions_with(
                &mut connection
            )
            .unwrap(),
            0
        );
        // Projection repair can reset to genesis/checkpoint and replay the old
        // relay observations with new API sequence numbers. Its durable input
        // frontier survives replacement and suppresses historical questions.
        drop(connection);
        save_materialized_session_to(&path, &MaterializedSession::empty("session-1")).unwrap();
        page(
            &path,
            observations().into_iter().chain(observations()).collect(),
            false,
        )
        .unwrap();
        let connection = open(&path).unwrap();
        assert_eq!(connection.query_row("SELECT count(*) FROM api_events WHERE json_extract(body,'$.type')='input_required'", [], |r| r.get::<_,u64>(0)).unwrap(), 4);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM mailbox_outbox", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            2
        );
        // A legacy worker leaves the next form pending. Simulate an older daemon
        // which stored its input event without creating its outbox message.
        page(
            &path,
            vec![RelayObservation::ElicitationRequested {
                request: question.clone(),
            }],
            false,
        )
        .unwrap();
        connection.execute("DELETE FROM mailbox_outbox WHERE event_key=(SELECT 'subagent-question-session-1-' || max(seq) FROM api_events)", []).unwrap();
        drop(connection);
        let mut reopened = open(&path).unwrap();
        assert_eq!(
            super::super::subagent_questions::reconcile_pending_subagent_questions_with(
                &mut reopened
            )
            .unwrap(),
            1
        );
        assert_eq!(
            super::super::subagent_questions::reconcile_pending_subagent_questions_with(
                &mut reopened
            )
            .unwrap(),
            0
        );
        assert_eq!(
            reopened
                .query_row("SELECT count(*) FROM mailbox_outbox", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn api_events_capture_free_text_questions_resolution_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let mut record = super::super::tests::session("session-1", "project-1");
        save_session_to(&path, &record).unwrap();
        let request = ElicitationRequest::from_acp_params("question-1", serde_json::json!({
            "mode": "form", "sessionId": "session-1", "message": "Which directory?",
            "requestedSchema": {"type": "object", "properties": {"directory": {"type": "string"}}, "required": ["directory"]}
        })).unwrap();
        let mut observations = turn();
        let RelayObservation::CommandQueued {
            command: RelayCommand::Prompt { prompt },
            ..
        } = &mut observations[0]
        else {
            unreachable!("turn fixture starts with a prompt")
        };
        prompt.push(agent_client_protocol::schema::v1::ContentBlock::from(
            "Inspect the chosen directory",
        ));
        observations.splice(
            2..2,
            [
                RelayObservation::ElicitationRequested {
                    request: request.clone(),
                },
                RelayObservation::ElicitationResolved {
                    elicitation_id: request.id.clone(),
                    action: "accept".into(),
                    reply: Some(request.reply_text(
                        &mj_core::elicitation::ElicitationResponse::Accept {
                            content: std::collections::BTreeMap::from([(
                                "directory".into(),
                                mj_core::elicitation::ElicitationValue::String("src".into()),
                            )]),
                        },
                    )),
                },
            ],
        );
        page(&path, observations, false).unwrap();
        record.last_error = Some("provisioning failed".into());
        save_session_to(&path, &record).unwrap();
        save_session_to(&path, &record).unwrap();
        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|e| e.event.kind())
                .collect::<Vec<_>>(),
            [
                "turn_started",
                "input_required",
                "input_resolved",
                "turn_ended",
                "session_fault"
            ]
        );
        let ApiEventData::InputRequired {
            request: actual,
            turn_id,
        } = &page.events[1].event
        else {
            panic!("question event")
        };
        assert_eq!(actual.as_ref(), Some(&request));
        assert_eq!(*turn_id, Some(1));
        assert!(
            matches!(&page.events[2].event, ApiEventData::InputResolved { turn_id: Some(1), action, .. } if action == "accept")
        );
        let current = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap();
        assert!(current.pending_elicitations.is_empty());
        assert!(current.active_turn.is_none());
        let reply = current
            .transcript
            .iter()
            .find(|item| {
                item.stable_id
                    .starts_with(mj_core::transcript::ELICITATION_REPLY_ITEM_PREFIX)
            })
            .unwrap();
        assert_eq!(
            mj_transcript::transcript::transcript_item_text(reply),
            "Which directory?\n\ndirectory: src"
        );
        assert!(!reply.is_turn_start());
        let turn_start = current
            .transcript
            .iter()
            .find(|item| item.is_turn_start())
            .unwrap()
            .position;
        assert_eq!(
            mj_core::state::latest_completed_turn_ordinal(&current),
            Some(turn_start)
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            super::super::materialized::last_materialized_turn_start(&connection, "session-1")
                .unwrap(),
            Some(turn_start)
        );
        assert_eq!(
            super::super::materialized::last_materialized_user_message(&connection, "session-1")
                .unwrap()
                .unwrap()
                .0,
            turn_start
        );
        assert_eq!(current.last_turn_outcome.unwrap().accepted_ordinal, Some(1));
    }

    // Hard-won: be5abcca: launch failure reasons were absent from the user-visible event stream.
    #[test]
    fn a_lifecycle_save_that_records_a_launch_failure_emits_one_error_event() {
        // The launch-failure path persists through `save_lifecycle_session`
        // (a plain UPDATE), so prove that path fires the fault transition once so
        // `mj events` shows the reason exactly once, not zero or twice.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let mut record = super::super::tests::session("session-1", "project-1");
        save_session_to(&path, &record).unwrap();

        record.state = mj_core::state::SessionState::Error;
        record.last_error = Some("worker bootstrap failed: Connection closed by host".into());
        save_lifecycle_session_to(&path, &record).unwrap();
        // An unchanged re-save must not add a second event.
        save_lifecycle_session_to(&path, &record).unwrap();

        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        let errors: Vec<_> = page
            .events
            .iter()
            .filter_map(|event| match &event.event {
                ApiEventData::SessionFault { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors,
            vec!["worker bootstrap failed: Connection closed by host".to_owned()],
            "a recorded launch failure surfaces as exactly one error event"
        );
    }

    #[test]
    fn api_events_preserve_command_identity_for_failed_completions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let mut observations = turn();
        let RelayObservation::CommandCompleted { outcome, .. } = observations.last_mut().unwrap()
        else {
            unreachable!()
        };
        *outcome = RelayCommandOutcome::Prompt {
            diagnostic: None,
            stop_reason: "provider_error".into(),
            usage: None,
        };
        page(&path, observations, false).unwrap();
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1].event, ApiEventData::TurnEnded { turn }
            if turn.command_id == "prompt-1" && turn.accepted_ordinal == Some(1)
                && turn.outcome.kind == mj_core::event_outcome::TurnResultKind::Failed
                && turn.outcome.stop_reason.as_deref() == Some("provider_error")));
    }

    #[test]
    fn api_activity_updates_only_current_state_without_reviving_forgotten_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let mut connection = open(&path).unwrap();
        let idle = ApiActivityState {
            state: "running".into(),
            details: Some(ApiActivityDetails {
                kind: ApiActivityKind::Idle,
                turn_started_at_ms: None,
                step_started_at_ms: None,
                background_started_at_ms: None,
                idle_since_ms: Some(100),
                last_activity_at_ms: None,
                label: None,
            }),
            is_idle: true,
            waiting_for_input: false,
            capacity_retry: false,
        };
        record_api_activities_with(
            &mut connection,
            vec![("session-1".into(), idle.clone())],
            100,
        )
        .unwrap();
        record_api_activities_with(
            &mut connection,
            vec![("session-1".into(), idle.clone())],
            200,
        )
        .unwrap();
        let mut background = idle.clone();
        background.is_idle = false;
        let details = background.details.as_mut().unwrap();
        details.kind = ApiActivityKind::Background;
        details.idle_since_ms = None;
        details.background_started_at_ms = Some(250);
        record_api_activities_with(
            &mut connection,
            vec![("session-1".into(), background.clone())],
            250,
        )
        .unwrap();
        background.waiting_for_input = true;
        record_api_activities_with(
            &mut connection,
            vec![("session-1".into(), background.clone())],
            300,
        )
        .unwrap();
        let unknown = ApiActivityState {
            state: "disconnected".into(),
            details: None,
            is_idle: false,
            waiting_for_input: false,
            capacity_retry: false,
        };
        record_api_activities_with(
            &mut connection,
            vec![("session-1".into(), unknown.clone())],
            400,
        )
        .unwrap();
        drop(connection);
        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        assert!(page.events.is_empty());
        let body: String = open(&path)
            .unwrap()
            .query_row(
                "SELECT body FROM api_session_activity WHERE session_id='session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ApiActivityState>(&body).unwrap(),
            unknown
        );
        delete_session_from(&path, "session-1").unwrap();
        record_api_activities_with(
            &mut open(&path).unwrap(),
            vec![("session-1".into(), idle)],
            500,
        )
        .unwrap();
        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.latest_seq, 0);
    }
    #[test]
    fn api_activity_cleanup_is_bounded_restartable_and_keeps_replay_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        let mut connection = open(&path).unwrap();
        let activity = ApiEventData::ActivityChanged {
            activity: ApiActivityState {
                state: "running".into(),
                details: None,
                is_idle: false,
                waiting_for_input: false,
                capacity_retry: false,
            },
        };
        let durable = ApiEventData::SessionFault {
            reason: OutcomeReason::StartupFailed,
            message: "activity_changed is only text here".into(),
            command_id: None,
        };
        let tx = connection.transaction().unwrap();
        // More than one batch and 64 KiB, with a durable event between two
        // stretches of disposable history and activity at the final frontier.
        for seq in 1..=2_003 {
            insert_api_event(
                &tx,
                "session-1",
                seq,
                if seq == 1_002 { &durable } else { &activity },
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let before =
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 1_000).unwrap();
        assert!(before.events.is_empty());
        assert_eq!(before.next_after_seq, 1_000);
        assert_eq!(
            prune_api_activity_events_with(&mut connection, 0, 1_000).unwrap(),
            Some((1_000, 1_000))
        );
        drop(connection);

        // Simulate restarting cleanup with only its committed deletes, then
        // append a durable event while later batches are still running.
        let mut connection = open(&path).unwrap();
        assert_eq!(
            prune_api_activity_events_with(&mut connection, 0, 1_000).unwrap(),
            Some((2_000, 999))
        );
        let tx = connection.transaction().unwrap();
        insert_api_event(&tx, "session-1", 2_004, &durable).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            prune_api_activity_events_with(&mut connection, 2_000, 1_000).unwrap(),
            Some((2_004, 3))
        );
        assert_eq!(
            prune_api_activity_events_with(&mut connection, 2_004, 1_000).unwrap(),
            None
        );
        let history =
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        assert_eq!(
            history
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [1_002, 2_004]
        );
        assert!(history.events.iter().all(|event| event.event == durable));

        // Removing the newest activity must not make an old cursor invalid or
        // let SQLite reuse its ID on the next durable insertion.
        let tx = connection.transaction().unwrap();
        insert_api_event(&tx, "session-1", 2_005, &activity).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            prune_api_activity_events_with(&mut connection, 2_004, 1_000).unwrap(),
            Some((2_005, 1))
        );
        let tail =
            load_api_events_from(&path, &ApiEventFilter::default(), Some(2_004), 100).unwrap();
        assert!(tail.events.is_empty());
        assert_eq!(tail.latest_seq, 2_005);
        assert_eq!(tail.next_after_seq, 2_005);
        let tx = connection.transaction().unwrap();
        insert_api_event(&tx, "session-1", 2_006, &durable).unwrap();
        tx.commit().unwrap();
        let tail =
            load_api_events_from(&path, &ApiEventFilter::default(), Some(2_005), 100).unwrap();
        assert_eq!(tail.events[0].seq, 2_006);
    }

    #[test]
    fn checkpoint_cleanup_does_not_end_a_turn_or_hide_control_failure() {
        use mj_core::event_outcome::TurnResultKind;
        use mj_core::relay::RelayCommandKind;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        page(&path, turn().into_iter().take(2).collect(), false).unwrap();
        for (id, reason) in [
            ("arbitrary-one", OutcomeReason::ControllerDisconnected),
            ("arbitrary-two", OutcomeReason::OwnerLostOnRestart),
        ] {
            page(
                &path,
                vec![RelayObservation::CommandInterrupted {
                    command_id: id.into(),
                    command: RelayCommandKind::BeginCheckpoint,
                    reason: Some(reason),
                    message: "diagnostic wording is not a contract".into(),
                }],
                false,
            )
            .unwrap();
        }
        page(
            &path,
            vec![RelayObservation::CommandRejected {
                command_id: "arbitrary-three".into(),
                command: RelayCommandKind::CompleteCheckpoint,
                reason: Some(OutcomeReason::CommandFailed),
                message: "Cannot save recovery floor".into(),
            }],
            false,
        )
        .unwrap();
        let current = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap();
        assert!(current.active_turn.is_some());
        assert!(current.last_turn_outcome.is_none());
        assert!(current.transcript.iter().all(|item| !matches!(&item.body, TranscriptBody::System { text } if text.contains("diagnostic wording"))));
        assert!(current.transcript.iter().any(|item| matches!(&item.body, TranscriptBody::System { text } if text.contains("Cannot save recovery floor"))));
        page(&path, vec![turn().pop().unwrap()], false).unwrap();
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 5);
        for event in &events[1..3] {
            assert!(
                matches!(&event.event, ApiEventData::CommandEnded { result } if result.outcome == CommandResultKind::Cancelled && result.command_kind == "begin_checkpoint")
            );
        }
        assert!(
            matches!(&events[3].event, ApiEventData::CommandEnded { result } if result.outcome == CommandResultKind::Failed)
        );
        assert!(
            matches!(&events[4].event, ApiEventData::TurnEnded { turn } if turn.outcome.kind == TurnResultKind::Completed)
        );
    }

    #[test]
    fn terminal_prompt_results_are_semantic_and_not_duplicate_error_events() {
        use mj_core::event_outcome::TurnResultKind;
        use mj_core::relay::RelayCommandKind;
        for (terminal, expected) in [
            (
                RelayObservation::CommandCompleted {
                    barrier_command_id: None,
                    command_id: "prompt-1".into(),
                    command: Some(RelayCommandKind::Prompt),
                    outcome: RelayCommandOutcome::Prompt {
                        stop_reason: "Cancelled".into(),
                        usage: None,
                        diagnostic: None,
                    },
                },
                TurnResultKind::Cancelled,
            ),
            (
                RelayObservation::CommandCompleted {
                    barrier_command_id: None,
                    command_id: "prompt-1".into(),
                    command: Some(RelayCommandKind::Prompt),
                    outcome: RelayCommandOutcome::Prompt {
                        stop_reason: "QuotaLimit".into(),
                        usage: None,
                        diagnostic: None,
                    },
                },
                TurnResultKind::Failed,
            ),
            (
                RelayObservation::CommandRejected {
                    command_id: "prompt-1".into(),
                    command: RelayCommandKind::Prompt,
                    reason: Some(OutcomeReason::AdmissionRejected),
                    message: "unavailable".into(),
                },
                TurnResultKind::Rejected,
            ),
            (
                RelayObservation::CommandInterrupted {
                    command_id: "prompt-1".into(),
                    command: RelayCommandKind::Prompt,
                    reason: Some(OutcomeReason::RuntimeStopped),
                    message: "runtime stopped".into(),
                },
                TurnResultKind::Interrupted,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("events.sqlite");
            save_session_to(
                &path,
                &super::super::tests::session("session-1", "project-1"),
            )
            .unwrap();
            let mut observations = turn();
            *observations.last_mut().unwrap() = terminal;
            page(&path, observations, false).unwrap();
            let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events;
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[1].event, ApiEventData::TurnEnded { turn } if turn.outcome.kind == expected && turn.command_id == "prompt-1")
            );
        }
    }

    #[test]
    fn typed_runtime_fault_does_not_rewrite_a_completed_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        page(&path, turn(), false).unwrap();
        page(
            &path,
            vec![RelayObservation::SessionFault {
                reason: OutcomeReason::RuntimeUnavailable,
                message: "runtime exited".into(),
            }],
            false,
        )
        .unwrap();
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 3);
        assert!(
            matches!(&events[1].event, ApiEventData::TurnEnded { turn } if turn.outcome.kind == mj_core::event_outcome::TurnResultKind::Completed)
        );
        assert!(matches!(
            &events[2].event,
            ApiEventData::SessionFault {
                reason: OutcomeReason::RuntimeUnavailable,
                ..
            }
        ));
    }

    #[test]
    fn checkpoint_recovery_consumes_attempt_identity_once_and_preserves_correlation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let mut record = super::super::tests::session("session-1", "project-1");
        record.state = SessionState::Running;
        save_session_to(&path, &record).unwrap();
        record.state = SessionState::Checkpointing;
        let mut connection = open(&path).unwrap();
        {
            let tx = connection.transaction().unwrap();
            begin_checkpoint_operation_with(&tx, &record, "attempt-1").unwrap();
            assert!(begin_checkpoint_operation_with(&tx, &record, "attempt-2").is_err());
            tx.execute(
                "UPDATE checkpoint_operations SET related_command_ids = '[\"barrier-1\"]'",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        drop(connection);
        assert_eq!(
            recover_interrupted_checkpointing_sessions_to(&path, "2026-09-27T12:00:00Z").unwrap(),
            1
        );
        assert_eq!(
            recover_interrupted_checkpointing_sessions_to(&path, "2026-09-27T12:00:01Z").unwrap(),
            0
        );
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].event, ApiEventData::CommandEnded { result }
            if result.command_id == "attempt-1" && result.related_command_ids == ["barrier-1"]
            && result.owner == CommandOwner::Daemon && result.reason == Some(OutcomeReason::ControllerRestarted))
        );
    }

    #[test]
    fn event_migration_preserves_cursors_and_keeps_unknown_history_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        save_session_to(
            &path,
            &super::super::tests::session("session-1", "project-1"),
        )
        .unwrap();
        page(&path, turn(), false).unwrap();
        let turn = load_materialized_session_from(&path, "session-1")
            .unwrap()
            .unwrap()
            .last_turn_outcome
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE api_events SET body = ?1 WHERE seq = 2",
                [serde_json::json!({"type":"turn_ended", "data":{"turn":turn}}).to_string()],
            )
            .unwrap();
        connection.execute("INSERT INTO api_events(seq, session_id, recorded_at_ms, body) VALUES (8, 'session-1', 1234, ?1)", [r#"{"type":"error","data":{"command_id":"arbitrary","message":"old diagnostic"}}"#]).unwrap();
        // This fixture predates semantic persisted turns as well as typed events.
        let legacy_turn = serde_json::to_string(&turn).unwrap();
        connection
            .execute("UPDATE session_turn_usage SET body=?1", [&legacy_turn])
            .unwrap();
        connection.execute("UPDATE materialized_sessions SET last_turn_outcome_json=?1 WHERE session_id='session-1'", [&legacy_turn]).unwrap();
        super::super::schema::restore_github_v84_schema(&connection);
        super::super::schema::restore_pre101_subagent_preference(&connection);
        connection.execute_batch("DROP TABLE checkpoint_operations; DELETE FROM schema_migrations WHERE version >= 57; UPDATE schema_compatibility SET minimum_compatible_version = 56; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 56;").unwrap();
        drop(connection);
        forget_verified_schema(&path);
        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100).unwrap();
        assert_eq!(page.latest_seq, 8);
        assert_eq!(page.next_after_seq, 8);
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [1, 2, 8]
        );
        assert_eq!(page.events[2].recorded_at_ms, 1234);
        assert!(
            matches!(&page.events[2].event, ApiEventData::LegacyNotice { original_type, command_id: Some(id), message } if original_type == "error" && id == "arbitrary" && message == "old diagnostic")
        );
        assert!(
            matches!(&page.events[1].event, ApiEventData::TurnEnded { turn } if turn.outcome.kind == mj_core::event_outcome::TurnResultKind::Completed)
        );
    }
    #[test]
    fn requested_checkpoint_result_commits_with_metadata_and_only_for_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let mut record = super::super::tests::session("session-1", "project-1");
        record.state = SessionState::Running;
        let previous_checkpoint = record.checkpoint.clone();
        save_session_to(&path, &record).unwrap();
        let mut connection = open(&path).unwrap();
        record.state = SessionState::Checkpointing;
        {
            let tx = connection.transaction().unwrap();
            begin_checkpoint_operation_with(&tx, &record, "owner").unwrap();
            tx.commit().unwrap();
        }
        record.state = SessionState::Running;
        record.checkpoint = Some(CheckpointMetadata {
            archive_path: dir.path().join("verified.tar"),
            sha256: "a".repeat(64),
            created_at: "2026-09-27T12:00:00Z".into(),
            event_frontier: 0,
        });
        {
            let tx = connection.transaction().unwrap();
            assert!(save_requested_checkpoint_with(&tx, &record, "stale-owner").is_err());
            finish_failed_checkpoint_with(
                &tx,
                &record,
                "stale-owner",
                false,
                "late failure".into(),
            )
            .unwrap();
            assert_eq!(
                tx.query_row("SELECT count(*) FROM api_events", [], |r| r
                    .get::<_, usize>(0))
                    .unwrap(),
                0
            );
            save_requested_checkpoint_with(&tx, &record, "owner").unwrap();
            // Drop rolls back both metadata and the terminal event.
        }
        assert_eq!(
            load_state_from(&path).unwrap().sessions["session-1"].checkpoint,
            previous_checkpoint
        );
        assert!(
            load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events
                .is_empty()
        );
        {
            let tx = connection.transaction().unwrap();
            save_requested_checkpoint_with(&tx, &record, "owner").unwrap();
            finish_failed_checkpoint_with(
                &tx,
                &record,
                "owner",
                false,
                "late cleanup failure".into(),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].event, ApiEventData::CommandEnded { result } if result.outcome == CommandResultKind::Succeeded && result.command_id == "owner")
        );
        assert_eq!(
            load_state_from(&path).unwrap().sessions["session-1"].checkpoint,
            record.checkpoint
        );
    }

    #[test]
    fn checkpoint_failure_and_deferral_are_distinct_and_preserve_previous_archive() {
        for deferred in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("events.sqlite");
            let mut record = super::super::tests::session("session-1", "project-1");
            record.state = SessionState::Running;
            record.checkpoint = Some(CheckpointMetadata {
                archive_path: dir.path().join("previous.tar"),
                sha256: "a".repeat(64),
                created_at: "2026-09-27T12:00:00Z".into(),
                event_frontier: 0,
            });
            save_session_to(&path, &record).unwrap();
            let previous = record.checkpoint.clone();
            let previous_warning = record.last_checkpoint_error.clone();
            let mut connection = open(&path).unwrap();
            let tx = connection.transaction().unwrap();
            record.state = SessionState::Checkpointing;
            begin_checkpoint_operation_with(&tx, &record, "owner").unwrap();
            record.state = SessionState::Running;
            if !deferred {
                record.last_checkpoint_error = Some("archive verification failed".into());
            }
            finish_failed_checkpoint_with(&tx, &record, "owner", deferred, "diagnostic".into())
                .unwrap();
            finish_failed_checkpoint_with(&tx, &record, "owner", deferred, "diagnostic".into())
                .unwrap();
            tx.commit().unwrap();
            let loaded = load_state_from(&path).unwrap();
            assert_eq!(loaded.sessions["session-1"].checkpoint, previous);
            assert_eq!(
                loaded.sessions["session-1"].last_checkpoint_error,
                if deferred {
                    previous_warning
                } else {
                    Some("archive verification failed".into())
                }
            );
            let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
                .unwrap()
                .events;
            assert_eq!(events.len(), 1);
            assert!(
                matches!(&events[0].event, ApiEventData::CommandEnded { result } if result.outcome == if deferred { CommandResultKind::Rejected } else { CommandResultKind::Failed })
            );
        }
    }
}
