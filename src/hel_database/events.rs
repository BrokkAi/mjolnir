//! Durable, ordered events for the native subagent API.
use super::*;
use crate::hel_elicitation::ElicitationRequest;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiEvent {
    pub seq: u64,
    pub session_id: String,
    pub recorded_at_ms: i64,
    #[serde(flatten)]
    pub event: ApiEventData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApiEventData {
    TurnStarted {
        turn: MaterializedTurn,
    },
    TurnEnded {
        turn: MaterializedTurnOutcome,
    },
    Error {
        message: String,
        command_id: Option<String>,
    },
    InputRequired {
        request: ElicitationRequest,
        turn_id: Option<u64>,
    },
    InputResolved {
        elicitation_id: String,
        turn_id: Option<u64>,
        action: String,
    },
    ActivityChanged {
        activity: ApiActivityState,
    },
}

impl ApiEventData {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnEnded { .. } => "turn_ended",
            Self::Error { .. } => "error",
            Self::InputRequired { .. } => "input_required",
            Self::InputResolved { .. } => "input_resolved",
            Self::ActivityChanged { .. } => "activity_changed",
        }
    }
}

/// These are the same structured facts rendered by the web and terminal UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiActivityState {
    pub state: String,
    pub details: Option<ApiActivityDetails>,
    pub is_idle: bool,
    pub waiting_for_input: bool,
    pub capacity_retry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiActivityDetails {
    pub kind: ApiActivityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiActivityKind {
    Turn,
    Step,
    Background,
    Idle,
    Lifecycle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEventFilter {
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiEventPage {
    pub events: Vec<ApiEvent>,
    pub next_after_seq: u64,
    pub latest_seq: u64,
}

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

fn record_api_activities_with(
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

pub fn record_api_error(session_id: String, message: String) -> Result<()> {
    submit_database_write("record_api_error", move |connection| {
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
                &ApiEventData::Error {
                    message,
                    command_id: None,
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_projection::{apply_committed_projection_event, project_relay_event};
    use crate::hel_worker::{
        RelayCommand, RelayCommandOutcome, RelayEvent, RelayObservation, relay_event_digest,
    };

    fn page(path: &Path, observations: Vec<RelayObservation>, fail: bool) -> Result<()> {
        let mut current = load_materialized_session_from(path, "session-1")?.unwrap();
        apply_projection_page_to(path, "session-1", |page| {
            for observation in observations {
                let mut event = RelayEvent {
                    format: crate::hel_worker::RELAY_EVENT_FORMAT_V1,
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
                command_id: "prompt-1".into(),
                outcome: RelayCommandOutcome::Prompt {
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
                "error"
            ]
        );
        let ApiEventData::InputRequired {
            request: actual,
            turn_id,
        } = &page.events[1].event
        else {
            panic!("question event")
        };
        assert_eq!(actual, &request);
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
            stop_reason: "provider_error".into(),
            usage: None,
        };
        page(&path, observations, false).unwrap();
        let events = load_api_events_from(&path, &ApiEventFilter::default(), Some(0), 100)
            .unwrap()
            .events;
        assert!(
            matches!(&events[1].event, ApiEventData::Error { command_id: Some(id), message } if id == "prompt-1" && message == "provider_error")
        );
        assert!(
            matches!(&events[2].event, ApiEventData::TurnEnded { turn } if turn.accepted_ordinal == Some(1))
        );
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
}
