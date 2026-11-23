use super::*;
use crate::acp::antigravity_tasks::AntigravityStateFollower;
use crate::relay::{BackgroundWorkPolicy, HarnessTurnPolicy};
use mj_core::activity::ActivityState;
use mj_core::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayExecutionState, RelayOperationalState,
    RelayRequestEnvelope, RelayResponseBody, RelayResponsePayload,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::Duration;

const NATIVE: &str = "conversation";
const IDLE: &str = "CASCADE_RUN_STATUS_IDLE";
const RUNNING: &str = "CASCADE_RUN_STATUS_RUNNING";

struct Databases {
    summary: Connection,
    steps: Connection,
}

impl Databases {
    fn create(home: &Path) -> Self {
        let dir = home.join("antigravity-cli");
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        let summary = Connection::open(dir.join("conversation_summaries.db")).unwrap();
        summary.pragma_update(None, "journal_mode", "WAL").unwrap();
        summary
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY, status TEXT, not_fully_idle INTEGER,
                killed INTEGER, step_count INTEGER, last_modified_time, raw_summary BLOB
            );",
            )
            .unwrap();
        summary
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1,?2,0,0,0,0,NULL)",
                params![NATIVE, IDLE],
            )
            .unwrap();
        let steps =
            Connection::open(dir.join("conversations").join(format!("{NATIVE}.db"))).unwrap();
        steps
            .execute_batch(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY,status INTEGER,task_details BLOB);",
            )
            .unwrap();
        Self { summary, steps }
    }

    fn set_summary(&self, status: &str, not_fully_idle: bool) {
        self.summary.execute(
            "UPDATE conversation_summaries SET status=?1,not_fully_idle=?2 WHERE conversation_id=?3",
            params![status, not_fully_idle, NATIVE],
        ).unwrap();
    }

    fn set_step_count(&self, step_count: i64) {
        self.summary
            .execute(
                "UPDATE conversation_summaries SET step_count=?1 WHERE conversation_id=?2",
                params![step_count, NATIVE],
            )
            .unwrap();
    }

    fn task(&self, index: i64) {
        // Native TaskDetails protobuf fields 1 (id) and 7 (title).
        let details = b"\x0a\x04task\x3a\x10Build validation";
        self.steps
            .execute(
                "INSERT INTO steps VALUES (?1,2,?2)",
                params![index, details],
            )
            .unwrap();
    }

    fn clear_tasks(&self) {
        self.steps.execute("DELETE FROM steps", []).unwrap();
    }
}

fn antigravity_relay(root: &Path) -> Arc<RelayCell> {
    let mut relay = DurableRelay::open(root, crate::relay::test_support::SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(HarnessKind::Antigravity);
    relay.set_harness_turn_policy(HarnessTurnPolicy::Antigravity);
    relay.set_background_work_policy(BackgroundWorkPolicy::AntigravityTasks);
    Arc::new(RelayCell::new(relay))
}

fn configured_event() -> RuntimeEvent {
    RuntimeEvent::SessionConfigured {
        config_options: Vec::new(),
    }
}

fn opened_event() -> RuntimeEvent {
    RuntimeEvent::SessionStarted {
        native_session_id: NATIVE.into(),
        resumed: true,
        execution_mode: None,
        native_continuity_lost: false,
        replaced_unused_native_session_id: None,
    }
}

async fn attach_monitor(home: &Path, relay: &Arc<RelayCell>) -> AntigravityTaskMonitor {
    let mut in_flight = BTreeMap::new();
    let mut configuration = SessionConfiguration::Opening;
    record_runtime_event_with_startup(
        relay,
        &mut in_flight,
        opened_event(),
        &mut configuration,
        None,
        None,
    )
    .unwrap();
    let mut monitor = Some(AntigravityTaskMonitor::new(Ok(home.to_owned())));
    let event = configured_event();
    prepare_antigravity_runtime_event(&mut monitor, relay, &event)
        .await
        .unwrap();
    record_runtime_event_with_startup(relay, &mut in_flight, event, &mut configuration, None, None)
        .unwrap();
    monitor.unwrap()
}

fn request(relay: &mut DurableRelay, request: RelayRequest) -> RelayResponsePayload {
    let response = relay.handle(RelayRequestEnvelope {
        request_id: "test-request".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request,
    });
    match response.body {
        RelayResponseBody::Ok { payload } => payload,
        other => panic!("unexpected relay response: {other:?}"),
    }
}

fn stop(relay: &mut DurableRelay, id: Option<&str>, fully_idle: Option<bool>, reason: &str) {
    assert!(matches!(
        request(
            relay,
            RelayRequest::AntigravityStop {
                conversation_id: id.map(str::to_owned),
                fully_idle,
                termination_reason: Some(reason.into()),
            }
        ),
        RelayResponsePayload::AntigravityStopObserved,
    ));
}

async fn stop_over_socket(home: &Path, relay: &Arc<RelayCell>, reason: &str) {
    let (server, client) = UnixStream::pair().unwrap();
    let (wake, _wakes) = mpsc::channel(1);
    let (fatal, _errors) = mpsc::channel(1);
    let task = mj_core::runtime::spawn(serve_client_with_memory(
        server,
        relay.clone(),
        wake,
        Err("fixture has no credentials".into()),
        ConnectionRuntime {
            antigravity_home: Some(home.to_owned()),
            ..Default::default()
        },
        fatal,
    ));
    let mut client = BufReader::new(client);
    let mut bytes = serde_json::to_vec(&RelayRequestEnvelope {
        request_id: "stop-hook".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::AntigravityStop {
            conversation_id: Some(NATIVE.into()),
            fully_idle: Some(false),
            termination_reason: Some(reason.into()),
        },
    })
    .unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let response: RelayResponseEnvelope = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        response.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::AntigravityStopObserved,
        },
    ));
    drop(client);
    task.await.unwrap().unwrap();
}

