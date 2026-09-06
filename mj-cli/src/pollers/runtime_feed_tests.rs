use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use hel::hel_config::{HarnessKind, HelConfig};
use hel::hel_state::{MaterializedSession, ProjectionWindow, SessionRecord, SessionState};
use hel::hel_worker::{RELAY_EVENT_GENESIS_DIGEST, RelayExecutionState, RelayOperationalState};
use mj_controller::hel_session_manager::ViewError;

use super::*;

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn session_record(id: &str) -> SessionRecord {
    SessionRecord {
        id: id.into(),
        workspace_id: "workspace-1".into(),
        title: format!("Session {id}"),
        harness_kind: HarnessKind::Codex,
        last_profile: "profile-1".into(),
        bundle_id: "bundle-1".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "target-1".into(),
        resource_allocation: None,
        container_cpus: None,
        container_memory: None,
        state: SessionState::Running,
        archived: false,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
        additional_mounts: Vec::new(),
    }
}

fn operational(session_id: &str) -> RelayOperationalState {
    RelayOperationalState {
        session_id: session_id.into(),
        execution: RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        acknowledged_through: 0,
        acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        native_session_id: None,
        agent_capabilities: None,
        agent_info: None,
        config_options: Vec::new(),
        modes: None,
        available_commands: Vec::new(),
        config: BTreeMap::new(),
        active_prompt: None,
        queued_prompts: Vec::new(),
        active_user_shells: Vec::new(),
        active_agent_terminals: Vec::new(),
        checkpoint_barrier: None,
        checkpoint_ready: None,
        last_acp_activity_at_ms: None,
        current_step_started_at_ms: None,
        foreground_tool_started_at_ms: None,
        harness_turn: None,
        last_harness_turn_started_ordinal: None,
        background_commands: Vec::new(),
    }
}

fn runtime_view(
    session_id: &str,
    projection_ordinal: u64,
    projection_digest: &str,
) -> daemon::RuntimeSessionView {
    daemon::RuntimeSessionView {
        session_id: session_id.into(),
        projection_ordinal,
        projection_digest: projection_digest.into(),
        operational: Some(operational(session_id)),
        latest_credential_sync_signal: None,
        connected: true,
        error: None,
    }
}

fn projection(
    session_id: &str,
    ordinal: u64,
    digest: &str,
) -> (MaterializedSession, ProjectionWindow) {
    let mut materialized = MaterializedSession::empty(session_id);
    materialized.applied_event_ordinal = ordinal;
    materialized.applied_event_digest = digest.into();
    let window = ProjectionWindow::of(&materialized);
    (materialized, window)
}

fn snapshot(
    revision: u64,
    sessions: Vec<daemon::RuntimeSessionView>,
    records: Vec<SessionRecord>,
) -> daemon::RuntimeSnapshot {
    daemon::RuntimeSnapshot {
        revision,
        config: HelConfig::default(),
        records,
        sessions,
        lifecycles: Vec::new(),
        reviews: Vec::new(),
        notices: Vec::new(),
    }
}

async fn wait_for_drop(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted feed work was dropped");
}

