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
    recorded_at_ms: i64,
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
        insert_api_event(
            &tx,
            &session_id,
            recorded_at_ms,
            &ApiEventData::ActivityChanged { activity },
        )?;
        tx.execute("INSERT INTO api_session_activity(session_id, body) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET body = excluded.body", params![session_id, body])?;
    }
    tx.commit()?;
    Ok(())
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
        let mut current = load_materialized_session_from(path, "session-1")?.unwrap();
        apply_projection_page_to(path, "session-1", |page| {
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
        observations.splice(
            2..2,
            [
                RelayObservation::ElicitationRequested {
                    request: request.clone(),
                },
                RelayObservation::ElicitationResolved {
                    elicitation_id: request.id.clone(),
                    action: "accept".into(),
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
        assert_eq!(current.last_turn_outcome.unwrap().accepted_ordinal, Some(1));
    }

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
    fn api_activity_events_track_changes_without_repeating_or_reviving_forgotten_sessions() {
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
        assert_eq!(page.events.len(), 4);
        assert_eq!(
            page.events.last().unwrap().event,
            ApiEventData::ActivityChanged { activity: unknown }
        );
        assert_eq!(
            page.events[2].event,
            ApiEventData::ActivityChanged {
                activity: background
            }
        );
        delete_session_from(&path, "session-1").unwrap();
        record_api_activities_with(
            &mut open(&path).unwrap(),
            vec![("session-1".into(), idle)],
            500,
        )
        .unwrap();
        let page = load_api_events_from(&path, &ApiEventFilter::default(), Some(4), 100).unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.latest_seq, 4);
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
        connection.execute_batch("DROP TABLE checkpoint_operations; DELETE FROM schema_migrations WHERE version >= 57; UPDATE schema_compatibility SET minimum_compatible_version = 56; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 56;").unwrap();
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