fn pre_invocation(relay: &mut DurableRelay, id: Option<&str>) {
    pre_invocation_with_steps(relay, id, None);
}

fn pre_invocation_with_steps(
    relay: &mut DurableRelay,
    id: Option<&str>,
    initial_num_steps: Option<i64>,
) {
    assert!(matches!(
        request(
            relay,
            RelayRequest::DrainMailbox {
                hook_event: "PostToolBatch".into(),
                conversation_id: id.map(str::to_owned),
                initial_num_steps,
            }
        ),
        RelayResponsePayload::MailboxDrained { .. },
    ));
}

fn reserve_idle(relay: &mut DurableRelay) -> bool {
    let RelayResponsePayload::IdleReservation { ordinal } = request(
        relay,
        RelayRequest::ReserveIdle {
            command_id: "replace-worker".into(),
        },
    ) else {
        panic!("expected idle reservation answer");
    };
    ordinal.is_some()
}

async fn wait_state(relay: &Arc<RelayCell>, mut ready: impl FnMut(&RelayOperationalState) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if ready(&relay.lock().unwrap().operational_state()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out at {:?}",
            relay.lock().unwrap().operational_state()
        )
    });
}

#[tokio::test]
async fn antigravity_coordinator_orders_stop_after_buffered_output_and_preserves_background_work() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    let relay = antigravity_relay(&temp.path().join("relay"));
    let (wake_tx, wakes) = mpsc::channel(8);
    let (event_tx, events) = mpsc::channel(8);
    let (commands, mut command_rx) = mpsc::channel(8);
    let (shell_events, _shell_rx) = mpsc::channel(1);
    let shells = crate::user_shell::UserShellRegistry::new(
        temp.path().to_owned(),
        BTreeMap::new(),
        shell_events,
    );
    event_tx.try_send(opened_event()).unwrap();
    event_tx.try_send(configured_event()).unwrap();
    let coordinator = mj_core::runtime::spawn(run_relay_coordinator(
        relay.clone(),
        events,
        wakes,
        commands,
        shells,
        TaskMonitors {
            kimi: None,
            antigravity: Some(AntigravityTaskMonitor::new(Ok(temp.path().to_owned()))),
        },
        None,
        None,
        None,
    ));
    wait_state(&relay, |state| state.native_session_is_ready()).await;
    assert!(matches!(
        request(
            &mut relay.lock().unwrap(),
            RelayRequest::Submit {
                command_id: "test-prompt".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                        "Run validation"
                    )],
                },
            }
        ),
        RelayResponsePayload::Accepted { .. },
    ));
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), command_rx.recv()).await.unwrap(),
        Some(CommandRequest::Prompt { request_id, .. }) if request_id == "test-prompt"
    ));

    {
        let mut state = relay.lock().unwrap();
        stop(&mut state, Some(NATIVE), Some(false), "END_TURN");
        assert_eq!(
            state.operational_state().background_commands[0].id,
            "antigravity:pending"
        );
        assert!(!reserve_idle(&mut state));
    }
    databases.task(1);
    event_tx
        .try_send(RuntimeEvent::PromptFinished {
            request_id: "test-prompt".into(),
            stop_reason: "end_turn".into(),
            usage: None,
            diagnostic: None,
        })
        .unwrap();
    wait_state(&relay, |state| state.active_prompt.is_none()).await;
    {
        let mut state = relay.lock().unwrap();
        let snapshot = state.operational_state();
        assert!(matches!(
            snapshot.activity_state(),
            ActivityState::Background { .. }
        ));
        assert_eq!(snapshot.background_commands[0].command, "Build validation");
        assert_eq!(
            state.activity_facts().antigravity_task_started_at_ms,
            Some(snapshot.background_commands[0].started_at_ms)
        );
        assert_eq!(state.activity_facts(), snapshot.facts());
        assert!(!snapshot.safe_to_replace(HarnessKind::Antigravity));
        assert!(!reserve_idle(&mut state));

        pre_invocation(&mut state, Some("subagent"));
        stop(&mut state, Some("subagent"), Some(true), "END_TURN");
        stop(&mut state, None, Some(true), "END_TURN");
        assert!(state.operational_state().harness_turn.is_none());
    }
    databases.clear_tasks();
    databases.set_summary(IDLE, true);
    wait_state(&relay, |state| {
        state
            .background_commands
            .iter()
            .all(|task| task.id == "antigravity:pending")
    })
    .await;
    {
        let mut state = relay.lock().unwrap();
        assert_eq!(
            state.operational_state().background_commands[0].id,
            "antigravity:pending"
        );
        pre_invocation(&mut state, Some(NATIVE));
        let snapshot = state.operational_state();
        assert_eq!(snapshot.execution, RelayExecutionState::Running);
        assert!(snapshot.tools_in_flight.is_empty());
        assert!(!reserve_idle(&mut state));

        stop(&mut state, Some("subagent"), Some(true), "END_TURN");
        assert!(!state.has_pending_antigravity_turn_settle());
        stop(&mut state, Some(NATIVE), Some(false), "END_TURN");
        assert!(state.has_pending_antigravity_turn_settle());
        assert!(!reserve_idle(&mut state));
    }
    wake_tx.try_send(()).unwrap();
    wait_state(&relay, |state| state.harness_turn.is_none()).await;
    {
        let mut state = relay.lock().unwrap();
        assert_eq!(
            state.operational_state().background_commands[0].id,
            "antigravity:pending"
        );
        assert!(!reserve_idle(&mut state));

        pre_invocation(&mut state, Some(NATIVE));
        event_tx
            .try_send(RuntimeEvent::SessionUpdate {
                update: serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "Validation finished."},
                }),
            })
            .unwrap();
        stop(&mut state, Some(NATIVE), Some(true), "END_TURN");
        assert!(!reserve_idle(&mut state));
    }
    wake_tx.try_send(()).unwrap();
    wait_state(&relay, |state| state.harness_turn.is_none()).await;
    {
        let mut state = relay.lock().unwrap();
        let events = state.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        let chunk = events.iter().find(|event| matches!(
            &event.observation,
            RelayObservation::SessionUpdate { update }
                if matches!(update.as_ref(), agent_client_protocol::v1::schema::SessionUpdate::AgentMessageChunk(_))
        )).unwrap();
        let settled = events.iter().rev().find(|event| matches!(
            &event.observation,
            RelayObservation::HarnessTurnSettled { origin: Some(origin), .. } if origin == "antigravity"
        )).unwrap();
        assert!(chunk.ordinal < settled.ordinal);
        assert!(reserve_idle(&mut state));
    }
    drop(event_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn antigravity_sqlite_recovery_fences_stale_steps_and_stop_generations() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    databases.set_summary(RUNNING, true);
    databases.task(1);
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;
    {
        let mut state = relay.lock().unwrap();
        assert!(
            state
                .operational_state()
                .safe_to_replace(HarnessKind::Antigravity)
        );
        pre_invocation(&mut state, None);
        pre_invocation(&mut state, Some(""));
        assert!(state.operational_state().harness_turn.is_none());
        stop(&mut state, Some(NATIVE), Some(true), "END_TURN");
    }
    monitor.refresh(&relay, true).await.unwrap();
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .background_commands
            .is_empty()
    );

    databases.set_summary(IDLE, false);
    monitor.refresh(&relay, true).await.unwrap();
    {
        let mut state = relay.lock().unwrap();
        stop(&mut state, Some(NATIVE), Some(false), "END_TURN");
    }
    monitor.refresh(&relay, true).await.unwrap();
    assert!(!reserve_idle(&mut relay.lock().unwrap()));

    databases
        .summary
        .execute(
            "INSERT INTO conversation_summaries VALUES ('subagent',?1,0,0,0,1,NULL)",
            [IDLE],
        )
        .unwrap();
    monitor.refresh(&relay, true).await.unwrap();
    assert!(!reserve_idle(&mut relay.lock().unwrap()));

    // A genuine WAL commit supersedes the pending Stop, even when the row
    // still looks idle. A failed scan must leave that commit for the retry.
    databases
        .summary
        .execute(
            "UPDATE conversation_summaries SET last_modified_time=1 WHERE conversation_id=?1",
            [NATIVE],
        )
        .unwrap();
    databases
        .steps
        .execute("INSERT INTO steps VALUES (2,2,X'00')", [])
        .unwrap();
    monitor.refresh(&relay, true).await.unwrap();
    assert_eq!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .background_work_known,
        Some(false)
    );
    databases
        .steps
        .execute("DELETE FROM steps WHERE idx=2", [])
        .unwrap();
    monitor.refresh(&relay, true).await.unwrap();
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .safe_to_replace(HarnessKind::Antigravity)
    );

    databases.clear_tasks();
    monitor.refresh(&relay, true).await.unwrap();
    databases.task(1);
    monitor.refresh(&relay, true).await.unwrap();
    assert_eq!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .background_commands[0]
            .command,
        "Build validation"
    );
}