#[tokio::test]
async fn runtime_feed_publishes_records_snapshot_before_session_projection() {
    type PollRequest = (
        u64,
        tokio::sync::oneshot::Sender<anyhow::Result<daemon::RuntimeSnapshot>>,
    );
    let (poll_started_tx, mut poll_started_rx) =
        tokio::sync::mpsc::unbounded_channel::<PollRequest>();
    let poll = move |_workspace_id: String, after_revision: u64| {
        let poll_started_tx = poll_started_tx.clone();
        async move {
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            poll_started_tx
                .send((after_revision, finish_tx))
                .expect("runtime feed poll receiver remains open");
            finish_rx
                .await
                .map_err(|_| anyhow::anyhow!("poll completion dropped"))?
        }
    };
    let load =
        move |session_id: String| async move { Ok(Some(projection(&session_id, 4, "digest-4"))) };
    let mut feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);

    let (after_revision, finish) = poll_started_rx.recv().await.expect("initial poll starts");
    assert_eq!(after_revision, 0);
    finish
        .send(Ok(snapshot(
            1,
            vec![runtime_view("session-1", 4, "digest-4")],
            vec![session_record("session-1")],
        )))
        .unwrap();

    let first = feed.updates.recv().await.expect("snapshot update");
    match first {
        RuntimeFeedUpdate::Snapshot(snapshot) => {
            assert_eq!(snapshot.records.len(), 1);
            assert_eq!(snapshot.records[0].id, "session-1");
        }
        RuntimeFeedUpdate::Session { .. } => panic!("session projection preceded its snapshot"),
        RuntimeFeedUpdate::Error(error) => panic!("unexpected feed error: {error}"),
    }

    let second = feed.updates.recv().await.expect("session update");
    match second {
        RuntimeFeedUpdate::Session { session_id, view } => {
            assert_eq!(session_id, "session-1");
            assert_eq!(view.snapshot.unwrap().materialized.applied_event_ordinal, 4);
        }
        RuntimeFeedUpdate::Snapshot(_) => panic!("session projection did not follow snapshot"),
        RuntimeFeedUpdate::Error(error) => panic!("unexpected feed error: {error}"),
    }

    drop(feed);
}

#[tokio::test]
async fn runtime_feed_skips_unchanged_fingerprints_and_loads_changed_projections() {
    type PollRequest = (
        u64,
        tokio::sync::oneshot::Sender<anyhow::Result<daemon::RuntimeSnapshot>>,
    );
    let (poll_started_tx, mut poll_started_rx) =
        tokio::sync::mpsc::unbounded_channel::<PollRequest>();
    let poll = move |_workspace_id: String, after_revision: u64| {
        let poll_started_tx = poll_started_tx.clone();
        async move {
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            poll_started_tx
                .send((after_revision, finish_tx))
                .expect("runtime feed poll receiver remains open");
            finish_rx
                .await
                .map_err(|_| anyhow::anyhow!("poll completion dropped"))?
        }
    };
    let load_count = Arc::new(AtomicUsize::new(0));
    let load = {
        let load_count = Arc::clone(&load_count);
        move |session_id: String| {
            let ordinal = match load_count.fetch_add(1, Ordering::SeqCst) {
                0 => 1,
                1 => 2,
                count => panic!("unexpected projection load {count}"),
            };
            async move {
                Ok(Some(projection(
                    &session_id,
                    ordinal,
                    &format!("digest-{ordinal}"),
                )))
            }
        }
    };
    let mut feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);

    let (_, finish) = poll_started_rx.recv().await.expect("first poll starts");
    finish
        .send(Ok(snapshot(
            1,
            vec![runtime_view("session-1", 1, "digest-1")],
            Vec::new(),
        )))
        .unwrap();
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(_))
    ));
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Session { .. })
    ));

    let (after_revision, finish) = poll_started_rx.recv().await.expect("unchanged poll starts");
    assert_eq!(after_revision, 1);
    finish
        .send(Ok(snapshot(
            2,
            vec![runtime_view("session-1", 1, "digest-1")],
            Vec::new(),
        )))
        .unwrap();
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(_))
    ));
    assert_eq!(load_count.load(Ordering::SeqCst), 1);

    let (after_revision, finish) = poll_started_rx.recv().await.expect("changed poll starts");
    assert_eq!(after_revision, 2);
    finish
        .send(Ok(snapshot(
            3,
            vec![runtime_view("session-1", 2, "digest-2")],
            Vec::new(),
        )))
        .unwrap();
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(_))
    ));
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Session { .. })
    ));
    assert_eq!(load_count.load(Ordering::SeqCst), 2);

    drop(feed);
}

