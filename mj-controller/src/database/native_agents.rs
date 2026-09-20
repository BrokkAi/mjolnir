//! Native children are projections of their owner's relay, not worker sessions.
use super::*;
use mj_core::native_agent::{NativeAgent, NativeAgentEvent, NativeAgentState, NativeAgentView};
use mj_core::relay::{RelayEvent, RelayObservation};

/// Only metadata crosses the shared runtime snapshot transport.
pub fn load_native_agent_summaries(
    owner: &str,
) -> Result<Vec<mj_core::native_agent::NativeAgentSummary>> {
    let connection = open_reader(&database_path())?;
    load_native_agent_summaries_from(&connection, owner)
}

fn load_native_agent_summaries_from(
    connection: &Connection,
    owner: &str,
) -> Result<Vec<mj_core::native_agent::NativeAgentSummary>> {
    let mut statement = connection
        .prepare("SELECT body FROM native_agents WHERE owner=?1 AND staging=0 ORDER BY child")?;
    let bodies = statement.query_map([owner], |row| row.get::<_, String>(0))?;
    bodies
        .map(|body| {
            let view: NativeAgentView = serde_json::from_str(&body?)?;
            Ok(mj_core::native_agent::NativeAgentSummary::of(&view))
        })
        .collect()
}

/// Read identity and tail from one snapshot so replay cannot mix generations.
pub fn load_native_agent_view(
    owner: &str,
    child: &str,
    limit: usize,
) -> Result<Option<NativeAgentView>> {
    let mut connection = open_reader(&database_path())?;
    let transaction = connection.transaction()?;
    load_native_agent_view_from(&transaction, owner, child, limit)
}

fn load_native_agent_view_from(
    connection: &Connection,
    owner: &str,
    child: &str,
    limit: usize,
) -> Result<Option<NativeAgentView>> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=0",
            params![owner, child],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| {
        let mut view: NativeAgentView = serde_json::from_str(&body)?;
        view.projection.transcript = transcript(connection, owner, child, 0, limit)?;
        Ok(view)
    })
    .transpose()
}

pub fn load_native_agents(owner: &str, limit: usize) -> Result<Vec<NativeAgentView>> {
    let mut connection = open_reader(&database_path())?;
    let transaction = connection.transaction()?;
    load_native_agents_from(&transaction, owner, limit)
}

pub(super) fn load_native_agents_from(
    connection: &Connection,
    owner: &str,
    limit: usize,
) -> Result<Vec<NativeAgentView>> {
    load_native_agents_at(connection, owner, limit, 0)
}