#[tokio::test]
async fn antigravity_stale_scan_cannot_clear_a_concurrent_stop() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    let relay = antigravity_relay(&temp.path().join("relay"));
    let _monitor = attach_monitor(temp.path(), &relay).await;
    let mut follower = AntigravityStateFollower::new(temp.path().to_owned(), NATIVE.into());
    // Establish the committed baseline cursor before the concurrent race.
    let (baseline, cursor) = follower.refresh_pending().unwrap();
    {
        let mut state = relay.lock().unwrap();
        let epoch = state.antigravity_hook_epoch();
        assert!(state.antigravity_snapshot_changed(baseline, epoch).unwrap());
    }
    follower.commit_refresh(cursor);

    {
        let mut state = relay.lock().unwrap();
        pre_invocation(&mut state, Some(NATIVE));
    }
    let scan_epoch = relay.lock().unwrap().antigravity_hook_epoch();
    // Simulate a summary commit read by an in-flight scan while a Stop hook
    // arrives concurrently: the stale scan must be rejected without consuming
    // the follower's cursor, so the retry still observes that same commit.
    databases
        .summary
        .execute(
            "UPDATE conversation_summaries SET status=?1,not_fully_idle=0,step_count=4 WHERE conversation_id=?2",
            params![IDLE, NATIVE],
        )
        .unwrap();
    let (stale_snapshot, _discarded_cursor) = follower.refresh_pending().unwrap();
    assert!(stale_snapshot.summary_commit_changed);
    stop_over_socket(temp.path(), &relay, "NO_TOOL_CALL").await;
    {
        let mut state = relay.lock().unwrap();
        assert!(state.settle_pending_antigravity_harness_turn().unwrap());
        assert!(
            !state
                .antigravity_snapshot_changed(stale_snapshot, scan_epoch)
                .unwrap()
        );
        assert!(!state.operational_state().background_commands.is_empty());
        assert!(!reserve_idle(&mut state));
    }
    // The follower sees the previously unseen commit again. It was already
    // on disk when Stop ran and cannot clear it.
    let epoch = relay.lock().unwrap().antigravity_hook_epoch();
    let (fresh, fresh_cursor) = follower.refresh_pending().unwrap();
    assert!(fresh.summary_commit_changed);
    let mut state = relay.lock().unwrap();
    assert!(state.antigravity_snapshot_changed(fresh, epoch).unwrap());
    follower.commit_refresh(fresh_cursor);
    assert!(!state.operational_state().background_commands.is_empty());
    assert!(!reserve_idle(&mut state));

    databases.set_step_count(6);
    assert!(
        state
            .antigravity_snapshot_changed(follower.refresh().unwrap(), epoch)
            .unwrap()
    );
    assert!(state.operational_state().background_commands.is_empty());
    assert!(reserve_idle(&mut state));
}