#[tokio::test(start_paused = true)]
async fn runtime_feed_reports_poll_failure_and_recovers() {
    type PollRequest = (
        u64,
        tokio::sync::oneshot::Sender<anyhow::Result<daemon::RuntimeSnapshot>>,
    );
    let (poll_started_tx, mut poll_started_rx) =
        tokio::sync::mpsc::unbounded_channel::<PollRequest>();
    let poll = move |_workspace_id: String, after_revision: u64| {
        let poll_started_tx = poll_started_tx.clone();
        async move {
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            poll_started_tx
                .send((after_revision, finish_tx))
                .expect("runtime feed poll receiver remains open");
            finish_rx
                .await
                .map_err(|_| anyhow::anyhow!("poll completion dropped"))?
        }
    };
    let load = move |_session_id: String| async { Ok(None) };
    let mut feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);

    let (after_revision, finish) = poll_started_rx.recv().await.expect("failing poll starts");
    assert_eq!(after_revision, 0);
    finish
        .send(Err(anyhow::anyhow!("first poll failed")))
        .unwrap();
    match feed.updates.recv().await.expect("poll error update") {
        RuntimeFeedUpdate::Error(error) => assert!(error.contains("first poll failed"), "{error}"),
        RuntimeFeedUpdate::Snapshot(_) | RuntimeFeedUpdate::Session { .. } => {
            panic!("poll failure was not surfaced")
        }
    }

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(250)).await;
    let (after_revision, finish) = poll_started_rx.recv().await.expect("recovery poll starts");
    assert_eq!(after_revision, 0);
    finish
        .send(Ok(snapshot(1, Vec::new(), Vec::new())))
        .unwrap();
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(snapshot)) if snapshot.revision == 1
    ));

    drop(feed);
}

#[tokio::test]
async fn dropping_runtime_feed_cancels_a_delayed_poll() {
    let poll_dropped = Arc::new(AtomicBool::new(false));
    let (poll_started_tx, mut poll_started_rx) = tokio::sync::mpsc::unbounded_channel();
    let poll = {
        let poll_dropped = Arc::clone(&poll_dropped);
        move |_workspace_id: String, _after_revision: u64| {
            let poll_started_tx = poll_started_tx.clone();
            let poll_dropped = Arc::clone(&poll_dropped);
            async move {
                poll_started_tx.send(()).expect("poll starts");
                let _guard = DropFlag(poll_dropped);
                std::future::pending::<()>().await;
                unreachable!("pending poll completed")
            }
        }
    };
    let load = move |_session_id: String| async { Ok(None) };
    let feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);
    poll_started_rx.recv().await.expect("delayed poll starts");

    drop(feed);
    wait_for_drop(&poll_dropped).await;
}

#[tokio::test]
async fn dropping_runtime_feed_cancels_a_delayed_projection_load() {
    let load_dropped = Arc::new(AtomicBool::new(false));
    let (load_started_tx, mut load_started_rx) = tokio::sync::mpsc::unbounded_channel();
    let poll = move |_workspace_id: String, _after_revision: u64| async {
        Ok(snapshot(
            1,
            vec![runtime_view("session-1", 1, "digest-1")],
            Vec::new(),
        ))
    };
    let load = {
        let load_dropped = Arc::clone(&load_dropped);
        move |session_id: String| {
            let load_started_tx = load_started_tx.clone();
            let load_dropped = Arc::clone(&load_dropped);
            async move {
                load_started_tx.send(()).expect("projection load starts");
                let _guard = DropFlag(load_dropped);
                std::future::pending::<()>().await;
                Ok(Some(projection(&session_id, 1, "digest-1")))
            }
        }
    };
    let mut feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(_))
    ));
    load_started_rx.recv().await.expect("delayed load starts");

    drop(feed);
    wait_for_drop(&load_dropped).await;
}