fn load_native_agents_at(
    connection: &Connection,
    owner: &str,
    limit: usize,
    staging: i64,
) -> Result<Vec<NativeAgentView>> {
    let mut statement = connection
        .prepare("SELECT body FROM native_agents WHERE owner=?1 AND staging=?2 ORDER BY child")?;
    let bodies = statement
        .query_map(params![owner, staging], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    bodies
        .into_iter()
        .map(|body| {
            let mut view: NativeAgentView = serde_json::from_str(&body)?;
            view.projection.transcript =
                transcript(connection, owner, &view.agent.session_id, staging, limit)?;
            Ok(view)
        })
        .collect()
}

fn transcript(
    connection: &Connection,
    owner: &str,
    child: &str,
    staging: i64,
    limit: usize,
) -> Result<Vec<Arc<TranscriptItem>>> {
    let mut statement = connection.prepare("SELECT body FROM (SELECT position, stable_id, body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 ORDER BY position DESC, stable_id DESC LIMIT ?4) ORDER BY position, stable_id")?;
    let bodies = statement
        .query_map(params![owner, child, staging, limit as i64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    bodies
        .into_iter()
        .map(|body| Ok(Arc::new(serde_json::from_str(&body)?)))
        .collect()
}

pub(super) fn apply_native_agent_event(
    connection: &Connection,
    owner: &str,
    relay: &RelayEvent,
) -> Result<()> {
    let RelayObservation::NativeAgent { event } = &relay.observation else {
        bail!("expected native agent observation")
    };
    match event {
        NativeAgentEvent::Availability { reports, complete } => {
            for mut view in load_native_agents_from(connection, owner, 0)? {
                view.agent.apply_availability(reports, *complete);
                save(connection, &view, 0)?;
            }
            return Ok(());
        }
        NativeAgentEvent::ReplayBegin => {
            for mut view in load_native_agents_from(connection, owner, 0)? {
                view.agent.invalidate_availability();
                if view.agent.state == NativeAgentState::Running {
                    view.agent.state = NativeAgentState::Disconnected;
                }
                save(connection, &view, 0)?;
            }
            connection.execute(
                "DELETE FROM native_agents WHERE owner=?1 AND staging=1",
                [owner],
            )?;
            connection.execute(
                "INSERT OR IGNORE INTO native_agent_replay(owner) VALUES (?1)",
                [owner],
            )?;
            return Ok(());
        }
        NativeAgentEvent::ReplayCommit => {
            let staging: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_agent_replay WHERE owner=?1)",
                [owner],
                |row| row.get(0),
            )?;
            if staging {
                for mut view in load_native_agents_at(connection, owner, 0, 1)? {
                    view.agent.finish_replay();
                    view.projection.execution = MaterializedExecutionState::Idle;
                    finish_streaming(connection, owner, &view.agent.session_id, 1)?;
                    save(connection, &view, 1)?;
                }
                connection.execute(
                    "DELETE FROM native_agents WHERE owner=?1 AND staging=0 AND child IN (SELECT child FROM native_agents WHERE owner=?1 AND staging=1)",
                    [owner],
                )?;
                connection.execute(
                    "UPDATE native_agents SET staging=0 WHERE owner=?1 AND staging=1",
                    [owner],
                )?;
                connection.execute("DELETE FROM native_agent_replay WHERE owner=?1", [owner])?;
            }
            return Ok(());
        }
        NativeAgentEvent::Disconnected => {
            connection.execute(
                "DELETE FROM native_agents WHERE owner=?1 AND staging=1",
                [owner],
            )?;
            connection.execute("DELETE FROM native_agent_replay WHERE owner=?1", [owner])?;
            for mut view in load_native_agents_from(connection, owner, 0)? {
                view.agent.invalidate_availability();
                if view.agent.state == NativeAgentState::Running {
                    view.agent.state = NativeAgentState::Disconnected;
                }
                view.projection.execution = MaterializedExecutionState::Idle;
                finish_streaming(connection, owner, &view.agent.session_id, 0)?;
                view.projection.applied_event_ordinal = relay.ordinal;
                save(connection, &view, 0)?;
            }
            return Ok(());
        }
        _ => {}
    }
    let staging: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_agent_replay WHERE owner=?1)",
        [owner],
        |row| row.get(0),
    )?;
    let child = match event {
        NativeAgentEvent::Spawned { session_id, .. }
        | NativeAgentEvent::State { session_id, .. }
        | NativeAgentEvent::Update { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    let stored: Option<String> = connection
        .query_row(
            "SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=?3",
            params![owner, child, staging],
            |row| row.get(0),
        )
        .optional()?;
    let mut view: NativeAgentView = match stored {
        Some(body) => serde_json::from_str(&body)?,
        None => {
            let NativeAgentEvent::Spawned {
                session_id,
                parent_session_id,
                name,
                task,
                capabilities,
            } = event
            else {
                bail!("native child {child} update precedes spawn for owner {owner}");
            };
            let agent = NativeAgent {
                availability: Default::default(),
                availability_reason: None,
                stable_id: None,
                owner_session_id: owner.to_owned(),
                session_id: session_id.clone(),
                parent_session_id: parent_session_id.clone(),
                name: name.clone(),
                task: task.clone(),
                capabilities: capabilities.clone(),
                state: NativeAgentState::Running,
            };
            let mut projection = MaterializedSession::empty(agent.view_id());
            projection.session_title = Some(name.clone());
            projection.execution = MaterializedExecutionState::Running {
                started_at_ms: relay.recorded_at_ms,
            };
            NativeAgentView {
                generation_ordinal: relay.ordinal,
                agent,
                projection,
            }
        }
    };
    match event {
        NativeAgentEvent::State { state, .. } => {
            view.agent.state = *state;
            if *state != NativeAgentState::Running {
                finish_streaming(connection, owner, child, staging)?;
            }
            view.projection.execution = if *state == NativeAgentState::Running {
                MaterializedExecutionState::Running {
                    started_at_ms: relay.recorded_at_ms,
                }
            } else {
                MaterializedExecutionState::Idle
            };
        }
        NativeAgentEvent::Update { update, .. } => {
            // Streaming text needs the tail; an old tool update additionally needs
            // its named row, even when many newer messages have followed it.
            view.projection.transcript = transcript(connection, owner, child, staging, 64)?;
            let tool_id = match update.as_ref() {
                agent_client_protocol::schema::v1::SessionUpdate::ToolCall(call) => {
                    Some(call.tool_call_id.to_string())
                }
                agent_client_protocol::schema::v1::SessionUpdate::ToolCallUpdate(call) => {
                    Some(call.tool_call_id.to_string())
                }
                _ => None,
            };
            if let Some(id) = tool_id {
                let stable_id = format!("tool:{id}");
                if !view
                    .projection
                    .transcript
                    .iter()
                    .any(|item| item.stable_id == stable_id)
                {
                    let body: Option<String> = connection.query_row("SELECT body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 AND stable_id=?4", params![owner, child, staging, stable_id], |row| row.get(0)).optional()?;
                    if let Some(body) = body {
                        view.projection
                            .transcript
                            .push(Arc::new(serde_json::from_str(&body)?));
                    }
                }
            }
            let mutation =
                mj_transcript::projection::project_native_update(&view.projection, relay, update)?;
            for change in mutation.transcript {
                match change {
                    TranscriptMutation::Upsert(item) => {
                        connection.execute("INSERT INTO native_agent_transcript(owner,child,staging,stable_id,position,body) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(owner,child,staging,stable_id) DO UPDATE SET body=excluded.body", params![owner, child, staging, item.stable_id, item.position, serde_json::to_string(&item)?])?;
                    }
                    TranscriptMutation::Remove { stable_id } => {
                        connection.execute("DELETE FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 AND stable_id=?4", params![owner, child, staging, stable_id])?;
                    }
                }
            }
            view.projection.transcript.clear();
        }
        _ => {}
    }
    view.projection.applied_event_ordinal = relay.ordinal;
    view.projection
        .applied_event_digest
        .clone_from(&relay.digest);
    view.projection.last_activity_at_ms = Some(relay.recorded_at_ms);
    save(connection, &view, staging)
}

fn finish_streaming(connection: &Connection, owner: &str, child: &str, staging: i64) -> Result<()> {
    connection.execute("UPDATE native_agent_transcript SET body=json_set(body,'$.body.streaming',json('false')) WHERE owner=?1 AND child=?2 AND staging=?3 AND json_extract(body,'$.body.kind') IN ('agent','thought')", params![owner, child, staging])?;
    Ok(())
}

fn save(connection: &Connection, view: &NativeAgentView, staging: i64) -> Result<()> {
    connection.execute("INSERT INTO native_agents(owner,child,staging,body) VALUES (?1,?2,?3,?4) ON CONFLICT(owner,child,staging) DO UPDATE SET body=excluded.body", params![view.agent.owner_session_id, view.agent.session_id, staging, serde_json::to_string(view)?])?;
    Ok(())
}

pub fn native_agent_history(
    owner: &str,
    child: &str,
    before: Option<(u64, String)>,
) -> Result<mj_core::native_agent::NativeAgentHistoryPage> {
    let mut reader = open_reader(&database_path())?;
    let connection = reader.transaction()?;
    native_agent_history_from(&connection, owner, child, before)
}

fn native_agent_history_from(
    connection: &Connection,
    owner: &str,
    child: &str,
    before: Option<(u64, String)>,
) -> Result<mj_core::native_agent::NativeAgentHistoryPage> {
    let body: String = connection.query_row(
        "SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=0",
        params![owner, child],
        |row| row.get(0),
    )?;
    let view: NativeAgentView = serde_json::from_str(&body)?;
    let (position, stable_id) = before.unwrap_or((i64::MAX as u64, String::new()));
    let mut statement = connection.prepare("SELECT body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=0 AND (position, stable_id) < (?3, ?4) ORDER BY position DESC, stable_id DESC LIMIT 201")?;
    let bodies = statement
        .query_map(params![owner, child, position, stable_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = bodies.len() > 200;
    let mut items = bodies
        .into_iter()
        .take(200)
        .map(|body| Ok(Arc::new(serde_json::from_str(&body)?)))
        .collect::<Result<Vec<_>>>()?;
    items.reverse();
    Ok(mj_core::native_agent::NativeAgentHistoryPage {
        generation_ordinal: view.generation_ordinal,
        items,
        has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::native_agent::NativeAgentCapabilities;
    use mj_core::relay::{RELAY_EVENT_FORMAT_V1, RELAY_EVENT_GENESIS_DIGEST, relay_event_digest};
    use serde_json::json;

    fn event(ordinal: u64, native: NativeAgentEvent) -> RelayEvent {
        let mut event = RelayEvent {
            format: RELAY_EVENT_FORMAT_V1,
            ordinal,
            previous_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            digest: String::new(),
            recorded_at_ms: ordinal as i64 * 100,
            command_id: None,
            observation: RelayObservation::NativeAgent { event: native },
        };
        event.digest = relay_event_digest(&event).unwrap();
        event
    }

    fn spawn(child: &str, parent: Option<&str>) -> NativeAgentEvent {
        NativeAgentEvent::Spawned {
            session_id: child.into(),
            parent_session_id: parent.map(str::to_owned),
            name: child.into(),
            task: "Inspect code".into(),
            capabilities: NativeAgentCapabilities::default(),
        }
    }

    fn text(child: &str, message: &str) -> NativeAgentEvent {
        NativeAgentEvent::Update { session_id: child.into(), update: Box::new(serde_json::from_value(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":message}})).unwrap()) }
    }

    #[test]
    fn completed_children_remain_reusable_but_replay_does_not_prove_availability() {
        use mj_core::native_agent::{NativeAgentAvailability, NativeAgentAvailabilityReport};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        save_session_to(&path, &super::super::tests::session("owner", "project")).unwrap();
        let connection = open(&path).unwrap();
        let apply = |ordinal, update| {
            apply_native_agent_event(&connection, "owner", &event(ordinal, update)).unwrap()
        };
        apply(1, spawn("child", None));
        apply(2, text("child", "retained context"));
        apply(
            3,
            NativeAgentEvent::State {
                session_id: "child".into(),
                state: NativeAgentState::Completed,
            },
        );
        apply(
            4,
            NativeAgentEvent::Availability {
                reports: vec![NativeAgentAvailabilityReport {
                    session_id: "child".into(),
                    stable_id: Some("stable-child".into()),
                    state: None,
                    availability: NativeAgentAvailability::Available,
                    reason: None,
                }],
                complete: true,
            },
        );
        let reusable = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(reusable[0].agent.state, NativeAgentState::Completed);
        assert_eq!(
            reusable[0].agent.availability,
            NativeAgentAvailability::Available
        );
        apply(
            5,
            NativeAgentEvent::State {
                session_id: "child".into(),
                state: NativeAgentState::Running,
            },
        );
        assert_eq!(
            load_native_agents_from(&connection, "owner", 200).unwrap()[0]
                .projection
                .transcript,
            reusable[0].projection.transcript
        );
        apply(6, NativeAgentEvent::ReplayBegin);
        apply(7, NativeAgentEvent::ReplayCommit);
        let retained = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(
            retained.len(),
            1,
            "missing replay must retain inspectable history"
        );
        assert_eq!(
            retained[0].agent.availability,
            NativeAgentAvailability::Unknown
        );
        assert_eq!(retained[0].agent.state, NativeAgentState::Disconnected);
        apply(
            8,
            NativeAgentEvent::Availability {
                reports: vec![],
                complete: false,
            },
        );
        assert_eq!(
            load_native_agents_from(&connection, "owner", 200).unwrap()[0]
                .agent
                .availability,
            NativeAgentAvailability::Unknown
        );
        apply(
            9,
            NativeAgentEvent::Availability {
                reports: vec![],
                complete: true,
            },
        );
        let missing = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(
            missing[0].agent.availability,
            NativeAgentAvailability::Unavailable
        );
        assert_eq!(missing[0].projection.transcript.len(), 1);
    }

    #[test]
    fn native_replay_is_atomic_and_does_not_duplicate_child_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        save_session_to(&path, &super::super::tests::session("owner", "project")).unwrap();
        let connection = open(&path).unwrap();
        let apply = |ordinal, update| {
            apply_native_agent_event(&connection, "owner", &event(ordinal, update)).unwrap()
        };
        apply(1, spawn("child", None));
        apply(2, text("child", &"x".repeat(80_000)));
        let mut original = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(original[0].projection.transcript.len(), 1);
        apply(3, NativeAgentEvent::ReplayBegin);
        original[0].agent.invalidate_availability();
        original[0].agent.state = NativeAgentState::Disconnected;
        apply(4, spawn("child", None));
        apply(5, text("child", "replacement"));
        assert_eq!(
            load_native_agents_from(&connection, "owner", 200).unwrap(),
            original
        );
        apply(6, NativeAgentEvent::ReplayCommit);
        let replaced = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(replaced[0].projection.transcript.len(), 1);
        assert!(
            serde_json::to_string(&replaced[0])
                .unwrap()
                .contains("replacement")
        );
        apply(7, NativeAgentEvent::ReplayBegin);
        apply(8, spawn("child", None));
        apply(9, text("child", "replacement"));
        apply(10, NativeAgentEvent::ReplayCommit);
        assert_eq!(
            load_native_agents_from(&connection, "owner", 200).unwrap()[0]
                .projection
                .transcript
                .len(),
            1
        );
        apply(11, NativeAgentEvent::ReplayBegin);
        apply(12, spawn("incomplete", None));
        apply(13, NativeAgentEvent::Disconnected);
        let recovered = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].agent.session_id, "child");
        assert_eq!(recovered[0].agent.state, NativeAgentState::Disconnected);
        assert!(matches!(
            recovered[0].projection.transcript[0].body,
            TranscriptBody::Agent {
                streaming: false,
                ..
            }
        ));
        assert!(
            load_materialized_session_from(&path, "owner")
                .unwrap()
                .unwrap()
                .transcript
                .is_empty()
        );
    }

    #[test]
    fn native_children_isolate_tool_ids_and_preserve_nesting_and_terminal_states() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        save_session_to(&path, &super::super::tests::session("owner", "project")).unwrap();
        let connection = open(&path).unwrap();
        for (ordinal, native) in [spawn("child", None), spawn("nested", Some("child"))]
            .into_iter()
            .enumerate()
        {
            apply_native_agent_event(&connection, "owner", &event(ordinal as u64 + 1, native))
                .unwrap();
        }
        for (index, child) in ["child", "nested"].into_iter().enumerate() {
            let update = serde_json::from_value(json!({"sessionUpdate":"tool_call","toolCallId":"same-id","title":child,"kind":"read","status":"in_progress"})).unwrap();
            apply_native_agent_event(
                &connection,
                "owner",
                &event(
                    index as u64 + 3,
                    NativeAgentEvent::Update {
                        session_id: child.into(),
                        update: Box::new(update),
                    },
                ),
            )
            .unwrap();
        }
        apply_native_agent_event(
            &connection,
            "owner",
            &event(
                5,
                NativeAgentEvent::State {
                    session_id: "nested".into(),
                    state: NativeAgentState::Failed,
                },
            ),
        )
        .unwrap();
        let views = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].projection.transcript.len(), 1);
        assert_eq!(views[1].projection.transcript.len(), 1);
        assert_eq!(views[0].agent.state, NativeAgentState::Running);
        assert_eq!(views[1].agent.state, NativeAgentState::Failed);
        assert_eq!(views[1].agent.parent_view_id(), views[0].agent.view_id());
    }
    #[test]
    fn native_history_pages_older_messages_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        save_session_to(&path, &super::super::tests::session("owner", "project")).unwrap();
        let connection = open(&path).unwrap();
        apply_native_agent_event(&connection, "owner", &event(1, spawn("child", None))).unwrap();
        for ordinal in 2..=232 {
            let update = serde_json::from_value(json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":format!("message {ordinal}: {}", "x".repeat(1024))}})).unwrap();
            apply_native_agent_event(
                &connection,
                "owner",
                &event(
                    ordinal,
                    NativeAgentEvent::Update {
                        session_id: "child".into(),
                        update: Box::new(update),
                    },
                ),
            )
            .unwrap();
        }
        let page = native_agent_history_from(&connection, "owner", "child", None).unwrap();
        assert_eq!(page.items.len(), 200);
        assert!(serde_json::to_vec(&page).unwrap().len() > 64 * 1024);
        assert!(page.has_more);
        let first = &page.items[0];
        let older = native_agent_history_from(
            &connection,
            "owner",
            "child",
            Some((first.position, first.stable_id.clone())),
        )
        .unwrap();
        assert_eq!(older.items.len(), 31);
        assert!(!older.has_more);
        assert_eq!(older.generation_ordinal, page.generation_ordinal);
        assert_eq!(older.items.last().unwrap().position + 1, first.position);
        assert_eq!(older.items[0].position, 2);
    }
    #[tokio::test]
    async fn native_transcripts_larger_than_a_frame_leave_runtime_snapshots_deliverable() {
        use mj_client::daemon::{
            DaemonReply, MAX_FRAME_BYTES, PROTOCOL_VERSION, ResponseEnvelope, RuntimeSnapshot,
            read_frame,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        save_session_to(&path, &super::super::tests::session("owner", "project")).unwrap();
        let connection = open(&path).unwrap();
        let large = "完整 transcript ".repeat(40_000);
        for index in 0..16 {
            let child = format!("child-{index}");
            apply_native_agent_event(
                &connection,
                "owner",
                &event(index * 2 + 1, spawn(&child, None)),
            )
            .unwrap();
            apply_native_agent_event(
                &connection,
                "owner",
                &event(index * 2 + 2, text(&child, &large)),
            )
            .unwrap();
        }
        let views = load_native_agents_from(&connection, "owner", 200).unwrap();
        assert!(serde_json::to_vec(&views).unwrap().len() > MAX_FRAME_BYTES);
        let summaries = load_native_agent_summaries_from(&connection, "owner").unwrap();
        for summary in &summaries {
            let view =
                load_native_agent_view_from(&connection, "owner", &summary.agent.session_id, 200)
                    .unwrap()
                    .unwrap();
            assert!(summary.is_satisfied_by(&view));
            assert_eq!(
                &view,
                views
                    .iter()
                    .find(|v| v.agent.session_id == summary.agent.session_id)
                    .unwrap()
            );
        }
        let snapshot = RuntimeSnapshot {
            native_agents: summaries,
            workspace_names: Default::default(),
            moves: Vec::new(),
            revision: 1,
            config: mj_core::config::Config::default(),
            records: Vec::new(),
            sessions: Vec::new(),
            lifecycles: Vec::new(),
            reviews: Vec::new(),
            notices: Vec::new(),
            subagents: Vec::new(),
        };
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < 32 * 1024);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let send = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            mj_client::daemon::write_frame(
                &mut stream,
                &ResponseEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: 1,
                    result: Ok(DaemonReply::RuntimeSnapshot(Box::new(snapshot))),
                },
            )
            .await
            .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let reply: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
        let Ok(DaemonReply::RuntimeSnapshot(snapshot)) = reply.result else {
            panic!("runtime reply");
        };
        assert_eq!(snapshot.native_agents.len(), 16);
        send.await.unwrap();
    }
}