#[tokio::test]
async fn antigravity_previous_turn_idle_flush_cannot_clear_stop() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;
    pre_invocation_with_steps(&mut relay.lock().unwrap(), Some(NATIVE), Some(5));

    stop_over_socket(temp.path(), &relay, "NO_TOOL_CALL").await;
    assert!(
        relay
            .lock()
            .unwrap()
            .settle_pending_antigravity_harness_turn()
            .unwrap()
    );
    databases.set_step_count(4);
    monitor.refresh(&relay, true).await.unwrap();
    {
        let mut state = relay.lock().unwrap();
        assert!(!state.operational_state().background_commands.is_empty());
        assert!(!reserve_idle(&mut state));
    }

    databases.set_step_count(6);
    monitor.refresh(&relay, true).await.unwrap();
    let mut state = relay.lock().unwrap();
    assert!(state.operational_state().background_commands.is_empty());
    assert!(reserve_idle(&mut state));
}

#[tokio::test]
async fn antigravity_hookless_running_summary_refuses_idle_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;
    {
        let mut state = relay.lock().unwrap();
        request(
            &mut state,
            RelayRequest::Submit {
                command_id: "first-prompt".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                        "Hello",
                    )],
                },
            },
        );
        let commands = state.claim_pending_commands(true).unwrap();
        assert_eq!(commands[0].command_id, "first-prompt");
        state
            .record_command_completed(
                "first-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    diagnostic: None,
                    usage: None,
                    work_facts: None,
                },
            )
            .unwrap();
    }
    monitor.refresh(&relay, true).await.unwrap();
    {
        let state = relay.lock().unwrap();
        assert!(
            state
                .operational_state()
                .safe_to_replace(HarnessKind::Antigravity)
        );
    }

    for status in [
        RUNNING,
        "CASCADE_RUN_STATUS_CANCELING",
        "CASCADE_RUN_STATUS_BUSY",
    ] {
        databases.set_summary(status, false);
        monitor.refresh(&relay, true).await.unwrap();
        let mut state = relay.lock().unwrap();
        let snapshot = state.operational_state();
        assert_eq!(
            snapshot.background_commands[0].command,
            "Antigravity turn running"
        );
        assert!(!reserve_idle(&mut state));
    }

    databases.set_summary(IDLE, false);
    monitor.refresh(&relay, true).await.unwrap();
    let mut state = relay.lock().unwrap();
    assert!(state.operational_state().background_commands.is_empty());
    assert!(reserve_idle(&mut state));
}