#[tokio::test]
async fn a_blocked_session_load_does_not_stop_other_sessions_with_four_workers() {
    let poll = move |_workspace_id: String, _after_revision: u64| async {
        Ok(snapshot(
            1,
            (1..=5)
                .map(|id| runtime_view(&format!("session-{id}"), 1, "digest-1"))
                .collect(),
            Vec::new(),
        ))
    };
    let (load_started_tx, mut load_started_rx) = tokio::sync::mpsc::unbounded_channel();
    let blocked_dropped = Arc::new(AtomicBool::new(false));
    let load = {
        let blocked_dropped = Arc::clone(&blocked_dropped);
        move |session_id: String| {
            let load_started_tx = load_started_tx.clone();
            let blocked_dropped = Arc::clone(&blocked_dropped);
            async move {
                load_started_tx
                    .send(session_id.clone())
                    .expect("load starts");
                if session_id == "session-1" {
                    let _guard = DropFlag(blocked_dropped);
                    std::future::pending::<()>().await;
                }
                Ok(Some(projection(&session_id, 1, "digest-1")))
            }
        }
    };
    let mut feed = spawn_runtime_feed_with("workspace-1".into(), poll, load);
    assert!(matches!(
        feed.updates.recv().await,
        Some(RuntimeFeedUpdate::Snapshot(_))
    ));

    let mut session_updates = Vec::new();
    while session_updates.len() < 4 {
        match tokio::time::timeout(Duration::from_secs(1), feed.updates.recv())
            .await
            .expect("other session projections remain live")
            .expect("runtime feed remains open")
        {
            RuntimeFeedUpdate::Session { session_id, .. } => {
                if session_id != "session-1" && !session_updates.contains(&session_id) {
                    session_updates.push(session_id);
                }
            }
            RuntimeFeedUpdate::Snapshot(_) => {}
            RuntimeFeedUpdate::Error(error) => panic!("unexpected feed error: {error}"),
        }
    }
    session_updates.sort();
    assert_eq!(
        session_updates,
        vec![
            "session-2".to_owned(),
            "session-3".to_owned(),
            "session-4".to_owned(),
            "session-5".to_owned(),
        ]
    );

    let mut starts = Vec::new();
    while let Ok(session_id) = load_started_rx.try_recv() {
        starts.push(session_id);
    }
    starts.sort();
    assert_eq!(
        starts,
        vec![
            "session-1".to_owned(),
            "session-2".to_owned(),
            "session-3".to_owned(),
            "session-4".to_owned(),
            "session-5".to_owned(),
        ]
    );

    drop(feed);
    wait_for_drop(&blocked_dropped).await;
}

#[test]
fn runtime_projection_view_accepts_matching_ordinal_and_digest() {
    let runtime = runtime_view("session-1", 7, "digest-7");
    let (materialized, window) = projection("session-1", 7, "digest-7");
    let mut convergence = ProjectionConvergence::default();

    let view = runtime_projection_view(runtime, Ok(Some((materialized, window))), &mut convergence)
        .expect("matching projection is publishable");
    assert!(view.snapshot.is_some());
    assert!(view.error.is_none());
}

#[test]
fn runtime_projection_view_reports_a_persistent_mismatch_after_bounded_retries() {
    let mut convergence = ProjectionConvergence::default();
    for _ in 0..PROJECTION_CONVERGENCE_RETRIES {
        let (materialized, window) = projection("session-1", 6, "digest-6");
        assert!(
            runtime_projection_view(
                runtime_view("session-1", 7, "digest-7"),
                Ok(Some((materialized, window))),
                &mut convergence,
            )
            .is_none()
        );
    }

    let (materialized, window) = projection("session-1", 6, "digest-6");
    let view = runtime_projection_view(
        runtime_view("session-1", 7, "digest-7"),
        Ok(Some((materialized, window))),
        &mut convergence,
    )
    .expect("bounded mismatch produces an error view");
    assert!(!view.connected);
    assert!(matches!(
        view.error,
        Some(ViewError::ProjectionIntegrity(detail)) if detail.contains("contains only 6")
    ));
    assert!(view.snapshot.is_none());
}