#[tokio::test]
async fn antigravity_missing_native_databases_fail_closed_after_activation() {
    let temp = tempfile::tempdir().unwrap();
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;

    // Before any prompt or hook in this ACP epoch, absent DB files are expected
    // for a brand-new conversation and must not wedge idle replacement.
    {
        let state = relay.lock().unwrap();
        assert_eq!(state.operational_state().background_work_known, Some(true));
        assert!(
            state
                .operational_state()
                .safe_to_replace(HarnessKind::Antigravity)
        );
    }

    let databases = Databases::create(temp.path());
    let steps = temp
        .path()
        .join("antigravity-cli/conversations/conversation.db");
    let summary = temp
        .path()
        .join("antigravity-cli/conversation_summaries.db");
    {
        let mut state = relay.lock().unwrap();
        stop(&mut state, Some(NATIVE), Some(true), "NO_TOOL_CALL");
        assert_eq!(state.operational_state().background_work_known, Some(false));
        assert!(!reserve_idle(&mut state));
    }

    std::fs::remove_file(&steps).unwrap();
    monitor.refresh(&relay, true).await.unwrap();
    {
        let mut state = relay.lock().unwrap();
        assert_eq!(state.operational_state().background_work_known, Some(false));
        assert!(!reserve_idle(&mut state));
    }

    std::fs::remove_file(&summary).unwrap();
    monitor.refresh(&relay, true).await.unwrap();
    {
        let mut state = relay.lock().unwrap();
        assert_eq!(state.operational_state().background_work_known, Some(false));
        assert!(!reserve_idle(&mut state));
    }

    drop(databases);
    let _restored = Databases::create(temp.path());
    monitor.refresh(&relay, true).await.unwrap();
    let mut state = relay.lock().unwrap();
    assert_eq!(state.operational_state().background_work_known, Some(true));
    assert!(reserve_idle(&mut state));
}

#[tokio::test]
async fn antigravity_missing_stop_recovers_and_cancel_accepts_an_autonomous_turn() {
    let temp = tempfile::tempdir().unwrap();
    let databases = Databases::create(temp.path());
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;
    {
        let mut state = relay.lock().unwrap();
        pre_invocation(&mut state, Some(NATIVE));
        let snapshot = state.operational_state();
        assert_eq!(snapshot.execution, RelayExecutionState::Running);
        assert!(snapshot.tools_in_flight.is_empty());
        assert!(snapshot.background_commands.is_empty());
        assert!(!reserve_idle(&mut state));
    }
    databases.set_summary(RUNNING, false);
    monitor.refresh(&relay, true).await.unwrap();
    databases.set_summary(IDLE, false);
    monitor.refresh(&relay, true).await.unwrap();

    let mut state = relay.lock().unwrap();
    assert!(state.has_pending_antigravity_turn_settle());
    assert!(!reserve_idle(&mut state));
    assert!(state.settle_pending_antigravity_harness_turn().unwrap());
    pre_invocation(&mut state, Some(NATIVE));
    let command = state
        .operational_state()
        .turn_controls
        .unwrap()
        .interrupt
        .unwrap();
    assert!(matches!(
        command,
        RelayCommand::CancelTurnTarget {
            target: mj_core::relay::TurnInterruptTarget::Autonomous { .. }
        }
    ));
    assert!(matches!(
        request(
            &mut state,
            RelayRequest::Submit {
                command_id: "test-cancel".into(),
                command,
            }
        ),
        RelayResponsePayload::Accepted { .. },
    ));
    assert_eq!(
        state.claim_pending_commands(true).unwrap()[0].command_id,
        "test-cancel"
    );
    state
        .record_command_completed("test-cancel", RelayCommandOutcome::Cancelled)
        .unwrap();
    let ordinal = state.unanswered_harness_turn_stop().unwrap();
    assert!(state.end_unanswered_harness_turn_stop(ordinal).unwrap());
    assert!(state.operational_state().harness_turn.is_none());
}

#[tokio::test]
async fn antigravity_broken_database_refuses_idle_reservation_until_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("antigravity-cli");
    std::fs::create_dir(&root).unwrap();
    let summary = root.join("conversation_summaries.db");
    std::fs::write(&summary, b"invalid SQLite database").unwrap();
    let relay = antigravity_relay(&temp.path().join("relay"));
    let mut monitor = attach_monitor(temp.path(), &relay).await;
    {
        let mut state = relay.lock().unwrap();
        let snapshot = state.operational_state();
        assert_eq!(snapshot.background_work_known, Some(false));
        assert!(!snapshot.safe_to_replace(HarnessKind::Antigravity));
        assert!(!reserve_idle(&mut state));
        assert!(
            state
                .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
                .unwrap()
                .iter()
                .any(
                    |event| matches!(&event.observation, RelayObservation::Warning { message }
                if message.contains("Antigravity background task tracking is unavailable"))
                )
        );
    }
    std::fs::remove_file(summary).unwrap();
    let _databases = Databases::create(temp.path());
    monitor.refresh(&relay, true).await.unwrap();
    assert!(reserve_idle(&mut relay.lock().unwrap()));
}
