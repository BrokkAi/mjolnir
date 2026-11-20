use super::*;
use mj_controller::controller::test_support::RefusingExecutor;
use mj_core::subagent::SubagentRecord;
use tokio::io::AsyncWriteExt;

#[test]
fn newer_daemon_protocol_requires_updating_the_client() {
    let metadata = |protocol_version| DaemonMetadata {
        protocol_version,
        pid: std::process::id(),
        address: "127.0.0.1:1".parse().unwrap(),
        token: "tok".into(),
        started_at: "2026-09-01T07:48:14Z".into(),
        build_version: "9.9.9".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    };
    assert!(ensure_supported_daemon_protocol(&metadata(PROTOCOL_VERSION)).is_ok());
    assert!(ensure_supported_daemon_protocol(&metadata(PROTOCOL_VERSION - 1)).is_ok());
    let error = ensure_supported_daemon_protocol(&metadata(PROTOCOL_VERSION + 1)).unwrap_err();
    let message = error.to_string();
    // The two protocol numbers are not enough to find the stale binary.
    assert!(message.contains("version 9.9.9"), "{message}");
    assert!(message.contains("first on PATH"), "{message}");
}

// Hard-won: 796d055c: actionable lifecycle refusals became generic HTTP 500 responses.
#[test]
fn a_lifecycle_failure_carries_a_refusal_across_its_result_channel() {
    let refused = LifecycleFailure::of(
        &anyhow::Error::new(Refusal::precondition(
            "repository \"app\" needs a network Git remote",
        ))
        .context("provision the session target"),
    );
    let rebuilt = refused.clone().into_error();
    assert_eq!(
        Refusal::of(&rebuilt).map(|refusal| refusal.message().to_owned()),
        Some("repository \"app\" needs a network Git remote".to_owned()),
        "a waiter reading the channel must still see the reason, not a bare string"
    );

    let internal = LifecycleFailure::of(&anyhow::anyhow!("ssh host build-07 refused"));
    assert!(internal.refusal.is_none());
    assert!(internal.detail.contains("build-07"));
}

#[test]
fn graceful_close_retires_worker_polling_only_during_target_teardown() {
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Suspend,
        Some(SessionState::Running)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Suspend,
        Some(SessionState::Checkpointing)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Suspend,
        Some(SessionState::Closing)
    ));
    assert!(lifecycle_owns_worker_target(
        LifecycleKind::Suspend,
        Some(SessionState::Destroying)
    ));
    assert!(lifecycle_owns_worker_target(
        LifecycleKind::ForceStop,
        Some(SessionState::Running)
    ));
    assert!(lifecycle_owns_worker_target(
        LifecycleKind::ForceDestroy,
        Some(SessionState::Running)
    ));
}

#[test]
fn restart_keeps_relay_ownership_through_checkpoint_and_can_recover_closing_state() {
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Restart,
        Some(SessionState::Running)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Restart,
        Some(SessionState::Checkpointing)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Restart,
        Some(SessionState::Closing)
    ));
    assert!(lifecycle_owns_worker_target(
        LifecycleKind::Restart,
        Some(SessionState::Error)
    ));
    assert_eq!(
        RuntimeLifecycleKind::from(LifecycleKind::Restart),
        RuntimeLifecycleKind::Resume,
        "surfaces continue to present Restart as a resume-style operation"
    );
}

#[test]
fn restart_recovery_requires_the_checkpoint_bound_to_its_intent() {
    let mut record = runtime_test_session("session-1", "workspace", SessionState::Closing);
    record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/tmp/session-1".into(),
    }));
    record.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path: "/tmp/session-1.tar".into(),
        sha256: "fresh-checkpoint".into(),
        created_at: "2026-10-03T00:00:00Z".into(),
        event_frontier: 7,
    }));
    let mut intent = mj_controller::database::SessionRestartIntent {
        session_id: "session-1".into(),
        operation_id: "restart-1".into(),
        phase: mj_controller::database::SessionRestartPhase::Stopping,
        handoff: None,
        checkpoint_sha256: None,
        checkpoint_started: true,
        container_recreation_acknowledged: true,
        automatic: false,
        container_upgrade: None,
    };

    assert!(!restart::restart_checkpoint_matches(&record, &intent));
    intent.checkpoint_sha256 = Some("older-checkpoint".into());
    assert!(!restart::restart_checkpoint_matches(&record, &intent));
    intent.checkpoint_sha256 = Some("fresh-checkpoint".into());
    assert!(restart::restart_checkpoint_matches(&record, &intent));
    intent.handoff = record.checkpoint.clone().map(|v| *v);
    intent.handoff.as_mut().unwrap().sha256 = "handoff".into();
    assert!(!restart::restart_checkpoint_matches(&record, &intent));
    intent.checkpoint_sha256 = Some("handoff".into());
    assert!(restart::restart_checkpoint_matches(&record, &intent));
    record.checkpoint = None;
    assert!(restart::restart_checkpoint_matches(&record, &intent));
}

#[test]
fn restart_recovery_discards_intents_superseded_by_suspend_or_destroy() {
    use mj_controller::database::SessionRestartPhase;

    assert!(restart::restart_intent_was_superseded(
        Some(SessionState::Stopped),
        SessionRestartPhase::Stopping
    ));
    assert!(restart::restart_intent_was_superseded(
        Some(SessionState::Destroying),
        SessionRestartPhase::Fallback
    ));
    assert!(!restart::restart_intent_was_superseded(
        Some(SessionState::Stopped),
        SessionRestartPhase::Fallback
    ));
}

#[tokio::test]
async fn restart_target_probe_only_allows_fallback_for_a_proven_missing_target() {
    struct StatusExecutor(i32);
    impl CommandExecutor for StatusExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: self.0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let state = test_runtime_state();
    let mut controller = state.controller_projection();
    let mut missing = runtime_test_session("session-1", "workspace", SessionState::Closing);
    missing.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: mj_core::targets::resource_name("session-1").unwrap(),
        workspace_storage: Default::default(),
    }));
    controller
        .state
        .sessions
        .insert(missing.id.clone(), missing);
    let unavailable =
        restart::restart_target_availability(&controller, "session-1", &StatusExecutor(1)).unwrap();
    assert_eq!(unavailable, restart::RestartTargetAvailability::Missing);
    assert!(restart::fallback_is_safe(unavailable));

    let mut reachable = runtime_test_session("bare-session", "workspace", SessionState::Closing);
    reachable.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/tmp/bare-session".into(),
    }));
    controller
        .state
        .sessions
        .insert(reachable.id.clone(), reachable);
    let availability =
        restart::restart_target_availability(&controller, "bare-session", &StatusExecutor(0))
            .unwrap();
    assert_eq!(availability, restart::RestartTargetAvailability::Reachable);
    assert!(!restart::fallback_is_safe(availability));

    let mut apple = runtime_test_session("apple-session", "workspace", SessionState::Closing);
    apple.target_template_id = "mac-apple".into();
    apple.target = Some(Box::new(mj_core::state::TargetLocator::SshAppleContainer {
        borrowed_from: None,
        host: "mac.test".into(),
        container_id: mj_core::targets::resource_name("apple-session").unwrap(),
    }));
    controller.config.targets.insert(
        "mac-apple".into(),
        serde_json::from_value(serde_json::json!({
            "kind": "ssh-apple-container",
            "host": "mac.test",
            "image": "example.invalid/agent:latest"
        }))
        .unwrap(),
    );
    controller.state.sessions.insert(apple.id.clone(), apple);
    // Hard-won: f6b68bcc9: SSH failure during Apple Restart was not classified as unreachable.
    let unreachable =
        restart::restart_target_availability(&controller, "apple-session", &StatusExecutor(255))
            .unwrap();
    assert_eq!(
        unreachable,
        restart::RestartTargetAvailability::Unreachable,
        "a lost SSH connection must preserve the retained Apple container"
    );
}

// Hard-won: 7b694803: Alt-X cancelled target teardown after its verified-checkpoint gate.
#[tokio::test]
async fn a_close_past_its_verified_checkpoint_cannot_be_cancelled() {
    // Whether a person may cancel an operation admitted on a record in this
    // state, before the operation itself commits.
    async fn cancellable_on(kind: LifecycleKind, durable: Option<SessionState>) -> bool {
        let state = test_runtime_state();
        if let Some(durable) = durable {
            state.owner().edit_sessions(|sessions| {
                let record = runtime_test_session("session-1", "workspace", durable);
                sessions.insert(record.id.clone(), record);
            });
        }
        let release = Arc::new(tokio::sync::Notify::new());
        let result = state
            .start_or_join_lifecycle("session-1".into(), kind, {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        let cancellable = state.active_lifecycles()[0].cancellable;
        let refused = state.cancel_lifecycle_if_active("session-1").await.is_err();
        assert_eq!(refused, !cancellable, "{kind:?} on {durable:?}");
        release.notify_one();
        let _ = RuntimeState::wait_lifecycle_result(result).await;
        cancellable
    }

    for durable in [
        None,
        Some(SessionState::Running),
        Some(SessionState::Checkpointing),
        Some(SessionState::Closing),
    ] {
        assert!(cancellable_on(LifecycleKind::Suspend, durable).await);
    }
    // A close or Restart admitted on a record already removing its target
    // (a recovery after a daemon restart) starts committed.
    for kind in [LifecycleKind::Suspend, LifecycleKind::Restart] {
        assert!(!cancellable_on(kind, Some(SessionState::Destroying)).await);
    }
    // Only a graceful close has this gate; a forced teardown keeps none.
    for kind in [LifecycleKind::ForceStop, LifecycleKind::Create] {
        assert!(cancellable_on(kind, Some(SessionState::Destroying)).await);
    }
    // A failed startup's cleanup finishes whichever operation runs it.
    for kind in [LifecycleKind::Create, LifecycleKind::StartupCleanup] {
        assert!(!cancellable_on(kind, Some(SessionState::StartupCleanup)).await);
    }
    // A sub-agent's stop, park and unpark belong to another operation.
    for kind in [
        LifecycleKind::StopSubagent,
        LifecycleKind::Park,
        LifecycleKind::Unpark,
    ] {
        assert!(!cancellable_on(kind, Some(SessionState::Running)).await);
    }
}

/// Run a daemon close of the scripted relay session in its own process and
/// store, holding the relay request that submits `held` (`begin_checkpoint`
/// or `close`) until `at_hold` has run. Returns the close's outcome, what
/// `at_hold` returned, and the session manager and the session's handle.
#[cfg(unix)]
async fn close_with_an_action_at_a_held_relay_command<T, F, Fut>(
    at_hold: F,
) -> (
    Result<()>,
    T,
    mj_controller::session_manager::SessionManagerChannels,
    mj_controller::session_manager::ManagedSessionHandle,
)
where
    F: FnOnce(Arc<RuntimeState>, LifecycleControl) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_HOLD_PATH, LATCH_RELAY_SESSION,
    };
    let (controller, channels, handle) =
        mj_controller::controller::checkpoint::stand_in::close_cut_controller(
            SessionState::Running,
        )
        .await;
    // The daemon's close loads its own controller from the store.
    controller.config.save().unwrap();
    let recovery = channels.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(channels.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        channels.control.clone(),
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    // A phone cancel before admission must not persist Closing or register work.
    let cancelled = LifecycleControl::default();
    assert!(cancelled.request_cancel("phone cancellation before admission"));
    assert!(
        state
            .start_suspend_session_controlled(LATCH_RELAY_SESSION.into(), true, false, cancelled,)
            .await
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert!(!state.owner().lifecycle.contains_key(LATCH_RELAY_SESSION));
    let control = LifecycleControl::default();
    let hold = PathBuf::from(std::env::var_os(LATCH_RELAY_HOLD_PATH).unwrap());
    let closing = mj_core::runtime::spawn({
        let state = state.clone();
        let control = control.clone();
        async move {
            let started = state
                .start_suspend_session_controlled(LATCH_RELAY_SESSION.into(), true, false, control)
                .await?;
            RuntimeState::wait_lifecycle_result(started.result).await?;
            Ok(())
        }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while !hold.with_extension("reached").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the close never reached the held relay command");
    let acted = at_hold(state.clone(), control).await;
    std::fs::write(hold.with_extension("release"), b"go").unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(60), closing)
        .await
        .expect("the close did not finish")
        .expect("close task");
    (outcome, acted, channels, handle)
}

/// Run `test` alone with its own store, behind the scripted relay, holding the
/// first request that submits `held`.
#[cfg(unix)]
fn in_held_close_child(test: &str, held: &str) -> bool {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_HOLD_COMMAND, LATCH_RELAY_HOLD_PATH, LATCH_RELAY_JOURNALS_BEFORE_CLOSE,
        LATE_HARNESS_NOTIFICATION,
    };
    const CHILD: &str = "MJ_TEST_HELD_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    mj_controller::controller::test_support::IsolatedTest::new(
        mj_controller::controller::test_support::test_name(module_path!(), test),
    )
    .env(CHILD, "1")
    .env(LATCH_RELAY_JOURNALS_BEFORE_CLOSE, LATE_HARNESS_NOTIFICATION)
    .env(LATCH_RELAY_HOLD_COMMAND, held)
    .env(LATCH_RELAY_HOLD_PATH, directory.path().join("relay-hold"))
    .isolated_store(directory.path())
    .run();
    false
}

/// The window #1218 hit: the close has installed its verified checkpoint and
/// committed to stopping, and is sealing the relay, but the record does not
/// say `Destroying` yet. A cancel that arrives there is refused, and the
/// target teardown that follows runs to the end.
// Hard-won: #1218: a cancel accepted after the suspend's checkpoint commit failed its teardown
// Hard-won: #1507: phone cancellation must share the admitted close commit control.
#[cfg(unix)]
#[tokio::test]
async fn a_cancel_after_the_close_commits_is_refused_and_the_teardown_finishes() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    if !in_held_close_child(
        "a_cancel_after_the_close_commits_is_refused_and_the_teardown_finishes",
        "close",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let (outcome, cancel, channels, _handle) =
        close_with_an_action_at_a_held_relay_command(|state, control| async move {
            assert!(!control.request_cancel("phone cancellation after commit"));
            state
                .cancel_lifecycle_with_intent(LATCH_RELAY_SESSION)
                .await
        })
        .await;
    channels.shutdown.shutdown().await.unwrap();
    if let Err(error) = &outcome {
        let stored =
            mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
        panic!(
            "the close failed after a late cancel: {error:#}; the session was left {:?} with {:?}",
            stored.state, stored.last_error
        );
    }
    let refusal = cancel.expect_err("a cancel after the commit is refused");
    assert!(
        format!("{refusal:#}").contains("has begun closing and cannot be cancelled"),
        "{refusal:#}"
    );
    let stored =
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(
        stored.state,
        SessionState::Stopped,
        "{:?}",
        stored.last_error
    );
    assert_eq!(stored.target, None);
    assert!(stored.checkpoint.is_some());
}

/// The other side of the #1218 commit: a cancel accepted while the close is
/// still taking its checkpoint wins, so the close stops before its point of
/// no return. No teardown runs, the relay is not sealed, and the session keeps
/// running with its target and checkpoint.
// Hard-won: #1507: a pre-admission phone cancel was ignored by the new Suspend owner.
#[cfg(unix)]
#[tokio::test]
async fn a_cancel_before_the_close_commits_leaves_the_session_running() {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_SESSION, wait_until_the_actor_serves_again,
    };
    if !in_held_close_child(
        "a_cancel_before_the_close_commits_leaves_the_session_running",
        "begin_checkpoint",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    // The local target's teardown removes the whole worker root.
    let worker_root = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap())
        .join("workers")
        .join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(worker_root.join("still-here"), b"no teardown ran").unwrap();
    let (outcome, cancel, channels, handle) =
        close_with_an_action_at_a_held_relay_command(|_state, control| async move {
            ensure!(
                control.request_cancel("phone cancellation before commit"),
                "phone cancellation refused"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await;
    cancel.expect("a cancel before the commit is accepted");
    let error = outcome.expect_err("a cancelled close does not stop the session");
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    assert!(worker_root.join("still-here").exists());
    let stored =
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(
        stored.state,
        SessionState::Running,
        "{:?}",
        stored.last_error
    );
    assert!(stored.target.is_some());
    assert!(stored.checkpoint.is_some());
    wait_until_the_actor_serves_again(&handle).await;
    let mut lease = handle
        .lease_connection(&mj_controller::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let snapshot = lease.connection_mut().sync().await.unwrap();
    lease.release();
    assert_eq!(snapshot.operational.checkpoint_barrier, None);
    assert!(
        !matches!(
            snapshot.operational.execution,
            mj_core::relay::RelayExecutionState::Closing
                | mj_core::relay::RelayExecutionState::Closed
        ),
        "{:?}",
        snapshot.operational.execution
    );
    channels.shutdown.shutdown().await.unwrap();
}

/// After a restart, every in-flight lifecycle state is either owned by
/// something that resumes it or reported to the user. Nothing stays in flight
/// with nobody behind it (#1070).
// Hard-won: 7aea6c5c: an interrupted provision remained stuck and invisible to recovery.
#[test]
fn startup_reports_every_in_flight_state_that_no_operation_owns() {
    let target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "a".repeat(64),
        workspace_storage: Default::default(),
    });
    let mut state = mj_core::state::State::default();
    for (id, session_state, session_target) in [
        ("provisioning", SessionState::Provisioning, None),
        ("closing-orphan", SessionState::Closing, None),
        ("destroying-orphan", SessionState::Destroying, None),
        ("closing-owned", SessionState::Closing, target.clone()),
        ("destroying-owned", SessionState::Destroying, target.clone()),
        ("running", SessionState::Running, target.clone()),
        ("stopped", SessionState::Stopped, None),
        ("moving", SessionState::Provisioning, None),
    ] {
        let mut session = runtime_test_session(id, "workspace", session_state);
        session.target = session_target.map(Box::new);
        state.sessions.insert(id.into(), session);
    }
    let controller = Controller {
        config: mj_core::config::Config::default(),
        state,
    };

    // A durable move intent owns its session, so reconciliation leaves it be.
    let owned = ["moving".to_owned()].into_iter().collect();
    let reconciled = unowned_interrupted_lifecycles(&controller, &owned);
    let ids = reconciled
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["closing-orphan", "destroying-orphan", "provisioning"]);
    let provisioning_cause = &reconciled
        .iter()
        .find(|(id, _)| id == "provisioning")
        .expect("the interrupted provision is reported")
        .1;
    assert!(
        provisioning_cause.contains("provisioning") && provisioning_cause.contains("recover scan"),
        "the cause tells the user what happened and where the container went: {provisioning_cause}"
    );
}

// Hard-won: 7b694803: a cancelled close stranded a Destroying session without a retry path.
#[test]
fn a_stop_on_a_record_left_mid_close_routes_to_recovery() {
    let target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "a".repeat(64),
        workspace_storage: Default::default(),
    });
    for state in [SessionState::Closing, SessionState::Destroying] {
        let mut session = runtime_test_session("session", "workspace", state);
        session.target = target.clone().map(Box::new);
        assert_eq!(
            close_route(Some(&session), false),
            CloseRoute::RecoverInterrupted
        );
        // Without a target there is nothing left for recovery to finish, and
        // nothing to checkpoint either: the close settles instead of waiting
        // on a relay that does not exist.
        session.target = None;
        assert_eq!(
            close_route(Some(&session), false),
            CloseRoute::SettleWithoutCheckpoint
        );
    }

    let mut running = runtime_test_session("session", "workspace", SessionState::Running);
    running.target = target.clone().map(Box::new);
    assert_eq!(close_route(Some(&running), false), CloseRoute::Graceful);
    assert_eq!(close_route(None, false), CloseRoute::Graceful);

    // A session wedged in provisioning has no harness state and no relay, with
    // or without the container it managed to create (#1059).
    let mut provisioning = runtime_test_session("session", "workspace", SessionState::Provisioning);
    assert_eq!(
        close_route(Some(&provisioning), false),
        CloseRoute::SettleWithoutCheckpoint
    );
    provisioning.target = target.clone().map(Box::new);
    assert_eq!(
        close_route(Some(&provisioning), false),
        CloseRoute::SettleWithoutCheckpoint
    );

    // The same session once startup reconciliation has failed it (#1070).
    let failed = runtime_test_session("session", "workspace", SessionState::Error);
    assert_eq!(
        close_route(Some(&failed), false),
        CloseRoute::SettleWithoutCheckpoint
    );
    // A failed session that still names its target keeps a workspace worth
    // checkpointing, so it takes the graceful close.
    let mut failed_with_target = failed.clone();
    failed_with_target.target = target.clone().map(Box::new);
    assert_eq!(
        close_route(Some(&failed_with_target), false),
        CloseRoute::Graceful
    );
    // Every child takes the ownership-aware close, including recovery of an
    // interrupted teardown; none needs its parent's workspace checkpoint.
    assert_eq!(
        close_route(Some(&failed_with_target), true),
        CloseRoute::Subagent
    );
    for state in [
        SessionState::Running,
        SessionState::Disconnected,
        SessionState::Checkpointing,
        SessionState::Closing,
        SessionState::Destroying,
        SessionState::Parked,
        SessionState::Stopped,
    ] {
        let mut child = running.clone();
        child.state = state;
        assert_eq!(close_route(Some(&child), true), CloseRoute::Subagent);
    }

    let mut stopped = runtime_test_session("session", "workspace", SessionState::Stopped);
    assert_eq!(close_route(Some(&stopped), false), CloseRoute::Done);
    stopped.target = target.map(Box::new);
    assert_eq!(
        close_route(Some(&stopped), false),
        CloseRoute::DeferredCleanup
    );
}

/// A process that has exited but has not been reaped still answers
/// `kill(pid, 0)`. The daemon-specific probe may reap its own child;
/// platform process tables do not all expose a reliable Zombie status.
#[cfg(unix)]
#[test]
fn a_process_that_exited_but_was_not_reaped_counts_as_gone() {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn a process that exits immediately");
    let pid = child.id();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let gone = loop {
        if !daemon_process_is_alive(pid) {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        gone,
        "an exited but unreaped process was reported as running"
    );
    // `daemon_process_is_alive` performed the waitpid reap. This explicit
    // wait is harmless (ECHILD on Unix) and documents that no Child is
    // abandoned.
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn attachment_liveness_probe_does_not_reap_children() {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new("true")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn a process that exits immediately");
    let pid = child.id();
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .expect("capture child stdout")
        .read_to_end(&mut output)
        .expect("observe child exit");

    let _ = process_is_alive(pid);
    let status = child.wait().expect("attachment probe left child waitable");
    assert!(status.success());
}

// Hard-won: live AFK test: AFK off reported success without persisting.
#[tokio::test]
async fn operator_away_off_action_persists_and_notifies_only_unblocked_sessions() {
    use mj_client::runtime_feed::RuntimeReplica;
    use mj_core::elicitation::ElicitationRequest;
    use mj_core::mailbox::{MailboxEvent, MailboxEventBody};
    use mj_core::relay::{RelayEvent, RelayObservation, relay_event_digest};
    const CHILD: &str = "MJ_OPERATOR_AWAY_ACTION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "operator_away_off_action_persists_and_notifies_only_unblocked_sessions",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    for id in ["blocked", "unblocked"] {
        mj_controller::database::save_session(&runtime_test_session(
            id,
            "default",
            SessionState::Running,
        ))
        .unwrap();
    }
    let mut state = test_runtime_state();
    Arc::get_mut(&mut state).unwrap().committed =
        Some(mj_controller::database::subscribe_committed_state().unwrap());
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    assert!(matches!(
        handle_action(
            DaemonAction::SetOperatorAway { away: true },
            &metadata,
            &state,
            &cancellation
        )
        .await
        .unwrap(),
        DaemonReply::Done
    ));
    let mut replica = RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    assert!(replica.projection.metadata.operator_presence.away);
    // Worker-target projections deliberately omit global preferences. Prime
    // their cache just as ordinary worker polling does before AFK-off.
    let _worker_inputs = state.worker_controller_projection();
    let request = ElicitationRequest::from_acp_params(
        "permission-1",
        serde_json::json!({
            "mode":"form", "sessionId":"afk-test", "message":"Claude Code requests permission:\ntouch afk-created.txt",
            "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}
        }),
    )
    .unwrap();
    for id in ["blocked", "unblocked"] {
        let mut current = mj_controller::database::load_materialized_session(id)
            .unwrap()
            .unwrap();
        let mut observations = vec![
            RelayObservation::ElicitationRequested {
                request: request.clone(),
            },
            RelayObservation::ElicitationResolved {
                elicitation_id: request.id.clone(),
                action: mj_core::elicitation::OPERATOR_AWAY_ACTION.into(),
                reply: None,
            },
        ];
        if id == "blocked" {
            observations.push(RelayObservation::ElicitationRequested {
                request: request.clone(),
            });
        }
        mj_controller::database::apply_projection_page(id, move |page| {
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
                let mutation =
                    mj_transcript::projection::project_relay_event(&current, &event)?.mutation;
                page.apply(
                    event.ordinal,
                    &event.previous_digest,
                    &event.digest,
                    &mutation,
                )?;
                mj_transcript::projection::apply_committed_projection_event(
                    &mut current,
                    &event,
                    mutation,
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    assert!(matches!(
        handle_action(
            DaemonAction::SetOperatorAway { away: false },
            &metadata,
            &state,
            &cancellation
        )
        .await
        .unwrap(),
        DaemonReply::Done
    ));
    let reloaded =
        mj_controller::database::load_state_from(&mj_controller::database::database_path())
            .unwrap();
    assert!(!reloaded.operator_presence.away);
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    assert_eq!(
        replica.projection.metadata.operator_presence,
        reloaded.operator_presence
    );
    let returned = mj_controller::database::pending_mailbox_events(100)
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let event: MailboxEvent = serde_json::from_str(&entry.event_json).unwrap();
            matches!(event.body, MailboxEventBody::OperatorBack { .. })
                .then_some((entry.target_session_id, event.wake))
        })
        .collect::<Vec<_>>();
    assert_eq!(returned, vec![("unblocked".into(), true)]);
}

#[tokio::test]
async fn client_presence_is_global_and_detach_and_prune_remove_it() {
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        DaemonAction::Attach {
            client_id: "client-a".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("attach presence");
    state.attachments().insert(
        "dead-client".into(),
        Attachment {
            renewed: std::time::Instant::now() - ATTACHMENT_LEASE,
        },
    );

    let DaemonReply::Status(status) =
        handle_action(DaemonAction::Status, &metadata, &state, &cancellation)
            .await
            .expect("status")
    else {
        panic!("status action returned a different reply");
    };
    assert_eq!(status.attached_clients, 1);

    handle_action(
        DaemonAction::Detach {
            client_id: "client-a".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("detach presence");
    assert!(state.attachments().is_empty());
}

#[tokio::test]
async fn subagent_discovery_does_not_hold_handoff_and_stops_when_the_daemon_shuts_down() {
    let action = DaemonAction::SubagentOptions {
        profile: "codex".into(),
        model: Some("gpt-6-luna".into()),
        config: Some(Box::new(mj_core::config::Config::default())),
    };
    assert!(upgrade_request_activity(&action).unwrap().is_none());
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = handle_action(action, &metadata, &state, &cancellation)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "subagent discovery cancelled by daemon shutdown"
    );
}

#[tokio::test]
async fn a_quota_refresh_request_wakes_the_daemons_poller_and_fails_without_one() {
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    handle_action(DaemonAction::RefreshQuota, &metadata, &state, &cancellation)
        .await
        .expect_err("no quota poller is attached, so nothing can probe");

    let (wake, mut woken) = tokio::sync::mpsc::unbounded_channel();
    state.attach_quota_refresh(wake);
    for _ in 0..3 {
        handle_action(DaemonAction::RefreshQuota, &metadata, &state, &cancellation)
            .await
            .expect("refresh quota");
    }
    for _ in 0..3 {
        assert_eq!(
            woken.try_recv().expect("the poller receives every intent"),
            QuotaRefreshIntent::Force
        );
    }
    assert!(woken.try_recv().is_err());
}

#[tokio::test]
async fn a_probe_quota_request_targets_named_profiles_even_disabled_ones() {
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    let mut config = Config::default();
    config.profiles.insert(
        "dsh".into(),
        mj_core::config::HarnessProfile {
            enabled: false,
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Dsh,
                "/home/agent/.dsh",
            )
        },
    );
    state.owner().install_config(config);
    let error = handle_action(
        DaemonAction::ProbeQuotaProfiles {
            profile_ids: vec!["missing".into()],
            forced: false,
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect_err("an unconfigured profile is refused");
    assert!(error.to_string().contains("no profile named"), "{error:#}");

    let (wake, mut woken) = tokio::sync::mpsc::unbounded_channel();
    state.attach_quota_refresh(wake);
    handle_action(
        DaemonAction::ProbeQuotaProfiles {
            profile_ids: vec!["dsh".into()],
            forced: true,
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("probe the disabled profile");
    let Some(QuotaRefreshIntent::ProbeProfiles { requests, forced }) = woken.try_recv().ok() else {
        panic!("the poller receives the probe");
    };
    assert!(forced, "the force flag reaches the poller");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].profile_id, "dsh");
}

#[tokio::test]
async fn published_quota_reaches_an_attached_client_through_the_runtime_feed() {
    if isolate_runtime_feed_test(
        module_path!(),
        "published_quota_reaches_an_attached_client_through_the_runtime_feed",
    ) {
        return;
    }

    use mj_client::quota::QuotaSnapshot;
    let state = test_runtime_state();
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    assert_eq!(
        *replica.projection.metadata.quotas,
        QuotaSnapshot::default()
    );

    let snapshot = QuotaSnapshot {
        probing: ["claude".to_owned()].into(),
        cycles: 4,
        ..Default::default()
    };
    state.publish_quotas(snapshot.clone());
    let frame = state
        .runtime_changes(replica.cursor.clone(), true)
        .await
        .unwrap();
    replica.apply(frame).unwrap();
    assert_eq!(*replica.projection.metadata.quotas, snapshot);

    // Publishing what is already published wakes nobody.
    let before = state.revisions().borrow().to_owned();
    state.publish_quotas(snapshot);
    assert_eq!(*state.revisions().borrow(), before);
}

#[tokio::test]
async fn workspace_deletion_guard_ignores_global_client_presence() {
    let state = test_runtime_state();
    state.attachments().insert(
        "client-a".into(),
        Attachment {
            renewed: std::time::Instant::now(),
        },
    );
    assert!(!state.workspace_has_active_resume("workspace-a"));

    let (_completed, result) = tokio::sync::watch::channel(None);
    state.owner().lifecycle.insert(
        "session-a".into(),
        ActiveLifecycle {
            origin: super::lifecycle::LifecycleOrigin::Person,
            acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
            move_operation_id: None,
            restart_intent: None,
            upgrade_work: Arc::new(Mutex::new(None)),
            phase: LifecyclePhase::Executing,
            operation_id: "resume-operation".into(),
            control: LifecycleControl::default(),
            kind: LifecycleKind::Resume,
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            progress: LifecycleProgress::new(),
            resume_workspace_id: Some("workspace-a".into()),
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            result,
        },
    );
    assert!(state.workspace_has_active_resume("workspace-a"));
}

// Hard-won: e23e4b1e: an unrelated session lifecycle blocked checkpoint export.
#[tokio::test]
async fn checkpoint_lifecycle_guard_is_per_session() {
    let state = test_runtime_state();
    let (_completed, result) = tokio::sync::watch::channel(None);
    state.owner().lifecycle.insert(
        "session-b".into(),
        ActiveLifecycle {
            origin: super::lifecycle::LifecycleOrigin::Person,
            acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
            move_operation_id: None,
            restart_intent: None,
            upgrade_work: Arc::new(Mutex::new(None)),
            phase: LifecyclePhase::Executing,
            operation_id: "resume-operation".into(),
            control: LifecycleControl::default(),
            kind: LifecycleKind::Resume,
            started_at_epoch_seconds: epoch_seconds().saturating_sub(45),
            active_stages: BTreeMap::new(),
            progress: LifecycleProgress::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            result,
        },
    );
    // An unrelated session's operation must not block another session's
    // checkpoint; only the session's own operation does (#1010).
    assert!(state.session_lifecycle_busy("session-a").is_none());
    let busy = state
        .session_lifecycle_busy("session-b")
        .expect("session-b is running its own operation");
    // A refusal has to say which operation is in the way and for how long, so
    // a person can wait for it or cancel it instead of retrying blind (#1010).
    assert_eq!(busy.operation, "resume");
    assert!(
        (45..60).contains(&busy.age_seconds),
        "the age is the operation's, not the clock's: {busy}"
    );
    let rename = ensure_no_active_lifecycle(&state).unwrap_err();
    let message = format!("{rename:#}");
    assert!(
        message
            .contains("cannot rename configuration while session session-b is busy with a resume"),
        "the rename refusal names the operation: {message}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn zombie_only_daemon_group_counts_as_gone() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let mut command = std::process::Command::new("true");
    command.process_group(0).stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("spawn process-group leader that exits immediately");
    let pid = libc::pid_t::try_from(child.id()).expect("child PID fits pid_t");
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .expect("capture child stdout")
        .read_to_end(&mut output)
        .expect("observe child exit");

    assert!(!owned_daemon_group_is_alive(pid));
    child.wait().expect("reap process-group leader");
}

pub(super) fn test_runtime_state() -> Arc<RuntimeState> {
    test_runtime_state_with(Controller {
        config: Config::default(),
        state: mj_core::state::State::default(),
    })
}

pub(super) fn test_runtime_state_with(controller: Controller) -> Arc<RuntimeState> {
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        controller,
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        || {
            Ok(Controller {
                config: Config::default(),
                state: mj_core::state::State::default(),
            })
        },
    ))
}

#[cfg(unix)]
fn test_runtime_state_loading_the_store_with_live_recovery() -> (
    Arc<RuntimeState>,
    Arc<RecoveryGate>,
    crate::worker_upgrade::WorkerUpgradeCoordinator,
) {
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    (state, recovery, upgrades)
}

/// The wait notice names what the daemon is waiting for, so `UpgradeBlockers`
/// must report a live gate label and stop reporting it once released. A
/// lifecycle hold also names its session, kind and current stage, read from the
/// lifecycle registry when the notice renders. The gate is process-wide, so
/// the test runs in a child process where no other test holds a label.
#[tokio::test]
async fn upgrade_blockers_names_the_daemon_owned_work_then_releases_it() {
    const CHILD: &str = "MJ_UPGRADE_BLOCKERS_TEST_CHILD";
    const LABEL: &str = "test-only upgrade blocker";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::upgrade_blockers_names_the_daemon_owned_work_then_releases_it",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        mj_controller::controller::test_support::IsolatedTest::new(name)
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "upgrade-blockers-test")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state();
    let metadata = test_metadata("127.0.0.1:1".parse().unwrap());
    let shutdown = CancellationToken::new();
    let blockers = || async {
        match handle_action(DaemonAction::UpgradeBlockers, &metadata, &state, &shutdown)
            .await
            .unwrap()
        {
            DaemonReply::UpgradeBlockers(labels) => labels,
            other => panic!("expected named blockers, got {other:?}"),
        }
    };
    let held = mj_controller::upgrade::activity(LABEL).unwrap();
    let named = blockers().await;
    assert!(
        named.iter().any(|blocker| blocker
            .strip_prefix(&format!("{LABEL} ("))
            .is_some_and(|age| age.ends_with("s)"))),
        "the blocker is named with its age: {named:?}"
    );
    drop(held);
    let released = blockers().await;
    assert!(
        !released.iter().any(|blocker| blocker.starts_with(LABEL)),
        "released work is still named: {released:?}"
    );

    // Four lifecycles hold the handoff; the notice names the oldest three. A
    // running Create holds nothing: the handoff stops it and the next daemon
    // finishes it from its create intent.
    let create_release = Arc::new(tokio::sync::Notify::new());
    let create = state
        .start_or_join_lifecycle("e5dfbd99-create".into(), LifecycleKind::Create, {
            let create_release = create_release.clone();
            move |_, _, _, _session_admission| async move {
                create_release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let sessions = ["e5dfbd99-a", "e5dfbd99-b", "e5dfbd99-c", "e5dfbd99-d"];
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut operations = Vec::new();
    for session in sessions {
        let release = release.clone();
        operations.push(
            state
                .start_or_join_lifecycle(session.into(), LifecycleKind::Resume, {
                    move |_, _, _, _session_admission| async move {
                        drop(release.acquire().await.unwrap());
                        Ok(DaemonLifecycleResult::Done)
                    }
                })
                .await
                .unwrap(),
        );
    }
    let lifecycle_blocker = || async {
        let named = blockers().await;
        let [blocker] = named.as_slice() else {
            panic!("only the lifecycles block: {named:?}")
        };
        blocker.clone()
    };
    let blocker = lifecycle_blocker().await;
    assert!(
        blocker.starts_with("session lifecycle x4: resume e5dfbd99 (")
            && blocker.matches("resume e5dfbd99 (").count() == 3
            && blocker.ends_with("; and 1 more"),
        "{blocker}"
    );
    assert!(
        !blocker.contains(','),
        "no stage has been reported: {blocker}"
    );
    // A stage the lifecycle registry records appears in the next notice.
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        sessions[0].into(),
    );
    executor.stage_started(ProvisionStage::Cloning);
    executor.stage_started(ProvisionStage::Syncing);
    let blocker = lifecycle_blocker().await;
    assert!(
        blocker.starts_with("session lifecycle x4: resume e5dfbd99 (Clone + Sync, "),
        "{blocker}"
    );
    executor.stage_finished(ProvisionStage::Cloning);
    executor.stage_finished(ProvisionStage::Syncing);
    release.add_permits(sessions.len());
    create_release.notify_one();
    RuntimeState::wait_lifecycle_result(create).await.unwrap();
    for operation in operations {
        RuntimeState::wait_lifecycle_result(operation)
            .await
            .unwrap();
    }
    assert!(
        blockers().await.is_empty(),
        "finished lifecycles are no longer named"
    );
}

#[tokio::test]
async fn automatic_upgrade_drains_a_lifecycle_without_cancelling_it() {
    const CHILD: &str = "MJ_UPGRADE_DRAIN_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::automatic_upgrade_drains_a_lifecycle_without_cancelling_it",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        mj_controller::controller::test_support::IsolatedTest::new(name)
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "upgrade-in-flight-test")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state();
    let metadata = test_metadata("127.0.0.1:1".parse().unwrap());
    let shutdown = CancellationToken::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let operation = state
        .start_or_join_lifecycle("upgrade-provisioning".into(), LifecycleKind::Resume, {
            let release = release.clone();
            move |_, _, cancelled, _session_admission| async move {
                release.notified().await;
                ensure!(
                    !cancelled.load(Ordering::Acquire),
                    "upgrade cancelled provisioning"
                );
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::time::pause();
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(60 * 60 * 24)).await;
        assert!(matches!(
            handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
                .await
                .unwrap(),
            DaemonReply::UpgradePending
        ));
        assert!(!shutdown.is_cancelled());
        assert!(matches!(
            handle_action(DaemonAction::Ping, &metadata, &state, &shutdown)
                .await
                .unwrap(),
            DaemonReply::Pong
        ));
        assert!(
            mj_controller::upgrade::activity("another control request").is_ok(),
            "waiting must leave controls available"
        );
    }
    tokio::time::resume();
    release.notify_one();
    RuntimeState::wait_lifecycle_result(operation)
        .await
        .unwrap();
    let (updates, mut receiver) = mj_controller::session_manager::coalesced_update_channel();
    updates.send(mj_controller::session_manager::SessionManagerUpdate {
        raw_observation: None,
        session_id: "completed-turn".into(),
        view: ManagedSessionView::default(),
    });
    assert!(
        matches!(
            handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
                .await
                .unwrap(),
            DaemonReply::UpgradePending
        ),
        "a queued completion can still start a review or continuation"
    );
    receiver.recv().await.unwrap();
    assert!(
        matches!(
            handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
                .await
                .unwrap(),
            DaemonReply::UpgradePending
        ),
        "the consumer still owns the completion until it asks for another update"
    );
    assert!(receiver.try_recv().is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
                    .await
                    .unwrap(),
                DaemonReply::Done
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(shutdown.is_cancelled());
    assert!(mj_controller::upgrade::activity("late operation").is_err());
}

pub(super) struct TestRemoteManager {
    control: SessionManagerControl,
    requests: RemoteSessionRequests,
    pub(super) publisher: mj_controller::session_manager::RemoteSessionPublisher,
    /// Background tasks the daemon asked a worker to stop, in order.
    pub(super) stopped_tasks: Vec<String>,
    _shutdown: SessionManagerShutdown,
    _targets: tokio::sync::watch::Sender<Vec<RelaySessionTarget>>,
}

impl TestRemoteManager {
    async fn new() -> Self {
        Self::for_session("session-1").await
    }

    /// A manager whose one session is `session_id`.
    pub(super) async fn for_session(session_id: &str) -> Self {
        Self::for_sessions(&[session_id]).await
    }

    /// A manager whose sessions are `session_ids`; the first is the one the
    /// tests wait on.
    pub(super) async fn for_sessions(session_ids: &[&str]) -> Self {
        let session_id = session_ids[0];
        let channels =
            mj_controller::session_manager::spawn_remote_session_manager().expect("remote manager");
        channels.targets.send_replace(
            session_ids
                .iter()
                .map(|id| RelaySessionTarget {
                    session_id: (*id).to_owned(),
                    spec: CommandSpec::new("true", Vec::<String>::new()),
                    worker_recovery: None,
                    project_memory: None,
                })
                .collect(),
        );
        let manager = Self {
            control: channels.control,
            requests: channels.requests,
            publisher: channels.publisher,
            stopped_tasks: Vec::new(),
            _shutdown: channels.shutdown,
            _targets: channels.targets,
        };
        manager
            .publisher
            .publish(session_id.to_owned(), ManagedSessionView::default())
            .await
            .expect("publish test session view");
        manager
            .control
            .wait_for_session(session_id, Duration::from_secs(5))
            .await
            .expect("remote manager creates test session");
        for other in &session_ids[1..] {
            manager
                .publisher
                .publish((*other).to_owned(), ManagedSessionView::default())
                .await
                .expect("publish test session view");
            manager
                .control
                .wait_for_session(other, Duration::from_secs(5))
                .await
                .expect("remote manager creates test session");
        }
        manager
    }

    /// Accepts every command the daemon submits until none arrives for a
    /// moment, and returns them with the session each went to. Run it beside
    /// the call under test, which waits for each acceptance.
    /// The session manager's control, for starting the delegation loop on it.
    pub(super) fn control_for_test(&self) -> SessionManagerControl {
        self.control.clone()
    }

    /// The next command the daemon submits, left unanswered so a test decides
    /// when it is acknowledged: the session, the command id and the reply.
    pub(super) async fn next_submit(
        &mut self,
    ) -> (
        String,
        String,
        tokio::sync::oneshot::Sender<std::result::Result<u64, mj_client::session::SubmitFailure>>,
    ) {
        loop {
            let request = tokio::time::timeout(Duration::from_secs(10), self.requests.recv())
                .await
                .expect("the daemon submits a command")
                .expect("the manager is running");
            if let mj_controller::session_manager::RemoteSessionRequest::Submit {
                session_id,
                command_id,
                reply,
                ..
            } = request
            {
                return (session_id, command_id, reply);
            }
        }
    }

    /// The first submissions fail with `failures`, in order, before the worker
    /// accepts anything.
    pub(super) async fn answer_submissions(
        &mut self,
        failures: Vec<mj_client::session::SubmitFailure>,
    ) -> Vec<(String, String, mj_core::relay::RelayCommand)> {
        let mut failures = failures.into_iter();
        let mut accepted = Vec::new();
        while let Ok(Some(request)) =
            tokio::time::timeout(Duration::from_millis(300), self.requests.recv()).await
        {
            if let mj_controller::session_manager::RemoteSessionRequest::Submit {
                session_id,
                command_id,
                command,
                reply,
            } = request
            {
                let _ = reply.send(match failures.next() {
                    Some(failure) => Err(failure),
                    None => Ok(99),
                });
                accepted.push((session_id, command_id, command));
            } else if let mj_controller::session_manager::RemoteSessionRequest::StopBackgroundTask {
                background_task_id,
                reply,
                ..
            } = request
            {
                self.stopped_tasks.push(background_task_id);
                let _ = reply.send(Ok(()));
            }
        }
        accepted
    }
}

pub(super) fn test_runtime_state_with_manager(manager: &TestRemoteManager) -> Arc<RuntimeState> {
    let recovery = manager.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(manager.control.clone(), &recovery);
    let mut runtime = RuntimeState::new_with_controller_loader(
        manager.control.clone(),
        Controller {
            config: Config::default(),
            state: mj_core::state::State::default(),
        },
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        || {
            Ok(Controller {
                config: Config::default(),
                state: mj_core::state::State::default(),
            })
        },
    );
    if std::env::var_os("MJ_TEST_DURABLE_STARTUP").is_some() {
        runtime.committed = Some(mj_controller::database::subscribe_committed_state().unwrap());
    }
    Arc::new(runtime)
}

pub(super) fn test_metadata(address: SocketAddr) -> DaemonMetadata {
    DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address,
        token: "right-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    }
}

#[tokio::test]
async fn daemon_client_eof_does_not_cancel_a_submitted_mutation() {
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(
            stream,
            test_metadata(address),
            state,
            CancellationToken::new(),
        )
        .await
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: "right-token".into(),
            action: DaemonAction::SubmitSessionCommand {
                inherited_draft: None,
                session_id: "session-1".into(),
                command_id: "command-1".into(),
                command: RelayCommand::ClearQueuedPrompts,
            },
        },
    )
    .await
    .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
        .await
        .expect("submit request did not reach the manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Submit { reply, .. } = request else {
        panic!("expected a submitted mutation")
    };
    drop(stream);
    reply
        .send(Ok(7))
        .expect("daemon incorrectly cancelled a non-reviewer mutation");
    let _ = server.await.expect("daemon task panicked");
}

/// Matching configuration for records produced by runtime_test_session.
pub(super) fn runtime_test_config() -> Config {
    use mj_core::config::{
        HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository, TargetTemplate,
    };
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile::new(HarnessKind::Codex, PathBuf::from("/home/dev/.codex")),
    );
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "main".into(),
            repositories: vec![ProjectRepository {
                id: "main".into(),
                local: Some(PathBuf::from("/workspace/project")),
                destination: PathBuf::from("."),
                ..Default::default()
            }],
        },
    );
    config
}

pub(crate) fn runtime_test_session(
    id: &str,
    workspace_id: &str,
    state: SessionState,
) -> SessionRecord {
    SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        id: id.into(),
        workspace_id: workspace_id.into(),
        title: id.into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        no_project_memory: false,
        no_github_events: false,
        no_github_credentials: false,
        target_template_id: "local".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state,
        archived: false,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-09-03T00:00:00Z".into(),
        updated_at: "2026-09-03T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

pub(super) fn runtime_test_subagent(
    child_session_id: &str,
    parent_session_id: &str,
) -> SubagentRecord {
    SubagentRecord {
        kind: mj_core::subagent::SubagentKind::Worker,
        reviewer_launch: None,
        child_session_id: child_session_id.into(),
        parent_session_id: parent_session_id.into(),
        task_name: "task".into(),
        profile_id: "codex".into(),
        model: None,
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "do the task".into(),
        request_key: format!("request-{child_session_id}"),
        created_at: "2026-09-03T00:00:00Z".into(),
        noticed_turn: None,
        reported_finish: None,
        handback_tool: false,
    }
}

// Hard-won: 25abde37: force-stopping a parent failed on an already-stopped child.
#[test]
fn active_child_session_ids_skips_children_that_already_stopped() {
    let parent = runtime_test_session("parent", "workspace", SessionState::Closing);
    let running_child = runtime_test_session("running-child", "workspace", SessionState::Running);
    let stopped_child = runtime_test_session("stopped-child", "workspace", SessionState::Stopped);
    let unrelated = runtime_test_session("unrelated", "workspace", SessionState::Running);
    let mut state = mj_core::state::State {
        sessions: [
            parent.clone(),
            running_child.clone(),
            stopped_child.clone(),
            unrelated.clone(),
        ]
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect(),
        ..mj_core::state::State::default()
    };
    state.subagents = [
        runtime_test_subagent(&running_child.id, &parent.id),
        runtime_test_subagent(&stopped_child.id, &parent.id),
    ]
    .into_iter()
    .map(|subagent| (subagent.child_session_id.clone(), subagent.into()))
    .collect();

    let children = state.active_child_session_ids(&parent.id);

    assert_eq!(children, vec!["running-child".to_owned()]);
}

#[tokio::test]
async fn daemon_records_definitive_missing_workspaces_without_an_attached_surface() {
    let state = test_runtime_state();
    let session = runtime_test_session("missing", "workspace", SessionState::Running);
    state.owner().edit_sessions(|sessions| {
        sessions.insert(session.id.clone(), session.clone());
    });
    assert!(state.attachments.lock().unwrap().is_empty());
    let mut view = ManagedSessionView {
        attention: Default::default(),
        snapshot: None,
        connected: false,
        error: Some(ViewError::Unreachable(
            "relay proxy disconnected during hello".into(),
        )),
    };
    assert!(state.missing_target_record(&session.id, &view).is_none());
    view.error = Some(ViewError::TargetMissing(
        "working directory /missing is gone".into(),
    ));
    assert_eq!(
        state.missing_target_record(&session.id, &view),
        Some((
            "working directory /missing is gone".into(),
            session.updated_at
        )),
    );
    state.owner().edit_sessions(|sessions| {
        sessions.get_mut(&session.id).unwrap().state = SessionState::Closing;
    });
    assert!(state.missing_target_record(&session.id, &view).is_none());
    state.owner().edit_sessions(|sessions| {
        sessions.get_mut(&session.id).unwrap().state = SessionState::Error;
    });
    assert!(state.missing_target_record(&session.id, &view).is_none());
}

#[cfg(unix)]
// Hard-won: #1444: a committed missing-target write lost its notice when the following reload failed.
#[tokio::test]
async fn missing_target_retry_completes_a_committed_transition() {
    const CHILD: &str = "MJ_TEST_MISSING_TARGET_RETRY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "missing_target_retry_completes_a_committed_transition",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Missing target retry").unwrap();
    let mut session = runtime_test_session("missing", &workspace.id, SessionState::Running);
    session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path: "/isolated/recovery.zip".into(),
        sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        created_at: "2026-09-03T00:00:00Z".into(),
        event_frontier: 1,
    }));
    mj_controller::database::save_session(&session).unwrap();
    static RELOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn fail_first_reload() -> Result<Controller> {
        if RELOADS.fetch_add(1, Ordering::SeqCst) == 0 {
            bail!("simulated controller reload failure after commit");
        }
        Controller::load()
    }
    RELOADS.store(0, Ordering::SeqCst);
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        fail_first_reload,
    ));
    let detail = "the worker working directory is missing";

    state
        .persist_missing_target(&session.id, detail.into(), session.updated_at.clone())
        .await;
    let stored = state.owner().controller().state.sessions[&session.id].clone();
    assert_eq!(stored.state, SessionState::Error);
    assert_eq!(stored.last_error.as_deref(), Some(detail));
    let notices = state.notices.lock().unwrap();
    assert_eq!(notices.len(), 2);
    assert!(notices.front().unwrap().notice.text.contains("retrying"));
    assert_eq!(notices.back().unwrap().notice.text, detail);
}

#[test]
fn late_runtime_revision_publication_cannot_move_cursor_backwards() {
    let revisions = RuntimeRevisions::new(40);
    let subscriber = revisions.subscribe();

    revisions.publish_allocated(42);
    revisions.publish_allocated(41);

    assert_eq!(*subscriber.borrow(), 42);
}

#[tokio::test]
async fn framing_round_trips_payloads_larger_than_a_pipe_buffer() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(&mut stream, &"x".repeat(512 * 1024))
            .await
            .unwrap();
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let received: String = read_frame(&mut stream).await.unwrap();
    sender.await.unwrap();
    assert_eq!(received.len(), 512 * 1024);
}

#[tokio::test]
async fn framing_rejects_an_oversized_frame_before_allocating_it() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_u32((MAX_FRAME_BYTES + 1) as u32)
            .await
            .unwrap();
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    assert!(read_frame::<String>(&mut stream).await.is_err());
    sender.await.unwrap();
}

/// A stopping daemon still holds a snapshot in memory, and used to serve
/// it through the whole epilogue -- from a store it had stopped reading.
/// Management stays answered so a client can still see it and stop it.
#[tokio::test]
async fn daemon_stops_serving_data_actions_once_shutdown_begins() {
    let state = test_runtime_state();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address,
        token: "right-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let server_metadata = metadata.clone();
    let server_cancellation = cancellation.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(stream, server_metadata, state, server_cancellation)
            .await
            .unwrap();
    });
    let mut stream = TcpStream::connect(address).await.unwrap();

    let mut request_id = 0;
    let mut ask = async |stream: &mut TcpStream, action: DaemonAction| {
        request_id += 1;
        write_frame(
            stream,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id,
                token: "right-token".to_owned(),
                action,
            },
        )
        .await
        .unwrap();
        read_frame::<ResponseEnvelope>(stream).await.unwrap().result
    };

    let refused = ask(
        &mut stream,
        DaemonAction::Snapshot {
            workspace_id: "workspace-a".into(),
        },
    )
    .await;
    assert_eq!(
        refused.unwrap_err(),
        "daemon is shutting down; retry to reach a fresh daemon"
    );
    assert!(matches!(
        ask(&mut stream, DaemonAction::Ping).await,
        Ok(DaemonReply::Pong)
    ));
    assert!(matches!(
        ask(&mut stream, DaemonAction::Status).await,
        Ok(DaemonReply::Status(_))
    ));
    assert!(matches!(
        ask(&mut stream, DaemonAction::Stop).await,
        Ok(DaemonReply::Done)
    ));

    drop(stream);
    server.await.unwrap();
}

/// The forced exit is the daemon's own bound, so it has to fire inside the
/// window a client waiting on a stop is prepared to wait.
#[test]
fn shutdown_force_exit_finishes_before_the_stop_deadline() {
    assert!(SHUTDOWN_FORCE_EXIT_TIMEOUT < STOP_TIMEOUT);
}

#[tokio::test]
async fn management_stop_is_bounded_when_the_daemon_never_acknowledges() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address: listener.local_addr().unwrap(),
        token: "test-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    };
    let mut client = ManagementClient::new(DaemonClient::connect(metadata).await.unwrap());
    let (mut peer, _) = listener.accept().await.unwrap();
    let stop = mj_core::runtime::spawn(async move { client.stop().await });
    let request: RequestEnvelope = read_frame(&mut peer).await.unwrap();
    assert!(matches!(request.action, DaemonAction::Stop));
    // Pause only after the real socket exchange reaches the fake daemon.
    tokio::time::pause();
    tokio::time::advance(STOP_TIMEOUT).await;
    let error = stop.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("did not acknowledge"));
}

#[tokio::test]
async fn daemon_rejects_a_request_with_the_wrong_owner_token() {
    let state = test_runtime_state();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address,
        token: "right-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    };
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(stream, server_metadata, state, CancellationToken::new())
            .await
            .unwrap();
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 42,
            token: "wrong-token".into(),
            action: DaemonAction::Ping,
        },
    )
    .await
    .unwrap();
    let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 42);
    assert_eq!(response.result.unwrap_err(), "daemon authentication failed");
    drop(stream);
    server.await.unwrap();
}

#[tokio::test]
async fn the_daemon_rejects_a_client_one_protocol_behind_before_dispatch() {
    let state = test_runtime_state();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address,
        token: "right-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: String::new(),
        placement: DaemonPlacement::Host,
    };
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(stream, server_metadata, state, CancellationToken::new())
            .await
            .unwrap();
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION - 1,
            request_id: 43,
            token: metadata.token,
            action: DaemonAction::PersistReadReceipt {
                client_id: "client-a".into(),
                workspace_id: "workspace-a".into(),
                session_id: "session-a".into(),
                through: 7,
            },
        },
    )
    .await
    .unwrap();
    let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 43);
    assert!(response.result.unwrap_err().contains(&format!(
        "incompatible daemon protocol {}; expected {PROTOCOL_VERSION}",
        PROTOCOL_VERSION - 1
    )));
    drop(stream);
    server.await.unwrap();
}

/// Pins the frozen management subset to its literal protocol-3 wire form.
/// If this test fails, the change breaks cross-version daemon management;
/// version the new behavior some other way.
#[test]
fn management_wire_shapes_stay_frozen_across_protocol_versions() {
    for (action, expected) in [
        (DaemonAction::Ping, serde_json::json!({"action": "ping"})),
        (
            DaemonAction::Status,
            serde_json::json!({"action": "status"}),
        ),
        (DaemonAction::Stop, serde_json::json!({"action": "stop"})),
    ] {
        let request = RequestEnvelope {
            protocol_version: 3,
            request_id: 7,
            token: "tok".into(),
            action,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "protocol_version": 3,
                "request_id": 7,
                "token": "tok",
                "action": expected,
            })
        );
    }

    let response: ResponseEnvelope = serde_json::from_value(serde_json::json!({
        "protocol_version": 3,
        "request_id": 7,
        "result": {"Ok": {"reply": "status", "value": {
            "pid": 4242,
            "started_at": "2026-09-01T07:48:14Z",
            "build_version": "0.3.1",
            "attached_clients": 1,
            "phone_status": {"state": "disabled"},
        }}}
    }))
    .unwrap();
    match response.result.unwrap() {
        DaemonReply::Status(status) => {
            assert_eq!(status.pid, 4242);
            assert_eq!(status.build_version, "0.3.1");
        }
        reply => panic!("unexpected reply {reply:?}"),
    }
}

#[tokio::test]
async fn daemon_serves_management_actions_for_any_protocol_version() {
    // 3 is an older shipped client; 5 stands in for a future one. Both
    // directions must stay manageable.
    for version in [3_u32, 5] {
        let state = test_runtime_state();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address,
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let server_metadata = metadata.clone();
        let server = mj_core::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, CancellationToken::new())
                .await
                .unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version: version,
                request_id: 44,
                token: "right-token".into(),
                action: DaemonAction::Status,
            },
        )
        .await
        .unwrap();
        let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
        assert_eq!(
            response.protocol_version, version,
            "reply must use the caller's dialect"
        );
        assert_eq!(response.request_id, 44);
        match response.result.unwrap() {
            DaemonReply::Status(status) => assert_eq!(status.build_version, "test"),
            reply => panic!("unexpected reply {reply:?}"),
        }
        drop(stream);
        server.await.unwrap();
    }
}

struct ProtocolTranscript {
    protocol_version: u32,
    daemon_build: &'static str,
    /// The exact frames the client must emit, in order (status, then stop).
    expected_requests: [serde_json::Value; 2],
    /// The exact frame bodies that version's daemon replies with, as raw
    /// JSON so the fixture cannot drift along with this build's types.
    responses: [&'static str; 2],
}

/// One transcript per released daemon protocol version, transcribed from
/// the release tags (v0.3.x speaks 3, v0.4.x speaks 4; v0.1/v0.2 predate
/// the daemon). When `PROTOCOL_VERSION` bumps, add the new version here —
/// the frozen management subset means the entry differs only in its
/// version number and build string. Do not edit existing entries: they are
/// what shipped.
fn released_protocol_transcripts() -> Vec<ProtocolTranscript> {
    let requests = |version: u32| {
        [
            serde_json::json!({
                "protocol_version": version,
                "request_id": 1,
                "token": "tok",
                "action": {"action": "status"},
            }),
            serde_json::json!({
                "protocol_version": version,
                "request_id": 2,
                "token": "tok",
                "action": {"action": "stop"},
            }),
        ]
    };
    vec![
        ProtocolTranscript {
            protocol_version: 3,
            daemon_build: "0.3.1",
            expected_requests: requests(3),
            responses: [
                r#"{"protocol_version":3,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"0.3.1","attached_clients":1,"phone_status":{"state":"ready","viewer_url":"https://example.test:1","viewer_code":"690451","qr_login_url":null,"fallback_reason":null}}}}}"#,
                r#"{"protocol_version":3,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 4,
            daemon_build: "0.4.1",
            expected_requests: requests(4),
            responses: [
                r#"{"protocol_version":4,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"0.4.1","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":4,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 11,
            daemon_build: "2.1.0",
            expected_requests: requests(11),
            responses: [
                r#"{"protocol_version":11,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"2.1.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":11,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 14,
            daemon_build: "2.1.4",
            expected_requests: requests(14),
            responses: [
                r#"{"protocol_version":14,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"2.1.4","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":14,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 15,
            daemon_build: "2.2.0",
            expected_requests: requests(15),
            responses: [
                r#"{"protocol_version":15,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"2.2.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":15,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 16,
            daemon_build: "2.4.0",
            expected_requests: requests(16),
            responses: [
                r#"{"protocol_version":16,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"2.4.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":16,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 47,
            daemon_build: "2.24.0",
            expected_requests: requests(47),
            responses: [
                r#"{"protocol_version":47,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-30T16:21:00Z","build_version":"2.24.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":47,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 48,
            daemon_build: "2.24.0",
            expected_requests: requests(48),
            responses: [
                r#"{"protocol_version":48,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-30T16:21:00Z","build_version":"2.24.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":48,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 49,
            daemon_build: "2.24.0",
            expected_requests: requests(49),
            responses: [
                r#"{"protocol_version":49,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-30T16:21:00Z","build_version":"2.24.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":49,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 56,
            daemon_build: "2.34.0",
            expected_requests: requests(56),
            responses: [
                r#"{"protocol_version":56,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-08T00:00:00Z","build_version":"2.34.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":56,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 61,
            daemon_build: "2.55.0",
            expected_requests: requests(61),
            responses: [
                r#"{"protocol_version":61,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-23T00:00:00Z","build_version":"2.55.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":61,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 62,
            daemon_build: "2.55.0",
            expected_requests: requests(62),
            responses: [
                r#"{"protocol_version":62,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-25T00:00:00Z","build_version":"2.55.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":62,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 63,
            daemon_build: "2.58.0",
            expected_requests: requests(63),
            responses: [
                r#"{"protocol_version":63,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-27T00:00:00Z","build_version":"2.58.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":63,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 64,
            daemon_build: "2.59.1",
            expected_requests: requests(64),
            responses: [
                r#"{"protocol_version":64,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-27T00:00:00Z","build_version":"2.59.1","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":64,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
        ProtocolTranscript {
            protocol_version: 65,
            daemon_build: "2.59.1",
            expected_requests: requests(65),
            responses: [
                r#"{"protocol_version":65,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-27T00:00:00Z","build_version":"2.59.1","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":65,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
    ]
}

// Hard-won: 848b4781f: direct CLI removal saved config before failing without the daemon database writer.
#[cfg(unix)]
#[tokio::test]
async fn remove_project_rpc_removes_missing_directory_from_config_and_runtime_feed() {
    const NAME: &str = "remove_project_rpc_removes_missing_directory_from_config_and_runtime_feed";
    const CHILD: &str = "MJ_TEST_REMOVE_MISSING_PROJECT_RPC";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }

    let writer = mj_controller::database::install_isolated_test_writer();
    let data_dir = std::env::var_os("MJ_DATA_DIR").unwrap();
    let missing = PathBuf::from(data_dir).join("missing-project-checkout");
    assert!(!missing.exists());
    let bundle_id = "missingpath";
    let bundle = mj_core::config::ProjectBundle {
        primary_repo: bundle_id.into(),
        repositories: vec![mj_core::config::ProjectRepository {
            id: bundle_id.into(),
            github: None,
            local: Some(missing.clone()),
            destination: PathBuf::from(bundle_id),
            git_ref: None,
            mbx_namespace: None,
        }],
    };
    mj_core::config::Config::update(|config| {
        config.bundles.insert(bundle_id.into(), bundle.clone());
        config.validate()
    })
    .unwrap();
    mj_controller::database::store_catalog_project(
        bundle_id,
        &mj_core::repository::ProjectBundleSnapshot {
            bundle,
            identities: BTreeMap::from([(
                bundle_id.into(),
                mj_core::repository::RepositoryIdentity::Local(missing),
            )]),
            network_sources: BTreeMap::new(),
            plain_directories: BTreeSet::new(),
        },
        true,
    )
    .unwrap();

    drop(writer);
    let error = mj_controller::controller::remove_bundle(bundle_id).unwrap_err();
    assert!(
        format!("{error:#}").contains("database writer is not available"),
        "unexpected removal failure: {error:#}"
    );
    assert!(
        mj_core::config::Config::load()
            .unwrap()
            .bundles
            .contains_key(bundle_id)
    );
    let _writer = mj_controller::database::install_isolated_test_writer();

    let state = test_runtime_state_loading_the_store();
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    assert!(
        replica
            .projection
            .metadata
            .config
            .bundles
            .contains_key(bundle_id)
    );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_state = state.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(
            stream,
            test_metadata(address),
            server_state,
            CancellationToken::new(),
        )
        .await
    });
    let mut client = mj_client::daemon::DaemonClient::connect(test_metadata(address))
        .await
        .unwrap();
    client.remove_project(bundle_id.into()).await.unwrap();
    drop(client);
    server.await.unwrap().unwrap();

    let cursor = replica.cursor.clone();
    replica
        .apply(state.runtime_changes(cursor, false).await.unwrap())
        .unwrap();
    assert!(
        !replica
            .projection
            .metadata
            .config
            .bundles
            .contains_key(bundle_id)
    );
    assert!(
        !mj_core::config::Config::load()
            .unwrap()
            .bundles
            .contains_key(bundle_id)
    );
    assert!(
        mj_controller::database::read_project_catalog()
            .unwrap()
            .projects
            .iter()
            .all(|project| project.bundle_id != bundle_id)
    );
}

#[tokio::test]
async fn management_client_talks_to_every_released_protocol_version() {
    for transcript in released_protocol_transcripts() {
        let protocol_version = transcript.protocol_version;
        let daemon_build = transcript.daemon_build;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (expected, response) in transcript
                .expected_requests
                .iter()
                .zip(transcript.responses)
            {
                let request: serde_json::Value = read_frame(&mut stream).await.unwrap();
                assert_eq!(
                    &request, expected,
                    "protocol {} daemon would reject this frame",
                    transcript.protocol_version
                );
                stream.write_u32(response.len() as u32).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        });
        let metadata = DaemonMetadata {
            protocol_version,
            pid: 4242,
            address,
            token: "tok".into(),
            started_at: "2026-09-01T07:48:14Z".into(),
            build_version: daemon_build.into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let mut client = ManagementClient::new(DaemonClient::connect(metadata).await.unwrap());
        let status = client.status().await.unwrap();
        assert_eq!(status.build_version, daemon_build);
        assert_eq!(status.attached_clients, 1);
        assert_eq!(client.protocol_version(), protocol_version);
        client.stop().await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn equivalent_lifecycle_requests_join_one_daemon_operation() {
    let state = test_runtime_state();
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let (started, ready) = tokio::sync::oneshot::channel();
    let first = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Suspend, {
            let starts = starts.clone();
            let release = release.clone();
            move |_state, _session_id, _cancelled, _session_admission| async move {
                starts.fetch_add(1, Ordering::AcqRel);
                started.send(()).unwrap();
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    ready.await.unwrap();
    let second = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::Suspend,
            |_state, _session_id, _cancelled, _session_admission| async move {
                panic!("joined lifecycle request started duplicate work")
            },
        )
        .await
        .unwrap();
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert!(
        state
            .start_automatic_lifecycle(
                "session-1".into(),
                LifecycleKind::Resume,
                |_state, _session_id, _cancelled, _session_admission| async move {
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await
            .is_err()
    );

    let mut waiting = Box::pin(state.start_or_join_lifecycle(
        "session-1".into(),
        LifecycleKind::Resume,
        |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
    ));
    assert!(futures::poll!(&mut waiting).is_pending());
    assert!(
        state.owner().lifecycle["session-1"]
            .control
            .cancel_cause()
            .is_none()
    );
    let mut waiting_second = Box::pin(state.start_or_join_lifecycle(
        "session-1".into(),
        LifecycleKind::Restart,
        |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
    ));
    assert!(futures::poll!(&mut waiting_second).is_pending());
    let cancel = LifecycleControl::default();
    let mut cancelled_wait = Box::pin(state.start_or_join_lifecycle_controlled(
        "session-1".into(),
        LifecycleKind::ForceStop,
        None,
        None,
        Some(cancel.clone()),
        |_, _, _, _session_admission| async { panic!("cancelled queued work ran") },
    ));
    assert!(futures::poll!(&mut cancelled_wait).is_pending());
    assert!(cancel.request_cancel("test cancels queued request"));
    assert!(cancelled_wait.await.is_err());
    assert_eq!(state.owner().lifecycle_intents["session-1"].len(), 2);
    // The daemon task is independent of either client waiter.
    drop(first);
    release.notify_one();
    assert!(matches!(
        RuntimeState::wait_lifecycle_result(second).await.unwrap(),
        DaemonLifecycleResult::Done
    ));
    RuntimeState::wait_lifecycle_result(waiting.await.unwrap())
        .await
        .unwrap();

    RuntimeState::wait_lifecycle_result(waiting_second.await.unwrap())
        .await
        .unwrap();
    // A person preempts an automatic Move only before its commit. The same
    // registry waits for a committed Move without setting its cancel flag.
    for committed in [false, true] {
        let release = Arc::new(tokio::sync::Notify::new());
        let automatic = state
            .start_automatic_lifecycle("automatic-move".into(), LifecycleKind::Move, {
                let release = release.clone();
                move |_, _, cancelled, _session_admission| async move {
                    tokio::select! {
                        () = release.notified() => {}
                        () = mj_controller::recovery_gate::wait_until_cancelled(&cancelled) => {}
                    }
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        let control = state.owner().lifecycle["automatic-move"].control.clone();
        if committed {
            assert!(control.grant_commit());
        }
        let mut manual = Box::pin(state.start_or_join_lifecycle(
            "automatic-move".into(),
            LifecycleKind::Resume,
            |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
        ));
        assert!(futures::poll!(&mut manual).is_pending());
        assert_eq!(control.cancelled.load(Ordering::Acquire), !committed);
        if committed {
            release.notify_one();
        }
        RuntimeState::wait_lifecycle_result(manual.await.unwrap())
            .await
            .unwrap();
        let outcome = RuntimeState::wait_lifecycle_result(automatic).await;
        if committed {
            outcome.unwrap();
        }
    }
    assert_eq!(starts.load(Ordering::Acquire), 1);
}

// Hard-won: 80934557: excluding graceful Close dropped the manager lease so a fast Stop failed as not managed.
#[tokio::test]
async fn close_keeps_worker_target_available_for_checkpoint_lease() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Suspend, {
            let release = release.clone();
            move |_state, _session_id, _cancelled, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();

    assert!(state.worker_poll_exclusion_session_ids().is_empty());

    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

#[tokio::test]
async fn move_joins_only_matching_selections_and_runs_other_sessions_concurrently() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let first = state
        .start_or_join_lifecycle_with_key(
            "move-join-one".into(),
            LifecycleKind::Move,
            None,
            Some("profile-a/target-a/discard".into()),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    assert!(mj_controller::controller::move_session::move_owns_session(
        "move-join-one"
    ));
    let duplicate = state
        .start_or_join_lifecycle_with_key(
            "move-join-one".into(),
            LifecycleKind::Move,
            None,
            Some("profile-a/target-a/discard".into()),
            |_, _, _, _session_admission| async move {
                panic!("duplicate move launched a second writer")
            },
        )
        .await
        .unwrap();
    let mut differing = Box::pin(state.start_or_join_lifecycle_with_key(
        "move-join-one".into(),
        LifecycleKind::Move,
        None,
        Some("profile-b/target-a/discard".into()),
        |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
    ));
    assert!(futures::poll!(&mut differing).is_pending());
    let unrelated = state
        .start_or_join_lifecycle_with_key(
            "move-join-two".into(),
            LifecycleKind::Move,
            None,
            Some("profile-b/target-b/start".into()),
            |_, _, _, _session_admission| async move { Ok(DaemonLifecycleResult::Done) },
        )
        .await
        .unwrap();
    let unrelated_channel = unrelated.clone();
    tokio::time::timeout(
        Duration::from_secs(2),
        RuntimeState::wait_lifecycle_result(unrelated),
    )
    .await
    .unwrap()
    .unwrap();
    state.remove_completed_lifecycle(&unrelated_channel);
    assert!(!mj_controller::controller::move_session::move_owns_session(
        "move-join-two"
    ));
    drop(first); // An initiating client can disappear without cancelling.
    release.notify_one();
    let channel = duplicate.clone();
    RuntimeState::wait_lifecycle_result(duplicate)
        .await
        .unwrap();
    state.remove_completed_lifecycle(&channel);
    let differing = differing.await.unwrap();
    let channel = differing.clone();
    RuntimeState::wait_lifecycle_result(differing)
        .await
        .unwrap();
    state.remove_completed_lifecycle(&channel);
    assert!(!mj_controller::controller::move_session::move_owns_session(
        "move-join-one"
    ));
}

#[tokio::test]
async fn move_task_panic_reports_failure_and_releases_mutation_hold() {
    let state = test_runtime_state();
    let result = state
        .start_or_join_lifecycle_with_key(
            "move-panics".into(),
            LifecycleKind::Move,
            None,
            Some("destination".into()),
            |_, _, _, _session_admission| async move { panic!("injected move task panic") },
        )
        .await
        .unwrap();
    let channel = result.clone();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        RuntimeState::wait_lifecycle_result(result),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("daemon lifecycle task failed"));
    state.remove_completed_lifecycle(&channel);
    assert!(!mj_controller::controller::move_session::move_owns_session(
        "move-panics"
    ));
}

#[tokio::test]
async fn daemon_lifecycle_reports_balanced_concurrent_stages() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let release = release.clone();
            move |_state, _session_id, _cancelled, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        state.worker_poll_exclusion_session_ids(),
        BTreeSet::from(["session-1".to_owned()])
    );
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    executor.stage_started(ProvisionStage::Cloning);
    executor.stage_started(ProvisionStage::Cloning);
    executor.stage_started(ProvisionStage::Syncing);
    executor.stage_finished(ProvisionStage::Cloning);
    {
        let lifecycle_owner = state.owner();
        let lifecycle = &lifecycle_owner.lifecycle;
        let stages = &lifecycle.get("session-1").unwrap().active_stages;
        assert_eq!(stages.get(&ProvisionStage::Cloning).unwrap().0, 1);
        assert_eq!(stages.get(&ProvisionStage::Syncing).unwrap().0, 1);
    }
    executor.stage_finished(ProvisionStage::Cloning);
    executor.stage_finished(ProvisionStage::Syncing);
    assert!(
        state
            .owner()
            .lifecycle
            .get("session-1")
            .unwrap()
            .active_stages
            .is_empty()
    );
    release.notify_one();
    assert!(matches!(
        RuntimeState::wait_lifecycle_result(result).await.unwrap(),
        DaemonLifecycleResult::Done
    ));
}

#[tokio::test]
async fn deferred_cleanup_is_visible_and_drains_before_shutdown_cancellation() {
    let state = test_runtime_state();
    let saw_early_cancellation = Arc::new(AtomicBool::new(false));
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Cleanup, {
            let saw_early_cancellation = saw_early_cancellation.clone();
            move |state, session_id, cancelled, _session_admission| async move {
                let executor = DaemonStageReportingExecutor::new(
                    mj_controller::targets::ProcessExecutor,
                    state,
                    session_id,
                );
                executor.stage_started(ProvisionStage::RemovingStorage);
                tokio::time::sleep(Duration::from_millis(20)).await;
                saw_early_cancellation.store(cancelled.load(Ordering::Acquire), Ordering::Release);
                executor.stage_finished(ProvisionStage::RemovingStorage);
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;

    let visible = state.active_lifecycles();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].kind, RuntimeLifecycleKind::Cleanup);
    assert_eq!(
        visible[0].active_stages[0].0,
        ProvisionStage::RemovingStorage
    );

    state.cancel_and_wait_lifecycles().await.unwrap();
    assert!(!saw_early_cancellation.load(Ordering::Acquire));
    assert!(matches!(
        RuntimeState::wait_lifecycle_result(result).await.unwrap(),
        DaemonLifecycleResult::Done
    ));
}

#[test]
fn create_cancellation_and_commit_have_one_winner() {
    for _ in 0..32 {
        let control = LifecycleControl::default();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let canceller = {
            let control = control.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                control.request_cancel("test cancellation")
            })
        };
        barrier.wait();
        let committed = control.grant_commit();
        let cancelled = canceller.join().expect("canceller panicked");
        assert_ne!(committed, cancelled);
        assert_eq!(control.cancelled.load(Ordering::Acquire), cancelled);
        assert!(!control.is_cancellable());
        assert!(!control.request_cancel("test cancellation"));
        assert!(!control.grant_commit());
    }
}

#[test]
fn lifecycle_cancel_cause_is_recorded_only_for_accepted_cancel() {
    let control = LifecycleControl::default();
    assert!(control.request_cancel("person cancellation"));
    assert_eq!(control.cancel_cause(), Some("person cancellation"));
    assert!(!control.request_cancel("shutdown"));
    assert_eq!(control.cancel_cause(), Some("person cancellation"));

    let committed = LifecycleControl::default();
    assert!(committed.grant_commit());
    assert!(!committed.request_cancel("late cancellation"));
    assert_eq!(committed.cancel_cause(), None);
}

/// A child launch that an explicit daemon stop cancelled reported only
/// "operation cancelled", which its parent could not tell from a fault on the
/// target. The control records which cancel won, so a person's cancel that
/// came first is never blamed on the shutdown that followed it.
// Hard-won: #1321: a daemon stop cancelled a child launch and the failure did not say so.
#[test]
fn only_a_shutdown_cancel_reports_that_the_daemon_stopped() {
    let operation = |kind: LifecycleKind, control: LifecycleControl| {
        let (_completed, result) = tokio::sync::watch::channel(None);
        ActiveLifecycle {
            origin: super::lifecycle::LifecycleOrigin::Person,
            acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
            move_operation_id: None,
            restart_intent: None,
            upgrade_work: Arc::new(Mutex::new(None)),
            phase: LifecyclePhase::Executing,
            operation_id: "operation".into(),
            control,
            kind,
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            progress: LifecycleProgress::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            result,
        }
    };

    let control = LifecycleControl::default();
    assert!(operation(LifecycleKind::Create, control.clone()).interrupt_for_shutdown());
    assert!(control.cancelled.load(Ordering::Acquire));
    assert!(control.stopped_for_shutdown());

    let control = LifecycleControl::default();
    let mut cancelled = operation(LifecycleKind::Create, control.clone());
    assert!(cancelled.request_cancel("test-session", "person cancellation"));
    assert!(!cancelled.interrupt_for_shutdown());
    assert!(control.cancelled.load(Ordering::Acquire));
    assert!(!control.stopped_for_shutdown());

    // A committed create is not stopped by shutdown, so its commands never
    // fail as cancelled and nothing may blame the daemon.
    let control = LifecycleControl::default();
    assert!(control.grant_commit());
    assert!(!operation(LifecycleKind::Create, control.clone()).interrupt_for_shutdown());
    assert!(!control.stopped_for_shutdown());

    // Any other committed operation is interrupted, and only shutdown does that.
    let control = LifecycleControl::default();
    assert!(control.grant_commit());
    assert!(operation(LifecycleKind::Suspend, control.clone()).interrupt_for_shutdown());
    assert!(control.stopped_for_shutdown());
}

#[tokio::test]
async fn completed_stop_stays_visible_until_cleanup_takes_ownership() {
    let state = test_runtime_state();
    let (complete, result) = tokio::sync::watch::channel(None);
    state.owner().lifecycle.insert(
        "cleanup-gap".into(),
        ActiveLifecycle {
            origin: super::lifecycle::LifecycleOrigin::Person,
            acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
            move_operation_id: None,
            restart_intent: None,
            upgrade_work: Arc::new(Mutex::new(None)),
            phase: LifecyclePhase::Executing,
            operation_id: "closing-operation".into(),
            control: LifecycleControl::default(),
            kind: LifecycleKind::Suspend,
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            progress: LifecycleProgress::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            result: result.clone(),
        },
    );
    state
        .owner()
        .lifecycle
        .get_mut("cleanup-gap")
        .unwrap()
        .phase = LifecyclePhase::Completed(Ok(DaemonLifecycleResult::DeferredCleanup));
    complete.send_replace(Some(Ok(DaemonLifecycleResult::DeferredCleanup)));
    state.remove_completed_lifecycle(&result);
    let view = state.active_lifecycles();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].operation_id, "closing-operation");
    assert!(!view[0].cancellable);
    let release = Arc::new(tokio::sync::Notify::new());
    let cleanup = state
        .start_or_join_lifecycle("cleanup-gap".into(), LifecycleKind::Cleanup, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    state.remove_completed_lifecycle(&result);
    let view = state.active_lifecycles();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].kind, RuntimeLifecycleKind::Cleanup);
    assert_ne!(view[0].operation_id, "closing-operation");
    release.notify_one();
    RuntimeState::wait_lifecycle_result(cleanup).await.unwrap();
    assert!(state.active_lifecycles().is_empty());
}

/// `session_projection` holds the controller lock while it reads the
/// lifecycle view, and that view also needs the controller. A re-lock on
/// the same thread hangs the daemon, so the read must finish promptly.
// Hard-won: 2980a9b9: web feed projection self-deadlocked on lifecycle state and hung the daemon.
#[tokio::test]
async fn session_projection_reads_lifecycles_without_relocking_the_controller() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let running = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Suspend, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let projecting = state.clone();
    std::thread::spawn(move || {
        let (_, lifecycles) = projecting.session_projection();
        let _ = sender.send(lifecycles.len());
    });
    let visible = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("session projection must not deadlock on the controller lock");
    assert_eq!(visible, 1);
    release.notify_one();
    RuntimeState::wait_lifecycle_result(running).await.unwrap();
}

#[tokio::test]
async fn lifecycle_identity_survives_join_but_changes_for_next_operation() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let first = state
        .start_or_join_lifecycle("identity".into(), LifecycleKind::Resume, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let first_id = state.active_lifecycles()[0].operation_id.clone();
    let joined = state
        .start_or_join_lifecycle(
            "identity".into(),
            LifecycleKind::Resume,
            |_, _, _, _session_admission| async { panic!("joined operation must not run twice") },
        )
        .await
        .unwrap();
    assert_eq!(state.active_lifecycles()[0].operation_id, first_id);
    release.notify_one();
    RuntimeState::wait_lifecycle_result(first.clone())
        .await
        .unwrap();
    RuntimeState::wait_lifecycle_result(joined).await.unwrap();
    state.remove_completed_lifecycle(&first);
    let second = state
        .start_or_join_lifecycle("identity".into(), LifecycleKind::Resume, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let second_id = state.active_lifecycles()[0].operation_id.clone();
    assert_ne!(second_id, first_id);
    state.remove_completed_lifecycle(&first);
    assert_eq!(state.active_lifecycles()[0].operation_id, second_id);
    release.notify_one();
    RuntimeState::wait_lifecycle_result(second).await.unwrap();
}

// Hard-won: #1384: an abandoned failed close blocked a fresh request with a different key.
#[cfg(unix)]
#[tokio::test]
async fn completed_close_allows_a_fresh_request_after_its_caller_disconnects() {
    if !in_isolated_parked_test(
        "completed_close_allows_a_fresh_request_after_its_caller_disconnects",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let first = state
        .start_or_join_lifecycle_with_key(
            "child-close".into(),
            LifecycleKind::Suspend,
            None,
            Some("first-request".into()),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    bail!("injected close failure")
                }
            },
        )
        .await
        .unwrap();
    let first_result = first.clone();
    drop(first);
    let fresh_request = || async {
        state
            .start_or_join_lifecycle_with_key(
                "child-close".into(),
                LifecycleKind::Suspend,
                None,
                Some("fresh-request".into()),
                |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
            )
            .await
    };
    let mut waiting = Box::pin(fresh_request());
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(waiting);
    assert!(state.owner().lifecycle_intents.is_empty());
    release.notify_one();
    let error = RuntimeState::wait_lifecycle_result(first_result.clone())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected close failure"));
    // Replay after completion, while the result is retained independently
    // of the original caller, must still return that same result.
    let replay = state
        .start_or_join_lifecycle_with_key(
            "child-close".into(),
            LifecycleKind::Suspend,
            None,
            Some("first-request".into()),
            |_, _, _, _session_admission| async {
                panic!("replaying a request must not run it twice")
            },
        )
        .await
        .unwrap();
    let error = RuntimeState::wait_lifecycle_result(replay)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected close failure"));
    // No caller removes the first operation's retained result.
    let second = fresh_request()
        .await
        .expect("a completed close must not reserve the child");
    state.remove_completed_lifecycle(&first_result);
    RuntimeState::wait_lifecycle_result(second).await.unwrap();
}

#[tokio::test]
async fn close_waits_for_cancelled_or_committed_provisioning_to_release_ownership() {
    for committed in [false, true] {
        let state = test_runtime_state();
        let control = LifecycleControl::default();
        let release = Arc::new(tokio::sync::Notify::new());
        state
            .start_or_join_lifecycle_controlled(
                "close-race".into(),
                LifecycleKind::Create,
                None,
                None,
                Some(control.clone()),
                {
                    let release = release.clone();
                    move |_, _, _, _session_admission| async move {
                        release.notified().await;
                        Ok(DaemonLifecycleResult::Done)
                    }
                },
            )
            .await
            .unwrap();
        state
            .owner()
            .lifecycle
            .get_mut("close-race")
            .unwrap()
            .origin = super::lifecycle::LifecycleOrigin::Automatic;
        if committed {
            assert!(control.grant_commit());
        }
        state.request_close("close-race");
        let waiter = {
            let state = state.clone();
            mj_core::runtime::spawn(async move { state.wait_before_close("close-race").await })
        };
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "cleanup must wait for the owning operation"
        );
        assert_eq!(control.cancelled.load(Ordering::Acquire), !committed);
        assert_eq!(
            state.session_state("close-race"),
            Some(SessionState::Closing)
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!state.owner().lifecycle.contains_key("close-race"));
    }
}

#[tokio::test]
async fn committed_creation_cannot_be_cancelled_by_another_surface() {
    let state = test_runtime_state();
    let control = LifecycleControl::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle_controlled(
            "committed".into(),
            LifecycleKind::Create,
            None,
            None,
            Some(control.clone()),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    assert!(state.active_lifecycles()[0].cancellable);
    assert!(control.grant_commit());
    assert!(!state.active_lifecycles()[0].cancellable);
    assert!(
        state
            .cancel_lifecycle_with_intent("committed")
            .await
            .is_err()
    );
    assert!(state.cancel_lifecycle_if_active("committed").await.is_err());
    assert!(!control.cancelled.load(Ordering::Acquire));
    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

#[tokio::test]
async fn phone_cancel_uses_the_restart_lifecycle_cancellability_guard() {
    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        let mut record = runtime_test_session("restart-guard", "workspace", SessionState::Closing);
        record.target = None;
        sessions.insert(record.id.clone(), record);
    });
    let release = Arc::new(tokio::sync::Notify::new());
    let operation = state
        .start_or_join_lifecycle("restart-guard".into(), LifecycleKind::Restart, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();

    assert!(state.active_lifecycles()[0].cancellable);
    state
        .begin_restart_container_teardown("restart-guard", "restart-owned")
        .unwrap();
    assert!(!state.active_lifecycles()[0].cancellable);
    assert!(
        state
            .cancel_lifecycle_if_active("restart-guard")
            .await
            .is_err()
    );
    assert!(
        !state
            .owner()
            .lifecycle
            .get("restart-guard")
            .unwrap()
            .control
            .cancelled
            .load(Ordering::Acquire)
    );
    state.finish_restart_container_teardown("restart-guard");
    assert!(state.active_lifecycles()[0].cancellable);

    release.notify_one();
    RuntimeState::wait_lifecycle_result(operation)
        .await
        .unwrap();
}

// Hard-won: b0bc92951: a failed Restart kept its settings lock through a later Suspend.
#[tokio::test]
async fn a_waiting_close_preserves_the_intent_of_reserved_restart_teardown() {
    const NAME: &str = "a_waiting_close_preserves_the_intent_of_reserved_restart_teardown";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Closing,
    );
    mj_controller::database::save_session(&session).unwrap();
    let intent = mj_controller::database::begin_session_restart(
        "session-1",
        "restart-owned",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(session.id.clone(), session);
    });
    let release = Arc::new(tokio::sync::Notify::new());
    let operation = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Restart, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    state
        .begin_restart_container_teardown("session-1", &intent.operation_id)
        .unwrap();
    let mut close = mj_core::runtime::spawn({
        let state = state.clone();
        async move { state.wait_before_close("session-1").await }
    });
    // Drive the waiting close while teardown is deliberately held. It must
    // neither cancel that owner nor delete the durable recovery permission.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut close)
            .await
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap(),
        vec![intent]
    );
    assert!(
        !state.owner().lifecycle["session-1"]
            .control
            .cancelled
            .load(Ordering::Acquire)
    );
    release.notify_one();
    RuntimeState::wait_lifecycle_result(operation)
        .await
        .unwrap();
    close.await.unwrap().unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );

    // A failed Restart retains its recovery intent. A later Suspend must
    // release that exact intent even though the old task is already complete.
    let intent = mj_controller::database::begin_session_restart(
        "session-1",
        "restart-failed",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    let failed = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::Restart,
            |_, _, _, _session_admission| async { Err(anyhow!("retained restart failed")) },
        )
        .await
        .unwrap();
    state
        .bind_restart_intent("session-1", &intent.operation_id)
        .unwrap();
    assert!(
        RuntimeState::wait_lifecycle_result(failed.clone())
            .await
            .is_err()
    );
    state.remove_completed_lifecycle(&failed);
    assert!(state.owner().lifecycle.contains_key("session-1"));
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap(),
        vec![intent]
    );
    state.wait_before_close("session-1").await.unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );

    let intent = mj_controller::database::begin_session_restart(
        "session-1",
        "restart-superseded",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    let failed = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::Restart,
            |_, _, _, _session_admission| async { Err(anyhow!("retained restart failed")) },
        )
        .await
        .unwrap();
    state
        .bind_restart_intent("session-1", &intent.operation_id)
        .unwrap();
    assert!(
        RuntimeState::wait_lifecycle_result(failed.clone())
            .await
            .is_err()
    );
    state.remove_completed_lifecycle(&failed);
    let resumed = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::Resume,
            |_, _, _, _session_admission| async {
                assert!(mj_controller::database::load_session_restarts()?.is_empty());
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await
        .unwrap();
    RuntimeState::wait_lifecycle_result(resumed).await.unwrap();
}

// Hard-won: 7b694803: Alt-X stayed available and could cancel teardown after the checkpoint gate.
#[tokio::test]
async fn a_close_removing_the_target_stops_offering_cancellation() {
    let state = test_runtime_state();
    let mut session = runtime_test_session("destroying", "workspace", SessionState::Closing);
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "a".repeat(64),
        workspace_storage: Default::default(),
    }));
    state.owner().edit_sessions(|sessions| {
        sessions.insert(session.id.clone(), session.clone());
    });
    let release = Arc::new(tokio::sync::Notify::new());
    let control = LifecycleControl::default();
    let result = state
        .start_or_join_lifecycle_controlled(
            "destroying".into(),
            LifecycleKind::Suspend,
            None,
            None,
            Some(control.clone()),
            {
                let release = release.clone();
                move |_state, _session_id, _cancelled, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();

    // Before the checkpoint gate the stop is still cancellable.
    assert!(state.active_lifecycles()[0].cancellable);

    // The close commits at its point of no return, before its record says
    // `Destroying`; from then on the control refuses a cancel.
    assert!(control.grant_commit());

    assert!(!state.active_lifecycles()[0].cancellable);
    let error = state
        .cancel_lifecycle_with_intent("destroying")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("has begun closing and cannot be cancelled"),
        "unexpected error: {error:#}"
    );
    assert!(
        !state
            .owner()
            .lifecycle
            .get("destroying")
            .expect("lifecycle entry")
            .control
            .cancelled
            .load(Ordering::Acquire),
        "a refused cancel must not reach the running teardown"
    );

    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

// Hard-won: 6c9e3f638: a queued personal lifecycle could take ownership between destroy preemption and admission.
#[tokio::test]
async fn force_destruction_preempts_a_running_lifecycle_and_waits_for_it() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let (started, ready) = tokio::sync::oneshot::channel();
    let first = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                started.send(()).unwrap();
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    ready.await.unwrap();
    let resumed = Arc::new(AtomicBool::new(false));
    let mut queued_resume =
        Box::pin(
            state.start_or_join_lifecycle("session-1".into(), LifecycleKind::Resume, {
                let resumed = resumed.clone();
                move |_, _, _, _session_admission| async move {
                    resumed.store(true, Ordering::Release);
                    Ok(DaemonLifecycleResult::Done)
                }
            }),
        );
    assert!(futures::poll!(&mut queued_resume).is_pending());
    let destroy_release = Arc::new(tokio::sync::Notify::new());
    let mut destroy =
        Box::pin(
            state.start_or_join_lifecycle("session-1".into(), LifecycleKind::ForceDestroy, {
                let release = destroy_release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            }),
        );
    assert!(futures::poll!(&mut destroy).is_pending());
    assert!(
        state.owner().lifecycle["session-1"]
            .control
            .cancelled
            .load(Ordering::Acquire)
    );
    release.notify_one();
    RuntimeState::wait_lifecycle_result(first).await.unwrap();
    // Even when the older waiter is polled first after completion, destroy
    // owns the next admission. It cannot slip behind a new personal operation.
    assert!(futures::poll!(&mut queued_resume).is_pending());
    let destroyed = destroy.await.unwrap();
    assert_eq!(
        state.owner().lifecycle["session-1"].kind,
        LifecycleKind::ForceDestroy
    );
    assert!(!resumed.load(Ordering::Acquire));
    assert!(futures::poll!(&mut queued_resume).is_pending());
    destroy_release.notify_one();
    RuntimeState::wait_lifecycle_result(destroyed)
        .await
        .unwrap();
    let result = queued_resume.await.unwrap();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
    assert!(resumed.load(Ordering::Acquire));
    assert!(state.owner().lifecycle_intents.is_empty());
}

/// A second destroy of a session that is already being destroyed (a person's
/// `mj destroy` of a sub-agent while its parent's destroy is destroying it)
/// waits for that teardown instead of cancelling it half way (I2-9).
// Hard-won: 4119688f: a second destroy cancelled the first midway and broke child cleanup.
#[tokio::test]
async fn force_destruction_waits_for_a_teardown_already_running_instead_of_cancelling_it() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::ForceDestroy, {
            let release = release.clone();
            move |_state, _session_id, cancelled, _session_admission| async move {
                release.notified().await;
                assert!(
                    !cancelled.load(Ordering::Acquire),
                    "a second destroy must not cancel a teardown in progress"
                );
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;

    let preempt_state = state.clone();
    let waited = mj_core::runtime::spawn(async move {
        preempt_state.preempt_active_lifecycle("session-1").await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waited.is_finished(), "the second destroy waits");
    assert!(
        !state
            .owner()
            .lifecycle
            .get("session-1")
            .expect("lifecycle entry")
            .control
            .cancelled
            .load(Ordering::Acquire)
    );

    release.notify_one();
    waited.await.expect("wait task").expect("teardown finished");
}

/// A teardown that never finishes is still cancelled, after the wait: force
/// destruction remains the escape hatch for a wedged operation.
#[tokio::test(start_paused = true)]
async fn force_destruction_cancels_a_teardown_that_outlives_the_wait() {
    let state = test_runtime_state();
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::ForceDestroy, {
            |_state, _session_id, cancelled, _session_admission| async move {
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;

    state
        .preempt_active_lifecycle("session-1")
        .await
        .expect("the wedged teardown is cancelled and stops");
}

/// A destroy that meets a close past its commit (#1218) treats it as the
/// teardown it is: it waits, and when the close outlives the wait it reports
/// that instead of cancelling a stop that can no longer be undone.
#[tokio::test(start_paused = true)]
async fn force_destruction_waits_for_a_committed_close_and_never_cancels_it() {
    let state = test_runtime_state();
    let control = LifecycleControl::default();
    state
        .start_or_join_lifecycle_controlled(
            "session-1".into(),
            LifecycleKind::Suspend,
            None,
            None,
            Some(control.clone()),
            |_state, _session_id, _cancelled, _session_admission| async move {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await
        .unwrap();
    assert!(control.grant_commit());
    tokio::task::yield_now().await;

    let error = state
        .preempt_active_lifecycle("session-1")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("can no longer be cancelled"),
        "{error:#}"
    );
    assert!(!control.cancelled.load(Ordering::Acquire));
}

// Hard-won: 6c9e3f638: registry admission must preserve the bounded destroy escape hatch.
#[tokio::test(start_paused = true)]
async fn force_destruction_preemption_times_out_without_destroying() {
    let state = test_runtime_state();
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            |_state, _session_id, _cancelled, _session_admission| async move {
                // A lifecycle that ignores cancellation forever.
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;

    let error = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::ForceDestroy,
            |_, _, _, _session_admission| async {
                panic!("destroy must not run beneath a lifecycle that ignores cancellation");
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("did not stop after cancellation"),
        "{error:#}"
    );
    assert!(state.owner().lifecycle_intents.is_empty());
}

/// A running Create whose stages keep starting and finishing is progressing,
/// however long it takes in total.
#[tokio::test(start_paused = true)]
async fn a_create_that_keeps_finishing_stages_is_not_failed_as_stalled() {
    let state = test_runtime_state();
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            |_state, _session_id, _cancelled, _session_admission| async move {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    // Four hours in total, each stage well under the bound.
    for _ in 0..24 {
        executor.stage_started(ProvisionStage::Syncing);
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        check_progress(&state, LIFECYCLE_STALL_BOUND)
            .expect("a Create that is making progress is not stalled");
        executor.stage_finished(ProvisionStage::Syncing);
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        check_progress(&state, LIFECYCLE_STALL_BOUND)
            .expect("a Create that is making progress is not stalled");
    }
}

/// The stall check as the reviewer start runs it while a Create is running.
fn check_progress(state: &Arc<RuntimeState>, bound: Duration) -> Result<()> {
    state.check_create_progress(
        "session-1",
        bound,
        &mut create::PostCreateWait::new("test wait"),
    )
}

/// A running Create with no work of its own, for the stall tests.
async fn start_idle_create(state: &Arc<RuntimeState>) {
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            |_state, _session_id, _cancelled, _session_admission| async move {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
}

/// Queueing for a turn (a container start slot, the worker binary store, an
/// SSH connection to open) is waiting, not a stalled stage: it may last longer
/// than the stall bound, and admission restarts the clock.
#[tokio::test(start_paused = true)]
async fn a_create_queued_for_a_turn_longer_than_the_bound_is_not_stalled() {
    let state = test_runtime_state();
    start_idle_create(&state).await;
    tokio::task::yield_now().await;
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    let bound = LIFECYCLE_STALL_BOUND;
    for queue in [
        ProvisionStage::WaitingForImagePull,
        ProvisionStage::WaitingForStartSlot,
        ProvisionStage::WaitingForWorkerStore,
        ProvisionStage::WaitingForSshConnection,
    ] {
        executor.stage_started(queue.clone());
        tokio::time::sleep(bound * 3).await;
        check_progress(&state, bound)
            .unwrap_or_else(|error| panic!("a Create queued in {queue:?} is not stalled: {error}"));
        executor.stage_finished(queue);
    }
    // Admitted: the clock starts again from admission, and the next stage can
    // stall like any other.
    executor.stage_started(ProvisionStage::Starting);
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    check_progress(&state, bound).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(check_progress(&state, bound).is_err());
}

// Hard-won: #1583: a reviewer Create cancelled a profile stream that was still moving bytes.
#[tokio::test(start_paused = true)]
async fn a_create_watches_each_stream_for_byte_progress() {
    use mj_core::targets::CommandActivity;

    let state = test_runtime_state();
    start_idle_create(&state).await;
    tokio::task::yield_now().await;
    let operation_id = state.owner().lifecycle["session-1"].operation_id.clone();
    let profile = state
        .allocate_lifecycle_stream_id("session-1", &operation_id)
        .unwrap();
    let worker = state
        .allocate_lifecycle_stream_id("session-1", &operation_id)
        .unwrap();
    let report = |id, purpose, activity| {
        state.record_lifecycle_command_activity("session-1", &operation_id, id, purpose, activity);
    };
    let send_profile = "send profile content";
    let upload_worker = "stage replacement Mjolnir worker";
    let bound = LIFECYCLE_STALL_BOUND;
    let half = bound / 2;

    // The stream has to queue for SSH first. Its idle clock begins only after
    // admission, even when queueing takes more than ten minutes.
    report(
        profile,
        send_profile,
        CommandActivity::StageStarted(ProvisionStage::WaitingForSshConnection),
    );
    tokio::time::advance(bound * 2).await;
    check_progress(&state, bound).unwrap();
    report(
        profile,
        send_profile,
        CommandActivity::StageFinished(ProvisionStage::WaitingForSshConnection),
    );
    report(profile, send_profile, CommandActivity::StdinStarted);
    for _ in 0..4 {
        tokio::time::advance(half).await;
        report(
            profile,
            send_profile,
            CommandActivity::StdinBytes(32 * 1024),
        );
        check_progress(&state, bound).expect("the profile is still transferring");
    }

    // A second stream stops moving bytes. Profile bytes and a queue on
    // another lane must not hide the stalled worker upload.
    report(worker, upload_worker, CommandActivity::StdinStarted);
    tokio::time::advance(half).await;
    report(
        worker,
        upload_worker,
        CommandActivity::StdinBytes(16 * 1024),
    );
    report(
        profile,
        send_profile,
        CommandActivity::StdinBytes(32 * 1024),
    );
    tokio::time::advance(half).await;
    report(
        profile,
        send_profile,
        CommandActivity::StdinBytes(32 * 1024),
    );
    check_progress(&state, bound).unwrap();
    report(
        profile,
        send_profile,
        CommandActivity::StageStarted(ProvisionStage::WaitingForWorkerStore),
    );
    tokio::time::advance(half).await;
    let error = check_progress(&state, bound).unwrap_err().to_string();
    assert!(
        error.contains("stage replacement Mjolnir worker moved no bytes for 600 s"),
        "{error}"
    );
    assert!(error.contains("its Create was cancelled"), "{error}");
}

// Hard-won: #1571: removing the transfer deadline left an unattended wake unable to end when its stream stopped.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_stalled_unpark_stream_releases_admission_and_can_retry() {
    use mj_controller::recovery_gate::BackgroundWork;
    use mj_core::targets::CommandActivity;

    if !in_isolated_parked_test("a_stalled_unpark_stream_releases_admission_and_can_retry") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Parked child stall").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent = "0123456789abcdef0123456789abcdef";
    let child = "55555555555555555555555555555555";
    mj_controller::database::save_session(&runtime_test_session(
        parent,
        &workspace.id,
        SessionState::Running,
    ))
    .unwrap();
    parked_child(root.path(), child, parent, &workspace.id);
    let (state, recovery, mut upgrades) = test_runtime_state_loading_the_store_with_live_recovery();

    let (admitted, started) = tokio::sync::oneshot::channel();
    let (cancel_seen, mut cancelled) = tokio::sync::oneshot::channel();
    let (finish, cleanup) = tokio::sync::oneshot::channel();
    let caller = state
        .start_or_join_lifecycle(child.into(), LifecycleKind::Unpark, {
            |_, _, flag, _| async move {
                admitted.send(()).unwrap();
                mj_controller::recovery_gate::wait_until_cancelled(&flag).await;
                cancel_seen.send(()).unwrap();
                cleanup.await.unwrap();
                anyhow::bail!("stream cancelled")
            }
        })
        .await
        .unwrap();
    started.await.unwrap();
    drop(caller);
    let (operation_id, result) = {
        let owner = state.owner();
        let active = &owner.lifecycle[child];
        (active.operation_id.clone(), active.result.clone())
    };
    let stream = state
        .allocate_lifecycle_stream_id(child, &operation_id)
        .unwrap();
    let report = |activity| {
        state.record_lifecycle_command_activity(
            child,
            &operation_id,
            stream,
            "send profile content",
            activity,
        );
    };

    let bound = LIFECYCLE_STALL_BOUND;
    report(CommandActivity::StageStarted(
        ProvisionStage::WaitingForSshConnection,
    ));
    tokio::time::sleep(bound * 2).await;
    assert!(matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    report(CommandActivity::StageFinished(
        ProvisionStage::WaitingForSshConnection,
    ));
    report(CommandActivity::StdinStarted);
    for _ in 0..4 {
        tokio::time::sleep(bound / 2).await;
        report(CommandActivity::StdinBytes(64 * 1024));
    }
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    assert!(matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::timeout(Duration::from_secs(2), cancelled)
        .await
        .expect("the daemon cancels the stream without a waiter")
        .unwrap();

    // A cancelled command still owns admission until process cleanup ends.
    let gate = state.session_gate();
    let refusal = gate
        .try_start(child, BackgroundWork::WorkerReplacement)
        .err()
        .expect("the cancelling Unpark still holds admission");
    assert!(refusal.worker_owned, "{refusal:?}");
    report(CommandActivity::StdinFinished);
    finish.send(()).unwrap();
    let error = RuntimeState::wait_lifecycle_result(result)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("send profile content made no progress for 10 minutes"),
        "{error:#}"
    );
    assert_eq!(state.session_state(child), Some(SessionState::Parked));
    assert!(
        mj_controller::database::load_worker_restart(child)
            .unwrap()
            .is_none()
    );
    drop(
        gate.try_start(child, BackgroundWork::WorkerReplacement)
            .unwrap(),
    );

    // With the original caller gone, its retained outcome cannot consume the
    // next wake. The same child can take a new Unpark immediately.
    let retry = state
        .start_or_join_lifecycle(child.into(), LifecycleKind::Unpark, |_, _, _, _| async {
            Ok(DaemonLifecycleResult::Done)
        })
        .await
        .unwrap();
    RuntimeState::wait_lifecycle_result(retry).await.unwrap();
    recovery.close();
    upgrades.shutdown().await.unwrap();
}

/// Harness preparation reports its own progress and failure, and one step (a
/// download) can be slow: it stays off the stall clock, as it is off the
/// native startup deadline.
// Hard-won: pre-push review of 2ac022d85: a reviewer whose harness preparation spent over ten minutes in one step was cancelled as stalled
#[tokio::test(start_paused = true)]
async fn a_create_preparing_a_harness_longer_than_the_bound_is_not_stalled() {
    let state = test_runtime_state();
    start_idle_create(&state).await;
    tokio::task::yield_now().await;
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    let bound = LIFECYCLE_STALL_BOUND;
    let preparing = ProvisionStage::PreparingHarness {
        harness: mj_core::config::HarnessKind::Codex,
        step: "Downloading".into(),
        since_ms: 0,
    };
    executor.stage_started(preparing.clone());
    tokio::time::sleep(bound * 3).await;
    check_progress(&state, bound).expect("harness preparation is not a stall");
    executor.stage_finished(preparing);
    // The clock starts again when preparation ends.
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    check_progress(&state, bound).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(check_progress(&state, bound).is_err());
}

// Hard-won: 9c928ddc2: a stalled Create only failed the reviewer start and kept running with its lifecycle admission held, which blocks daemon handoff.
#[tokio::test(start_paused = true)]
async fn a_stalled_create_is_cancelled_and_releases_its_admission() {
    let state = test_runtime_state();
    let cancel_seen = Arc::new(AtomicBool::new(false));
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let cancel_seen = cancel_seen.clone();
            |_state, _session_id, cancelled, _session_admission| async move {
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                cancel_seen.store(true, Ordering::Release);
                anyhow::bail!("create cancelled")
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    executor.stage_started(ProvisionStage::Syncing);
    let bound = LIFECYCLE_STALL_BOUND;
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    check_progress(&state, bound).unwrap();
    assert!(!cancel_seen.load(Ordering::Acquire));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let error = check_progress(&state, bound).unwrap_err().to_string();
    assert!(error.contains("its Create was cancelled"), "{error}");
    // Asked again while the cancel is still under way, the message says so
    // and does not claim the Create had committed.
    let again = check_progress(&state, bound).unwrap_err().to_string();
    assert!(again.contains("already being cancelled"), "{again}");
    // The cancel reached the Create's own flag, and the Create ended, which
    // is what drops its admission.
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        RuntimeState::wait_lifecycle_result(result),
    )
    .await
    .expect("the cancelled Create ends");
    assert!(outcome.is_err());
    assert!(cancel_seen.load(Ordering::Acquire));
}

// Hard-won: 9c928ddc2: readiness waits after the Create finished had no bound, so a harness that never reported its capabilities waited for ever.
#[tokio::test(start_paused = true)]
async fn a_readiness_wait_with_no_create_running_fails_at_the_startup_timeout() {
    let state = test_runtime_state();
    let timeout = mj_controller::controller::NATIVE_SESSION_STARTUP_TIMEOUT;
    let mut waiting = create::PostCreateWait::new("report its capabilities");
    // The wait began while a Create ran: that time is the Create's, and its
    // own stall check bounds it.
    let finish = Arc::new(tokio::sync::Notify::new());
    let watch = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let finish = finish.clone();
            |_state, _session_id, _cancelled, _session_admission| async move {
                finish.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::sleep(timeout * 4).await;
    state
        .check_create_progress("session-1", LIFECYCLE_STALL_BOUND * 100, &mut waiting)
        .unwrap();
    // The Create ends; the wait now has the full bound from that moment.
    finish.notify_one();
    RuntimeState::wait_lifecycle_result(watch).await.unwrap();
    state
        .check_create_progress("session-1", LIFECYCLE_STALL_BOUND, &mut waiting)
        .unwrap();
    tokio::time::sleep(timeout - Duration::from_secs(1)).await;
    state
        .check_create_progress("session-1", LIFECYCLE_STALL_BOUND, &mut waiting)
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let error = state
        .check_create_progress("session-1", LIFECYCLE_STALL_BOUND, &mut waiting)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("did not report its capabilities within 300 s"),
        "{error}"
    );
}

// Hard-won: #1438: a 120 s total deadline cancelled slow Creates mid-provision, and removing it outright left a command that hangs on a live connection with nothing to end its start.
#[tokio::test(start_paused = true)]
async fn a_create_stalled_in_one_stage_fails_its_start_and_names_the_stage() {
    let state = test_runtime_state();
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            |_state, _session_id, _cancelled, _session_admission| async move {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    executor.stage_started(ProvisionStage::Syncing);
    let bound = LIFECYCLE_STALL_BOUND;
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    check_progress(&state, bound).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let error = check_progress(&state, bound).unwrap_err().to_string();
    assert!(
        error.contains("stalled in: Sync")
            && error.contains("no stage started or finished for 601 s"),
        "{error}"
    );
}

/// A background image download belongs to the daemon, not to a session, so
/// whichever workspace the person is looking at shows it.
/// A view of `session-1` whose harness is ready for its first prompt.
pub(super) fn ready_startup_view() -> ManagedSessionView {
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let operational = mj_core::relay::RelayOperationalState {
        assessment: None,
        open_request: None,
        notification_stack: Default::default(),
        notifications_read_through: 0,
        person_input_through: 0,
        work_liveness: None,
        undelivered_mailbox_event_keys: Vec::new(),
        assessment_context: None,
        turn_completion: None,
        continuation: Default::default(),
        relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
        native_agents: Vec::new(),
        steering: None,
        cancelling_prompt_id: None,
        clear_context: false,
        clear_context_started_at_ms: None,
        native_agent_count: 0,
        expected_continuation: None,
        inferred_idle_since_ms: None,
        task_settled_at_ms: None,
        background_needed: None,
        goal: Default::default(),
        capacity_retry: None,
        retry_assessment_pending: false,
        authentication_retry_waiting: None,
        jev_rate_limit: None,
        held_login_fingerprint: None,
        authentication_retry_intent: None,
        activity_turn_started_at_ms: None,
        idle_since_ms: None,
        store_id: None,
        session_id: "session-1".into(),
        execution: mj_core::relay::RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        acknowledged_through: 0,
        acknowledged_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        native_session_id: Some("native-1".into()),
        native_continuity_lost: false,
        replaced_unused_native_session_id: None,
        checkpoint_only: false,
        acp_ready: Some(true),
        harness_preparation: None,
        agent_capabilities: None,
        agent_info: None,
        runtime: None,
        steering_supported: None,
        mailbox_delivery: None,
        turn_controls: None,
        config_options: Vec::new().into(),
        modes: None,
        available_commands: Vec::new().into(),
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
        background_work_known: None,
        tools_in_flight: Vec::new(),
        activity: None,
    };
    ManagedSessionView {
        attention: Default::default(),
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            subagent_requests: Default::default(),
            subagent_completions: Default::default(),
            subagent_queue_error: None,
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: operational.into(),
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
        connected: true,
        error: None,
    }
}

/// Put `session-1` into the daemon's in-memory controller as a session that
/// is still coming up, which is when a startup prompt can be queued.
fn insert_starting_session(state: &Arc<RuntimeState>, draft: &str) {
    let mut session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Provisioning,
    );
    draft.clone_into(&mut session.draft_input);
    mj_controller::database::save_session(&session).unwrap();
    assert!(state.session_record(&session.id).is_some());
}

fn startup_prompt_test_store(name: &str) -> Option<mj_controller::database::DatabaseWriterOwner> {
    if std::env::var_os("MJ_TEST_DURABLE_STARTUP").is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), name),
        )
        .env("MJ_TEST_DURABLE_STARTUP", "1")
        .isolated_store(root.path())
        .run();
        return None;
    }
    Some(mj_controller::database::install_isolated_test_writer())
}

// Hard-won: 89a44ed8c: a failed Move remained visible after turns started outside phone actions.
#[tokio::test]
async fn relay_turn_start_retires_only_the_recorded_public_lifecycle_failure() {
    const NAME: &str = "relay_turn_start_retires_only_the_recorded_public_lifecycle_failure";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    use mj_core::relay::{RelayEvent, RelayObservation, relay_event_digest};
    use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};

    fn observe(view: &mut ManagedSessionView, observation: RelayObservation) {
        let snapshot = view.snapshot.as_mut().unwrap();
        let materialized = &mut snapshot.materialized;
        let mut event = RelayEvent {
            format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
            ordinal: materialized.applied_event_ordinal + 1,
            previous_digest: materialized.applied_event_digest.clone(),
            digest: String::new(),
            recorded_at_ms: 100,
            command_id: None,
            observation,
        };
        event.digest = relay_event_digest(&event).unwrap();
        let projected = project_relay_event(materialized, &event).unwrap();
        apply_committed_projection_event(materialized, &event, projected.mutation).unwrap();
        snapshot.window = mj_core::state::ProjectionWindow::of(materialized);
    }

    let mut session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    session.last_error = Some(format!(
        "{} while checkpointing",
        mj_core::state::MOVE_FAILURE_PREFIX
    ));
    mj_controller::database::save_session(&session).unwrap();
    let mut state = test_runtime_state();
    Arc::get_mut(&mut state).unwrap().committed =
        Some(mj_controller::database::subscribe_committed_state().unwrap());
    state
        .owner()
        .install_relay_sessions([session.id.clone()].into());

    // Reattachment to a turn already running when the failure was recorded
    // establishes a frontier. Neither replay nor its completion is recovery.
    let mut view = ready_startup_view();
    observe(
        &mut view,
        RelayObservation::HarnessTurnStarted { started_at_ms: 100 },
    );
    state
        .publish_session(session.id.clone(), view.clone())
        .await
        .unwrap();
    observe(
        &mut view,
        RelayObservation::HarnessTurnSettled {
            autonomous: None,
            origin: None,
            prompt_in_flight: false,
        },
    );
    state
        .publish_session(session.id.clone(), view.clone())
        .await
        .unwrap();
    assert_eq!(
        state.session_record(&session.id).unwrap().last_error,
        session.last_error
    );

    // A mailbox/event wake has no ControllerAction. Coalesce its whole turn
    // to ensure recovery is based on the start, even if it already ended.
    observe(
        &mut view,
        RelayObservation::HarnessTurnStarted { started_at_ms: 200 },
    );
    observe(
        &mut view,
        RelayObservation::HarnessTurnSettled {
            autonomous: None,
            origin: None,
            prompt_in_flight: false,
        },
    );
    state
        .publish_session(session.id.clone(), view.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while state
            .session_record(&session.id)
            .unwrap()
            .last_error
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the new relay turn clears the public failure");
    assert!(
        mj_controller::database::load_session_record(&session.id)
            .unwrap()
            .unwrap()
            .last_error
            .is_none()
    );

    let raw_error = "ssh host private-worker: connection refused";
    session.last_error = Some(raw_error.into());
    session.updated_at = "2026-10-26T12:00:00Z".into();
    mj_controller::database::save_session(&session).unwrap();
    observe(
        &mut view,
        RelayObservation::HarnessTurnStarted { started_at_ms: 300 },
    );
    state
        .publish_session(session.id.clone(), view.clone())
        .await
        .unwrap();
    state
        .clear_recorded_lifecycle_failure(&session.id, raw_error)
        .await;
    assert_eq!(
        mj_controller::database::load_session_record(&session.id)
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some(raw_error)
    );

    // A background clear may run after another lifecycle failure was written.
    session.last_error = Some(format!("{} again", mj_core::state::MOVE_FAILURE_PREFIX));
    mj_controller::database::save_session(&session).unwrap();
    let observed_error = session.last_error.clone().unwrap();
    let mut controller = Controller::load().unwrap();
    // A different failure must survive even when the timestamp is unchanged.
    session.last_error = Some(format!(
        "{} a later operation",
        mj_core::state::MOVE_FAILURE_PREFIX
    ));
    mj_controller::database::save_session(&session).unwrap();
    state
        .clear_recorded_lifecycle_failure(&session.id, &observed_error)
        .await;
    assert_eq!(
        mj_controller::database::load_session_record(&session.id)
            .unwrap()
            .unwrap()
            .last_error,
        session.last_error
    );
    assert!(
        !controller
            .clear_recorded_lifecycle_failure(&session.id, &observed_error)
            .unwrap()
    );

    // Unrelated writes can bump updated_at while a clear waits to run.
    // Exercise both the daemon entry and a controller loaded before the write.
    let observed_error = session.last_error.clone().unwrap();
    let mut controller = Controller::load().unwrap();
    session.updated_at = "2026-10-26T12:00:01Z".into();
    mj_controller::database::save_session(&session).unwrap();
    assert!(
        controller
            .clear_recorded_lifecycle_failure(&session.id, &observed_error)
            .unwrap()
    );
    session.updated_at = "2026-10-26T12:00:02Z".into();
    mj_controller::database::save_session(&session).unwrap();
    state
        .clear_recorded_lifecycle_failure(&session.id, &observed_error)
        .await;
    assert!(
        mj_controller::database::load_session_record(&session.id)
            .unwrap()
            .unwrap()
            .last_error
            .is_none()
    );
}

// Hard-won: #1326: image maintenance recreated an otherwise current session container.
#[tokio::test]
async fn a_current_worker_in_an_older_container_keeps_its_environment() {
    const NAME: &str = "a_current_worker_in_an_older_container_keeps_its_environment";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    session.target_template_id = "docker".into();
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalDocker {
        borrowed_from: None,
        container_id: mj_core::targets::resource_name("session-1").unwrap(),
    }));
    let template = mj_core::config::TargetTemplate::LocalDocker {
        container: serde_json::from_value(serde_json::json!({})).unwrap(),
    };
    let mut runtime = mj_core::state::TargetRuntimeSettings::from(&template);
    let mut applied = mj_core::state::ContainerSettings::requested(&session);
    applied.default_image = Some(Some("ghcr.io/brokkai/mjolnir/agent-dev:2.51.0".into()));
    runtime.container_settings = Some(applied);
    session.target_runtime = Some(runtime.into());
    mj_controller::database::save_session(&session).unwrap();
    // A historical default marker survives persistence and follows the
    // current release image, not the new session default.
    let recorded = mj_controller::database::load_session_record("session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        mj_core::state::ContainerSettings::requested(&recorded).default_image,
        Some(Some(mj_core::config::RELEASE_CONTAINER_IMAGE.to_owned()))
    );
    // Hard-won: review of a13f351: upgrading an agent-dev session onto buildpack-deps lost its hel account, home and control directory.
    assert_eq!(
        mj_core::state::ContainerSettings::upgrade_image(
            &recorded,
            mj_core::config::DEFAULT_SESSION_IMAGE
        ),
        mj_core::config::RELEASE_CONTAINER_IMAGE
    );
    let state = test_runtime_state();
    let controller = Controller {
        config: Config::default(),
        state: mj_controller::database::load_state().unwrap(),
    };
    let outcome = controller
        .upgrade_session_worker(
            &mj_controller::controller::test_support::session_admission("session-1"),
            "session-1",
            &mj_controller::targets::ProcessExecutor,
            &state.session_manager,
            Some(mj_core::worker_build::worker_build_id()),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        mj_controller::controller::WorkerUpgradeOutcome::AlreadyCurrent { .. }
    ));
    assert_eq!(
        mj_controller::database::load_session_record("session-1")
            .unwrap()
            .unwrap(),
        session
    );
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );
}

// Hard-won: #1326: a persisted automatic intent recreated a session after a daemon handoff.
#[tokio::test]
async fn an_unstarted_automatic_recreation_is_cancelled_on_daemon_recovery() {
    const NAME: &str = "an_unstarted_automatic_recreation_is_cancelled_on_daemon_recovery";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    mj_controller::database::begin_session_restart(
        "session-1",
        "legacy-automatic",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    rusqlite::Connection::open(mj_controller::database::database_path())
        .unwrap()
        .execute(
            "UPDATE session_restart_intents SET automatic=1 WHERE session_id='session-1'",
            [],
        )
        .unwrap();
    let state = test_runtime_state();
    let owned = state
        .recover_restarts(mj_controller::database::load_session_restarts().unwrap())
        .await
        .unwrap();
    assert!(owned.is_empty());
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mj_controller::database::load_session_record("session-1")
            .unwrap()
            .unwrap(),
        session
    );
}

// A completed preparation failure must retain a cancellable owner, rather than
// leaving an invisible durable intent that prevents every later lifecycle.
#[tokio::test]
async fn failed_container_preparation_can_be_cancelled_after_result_publication() {
    const NAME: &str = "failed_container_preparation_can_be_cancelled_after_result_publication";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state();
    let result = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::UpgradeContainer,
            |state, id, cancelled, _session_admission| async move {
                let intent = mj_controller::database::begin_container_upgrade(
                    &id,
                    "upgrade-test",
                    cancelled,
                    mj_controller::targets::container_upgrade_test_plan(&id),
                    None,
                )?;
                state.bind_restart_intent(&id, &intent.operation_id)?;
                anyhow::bail!("fixture image preparation failure before stopping the worker")
            },
        )
        .await
        .unwrap();
    assert!(
        RuntimeState::wait_lifecycle_result(result.clone())
            .await
            .is_err()
    );
    state.remove_completed_lifecycle(&result);
    assert!(
        state
            .active_lifecycles()
            .iter()
            .any(|view| view.session_id == "session-1" && view.cancellable)
    );
    state.cancel_container_upgrade("session-1").await.unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );
    assert!(state.active_lifecycles().is_empty());
    let mut saved = mj_controller::database::load_session_record("session-1")
        .unwrap()
        .unwrap();
    assert!(
        saved
            .public_error()
            .unwrap()
            .starts_with(mj_core::state::CONTAINER_UPGRADE_FAILURE_PREFIX)
    );
    saved.last_error = None;
    saved.updated_at = session.updated_at.clone();
    // Recording the failure advanced the database-owned generation.
    saved.failure_generation = session.failure_generation;
    assert_eq!(saved, session);

    // A late upgrade Cancel must not cancel the next ordinary operation.
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Suspend, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    assert!(state.cancel_container_upgrade("session-1").await.is_err());
    assert!(
        !state.owner().lifecycle["session-1"]
            .control
            .cancel_accepted()
    );
    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

#[tokio::test]
async fn a_pending_container_upgrade_cancels_without_recording_a_session_failure() {
    const NAME: &str = "a_pending_container_upgrade_cancels_without_recording_a_session_failure";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let record = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&record).unwrap();
    let state = test_runtime_state();
    let ready = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::UpgradeContainer, {
            let ready = ready.clone();
            move |state, id, cancelled, _session_admission| async move {
                mj_controller::database::begin_container_upgrade(
                    &id,
                    "upgrade-test",
                    cancelled.clone(),
                    mj_controller::targets::container_upgrade_test_plan(&id),
                    None,
                )?;
                state.bind_restart_intent(&id, "upgrade-test")?;
                ready.notify_one();
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                anyhow::bail!("pending container upgrade cancelled; environment retained")
            }
        })
        .await
        .unwrap();
    ready.notified().await;
    state.cancel_container_upgrade("session-1").await.unwrap();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mj_controller::database::load_session_record("session-1")
            .unwrap()
            .unwrap(),
        record
    );
}

// A recovered committed request must keep the same cancellation boundary even
// when inspection fails before the replacement helper can be entered again.
#[tokio::test]
async fn recovered_container_replacement_keeps_commit_authority_after_failure() {
    const NAME: &str = "recovered_container_replacement_keeps_commit_authority_after_failure";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut record = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    record.target_template_id = "podman".into();
    record.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        container_id: mj_core::targets::resource_name("session-1").unwrap(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    mj_controller::database::save_session(&record).unwrap();
    mj_controller::database::begin_container_upgrade(
        "session-1",
        "upgrade-test",
        Arc::new(AtomicBool::new(false)),
        mj_controller::targets::container_upgrade_test_plan("session-1"),
        None,
    )
    .unwrap();
    assert!(
        mj_controller::database::try_commit_container_upgrade("session-1", "upgrade-test")
            .unwrap()
            .is_committed()
    );
    let state = test_runtime_state();
    let result = state
        .admit_lifecycle(
            "session-1".into(),
            LifecycleKind::UpgradeContainer,
            super::lifecycle::LifecycleStart {
                origin: super::lifecycle::LifecycleOrigin::Automatic,
                resume_workspace_id: None,
                request_key: Some("recovered".into()),
                control: None,
                phase: LifecyclePhase::RetiringContainer,
                move_operation_id: None,
                deferrable_admission: None,
                holds_through_drain: false,
            },
            |state, id, _, _session_admission| async move {
                state.bind_restart_intent(&id, "upgrade-test")?;
                anyhow::bail!("fixture engine unavailable before committed recovery")
            },
        )
        .await
        .unwrap();
    assert!(
        RuntimeState::wait_lifecycle_result(result.clone())
            .await
            .is_err()
    );
    state.remove_completed_lifecycle(&result);
    assert!(
        state
            .cancel_lifecycle_with_intent("session-1")
            .await
            .is_err()
    );
    assert!(
        state
            .start_or_join_lifecycle(
                "session-1".into(),
                LifecycleKind::Suspend,
                |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) }
            )
            .await
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap()[0].phase,
        mj_controller::database::SessionRestartPhase::RecreatingContainer
    );
    assert!(state.owner().lifecycle["session-1"].control.is_committed());
    assert!(
        !state.owner().lifecycle["session-1"]
            .control
            .cancel_accepted()
    );

    let missing = ManagedSessionView {
        attention: Default::default(),
        snapshot: None,
        connected: false,
        error: Some(ViewError::TargetMissing(
            "retained source was renamed".into(),
        )),
    };
    assert!(state.missing_target_record("session-1", &missing).is_none());
    assert!(
        mj_controller::database::mark_session_target_missing(
            "session-1",
            "stale missing-target observation",
            "later"
        )
        .unwrap()
        .is_none()
    );
    let saved = mj_controller::database::load_session_record("session-1")
        .unwrap()
        .unwrap();
    assert_eq!(saved.target, record.target);
    assert_eq!(saved.state, SessionState::Running);
    assert!(
        mj_controller::recovery_gate::current_background_session(&saved)
            .unwrap()
            .is_none()
    );
    struct NoEnvironmentMutation;
    impl mj_controller::targets::CommandExecutor for NoEnvironmentMutation {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(
            &self,
            _: &mj_controller::targets::CommandSpec,
        ) -> Result<mj_controller::targets::CommandOutput> {
            anyhow::bail!("background recovery accessed retained upgrade storage")
        }
    }
    let recovery = mj_controller::session_manager::WorkerRecoveryPlan {
        source_target: *saved.target.clone().unwrap(),
        target: None,
        workspace: None,
        liveness_probe: mj_controller::targets::CommandSpec::new(
            "must-not-run",
            std::iter::empty::<String>(),
        )
        .into(),
        exit_record: None,
        binary_refresh: None,
        launch_refresh: None,
        restart: mj_controller::targets::CommandPlan {
            description: "background recovery must stay suppressed".into(),
            commands: Vec::new(),
        }
        .into(),
    };
    assert_eq!(
        mj_controller::session_manager::recover_worker_controlled(
            recovery,
            true,
            Some("session-1"),
            &NoEnvironmentMutation
        )
        .unwrap(),
        mj_controller::session_manager::WorkerRecoveryOutcome::Suppressed
    );
    let controller = Controller {
        config: Config::default(),
        state: mj_controller::database::load_state().unwrap(),
    };
    assert!(matches!(
        controller
            .upgrade_session_worker(
                &mj_controller::controller::test_support::session_admission("session-1"),
                "session-1",
                &NoEnvironmentMutation,
                &state.session_manager,
                Some("older-build")
            )
            .await
            .unwrap(),
        mj_controller::controller::WorkerUpgradeOutcome::Deferred
    ));
}

// Child wake/creation and container replacement must choose under the same
// database writer, even if the parent's previous idle observation is stale.
#[tokio::test]
async fn container_replacement_serializes_with_child_wake_and_registration() {
    const NAME: &str = "container_replacement_serializes_with_child_wake_and_registration";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let parent = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = runtime_test_session(
        "child-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Parked,
    );
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/isolated/child-1".into(),
    }));
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent("child-1", "session-1"),
    )
    .unwrap();
    let plan = mj_controller::targets::container_upgrade_test_plan("session-1");
    mj_controller::database::begin_container_upgrade(
        "session-1",
        "upgrade-test",
        Arc::new(AtomicBool::new(false)),
        plan.clone(),
        None,
    )
    .unwrap();
    let wake = mj_controller::database::WorkerRestartIntent {
        operation_id: "wake-1".into(),
        target: *child.target.clone().unwrap(),
        desired_build: "fixture".into(),
    };
    let owner =
        mj_controller::controller::test_support::worker_test_permit("child-1", "test child wake")
            .unwrap()
            .unwrap();
    owner
        .scope(async {
            mj_controller::database::begin_subagent_unpark("child-1", &wake, None).unwrap();
            assert!(
                !mj_controller::database::try_commit_container_upgrade("session-1", "upgrade-test")
                    .unwrap()
                    .is_committed()
            );
            mj_controller::database::finish_worker_restart("child-1", "wake-1").unwrap();
            child.state = SessionState::Parked;
            mj_controller::database::save_session(&child).unwrap();
            let mut preparing = runtime_test_session(
                "preparing-child",
                mj_core::workspace::DEFAULT_WORKSPACE_ID,
                SessionState::Provisioning,
            );
            mj_controller::database::save_subagent_session(
                &preparing,
                &runtime_test_subagent("preparing-child", "session-1"),
            )
            .unwrap();
            assert!(
                !mj_controller::database::try_commit_container_upgrade("session-1", "upgrade-test")
                    .unwrap()
                    .is_committed(),
                "a registered child owns startup before its target is assigned"
            );
            preparing.state = SessionState::Stopped;
            mj_controller::database::save_session(&preparing).unwrap();
            assert!(
                mj_controller::database::try_commit_container_upgrade("session-1", "upgrade-test")
                    .unwrap()
                    .is_committed()
            );
            assert!(
                mj_controller::database::begin_subagent_unpark("child-1", &wake, None).is_err()
            );
        })
        .await;
    let new_child = runtime_test_session(
        "child-2",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Provisioning,
    );
    assert!(
        mj_controller::database::save_subagent_session(
            &new_child,
            &runtime_test_subagent("child-2", "session-1")
        )
        .is_err()
    );
    assert!(
        mj_controller::database::load_session_record("child-2")
            .unwrap()
            .is_none()
    );
    assert!(
        mj_controller::database::load_worker_restart("child-1")
            .unwrap()
            .is_none()
    );
    assert!(
        mj_controller::database::request_session_restart_cancel("session-1", "upgrade-test")
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap()[0]
            .container_upgrade
            .as_ref(),
        Some(&plan)
    );
}

// Hard-won: 955f4204: Restart admission must serialize settings edits without granting replacement authority.
#[tokio::test]
async fn restart_compares_persisted_applied_settings_and_serializes_mount_edits() {
    const NAME: &str = "restart_compares_persisted_applied_settings_and_serializes_mount_edits";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: mj_core::targets::resource_name("session-1").unwrap(),
        workspace_storage: Default::default(),
    }));
    session.additional_mounts = vec![mj_core::targets::AdditionalMount {
        source: "/srv/reference".into(),
        destination: "/reference".into(),
        access: mj_core::targets::MountAccess::Ro,
    }];
    let template = mj_core::config::TargetTemplate::LocalPodman {
        container: serde_json::from_value(serde_json::json!({})).unwrap(),
    };
    let mut runtime = mj_core::state::TargetRuntimeSettings::from(&template);
    let applied = mj_core::state::ContainerSettings::applied(
        &session,
        mj_core::config::DEFAULT_SESSION_IMAGE,
    );
    runtime.container_settings = Some(applied.clone());
    session.target_runtime = Some(runtime.into());
    mj_controller::database::save_session(&session).unwrap();
    let intent = mj_controller::database::begin_session_restart(
        "session-1",
        "restart-settings",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    assert!(!intent.container_recreation_acknowledged);
    assert!(intent.container_upgrade.is_none());
    let refused =
        mj_controller::database::set_session_container_settings("session-1", None, &[], "later")
            .unwrap_err();
    assert!(mj_core::refusal::Refusal::of(&refused).is_some());

    assert!(
        mj_controller::database::advance_session_restart(
            "session-1",
            "restart-settings",
            mj_controller::database::SessionRestartPhase::RecreatingContainer
        )
        .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap()[0].phase,
        mj_controller::database::SessionRestartPhase::Stopping
    );
    mj_controller::database::cancel_session_restart("session-1").unwrap();
    mj_controller::database::set_session_container_settings("session-1", None, &[], "later")
        .unwrap();
}

#[tokio::test]
async fn restart_intent_checks_cancellation_and_records_its_sealed_checkpoint() {
    const NAME: &str = "restart_intent_checks_cancellation_and_records_its_sealed_checkpoint";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(session.id.clone(), session.clone());
    });

    let cancelled = Arc::new(AtomicBool::new(true));
    let result = mj_core::runtime::spawn_blocking({
        let cancelled = cancelled.clone();
        || {
            mj_controller::database::begin_session_restart(
                "session-1",
                "restart-cancelled",
                cancelled,
                true,
            )
        }
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );

    let control = LifecycleControl::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let operation = state
        .start_or_join_lifecycle_controlled(
            "session-1".into(),
            LifecycleKind::Restart,
            None,
            None,
            Some(control.clone()),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    let cancelled = control.cancelled.clone();
    let intent = mj_core::runtime::spawn_blocking({
        let cancelled = cancelled.clone();
        || {
            mj_controller::database::begin_session_restart(
                "session-1",
                "restart-accepted",
                cancelled,
                true,
            )
        }
    })
    .await
    .unwrap()
    .unwrap();
    state
        .bind_restart_intent("session-1", &intent.operation_id)
        .unwrap();
    assert!(!intent.checkpoint_started);
    mj_core::runtime::spawn_blocking(|| {
        mj_controller::database::mark_session_restart_checkpoint_started(
            "session-1",
            "restart-accepted",
        )?;
        mj_controller::database::record_session_restart_checkpoint(
            "session-1",
            "restart-accepted",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
    })
    .await
    .unwrap()
    .unwrap();

    mj_core::runtime::spawn_blocking(|| {
        let mut session = mj_controller::database::load_state()?
            .sessions
            .remove("session-1")
            .unwrap();
        session.state = SessionState::Closing;
        session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
            archive_path: "/isolated/checkpoint.hel.zip".into(),
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            created_at: "now".into(),
            event_frontier: 1,
        }));
        mj_controller::database::save_checkpointed_session(&session)?;
        mj_controller::database::advance_session_restart(
            "session-1",
            "restart-accepted",
            mj_controller::database::SessionRestartPhase::RestoringInPlace,
        )
    })
    .await
    .unwrap()
    .unwrap();
    let saved = mj_controller::database::load_session_restarts().unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].checkpoint_started);
    assert_eq!(
        saved[0].phase,
        mj_controller::database::SessionRestartPhase::RestoringInPlace
    );
    assert_eq!(
        saved[0].checkpoint_sha256.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    state
        .cancel_lifecycle_with_intent("session-1")
        .await
        .unwrap();
    assert!(cancelled.load(Ordering::Acquire));
    assert_eq!(
        mj_controller::database::load_session_restarts().unwrap()[0].phase,
        mj_controller::database::SessionRestartPhase::CancelRequested
    );
    let refused = mj_controller::database::set_session_container_settings(
        "session-1",
        None,
        &[],
        "while cancelling",
    )
    .unwrap_err();
    assert!(mj_core::refusal::Refusal::of(&refused).is_some());
    release.notify_one();
    RuntimeState::wait_lifecycle_result(operation)
        .await
        .unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );

    let session = runtime_test_session(
        "session-2",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(session.id.clone(), session.clone());
    });
    let control = LifecycleControl::default();
    let release = Arc::new(tokio::sync::Notify::new());
    state
        .start_or_join_lifecycle_controlled(
            "session-2".into(),
            LifecycleKind::Restart,
            None,
            None,
            Some(control.clone()),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    mj_core::runtime::spawn_blocking({
        let cancelled = control.cancelled.clone();
        || {
            mj_controller::database::begin_session_restart(
                "session-2",
                "restart-preempted",
                cancelled,
                true,
            )
        }
    })
    .await
    .unwrap()
    .unwrap();
    state.owner().lifecycle.get_mut("session-2").unwrap().origin =
        super::lifecycle::LifecycleOrigin::Automatic;
    state
        .bind_restart_intent("session-2", "restart-preempted")
        .unwrap();
    let closing_state = state.clone();
    let close =
        mj_core::runtime::spawn(async move { closing_state.wait_before_close("session-2").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !control.cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Suspend requested cancellation before waiting for Restart");
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .iter()
            .any(|intent| intent.session_id == "session-2")
    );
    release.notify_one();
    close.await.unwrap().unwrap();
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );

    let mut session = runtime_test_session(
        "cancelled-recovery",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Closing,
    );
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: mj_core::targets::resource_name(&session.id).unwrap(),
        workspace_storage: Default::default(),
    }));
    mj_controller::database::save_session(&session).unwrap();
    let intent = mj_controller::database::begin_session_restart(
        &session.id,
        "cancel-survives-crash",
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .unwrap();
    mj_controller::database::request_session_restart_cancel(&session.id, &intent.operation_id)
        .unwrap();
    assert!(
        state
            .recover_restarts(mj_controller::database::load_session_restarts().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        mj_controller::database::load_session_restarts()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[&session.id].state,
        SessionState::Error
    );
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[&session.id].target,
        session.target
    );
}

/// The smallest archived snapshot a hand-off step can carry.
fn empty_archive_snapshot() -> mj_core::archive::CanonicalSessionSnapshot {
    mj_core::archive::CanonicalSessionSnapshot {
        command_ledger: None,
        assessment_state: None,
        notification_stack: None,
        event_frontier: 0,
        event_frontier_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        session: mj_core::archive::CanonicalSessionState {
            execution: mj_core::archive::CanonicalExecutionState::Idle,
            last_activity_at_ms: None,
            session_title: None,
            configuration: Default::default(),
        },
        transcript: Vec::new(),
        queued_prompts: Vec::new(),
        mailbox: None,
    }
}

fn queued_prompt_action(text: &str) -> DaemonAction {
    DaemonAction::QueueStartupPrompt {
        move_operation_id: None,
        session_id: "session-1".into(),
        command_id: format!("startup-{text}"),
        text: text.into(),
        prompt: vec![ContentBlock::Text(TextContent::new(text))],
        encoded_draft: text.into(),
        inherited_draft: None,
    }
}

fn queued_move_prompt_action(text: &str, origin: &str) -> DaemonAction {
    let mut action = queued_prompt_action(text);
    if let DaemonAction::QueueStartupPrompt {
        move_operation_id, ..
    } = &mut action
    {
        *move_operation_id = Some(origin.to_owned());
    }
    action
}

fn submitted_prompt_text(command: &RelayCommand) -> String {
    let RelayCommand::Prompt { prompt } = command else {
        panic!("expected a prompt command, got {command:?}")
    };
    prompt
        .iter()
        .map(|block| match block {
            agent_client_protocol::v1::schema::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        })
        .collect()
}

fn session_draft_input(state: &RuntimeState) -> String {
    state
        .session_record("session-1")
        .expect("the test session is still in memory")
        .draft_input
}

/// A failure whose delivery is unknown, as the session actor reports a lost
/// reply or a broken connection. A bare string is a definite refusal instead.
fn unconfirmed(message: &str) -> mj_client::session::SubmitFailure {
    mj_client::session::SubmitFailure {
        message: message.to_owned(),
        unconfirmed: true,
        refused: false,
    }
}

/// A failure the worker itself returned after reading the command.
fn refused(message: &str) -> mj_client::session::SubmitFailure {
    mj_client::session::SubmitFailure {
        message: message.to_owned(),
        unconfirmed: false,
        refused: true,
    }
}

fn notice_texts(state: &RuntimeState) -> Vec<String> {
    state
        .notices
        .lock()
        .unwrap()
        .iter()
        .map(|entry| entry.notice.text.clone())
        .collect()
}

async fn next_submit(
    manager: &mut TestRemoteManager,
) -> (
    String,
    tokio::sync::oneshot::Sender<std::result::Result<u64, mj_client::session::SubmitFailure>>,
) {
    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .expect("a queued prompt did not reach the session manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Submit { command, reply, .. } = request else {
        panic!("expected a submitted prompt")
    };
    (submitted_prompt_text(&command), reply)
}

/// The text is held until the harness reports itself ready, and queued
/// prompts are then delivered in the order they were typed.
#[tokio::test]
async fn queued_startup_prompts_are_delivered_in_order_once_the_harness_is_ready() {
    let Some(_writer) = startup_prompt_test_store(
        "queued_startup_prompts_are_delivered_in_order_once_the_harness_is_ready",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        queued_prompt_action("first prompt"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the first startup prompt");

    // Nothing is submitted while the published view says the session is not
    // connected yet.
    assert!(
        tokio::time::timeout(Duration::from_millis(600), manager.requests.recv())
            .await
            .is_err(),
        "a prompt was submitted before the harness was ready"
    );

    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "first prompt");
    reply
        .send(Ok(1))
        .expect("the drain awaits the submit reply");

    handle_action(
        queued_prompt_action("second prompt"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the second startup prompt");
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "second prompt");
    reply
        .send(Ok(2))
        .expect("the drain awaits the submit reply");
    assert!(notice_texts(&state).is_empty(), "delivery posted a notice");
}

/// Peer messages accepted while a session is starting stay behind its first
/// prompt, then land in the mailbox in send order. They are not a second
/// prompt (#1553).
#[tokio::test]
async fn session_messages_while_starting_follow_the_first_prompt() {
    let Some(_writer) =
        startup_prompt_test_store("session_messages_while_starting_follow_the_first_prompt")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    state
        .queue_api_followup(
            "session-1",
            "api-startup-order".into(),
            crate::server::api::StartFollowup {
                prompt: Some("open the brief".into()),
                ..Default::default()
            },
        )
        .await
        .expect("queue the startup prompt");
    for (request_id, text, at) in [
        ("first-note", "first note", 1),
        ("second-note", "second note", 2),
    ] {
        state
            .queue_startup_session_message(
                "session-1",
                crate::daemon::startup_followup::QueuedSessionMessage {
                    request_id: request_id.into(),
                    sender_id: "sender-1".into(),
                    sender_title: "sender".into(),
                    text: text.into(),
                    created_at_ms: at,
                    waited_on_group: Some("api-startup-order".into()),
                },
            )
            .await
            .expect("queue the session message");
    }
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "open the brief");
    reply
        .send(Ok(1))
        .expect("the drain awaits the submit reply");

    let pending = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pending = mj_controller::database::pending_mailbox_events(10).unwrap();
            if pending.len() >= 2 {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the session messages were not queued behind the prompt");
    let texts = pending
        .iter()
        .map(|row| {
            assert_eq!(row.target_session_id, "session-1");
            assert!(!row.unpark);
            let event: mj_core::mailbox::MailboxEvent =
                serde_json::from_str(&row.event_json).unwrap();
            assert_eq!(event.source, "session_message");
            assert!(event.wake);
            let mj_core::mailbox::MailboxEventBody::SessionMessage { text, .. } = event.body else {
                panic!("expected a session message");
            };
            text
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["first note", "second note"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "a held session message was submitted as its own prompt"
    );
}

/// A message held for a session that will never finish starting is returned
/// to the sender, and the startup row stays so a retry is not delivered later.
#[tokio::test]
async fn a_session_message_while_starting_is_returned_when_startup_fails() {
    let Some(_writer) = startup_prompt_test_store(
        "a_session_message_while_starting_is_returned_when_startup_fails",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let sender = runtime_test_session(
        "sender-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&sender).unwrap();
    let command_id =
        mj_core::continuation::GeneratedPrompt::SessionMessageTurn.command_id("bring-logs");
    state
        .queue_startup_session_message(
            "session-1",
            crate::daemon::startup_followup::QueuedSessionMessage {
                request_id: "bring-logs".into(),
                sender_id: "sender-1".into(),
                sender_title: "sender".into(),
                text: "bring the logs".into(),
                created_at_ms: 5,
                waited_on_group: None,
            },
        )
        .await
        .expect("queue the session message");

    let mut session = state
        .session_record("session-1")
        .expect("the starting session is published");
    session.state = SessionState::Error;
    session.last_error = Some("provisioning failed".into());
    mj_controller::database::save_session(&session).unwrap();

    // The notice is committed before the failed phase. Both are the receipt.
    let (notice, stored_error) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pending = mj_controller::database::pending_mailbox_events(10).unwrap();
            let notice = pending.iter().find_map(|row| {
                (row.target_session_id == "sender-1").then(|| row.event_json.clone())
            });
            let failed = mj_controller::database::load_startup_delivery(&command_id)
                .unwrap()
                .filter(|row| row.phase == "failed")
                .and_then(|row| row.error);
            if let (Some(notice), Some(error)) = (notice, failed) {
                return (notice, error);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the sender was not told the message was dropped");
    let event: mj_core::mailbox::MailboxEvent = serde_json::from_str(&notice).unwrap();
    let mj_core::mailbox::MailboxEventBody::PlainText { text } = event.body else {
        panic!("the undelivered notice is plain text, not a reply from the target");
    };
    assert!(text.contains("was not delivered"), "{text}");
    assert!(text.contains("bring the logs"), "{text}");
    assert!(text.contains("provisioning failed"), "{text}");
    assert!(
        stored_error.contains("was not delivered"),
        "a retry has to read that the message was not delivered: {stored_error}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "the undelivered message was submitted"
    );
}

/// A restored session's hand-off is queued ahead of any prompt, so the prompt
/// is never submitted before the context it is meant to read. Here the
/// hand-off cannot be installed, so the prompt behind it remains durable
/// without going out before its context.
#[tokio::test]
async fn a_queued_hand_off_runs_before_the_prompt_behind_it() {
    let Some(_writer) =
        startup_prompt_test_store("a_queued_hand_off_runs_before_the_prompt_behind_it")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let cancellation = CancellationToken::new();

    state
        .queue_startup_step(
            "session-1",
            StartupStep::InstallHandoff(Box::new(empty_archive_snapshot())),
            &cancellation,
        )
        .await
        .expect("queue the hand-off");
    state
        .queue_startup_step(
            "session-1",
            StartupStep::Prompt {
                move_operation_id: None,
                prompt: None,
                encoded_draft: None,
                text: "after the hand-off".into(),
                inherited_draft: None,
            },
            &cancellation,
        )
        .await
        .expect("queue the prompt behind the hand-off");
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit { command, reply, .. } = request else {
        panic!("expected handoff command")
    };
    assert!(matches!(command, RelayCommand::InstallPromptContext { .. }));
    reply.send(Err("handoff rejected".into())).unwrap();
    wait_for_startup_pause(&state).await;
    assert_eq!(session_draft_input(&state), "");
    assert_eq!(
        mj_controller::database::load_startup_deliveries()
            .unwrap()
            .len(),
        2
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), manager.requests.recv())
            .await
            .is_err()
    );
}

/// A session that stops while its prompt waits ends the wait at once, and the
/// queue remains durable and its independent draft is preserved.
#[tokio::test]
async fn a_session_that_stops_preserves_its_durable_startup_queue() {
    let Some(_writer) =
        startup_prompt_test_store("a_session_that_stops_preserves_its_durable_startup_queue")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "half-written note");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        queued_prompt_action("undelivered prompt"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the startup prompt");
    state.owner().edit_sessions(|sessions| {
        sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
    });

    wait_for_startup_pause(&state).await;
    assert_eq!(session_draft_input(&state), "half-written note");
    let saved = mj_controller::database::load_startup_deliveries().unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].step_json.contains("undelivered prompt"));
}

/// An uncertain submit keeps its stable command and the following prompts
/// without creating a second copy in the composer.
#[tokio::test]
async fn a_refused_submit_preserves_order_without_creating_duplicate_drafts() {
    let Some(_writer) = startup_prompt_test_store(
        "a_refused_submit_preserves_order_without_creating_duplicate_drafts",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    for text in ["refused prompt", "prompt behind it"] {
        handle_action(queued_prompt_action(text), &metadata, &state, &cancellation)
            .await
            .expect("queue a startup prompt");
    }
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "refused prompt");
    reply
        .send(Err(unconfirmed("the reply to the prompt was lost")))
        .expect("the drain awaits the submit reply");

    wait_for_startup_pause(&state).await;
    assert_eq!(session_draft_input(&state), "");
    let saved = mj_controller::database::load_startup_deliveries().unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0].phase, "delivering");
    assert_eq!(saved[1].phase, "pending");
}

/// Shutdown cancels a drain that is still waiting for a harness and returns
/// well inside its own bound, so quitting the daemon stays responsive.
#[tokio::test]
async fn cancelling_startup_prompts_returns_while_a_drain_is_waiting() {
    let Some(_writer) =
        startup_prompt_test_store("cancelling_startup_prompts_returns_while_a_drain_is_waiting")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        queued_prompt_action("never delivered"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the startup prompt");

    let started = tokio::time::Instant::now();
    state
        .cancel_and_join_startup_prompts()
        .await
        .expect("the waiting drain stopped cleanly");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancelling the startup queues took {:?}",
        started.elapsed()
    );
    assert_eq!(session_draft_input(&state), "");
    assert_eq!(
        mj_controller::database::load_startup_deliveries()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn startup_restart_reuses_the_durable_command_identity_after_a_lost_reply() {
    let Some(_writer) = startup_prompt_test_store(
        "startup_restart_reuses_the_durable_command_identity_after_a_lost_reply",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let cancellation = CancellationToken::new();
    state
        .queue_startup_step(
            "session-1",
            StartupStep::Prompt {
                move_operation_id: None,
                prompt: None,
                encoded_draft: None,
                text: "once".into(),
                inherited_draft: None,
            },
            &cancellation,
        )
        .await
        .unwrap();
    let saved_id = mj_controller::database::load_startup_deliveries().unwrap()[0]
        .command_id
        .clone();
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit {
        command_id, reply, ..
    } = request
    else {
        panic!("expected prompt")
    };
    assert_eq!(command_id, saved_id);
    reply
        .send(Err(unconfirmed("connection lost after acceptance")))
        .unwrap();
    wait_for_startup_pause(&state).await;
    state.cancel_and_join_startup_prompts().await.unwrap();
    assert_eq!(session_draft_input(&state), "");
    let recovered = test_runtime_state_with_manager(&manager);
    recovered
        .restore_startup_deliveries(&CancellationToken::new())
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit {
        command_id, reply, ..
    } = request
    else {
        panic!("expected recovered prompt")
    };
    assert_eq!(
        command_id, saved_id,
        "restart assigned a new execution identity"
    );
    reply.send(Ok(7)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// A prompt the worker refuses outright was never accepted, so it goes back
/// to the composer with a notice and the queue moves on, instead of retrying
/// something that will be refused again.
#[tokio::test]
async fn a_definitely_refused_startup_prompt_returns_to_the_draft() {
    let Some(_writer) =
        startup_prompt_test_store("a_definitely_refused_startup_prompt_returns_to_the_draft")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    for text in ["refused prompt", "prompt behind it"] {
        handle_action(queued_prompt_action(text), &metadata, &state, &cancellation)
            .await
            .expect("queue a startup prompt");
    }
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "refused prompt");
    reply
        .send(Err(refused("the harness refused the prompt")))
        .expect("the drain awaits the submit reply");
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(
        text, "prompt behind it",
        "the queue continues past a refusal"
    );
    reply.send(Ok(9)).unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        while !mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("settled steps outside an API group leave the table");
    assert_eq!(session_draft_input(&state), "refused prompt");
    assert!(
        notice_texts(&state)
            .iter()
            .any(|notice| notice.contains("returned to your draft")),
        "the refusal is explained: {:?}",
        notice_texts(&state)
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn startup_batch_identity_rejects_changed_payload_and_keeps_order() {
    let Some(_writer) =
        startup_prompt_test_store("startup_batch_identity_rejects_changed_payload_and_keeps_order")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let cancellation = CancellationToken::new();
    let steps = || {
        vec![
            (
                "request-first".into(),
                StartupStep::Prompt {
                    move_operation_id: None,
                    prompt: None,
                    encoded_draft: None,
                    text: "first".into(),
                    inherited_draft: None,
                },
            ),
            (
                "request-second".into(),
                StartupStep::Prompt {
                    move_operation_id: None,
                    prompt: None,
                    encoded_draft: None,
                    text: "second".into(),
                    inherited_draft: None,
                },
            ),
        ]
    };
    state
        .queue_startup_steps_with_ids("session-1", steps(), Some("request".into()), &cancellation)
        .await
        .unwrap();
    state
        .queue_startup_steps_with_ids("session-1", steps(), Some("request".into()), &cancellation)
        .await
        .unwrap();
    let mut changed = steps();
    changed.pop();
    assert!(
        state
            .queue_startup_steps_with_ids(
                "session-1",
                changed,
                Some("request".into()),
                &cancellation
            )
            .await
            .is_err()
    );
    let rows = mj_controller::database::load_startup_deliveries().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].command_id, "request-first");
    assert_eq!(rows[1].command_id, "request-second");
    state.cancel_and_join_startup_prompts().await.unwrap();
    mj_controller::database::cancel_startup_groups("session-1").unwrap();
    assert!(
        mj_controller::database::set_startup_delivery_phase("request-first", "delivering", None)
            .is_err()
    );
    assert!(
        mj_controller::database::set_startup_delivery_accepted("request-first", Some(9)).is_err()
    );
    assert_eq!(
        mj_controller::database::next_startup_delivery("session-1")
            .unwrap()
            .unwrap()
            .phase,
        "cancelling"
    );
}

/// Withdrawing a queued prompt before delivery cancels it: nothing is
/// submitted when the harness becomes ready, the drain still delivers what is
/// queued afterwards, and no durable row is left to send it later.
// Hard-won: 888bd1d6: the composer removed a prompt preview while the durable queue still sent it.
#[tokio::test]
async fn withdrawing_a_queued_startup_prompt_before_delivery_cancels_it() {
    let Some(_writer) =
        startup_prompt_test_store("withdrawing_a_queued_startup_prompt_before_delivery_cancels_it")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        queued_prompt_action("edit me"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the prompt");
    let reply = handle_action(
        DaemonAction::WithdrawStartupPrompt {
            session_id: "session-1".into(),
            command_id: "startup-edit me".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("withdraw the queued prompt");
    assert!(
        matches!(reply, DaemonReply::PromptWithdrawn(true)),
        "{reply:?}"
    );

    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");
    assert!(
        tokio::time::timeout(Duration::from_millis(800), manager.requests.recv())
            .await
            .is_err(),
        "a withdrawn prompt was delivered"
    );

    handle_action(
        queued_prompt_action("the edited prompt"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the edited prompt");
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "the edited prompt");
    reply
        .send(Ok(1))
        .expect("the drain awaits the submit reply");
    assert!(
        notice_texts(&state).is_empty(),
        "withdrawal posted a notice"
    );
}

/// Once the daemon has started delivering, the prompt is on its way and a
/// withdrawal is refused with the fact that it was sent.
#[tokio::test]
async fn withdrawing_a_startup_prompt_after_delivery_started_is_refused() {
    let Some(_writer) =
        startup_prompt_test_store("withdrawing_a_startup_prompt_after_delivery_started_is_refused")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        queued_prompt_action("already going"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("queue the prompt");
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "already going");

    let answer = handle_action(
        DaemonAction::WithdrawStartupPrompt {
            session_id: "session-1".into(),
            command_id: "startup-already going".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("the daemon answers a late withdrawal");
    assert!(
        matches!(answer, DaemonReply::PromptWithdrawn(false)),
        "a prompt being delivered was withdrawn: {answer:?}"
    );
    reply
        .send(Ok(1))
        .expect("the drain awaits the submit reply");
}

async fn wait_for_startup_pause(state: &Arc<RuntimeState>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !notice_texts(state)
            .iter()
            .any(|notice| notice.contains("remains saved"))
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("startup delivery did not report its saved work");
}

#[cfg(unix)]
const DISCARD_LOST_TEST_CHILD: &str = "MJ_TEST_DISCARD_LOST_CHILD";

/// A session whose managed target is gone has no checkpoint to resume and no
/// action but Destroy, so the daemon removes its record rather than leaving a
/// tombstone for the person to find in the resume dialog. Nothing on that
/// automatic path may discard work: a checkout with uncommitted changes stays,
/// with its branch, and the notice says where it is.
#[cfg(unix)]
#[tokio::test]
async fn discarding_a_lost_session_removes_its_record_but_keeps_a_dirty_checkout() {
    const TEST: &str = "discarding_a_lost_session_removes_its_record_but_keeps_a_dirty_checkout";
    if std::env::var_os(DISCARD_LOST_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(DISCARD_LOST_TEST_CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let repository = mj_controller::controller::test_support::committed_repository();
    let clean_id = "0123456789abcdef0123456789abcdef";
    let dirty_id = "fedcba9876543210fedcba9876543210";
    let mut lost = Vec::new();
    for session_id in [clean_id, dirty_id] {
        let mut session = mj_controller::controller::test_support::managed_worktree_session(
            repository.path(),
            session_id,
        );
        session.state = SessionState::Lost;
        session.checkpoint = None;
        session.last_error = Some("working directory is gone".into());
        mj_controller::database::save_session(&session).unwrap();
        lost.push(session);
    }
    let dirty_checkout = repository.path().join(".mj/worktrees").join(dirty_id);
    std::fs::write(dirty_checkout.join("unsaved.txt"), "work in progress\n").unwrap();

    let state = test_runtime_state_loading_the_store();
    for session in &lost {
        state.discard_lost_session(session.id.clone()).await;
    }

    let stored = mj_controller::database::load_state().unwrap().sessions;
    assert!(
        !stored.contains_key(clean_id) && !stored.contains_key(dirty_id),
        "a lost session must not leave a record behind: {:?}",
        stored.keys().collect::<Vec<_>>()
    );
    assert!(
        !repository
            .path()
            .join(".mj/worktrees")
            .join(clean_id)
            .exists(),
        "a clean checkout holds nothing, so it goes with the session"
    );
    assert!(
        dirty_checkout.join("unsaved.txt").exists(),
        "uncommitted work must survive a discard nobody confirmed"
    );
    for session_id in [clean_id, dirty_id] {
        assert_eq!(
            mj_controller::controller::test_support::test_git(
                repository.path(),
                &["branch", "--list", &format!("mj/{session_id}")]
            )
            .trim()
            .trim_start_matches(['*', '+', ' ']),
            format!("mj/{session_id}"),
            "a discard keeps every session branch"
        );
    }
    let notices = notice_texts(&state);
    for session_id in [clean_id, dirty_id] {
        let expected = format!(
            "Session {} was lost because its managed target no longer exists; its record was removed.",
            mj_core::state::short_id(session_id)
        );
        assert_eq!(
            notices
                .iter()
                .filter(|notice| notice.starts_with(&expected))
                .count(),
            1,
            "each discard reports itself exactly once: {notices:?}"
        );
    }
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains(&dirty_checkout.display().to_string())),
        "the retained checkout must be named where the person will see it: {notices:?}"
    );
}

/// R2-11: a session that was created and destroyed between two SessionWiki
/// passes was never findable, because destroy deleted its record and stored
/// conversation and no pass had indexed them. `mj sessions --session <id>`
/// then said the id "names neither a Mjolnir session nor an indexed one".
/// Destroy now indexes the session first.
#[cfg(unix)]
// Hard-won: ab7d7f49: SessionWiki missed a prompted session created and destroyed between syncs.
#[tokio::test]
async fn a_destroyed_session_is_still_found_by_its_id() {
    const DESTROY_INDEX_TEST_CHILD: &str = "MJ_TEST_DESTROY_INDEX_CHILD";
    const TEST: &str = "a_destroyed_session_is_still_found_by_its_id";
    if std::env::var_os(DESTROY_INDEX_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        // The sync walks every harness store under the home directory, so the
        // child gets an empty one of its own, and an index of its own.
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(DESTROY_INDEX_TEST_CHILD, "1")
        .isolated_store(directory.path())
        .env(
            mj_core::config::SESSION_INDEX_ENV,
            directory.path().join("sessionwiki"),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let session_id = "0123456789abcdef0123456789abcdef";
    // Create: a running session.
    let mut session = runtime_test_session(
        session_id,
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    session.title = "destroyed before any sync".into();
    mj_controller::database::save_session(&session).unwrap();
    // Prompt: its stored conversation holds a prompt and a reply.
    let mut conversation = mj_core::state::MaterializedSession::empty(session_id);
    conversation.applied_event_ordinal = 2;
    conversation.applied_event_digest = format!("{:064x}", 2);
    conversation.last_activity_at_ms = Some(1_700_000_000_002);
    for (position, body) in [
        mj_core::transcript::TranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": "find me after destroy"})],
        },
        mj_core::transcript::TranscriptBody::Agent {
            chunks: vec![serde_json::json!({"content": {"type": "text", "text": "noted"}})],
            streaming: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let position = position as u64 + 1;
        let streamed = matches!(body, mj_core::transcript::TranscriptBody::Agent { .. });
        conversation
            .transcript
            .push(Arc::new(mj_core::transcript::TranscriptItem {
                stable_id: format!("item-{position}"),
                position,
                latest_content_event_ordinal: streamed.then_some(position),
                created_at_ms: 1_700_000_000_000 + position as i64,
                last_changed_at_ms: 1_700_000_000_000 + position as i64,
                body,
            }));
    }
    mj_controller::database::save_materialized_session(&conversation).unwrap();
    // The index exists but has not seen this session: the state between two
    // sync passes.
    sessionwiki::index::open().unwrap();

    let state = test_runtime_state_loading_the_store();
    state
        .force_destroy_session(session_id.to_owned(), BranchDisposition::Keep)
        .await
        .unwrap();

    assert!(
        !mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(session_id),
        "destroy removes the record"
    );
    let found = state
        .wiki_session(session_id.to_owned())
        .await
        .unwrap()
        .expect("the destroyed session is found by its id");
    assert_eq!(found.status, mj_client::daemon::WikiSessionStatus::Archived);
    assert_eq!(found.mjolnir_session_id.as_deref(), Some(session_id));
    assert!(
        !found.nothing_to_restore,
        "the indexed conversation keeps the prompt a restore starts from"
    );

    // A second session has no surviving machine configuration. Destruction
    // must still remove its record and publish a safe warning independently.
    session.id = "abcdef0123456789abcdef0123456789".into();
    session.target_template_id = "removed-machine".into();
    session.target = Some(Box::new(mj_core::state::TargetLocator::SshPodman {
        host: "removed.invalid".into(),
        container_id: "gone-container".into(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    mj_controller::database::save_session(&session).unwrap();
    state.push_notice(&session.id, "private diagnostic /secret/path");
    // Hard-won: 27c51a34c: unavailable target configuration blocked Destroy too.
    state
        .destroy_stopped_session(session.id.clone(), BranchDisposition::Keep)
        .await
        .unwrap();
    assert!(
        !mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(&session.id)
    );
    let notices = state.public_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].session_id, session.id);
    assert!(notices[0].text.contains("Resources may remain"));
    let wire = serde_json::to_string(&crate::server::ViewerSnapshot {
        notices,
        ..Default::default()
    })
    .unwrap();
    assert!(wire.contains("Resources may remain"));
    assert!(!wire.contains("private diagnostic"));
    assert!(!wire.contains("removed.invalid"));

    let repository = mj_controller::controller::test_support::committed_repository();
    let parent_id = "22222222222222222222222222222222";
    let child_id = "33333333333333333333333333333333";
    let mut parent = mj_controller::controller::test_support::managed_clone_session(
        repository.path(),
        parent_id,
    );
    let checkout = parent
        .managed_worktree
        .as_ref()
        .unwrap()
        .worktree_root
        .clone();
    let worker_root = repository.path().join(parent_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(worker_root.join("keep"), "parent worker evidence").unwrap();
    parent.state = SessionState::Running;
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    session.id = child_id.into();
    mj_controller::database::save_subagent_session(
        &session,
        &runtime_test_subagent(child_id, parent_id),
    )
    .unwrap();
    let environment = mj_controller::database::session_environment(parent_id)
        .unwrap()
        .unwrap();
    // Hard-won: ae628e069: a direct child Destroy lost its writer before a later parent Destroy.
    state
        .force_destroy_session(child_id.into(), BranchDisposition::Keep)
        .await
        .unwrap();
    assert!(
        !mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(child_id)
    );
    assert!(
        mj_controller::database::environment_workers(&environment.id)
            .unwrap()
            .iter()
            .any(|worker| worker.session_id == child_id)
    );
    // Rebuild daemon-owned state from durable records, with no cascade memory.
    let state = test_runtime_state_loading_the_store();
    let connection = rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_test_destroy BEFORE DELETE ON sessions WHEN OLD.session_id = '22222222222222222222222222222222' BEGIN SELECT RAISE(ABORT, 'test root deletion unavailable'); END;").unwrap();
    // Hard-won: 5b9868d0d: forgetting an unconfirmed child lost the evidence needed to protect its parent's shared checkout.
    assert!(
        state
            .force_destroy_session(parent_id.into(), BranchDisposition::Keep)
            .await
            .is_err()
    );
    let retained = mj_controller::database::load_state().unwrap();
    assert!(retained.sessions.contains_key(parent_id));
    assert!(!retained.sessions.contains_key(child_id));
    assert!(
        mj_controller::database::environment_workers(&environment.id)
            .unwrap()
            .iter()
            .any(|worker| worker.session_id == child_id)
    );
    assert!(checkout.exists());
    assert!(worker_root.join("keep").exists());
    connection
        .execute_batch("DROP TRIGGER reject_test_destroy;")
        .unwrap();
    state
        .force_destroy_session(parent_id.into(), BranchDisposition::Keep)
        .await
        .unwrap();
    let removed = mj_controller::database::load_state().unwrap();
    assert!(!removed.sessions.contains_key(parent_id));
    assert!(!removed.sessions.contains_key(child_id));
    assert!(
        checkout.exists(),
        "a surviving child may still write the shared checkout"
    );
    assert!(
        worker_root.join("keep").exists(),
        "parent cleanup cannot remove resources still used by its child"
    );
    let root_notices: Vec<_> = state
        .public_notices()
        .into_iter()
        .filter(|notice| notice.session_id == parent_id)
        .collect();
    assert!(
        root_notices
            .iter()
            .any(|notice| notice.text.contains("Resources may remain")),
        "the advertised parent notice path reports retained physical resources"
    );
    // Hard-won: 770a4e74c: descendant warnings were invisible under the requested parent ID.
    let requested = "77777777777777777777777777777777";
    let descendant = "88888888888888888888888888888888";
    let mut root = runtime_test_session(
        requested,
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    root.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: repository.path().join(requested),
    }));
    mj_controller::database::save_session(&root).unwrap();
    session.id = descendant.into();
    mj_controller::database::save_subagent_session(
        &session,
        &runtime_test_subagent(descendant, requested),
    )
    .unwrap();
    state
        .force_destroy_session(requested.into(), BranchDisposition::Keep)
        .await
        .unwrap();
    assert!(
        state
            .public_notices()
            .iter()
            .any(|notice| notice.session_id == requested && notice.text.contains(descendant)),
        "the advertised root notice query must include descendant cleanup failures"
    );
}

/// A runtime whose controller comes from the real store, for the tests that
/// exercise a daemon operation end to end in an isolated data directory.
pub(super) fn test_runtime_state_loading_the_store() -> Arc<RuntimeState> {
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ))
}

// Hard-won: 1d746fab4: each sweep rebuilt reports for every historical parent.
#[cfg(unix)]
#[tokio::test]
async fn report_candidates_follow_committed_child_lifecycle_without_losing_failed_resumes() {
    const CHILD: &str = "MJ_TEST_REPORT_PARENT_MEMBERSHIP";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "report_candidates_follow_committed_child_lifecycle_without_losing_failed_resumes",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "report-parent-membership")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let mut parent =
        mj_controller::controller::test_support::checkpoint_test_session("report-parent");
    parent.state = SessionState::Running;
    mj_controller::database::save_session(&parent).unwrap();
    let mut child =
        mj_controller::controller::test_support::checkpoint_test_session("report-child");
    child.state = SessionState::Stopped;
    child.last_error = None;
    let relation = runtime_test_subagent(&child.id, &parent.id);
    mj_controller::database::save_subagent_session(&child, &relation).unwrap();
    parent.state = SessionState::Stopped;
    mj_controller::database::save_session(&parent).unwrap();
    let mut state = test_runtime_state_loading_the_store();
    Arc::get_mut(&mut state).unwrap().committed =
        Some(mj_controller::database::subscribe_committed_state().unwrap());
    assert!(state.subagent_report_parent_ids().is_empty());
    for lifecycle in [
        SessionState::Running,
        SessionState::Parked,
        SessionState::Lost,
        SessionState::Error,
    ] {
        child.state = lifecycle;
        mj_controller::database::save_session(&child).unwrap();
        assert_eq!(
            state.subagent_report_parent_ids(),
            [parent.id.clone()],
            "a stopped parent must still get its child's report"
        );
    }
    child.state = SessionState::Stopped;
    child.last_error = Some("resume failed".into());
    mj_controller::database::save_session(&child).unwrap();
    assert_eq!(state.subagent_report_parent_ids(), [parent.id.clone()]);
    child.last_error = None;
    mj_controller::database::save_session(&child).unwrap();
    assert!(state.subagent_report_parent_ids().is_empty());
    child.state = SessionState::Provisioning;
    mj_controller::database::save_session(&child).unwrap();
    assert_eq!(state.subagent_report_parent_ids(), [parent.id]);
}

/// A create stopped by shutdown before the phone was told it exists is rolled
/// back and leaves no intent, so the next daemon provisions nothing for it. One
/// stopped after publication keeps its intent (see the restart end-to-end test).
#[tokio::test]
async fn a_create_stopped_for_shutdown_before_publication_leaves_nothing_to_resume() {
    const NAME: &str = "a_create_stopped_for_shutdown_before_publication_leaves_nothing_to_resume";
    const CHILD: &str = "MJ_TEST_CREATE_STOPPED_UNPUBLISHED";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Unpublished").unwrap();
    let session = runtime_test_session(
        "unpublished-create",
        &workspace.id,
        SessionState::Provisioning,
    );
    mj_controller::database::save_new_session(&session, None, false, false).unwrap();
    assert_eq!(
        mj_controller::database::load_session_create_intents()
            .unwrap()
            .len(),
        1
    );
    let state = test_runtime_state_loading_the_store();
    let control = LifecycleControl::default();
    // The phone never publishes: the sender stays alive and silent.
    let (_publisher, publication) = tokio::sync::oneshot::channel();
    state
        .start_create_lifecycle(
            session.id.clone(),
            control.clone(),
            create::CreateInputs::TopLevel {
                publication: Some(publication),
                remember_launch_choices: false,
            },
            None,
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    control.cancel_for_shutdown();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        state.wait_create_outcome(&session.id),
    )
    .await
    .expect("the stopped create ends");
    drop(outcome);
    assert!(
        mj_controller::database::load_session_create_intents()
            .unwrap()
            .is_empty(),
        "an unpublished create must not be left for the next daemon"
    );
    let record = mj_controller::database::load_session_record(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SessionState::Error);
}

/// A create wait reads durable state alone: with an intent row and no
/// lifecycle entry (what a new daemon sees before it has re-admitted the
/// create, and what a waiter on the old daemon sees), it keeps waiting; it
/// ends when the record leaves the creating states and the intent settles,
/// with the real failure for `Error`.
#[tokio::test]
async fn a_create_wait_is_answered_from_the_intent_row_and_the_record() {
    const NAME: &str = "a_create_wait_is_answered_from_the_intent_row_and_the_record";
    const CHILD: &str = "MJ_TEST_WAIT_CREATE_FROM_RECORD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Wait create").unwrap();
    for id in ["wait-finishes", "wait-fails"] {
        let creating = runtime_test_session(id, &workspace.id, SessionState::Provisioning);
        mj_controller::database::save_new_session(&creating, None, false, false).unwrap();
    }
    // A finished session whose worker dropped holds no create intent.
    let dropped = runtime_test_session("wait-dropped", &workspace.id, SessionState::Disconnected);
    mj_controller::database::save_session(&dropped).unwrap();
    let state = test_runtime_state_loading_the_store();
    use create::CreateWait;
    assert!(matches!(
        state.wait_create_outcome("wait-dropped").await,
        CreateWait::Finished
    ));
    assert!(matches!(
        state.wait_create_outcome("no-such-session").await,
        CreateWait::Failed(_)
    ));
    let waiting = |id: &'static str| {
        let state = state.clone();
        mj_core::runtime::spawn(async move { state.wait_create_outcome(id).await })
    };
    let finishes = waiting("wait-finishes");
    let fails = waiting("wait-fails");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !finishes.is_finished() && !fails.is_finished(),
        "an unfinished create keeps its waiters waiting"
    );
    let mut record = runtime_test_session("wait-finishes", &workspace.id, SessionState::Running);
    mj_controller::database::save_session(&record).unwrap();
    record = runtime_test_session("wait-fails", &workspace.id, SessionState::Error);
    record.last_error = Some("clone refused".to_owned());
    mj_controller::database::save_session(&record).unwrap();
    state.publish_revision();
    let finished = tokio::time::timeout(Duration::from_secs(10), finishes)
        .await
        .expect("the wait wakes on the daemon's change notification")
        .unwrap();
    assert!(matches!(finished, CreateWait::Finished));
    let CreateWait::Failed(failed) = tokio::time::timeout(Duration::from_secs(10), fails)
        .await
        .expect("the wait wakes on the daemon's change notification")
        .unwrap()
    else {
        panic!("a failed create answers with its reason");
    };
    assert!(
        format!("{failed:#}").contains("clone refused"),
        "{failed:#}"
    );
}

#[cfg(unix)]
// Hard-won: a5eed2e0: synchronous checkpoint database I/O blocked unrelated pings and timers.
#[tokio::test]
async fn a_checkpoint_waiting_for_io_keeps_daemon_requests_and_timers_responsive() {
    const NAME: &str = "a_checkpoint_waiting_for_io_keeps_daemon_requests_and_timers_responsive";
    const CHILD: &str = "MJ_TEST_CHECKPOINT_IO_RESPONSIVENESS";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Checkpoint I/O").unwrap();
    let session = runtime_test_session("checkpoint-io", &workspace.id, SessionState::Stopped);
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state_loading_the_store();

    // Hold the checkpoint's first write at a real I/O boundary. The native
    // watchdog releases it even if the checkpoint blocks this current-thread
    // runtime, so the old implementation fails instead of hanging the suite.
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let lock = std::thread::spawn(move || {
        let connection =
            rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        match release_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("checkpoint test release channel failed: {error}"),
        }
        connection.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx.await.unwrap();
    let checkpoint = mj_core::runtime::spawn({
        let state = state.clone();
        async move { state.checkpoint_for_bundle_export("checkpoint-io").await }
    });
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let elapsed = started.elapsed();
    let pending = !checkpoint.is_finished();
    let reply = handle_action(
        DaemonAction::Ping,
        &test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        &state,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let released = release_tx.send(());
    lock.join().unwrap();
    // This fixture has no live target: after the write is released it must
    // report the checkpoint failure and leave the session in its prior state.
    assert!(checkpoint.await.unwrap().is_err());
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions["checkpoint-io"].state,
        SessionState::Stopped
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "timer stalled for {elapsed:?}"
    );
    released.unwrap();
    assert!(pending, "checkpoint should still be waiting for I/O");
    assert!(matches!(reply, DaemonReply::Pong));
}

/// Whether `pid` names a live process. A zombie has already died; only its
/// parent has not collected it yet.
#[cfg(target_os = "linux")]
fn process_is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // The state follows the parenthesised command name, which may itself
        // contain spaces or parentheses.
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

/// Every process id the held stand-in has recorded in `directory`.
#[cfg(target_os = "linux")]
fn held_command_pids(directory: &Path) -> Vec<u32> {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().to_str()?.parse().ok())
        .collect()
}

/// Session launches whose target commands hang must leave the daemon serving
/// other requests, and stopping them must end those commands.
///
/// The runtime has two async worker threads and four launches hang in their
/// harness preflight, a real child process that waits for a file. A daemon
/// that waits for target commands on its async workers stops answering once
/// two of them hang. A watchdog thread releases the commands if the daemon
/// stalls, so that implementation fails the timing assertions instead of
/// hanging the suite.
#[cfg(target_os = "linux")]
// Hard-won: 34e86406: hung lifecycle subprocesses exhausted async workers and blocked cancellation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hung_launch_commands_leave_the_daemon_serving_and_end_when_cancelled() {
    const NAME: &str = "hung_launch_commands_leave_the_daemon_serving_and_end_when_cancelled";
    const CHILD: &str = "MJ_TEST_HUNG_LAUNCH_COMMANDS";
    const BOUND: Duration = Duration::from_secs(5);
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "hung-launch-commands")
        .env(
            "MJ_WORKER_BINARY",
            mj_core::test_hooks::fake_worker_dispatcher(),
        )
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    let held = root.path().join("held");
    let release = root.path().join("release");
    let project = root.path().join("project");
    let home = root.path().join("codex-home");
    for directory in [&tools, &held, &project, &home] {
        std::fs::create_dir_all(directory).unwrap();
    }
    mj_core::test_hooks::install_fake_command(&tools, "node", "#!/bin/sh\nexit 0\n");
    // Every launch's preflight runs npm, which records its process id and
    // waits for the release file.
    mj_core::test_hooks::install_fake_command(
        &tools,
        "npm",
        &format!(
            "#!/bin/sh\necho > '{}/'$$\nwhile [ ! -e '{}' ]; do /bin/sleep 0.05; done\nexit 0\n",
            held.display(),
            release.display()
        ),
    );
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }
    let _release = Release(release.clone());
    let (finished, watchdog_finished) = std::sync::mpsc::channel::<()>();
    let watchdog = {
        let release = release.clone();
        std::thread::spawn(move || {
            if watchdog_finished
                .recv_timeout(Duration::from_secs(30))
                .is_err()
            {
                let _ = std::fs::write(release, b"");
            }
        })
    };

    let mut config = Config::default();
    let mut environment = std::collections::BTreeMap::new();
    environment.insert("PATH".to_owned(), tools.to_string_lossy().into_owned());
    config.profiles.insert(
        "codex".into(),
        mj_core::config::HarnessProfile {
            enabled: true,
            environment: environment.into_iter().collect(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_muse_auto_review: None,
            load_balancing_may_consume_credits: false,
            claude_permission_mode: None,
            mjolnir_guardian: true,
            aws: None,
            models: Default::default(),
            legacy_subagents: None,
            ..mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Codex, home)
        },
    );
    config.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    config.save().unwrap();
    let workspace = mj_controller::database::create_workspace("Hung launches").unwrap();
    let project = project.canonicalize().unwrap();
    let state = test_runtime_state_loading_the_store();
    // Hard-won: 5e70f76a1: admission fixtures omitted the serving profile catalog.
    state.profile_catalog.get_or_init(|| {
        mj_controller::profile_catalog::ProfileCatalog::new(
            tokio_util::sync::CancellationToken::new(),
        )
    });
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let shutdown = CancellationToken::new();
    let create = |title: &str| {
        DaemonAction::StartCreateSession(mj_client::daemon::CreateSessionRequest {
            oracle: None,
            oracle_choice: None,
            remember_launch_choices: true,
            remember_container_size: true,
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: false,
            notifications_ignored: false,
            create_managed_worktree: Some(false),
            at: None,
            branch: None,
            base: None,
            no_later_history: false,
            subagents: None,
            initial_prompt: None,
            workspace_id: workspace.id.clone(),
            profile_id: "codex".into(),
            bundle_id: "project".into(),
            project_directory: Some(project.clone()),
            target_template_id: "localhost".into(),
            additional_mounts: Vec::new(),
            resource_allocation: None,
            title: title.into(),
            session_title_override: None,
        })
    };
    let registered = |reply: DaemonReply| match reply {
        DaemonReply::RegisteredSession(registered) => registered.session.id,
        other => panic!("expected a registered session, got {other:?}"),
    };
    let wait_for_held = |count: usize| {
        let held = held.clone();
        async move {
            let deadline = std::time::Instant::now() + BOUND;
            while held_command_pids(&held).len() < count {
                assert!(
                    std::time::Instant::now() < deadline,
                    "only {} of {count} launches reached their preflight",
                    held_command_pids(&held).len()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };

    let started = std::time::Instant::now();
    let mut sessions = Vec::new();
    for title in ["first", "second", "third"] {
        sessions.push(registered(
            handle_action(create(title), &metadata, &state, &shutdown)
                .await
                .unwrap(),
        ));
    }
    wait_for_held(3).await;
    // Three commands hang on a runtime with two async workers. An unrelated
    // request and another launch are still answered.
    assert!(matches!(
        handle_action(DaemonAction::Ping, &metadata, &state, &shutdown)
            .await
            .unwrap(),
        DaemonReply::Pong
    ));
    sessions.push(registered(
        handle_action(create("fourth"), &metadata, &state, &shutdown)
            .await
            .unwrap(),
    ));
    wait_for_held(4).await;
    assert!(
        started.elapsed() < BOUND,
        "the daemon stalled behind hung launch commands for {:?}",
        started.elapsed()
    );
    assert!(
        !release.exists(),
        "the watchdog had to release the hung commands"
    );

    // Cancelling one launch kills its command, and that launch reports the
    // cancellation while the others keep waiting.
    let before = held_command_pids(&held)
        .into_iter()
        .filter(|pid| process_is_running(*pid))
        .count();
    assert_eq!(before, 4);
    let cancelled = std::time::Instant::now();
    handle_action(
        DaemonAction::CancelLifecycle {
            session_id: sessions[0].clone(),
        },
        &metadata,
        &state,
        &shutdown,
    )
    .await
    .unwrap();
    let outcome = tokio::time::timeout(
        BOUND,
        handle_action(
            DaemonAction::WaitCreateSession {
                session_id: sessions[0].clone(),
            },
            &metadata,
            &state,
            &shutdown,
        ),
    )
    .await
    .expect("a cancelled launch finishes promptly");
    assert!(outcome.is_err(), "a cancelled launch must not succeed");
    let running = || {
        held_command_pids(&held)
            .into_iter()
            .filter(|pid| process_is_running(*pid))
            .count()
    };
    while running() > 3 {
        assert!(
            cancelled.elapsed() < BOUND,
            "the cancelled launch left its command running"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        running(),
        3,
        "cancelling one launch ends exactly its command"
    );

    // Stopping the daemon cancels the rest the same way.
    let stopping = std::time::Instant::now();
    state.cancel_and_wait_lifecycles().await.unwrap();
    assert!(
        stopping.elapsed() < BOUND,
        "stopping waited {:?} for hung commands",
        stopping.elapsed()
    );
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        let survivors = held_command_pids(&held)
            .into_iter()
            .filter(|pid| process_is_running(*pid))
            .collect::<Vec<_>>();
        if survivors.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stopping left hung commands running: {survivors:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !release.exists(),
        "cancellation, not the release, ended them"
    );
    finished.send(()).unwrap();
    watchdog.join().unwrap();
}

/// The startup sweep picks up tombstones an older build left behind and any
/// discard a daemon stop interrupted, and nothing else.
#[test]
fn startup_selects_every_record_that_is_only_a_tombstone() {
    let sessions = [
        runtime_test_session("lost", "workspace", SessionState::Lost),
        runtime_test_session(
            "data-loss",
            "workspace",
            SessionState::DestroyedWithDataLoss,
        ),
        runtime_test_session("stopped", "workspace", SessionState::Stopped),
        runtime_test_session("running", "workspace", SessionState::Running),
        runtime_test_session("failed", "workspace", SessionState::Error),
    ];
    let controller = Controller {
        config: Config::default(),
        state: mj_core::state::State {
            sessions: sessions
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            ..mj_core::state::State::default()
        },
    };

    assert_eq!(
        tombstone_session_ids(&controller),
        vec!["data-loss".to_owned(), "lost".to_owned()]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_close_retains_history_discards_drafts_and_refuses_resume_races() {
    const TEST: &str = "workspace_close_retains_history_discards_drafts_and_refuses_resume_races";
    const CHILD: &str = "MJ_TEST_WORKSPACE_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Close me").unwrap();
    let mut history = runtime_test_session("history-close", &workspace.id, SessionState::Stopped);
    history.draft_input = "unsent".into();
    mj_controller::database::save_session(&history).unwrap();
    mj_controller::database::save_detached_draft(
        &workspace.id,
        Some(&history.id),
        "terminal",
        Some(42),
        "saved draft",
    )
    .unwrap();
    let state = test_runtime_state_loading_the_store();
    state.refresh_workspaces().await.unwrap();
    assert!(
        state
            .workspaces()
            .borrow()
            .iter()
            .any(|w| w.id == workspace.id)
    );
    let admission = state
        .workspace_resume_gate(&workspace.id)
        .read_owned()
        .await;
    let error = state
        .close_workspace(workspace.id.clone())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("resume is in progress"),
        "{error:#}"
    );
    assert_eq!(
        mj_controller::database::list_detached_drafts(&workspace.id)
            .unwrap()
            .len(),
        1
    );
    assert!(
        state.workspace_closes.lock().unwrap().is_empty(),
        "failed close is retryable"
    );
    drop(admission);
    // An unrelated workspace's resume must not prevent this close.
    let _other_resume = state.workspace_resume_gate("other").read_owned().await;
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    assert!(matches!(
        handle_action(
            DaemonAction::DeleteWorkspace {
                workspace_id: workspace.id.clone(),
            },
            &metadata,
            &state,
            &CancellationToken::new(),
        )
        .await
        .unwrap(),
        DaemonReply::Done
    ));
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(stored.sessions[&history.id].state, SessionState::Stopped);
    assert!(stored.sessions[&history.id].draft_input.is_empty());
    assert!(
        mj_controller::database::list_detached_drafts(&workspace.id)
            .unwrap()
            .is_empty()
    );
    assert!(
        mj_controller::database::list_workspaces()
            .unwrap()
            .iter()
            .all(|w| w.id != workspace.id)
    );
    assert!(
        state
            .workspaces()
            .borrow()
            .iter()
            .all(|w| w.id != workspace.id)
    );
    assert!(
        !state
            .runtime_publication()
            .unwrap()
            .metadata
            .workspace_names
            .contains_key(&workspace.id)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_feed_tracks_names_and_a_delayed_refresh_cannot_restore_a_deleted_tab() {
    const TEST: &str =
        "workspace_feed_tracks_names_and_a_delayed_refresh_cannot_restore_a_deleted_tab";
    const CHILD: &str = "MJ_TEST_WORKSPACE_FEED_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state_loading_the_store();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    let DaemonReply::Workspace(workspace) = handle_action(
        DaemonAction::CreateWorkspace {
            name: "Before".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap() else {
        panic!("create did not return the workspace");
    };
    assert_eq!(state.workspaces().borrow()[0].id, workspace.id);

    handle_action(
        DaemonAction::RenameWorkspace {
            workspace_id: workspace.id.clone(),
            name: "After".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    assert_eq!(state.workspaces().borrow()[0].name, "After");
    assert_eq!(
        state
            .runtime_publication()
            .unwrap()
            .metadata
            .workspace_names[&workspace.id],
        "After"
    );

    // A refresh queued before deletion must read after the writer commits,
    // rather than publish a workspace list captured before deletion.
    let refresh_guard = state.workspace_refresh.lock().await;
    let delayed = mj_core::runtime::spawn({
        let state = state.clone();
        async move { state.refresh_workspaces().await }
    });
    tokio::task::yield_now().await;
    mj_controller::database::close_workspace(&workspace.id).unwrap();
    drop(refresh_guard);
    delayed.await.unwrap().unwrap();
    assert!(
        state
            .workspaces()
            .borrow()
            .iter()
            .all(|w| w.id != workspace.id)
    );
}

#[tokio::test]
async fn workspace_close_cancellation_marks_only_the_requested_operation() {
    let state = test_runtime_state();
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    state.workspace_closes.lock().unwrap().extend([
        ("first".into(), first.clone()),
        ("second".into(), second.clone()),
    ]);
    state.cancel_workspace_close("first").unwrap();
    assert!(first.load(Ordering::Acquire));
    assert!(!second.load(Ordering::Acquire));
    assert!(state.cancel_workspace_close("missing").is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_close_stops_independent_sessions_concurrently_and_retries_after_a_new_create() {
    const TEST: &str =
        "workspace_close_stops_independent_sessions_concurrently_and_retries_after_a_new_create";
    const CHILD: &str = "MJ_TEST_WORKSPACE_CLOSE_CONCURRENCY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Concurrent close").unwrap();
    for id in ["close-first", "close-second"] {
        mj_controller::database::save_session(&runtime_test_session(
            id,
            &workspace.id,
            SessionState::Provisioning,
        ))
        .unwrap();
    }
    mj_controller::database::save_detached_draft(
        &workspace.id,
        None,
        "terminal",
        Some(42),
        "do not discard on failure",
    )
    .unwrap();
    let state = test_runtime_state_loading_the_store();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let finish = Arc::new(tokio::sync::Semaphore::new(0));
    for id in ["close-first", "close-second"] {
        let barrier = barrier.clone();
        let finish = finish.clone();
        // A close supersedes automatic work; it waits for a person's own.
        state
            .start_automatic_lifecycle(
                id.into(),
                LifecycleKind::Create,
                move |_, _, cancelled, _session_admission| async move {
                    while !cancelled.load(Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    barrier.wait().await;
                    finish.acquire().await.unwrap().forget();
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await
            .unwrap();
    }
    let closing = mj_core::runtime::spawn({
        let state = state.clone();
        let id = workspace.id.clone();
        async move { state.close_workspace(id).await }
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.wait())
        .await
        .expect("both independent closes must run concurrently");
    // The user starts another session while the originally confirmed sessions stop.
    mj_controller::database::save_session(&runtime_test_session(
        "close-late",
        &workspace.id,
        SessionState::Provisioning,
    ))
    .unwrap();
    finish.add_permits(2);
    let error = tokio::time::timeout(Duration::from_secs(10), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("active sessions remain"),
        "{error:#}"
    );
    assert_eq!(
        mj_controller::database::list_detached_drafts(&workspace.id)
            .unwrap()
            .len(),
        1
    );
    let stored = mj_controller::database::load_state().unwrap();
    for id in ["close-first", "close-second"] {
        assert_eq!(stored.sessions[id].state, SessionState::Stopped);
    }
    assert_eq!(
        stored.sessions["close-late"].state,
        SessionState::Provisioning
    );
    state.close_workspace(workspace.id.clone()).await.unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    for id in ["close-first", "close-second", "close-late"] {
        assert_eq!(stored.sessions[id].state, SessionState::Stopped);
    }
    assert!(
        mj_controller::database::list_detached_drafts(&workspace.id)
            .unwrap()
            .is_empty()
    );
}

/// #1218's workspace close: the person cancels while one session's stop is
/// past its commit and another's is not. The first finishes, the second
/// stops, and neither is reported as a failure to retry; the answer is the
/// cancellation, and the workspace and its drafts stay.
#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_workspace_close_lets_committed_stops_finish_and_reports_the_cancel() {
    const TEST: &str =
        "a_cancelled_workspace_close_lets_committed_stops_finish_and_reports_the_cancel";
    const CHILD: &str = "MJ_TEST_CANCELLED_WORKSPACE_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Cancelled close").unwrap();
    for id in ["past-commit", "before-commit"] {
        mj_controller::database::save_session(&runtime_test_session(
            id,
            &workspace.id,
            SessionState::Running,
        ))
        .unwrap();
    }
    mj_controller::database::save_detached_draft(
        &workspace.id,
        None,
        "terminal",
        Some(7),
        "keep me",
    )
    .unwrap();
    let state = test_runtime_state_loading_the_store();
    // Each stop stands in for a close the workspace close joins: one already
    // committed and still removing its target, one still checkpointing.
    let committed = LifecycleControl::default();
    let finish_teardown = Arc::new(tokio::sync::Notify::new());
    let key = serde_json::to_string(&(true, false)).unwrap();
    let (committed_acceptance, committed_watch) = tokio::sync::watch::channel(Some(Ok(())));
    let started = state
        .admit_prepared_lifecycle(
            "past-commit".into(),
            LifecycleKind::Suspend,
            super::lifecycle::LifecycleStart::request(key.clone(), None, Some(committed.clone())),
            Some(committed_watch),
            {
                let finish_teardown = finish_teardown.clone();
                move |_, _, _, _session_admission| async move {
                    finish_teardown.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    drop(started);
    let committed_baseline = committed_acceptance.receiver_count();
    assert!(committed.grant_commit());
    let (cancellable_acceptance, cancellable_watch) = tokio::sync::watch::channel(Some(Ok(())));
    let started = state
        .admit_prepared_lifecycle(
            "before-commit".into(),
            LifecycleKind::Suspend,
            super::lifecycle::LifecycleStart::request(key, None, None),
            Some(cancellable_watch),
            move |_, _, cancelled, _session_admission| async move {
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                bail!("operation cancelled before the session began to stop")
            },
        )
        .await
        .unwrap();
    drop(started);
    let cancellable_baseline = cancellable_acceptance.receiver_count();
    let mut closing = mj_core::runtime::spawn({
        let state = state.clone();
        let id = workspace.id.clone();
        async move { state.close_workspace(id).await }
    });
    // A fresh stop already sets its close marker. Only an additional acceptance
    // receiver proves that the workspace close has joined that existing stop.
    tokio::select! {
        result = &mut closing => panic!("workspace close completed before joining both stops: {result:?}"),
        joined = tokio::time::timeout(Duration::from_secs(5), async {
            while committed_acceptance.receiver_count() <= committed_baseline
                || cancellable_acceptance.receiver_count() <= cancellable_baseline
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }) => joined.expect("the workspace close never joined both stops"),
    }
    state.cancel_workspace_close(&workspace.id).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state
            .owner()
            .lifecycle
            .get("before-commit")
            .is_some_and(|active| active.is_running())
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the stop before its commit was not cancelled");
    assert!(
        !committed.cancelled.load(Ordering::Acquire),
        "the committed stop must not be cancelled"
    );
    finish_teardown.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(10), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "Workspace deletion cancelled; workspace and drafts retained. Completed suspensions were not undone."
    );
    assert!(
        mj_controller::database::list_workspaces()
            .unwrap()
            .iter()
            .any(|retained| retained.id == workspace.id)
    );
    assert_eq!(
        mj_controller::database::list_detached_drafts(&workspace.id)
            .unwrap()
            .len(),
        1
    );
}

// Hard-won: 1b13e4a69: reopening a dashboard before the config refresh restored a stale sidebar size.
#[cfg(unix)]
#[tokio::test]
async fn saved_sidebar_size_is_in_the_next_dashboard_startup_without_a_refresh() {
    const CHILD: &str = "MJ_SIDEBAR_SAVE_STARTUP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "saved_sidebar_size_is_in_the_next_dashboard_startup_without_a_refresh",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "sidebar-save-startup")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state_loading_the_store();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    // No periodic config refresher runs in this fixture: the save itself must
    // install the preference before acknowledging it to the terminal.
    for size in [
        mj_core::workspace::PaneSize::Minimized,
        mj_core::workspace::PaneSize::Standard,
    ] {
        let reply = handle_action(
            DaemonAction::SaveSidebarSize { size },
            &metadata,
            &state,
            &cancellation,
        )
        .await
        .unwrap();
        assert!(matches!(reply, DaemonReply::Done));
        assert_eq!(Config::load().unwrap().sidebar_size, size);
        let reply = handle_action(
            DaemonAction::DashboardStartup {
                client_id: "reopened-dashboard".into(),
                workspace_id: "default".into(),
            },
            &metadata,
            &state,
            &cancellation,
        )
        .await
        .unwrap();
        let DaemonReply::DashboardStartup(startup) = reply else {
            panic!("dashboard startup reply");
        };
        assert_eq!(
            startup
                .runtime
                .projection
                .metadata
                .installed_config()
                .sidebar_size,
            size
        );
    }
}

// Hard-won: 0bd61a289: dashboard startup failed when live session state exceeded one frame.
#[tokio::test]
async fn dashboard_startup_larger_than_a_frame_arrives_whole() {
    let mut session = runtime_test_session("large-draft", "workspace", SessionState::Running);
    session.draft_input = "draft 🙂\n".repeat(MAX_FRAME_BYTES / 8);
    let startup = DashboardStartup {
        runtime: mj_client::runtime_feed::RuntimeBootstrap {
            cursor: mj_client::runtime_feed::RuntimeCursor {
                alert_sequence: 0,
                incarnation: "startup".into(),
                sequence: 1,
                subagent_parents: Default::default(),
            },
            projection: mj_client::runtime_feed::RuntimeProjection {
                records: [(session.id.clone(), session)].into_iter().collect(),
                ..Default::default()
            },
        },
        metadata: Default::default(),
        pane_sizes: Default::default(),
        conversation_layouts: Default::default(),
        queued_prompts: Default::default(),
    };
    let expected = serde_json::to_value(&startup).unwrap();
    let payload_bytes = serde_json::to_vec(&startup).unwrap().len();
    assert!(payload_bytes > MAX_FRAME_BYTES);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = mj_core::runtime::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        for reply in [
            DaemonReply::DashboardStartup(Box::new(startup)),
            DaemonReply::Pong,
        ] {
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            match &reply {
                DaemonReply::DashboardStartup(_) => assert!(matches!(
                    request.action,
                    DaemonAction::DashboardStartup { client_id, workspace_id } if client_id == "large-dashboard" && workspace_id == "workspace"
                )),
                _ => assert!(matches!(request.action, DaemonAction::Ping)),
            }
            super::serve::write_response(
                &mut stream,
                ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Ok(reply),
                },
            )
            .await
            .unwrap();
        }
    });
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let copied = mj_core::runtime::spawn(async move {
        let (mut incoming, _) = proxy.accept().await.unwrap();
        let mut outgoing = TcpStream::connect(address).await.unwrap();
        tokio::io::copy_bidirectional(&mut incoming, &mut outgoing)
            .await
            .unwrap()
    });
    let mut client = mj_client::daemon::DaemonClient::connect(test_metadata(proxy_address))
        .await
        .unwrap();
    let received = client
        .dashboard_startup("large-dashboard".into(), "workspace".into())
        .await
        .expect("load the complete dashboard startup state");
    assert_eq!(serde_json::to_value(received).unwrap(), expected);
    assert!(matches!(
        client.request(DaemonAction::Ping).await.unwrap(),
        DaemonReply::Pong
    ));
    drop(client);
    server.await.unwrap();
    let (_, response_bytes) = copied.await.unwrap();
    assert!(
        response_bytes < (payload_bytes * 3 / 2) as u64,
        "base64 chunks must stay near 4/3 of JSON size: {response_bytes} wire bytes for {payload_bytes} JSON bytes"
    );
}

#[tokio::test]
async fn oversized_response_reports_its_size_and_keeps_connection_usable() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        for (request_id, result) in [
            (7, Ok(DaemonReply::Text("x".repeat(MAX_FRAME_BYTES)))),
            (8, Ok(DaemonReply::Pong)),
        ] {
            super::serve::write_response(
                &mut stream,
                ResponseEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    result,
                },
            )
            .await
            .unwrap();
        }
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 7);
    let error = response.result.unwrap_err();
    assert!(error.contains("text response for request 7"), "{error}");
    assert!(error.contains(&MAX_FRAME_BYTES.to_string()), "{error}");
    let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 8);
    assert!(matches!(response.result, Ok(DaemonReply::Pong)));
    sender.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn suspension_intent_survives_restart_and_missing_worker_reports_failure() {
    const NAME: &str = "suspension_intent_survives_restart_and_missing_worker_reports_failure";
    const CHILD: &str = "MJ_TEST_SUSPENSION_RESTART_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let log = mj_controller::test_log::CapturedLog::default();
    let _log = tracing::subscriber::set_default(log.clone());
    let workspace = mj_controller::database::create_workspace("Suspend restart").unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut session = runtime_test_session("restart-suspend", &workspace.id, SessionState::Running);
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(&session.id),
    }));
    session.last_error = Some("old connection failure".into());
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state_loading_the_store();
    state.prepare_suspension(&session.id).await.unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(stored.sessions[&session.id].state, SessionState::Closing);
    assert!(interrupted_suspend_session_ids(&Controller::load().unwrap()).contains(&session.id));
    // A new runtime has no in-memory requests from the earlier daemon.
    let restarted = test_runtime_state_loading_the_store();
    let failure = tokio::time::timeout(
        Duration::from_secs(15),
        restarted.suspend_session(session.id.clone()),
    )
    .await
    .unwrap();
    assert!(failure.is_err());
    let restored = mj_controller::database::load_state().unwrap();
    let retained = &restored.sessions[&session.id];
    assert_eq!(retained.target, session.target);
    assert_eq!(retained.state, SessionState::Error);
    let message = retained.public_error().unwrap();
    assert!(
        message.contains("could not contact the worker"),
        "{message}"
    );
    assert!(message.contains("Resume the session"), "{message}");
    assert!(message.contains("Reference "), "{message}");
    assert!(
        retained
            .public_error()
            .unwrap()
            .starts_with(mj_core::state::CLOSE_FAILURE_PREFIX)
    );
    assert!(!restarted.close_is_requested(&session.id));
    let diagnostic = log.at(tracing::Level::WARN);
    assert!(
        diagnostic
            .iter()
            .any(|line| line.contains("suspension failure worker diagnostic")
                && line.contains("worker_pids=[]")
                && line.contains("recorded no startup step")),
        "{diagnostic:?}"
    );
    assert!(
        diagnostic
            .iter()
            .any(|line| line.contains("phase=\"lease worker connection\"")
                || line.contains("phase=\"wait for session actor\"")),
        "{diagnostic:?}"
    );
    assert!(
        !root.path().join(&session.id).exists(),
        "diagnosis must not create worker state"
    );
}

// Hard-won: #1542: Suspend superseding a Move mid-clone left the session in Closing indefinitely
#[cfg(unix)]
#[tokio::test]
async fn suspend_superseding_move_before_worker_install_settles_the_source() {
    const NAME: &str = "suspend_superseding_move_before_worker_install_settles_the_source";
    const CHILD: &str = "MJ_TEST_SUPERSEDED_MOVE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let bin = directory.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        mj_core::test_hooks::install_fake_command(
            &bin,
            "aws",
            r#"
printf '%s\n' "$*" >> "$MJ_TEST_MOVE_AWS_LOG"
case "$*" in
  *"ec2 terminate-instances"*|*"ec2 wait instance-terminated"*) exit 0 ;;
  *) exit 92 ;;
esac
"#,
        );
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .isolated_store(directory.path())
        .env(CHILD, "1")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("PATH", path)
        .env("MJ_TEST_MOVE_AWS_LOG", directory.path().join("aws-calls"))
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Superseded Move").unwrap();
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Origin {
        Suspend,
        Cancel,
        StaleCancel,
    }
    for (retained_workspace, restart, sealed, origin) in [
        (false, false, true, Origin::Suspend),
        (true, false, true, Origin::Suspend),
        (false, true, true, Origin::Suspend),
        (true, true, true, Origin::Suspend),
        (true, false, false, Origin::Suspend),
        (true, true, false, Origin::Suspend),
        (true, true, true, Origin::Cancel),
        (true, true, true, Origin::StaleCancel),
    ] {
        let requested_close = origin == Origin::Suspend;
        let root = tempfile::tempdir().unwrap();
        let id = match origin {
            Origin::Cancel => "15420000000000000000000000000007",
            Origin::StaleCancel => "15420000000000000000000000000008",
            Origin::Suspend => match (retained_workspace, restart, sealed) {
                (false, false, true) => "15420000000000000000000000000001",
                (true, false, true) => "15420000000000000000000000000002",
                (false, true, true) => "15420000000000000000000000000003",
                (true, true, true) => "15420000000000000000000000000004",
                (_, false, false) => "15420000000000000000000000000005",
                (_, true, false) => "15420000000000000000000000000006",
            },
        };
        let source_root = root.path().join("source").join(id);
        let mut destination_root = root.path().join("destination").join(id);
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&destination_root).unwrap();
        std::fs::write(
            source_root.join("uncheckpointed-work"),
            "keep source workspace",
        )
        .unwrap();
        std::fs::write(
            destination_root.join("clone.partial"),
            "unfinished checkout",
        )
        .unwrap();
        let checkpoint = mj_controller::controller::test_support::write_checkpoint_gate_archive(
            root.path(),
            id,
            7,
        );
        let mut source = runtime_test_session(id, &workspace.id, SessionState::Stopped);
        source.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: source_root.clone(),
        }));
        source.checkpoint = Some(Box::new(checkpoint.clone()));
        mj_controller::database::save_session(&source).unwrap();
        let mut operation = mj_core::state::MoveOperation {
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: (retained_workspace && sealed).then(|| {
                let mut runtime = mj_core::state::TargetRuntimeSettings::from(
                    &mj_core::config::TargetTemplate::LocalBare,
                );
                runtime.kind = "aws-ec2".into();
                runtime.connection = mj_core::state::TargetConnection::Aws {
                    profile: "isolated-fixture".into(),
                    region: "us-east-1".into(),
                    ssh_user: "fixture".into(),
                    identity_file: None,
                    ssh_args: Vec::new(),
                };
                mj_core::state::PreparedMoveDestination {
                    launch_args: Vec::new(),
                    runtime,
                    state: if restart {
                        mj_core::state::PreparedDestinationState::CleanupPending {
                            instance_id: Some(format!("i-{id}")),
                        }
                    } else {
                        mj_core::state::PreparedDestinationState::Created {
                            instance_id: format!("i-{id}"),
                        }
                    },
                }
            }),
            accepted_preparation: None,
            acknowledge_interruption: false,
            workspace_transfer: retained_workspace.then(|| {
                mj_core::move_workspace::WorkspaceTransfer {
                    assessment: Default::default(),
                    source: Box::new(source.clone()),
                    source_stage: root.path().join("source-stage"),
                    controller_stage: root.path().join("controller-stage"),
                    phase: mj_core::move_workspace::WorkspaceTransferPhase::SourceStopped,
                }
            }),
            handoff: retained_workspace.then(|| checkpoint.clone()),
            source_checkpoint_only: false,
            in_place: false,
            operation_id: format!("mid-clone-{id}"),
            selection: mj_core::state::MoveSelection {
                session_id: id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("local".into()),
                workspace: Default::default(),
                clear_resource_allocation: false,
                additional_mounts: None,
                resource_allocation: None,
                subagents: None,
                oracle: None,
                oracle_choice: None,
            },
            source_profile_id: source.last_profile.clone(),
            source_target_template_id: source.target_template_id.clone(),
            source_target: source.target.clone().map(|target| *target),
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: "mid-clone".into(),
            checkpoint: Some(checkpoint.clone()),
            recovery_session: sealed.then(|| source.clone()),
            queue: mj_core::state::ResumeQueueDisposition::Discard,
            phase: mj_core::state::MovePhase::ResumingDestination,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: source.created_at.clone(),
            updated_at: source.updated_at.clone(),
            error: None,
        };
        mj_controller::database::save_move_operation(&operation).unwrap();
        let mut destination = source.clone();
        destination.state = SessionState::Provisioning;
        destination.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: destination_root.clone(),
        }));
        mj_controller::database::save_resumed_session(&destination, None).unwrap();
        if restart {
            operation.phase = mj_core::state::MovePhase::Cancelled;
            operation.cancellation_requested = true;
            mj_controller::database::save_move_operation(&operation).unwrap();
            destination.state = SessionState::Closing;
            mj_controller::database::save_lifecycle_session(&destination).unwrap();
        }
        if requested_close && restart && sealed {
            // Restore can publish Running just before Move records StartingQueue.
            // Closing and that destination's readiness must become durable together.
            destination.state = SessionState::Running;
            mj_controller::database::save_lifecycle_session(&destination).unwrap();
            let mut ready_operation = operation.clone();
            ready_operation.prepared_destination = None;
            mj_controller::database::save_move_operation(&ready_operation).unwrap();
            let ready = test_runtime_state_loading_the_store();
            ready.prepare_suspension(id).await.unwrap();
            let saved = mj_controller::database::load_session_record(id)
                .unwrap()
                .unwrap();
            let saved_move = mj_controller::database::load_move_operation(id)
                .unwrap()
                .unwrap();
            assert_eq!(saved.state, SessionState::Closing);
            assert_eq!(
                saved_move.destination_target.as_ref(),
                destination.target.as_deref()
            );
            let mut controller = Controller::load().unwrap();
            assert!(
                !controller
                    .settle_superseded_move_for_close(
                        id,
                        &RefusingExecutor("a ready destination must never be rolled back"),
                        None,
                    )
                    .await
                    .unwrap()
            );
            let destination_archives = root.path().join("destination-checkpoints");
            std::fs::create_dir(&destination_archives).unwrap();
            let destination_checkpoint =
                mj_controller::controller::test_support::write_checkpoint_gate_archive(
                    &destination_archives,
                    id,
                    17,
                );
            controller.state.sessions.get_mut(id).unwrap().checkpoint =
                Some(Box::new(destination_checkpoint.clone()));
            mj_controller::database::save_checkpointed_session(&controller.state.sessions[id])
                .unwrap();
            // The daemon stopped after committing verified teardown, before
            // publishing Stopped. Startup must give this cut to close, not Move.
            controller.state.sessions.get_mut(id).unwrap().state = SessionState::Destroying;
            mj_controller::database::save_lifecycle_session(&controller.state.sessions[id])
                .unwrap();
            controller
                .record_failed_close(
                    id,
                    &format!(
                        "{}: Teardown was interrupted; retry suspension.",
                        mj_core::state::CLOSE_FAILURE_PREFIX
                    ),
                )
                .unwrap();
            let teardown = test_runtime_state_loading_the_store();
            let (teardown_moves, close_operations) =
                mj_controller::database::load_move_recovery().unwrap();
            let owners = teardown
                .recover_moves(teardown_moves, &close_operations)
                .await
                .unwrap();
            assert!(!owners.moves.contains(id));
            assert!(owners.closes.contains(id));
            tokio::time::timeout(Duration::from_secs(30), teardown.suspend_session(id.into()))
                .await
                .unwrap()
                .unwrap();
            let closed = mj_controller::database::load_session_record(id)
                .unwrap()
                .unwrap();
            let resolved = mj_controller::database::load_move_operation(id)
                .unwrap()
                .unwrap();
            assert_eq!(closed.state, SessionState::Stopped);
            assert_eq!(closed.checkpoint.as_deref(), Some(&destination_checkpoint));
            assert!(resolved.is_resolved());
            assert!(
                !resolved.holds_source_environment(),
                "Resume must accept the destination checkpoint"
            );
            // Return to the earlier clone cut for the restart assertion below.
            mj_controller::database::save_move_operation(&operation).unwrap();
            destination_root = root.path().join("destination-after-close").join(id);
            destination.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
                worker_root: destination_root.clone(),
            }));
            std::fs::create_dir_all(&destination_root).unwrap();
            std::fs::write(
                destination_root.join("clone.partial"),
                "unfinished checkout",
            )
            .unwrap();
            destination.state = SessionState::Closing;
            mj_controller::database::save_resumed_session(&destination, None).unwrap();
        }
        if requested_close && restart {
            test_runtime_state_loading_the_store()
                .prepare_suspension(id)
                .await
                .unwrap();
        }
        if requested_close && restart && retained_workspace && sealed {
            let connection =
                rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
            connection.execute_batch("CREATE TRIGGER reject_move_settlement BEFORE UPDATE OF operation_json ON session_moves
                WHEN json_extract(NEW.operation_json, '$.queue_admission_finished')=1
                BEGIN SELECT RAISE(ABORT, 'injected Move resolution failure'); END;").unwrap();
            let source_environment =
                mj_controller::database::move_source_environment(&operation.operation_id)
                    .unwrap()
                    .unwrap();
            let mut restored = source.clone();
            restored.state = SessionState::Error;
            // Hard-won: 59b3ff0: restoring the source before resolving Move blocked Resume after a crash.
            assert!(
                mj_controller::database::settle_superseded_move_source(
                    &restored,
                    Some(&source_environment.id),
                    &operation.operation_id
                )
                .is_err()
            );
            let unchanged = mj_controller::database::load_session_record(id)
                .unwrap()
                .unwrap();
            assert_eq!(unchanged.target, destination.target);
            assert_eq!(unchanged.state, SessionState::Closing);
            assert!(
                !mj_controller::database::load_move_operation(id)
                    .unwrap()
                    .unwrap()
                    .is_resolved()
            );
            connection
                .execute_batch("DROP TRIGGER reject_move_settlement")
                .unwrap();
        }
        if !requested_close {
            let (_, close_operations) = mj_controller::database::load_move_recovery().unwrap();
            assert!(
                !close_operations.contains(&operation.operation_id),
                "NULL grants no close authority"
            );
            if origin == Origin::StaleCancel {
                let raw =
                    rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
                raw.execute("UPDATE session_moves SET close_requested_operation_id='stale-other-operation' WHERE session_id=?1", [id]).unwrap();
            }
            let (operations, close_operations) =
                mj_controller::database::load_move_recovery().unwrap();
            assert!(!close_operations.contains(&operation.operation_id));
            let cancelled = test_runtime_state_loading_the_store();
            // Hard-won: 4c3c932: plain Move cancellation incorrectly granted Suspend authority after restart.
            let owners = cancelled
                .recover_moves(operations, &close_operations)
                .await
                .unwrap();
            assert!(owners.moves.contains(id));
            assert!(!owners.closes.contains(id));
            tokio::time::timeout(Duration::from_secs(30), async {
                while cancelled
                    .owner()
                    .lifecycle
                    .get(id)
                    .is_some_and(|active| active.is_running())
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let kept = mj_controller::database::load_session_record(id)
                .unwrap()
                .unwrap();
            assert_eq!(kept.state, SessionState::Error);
            assert_eq!(kept.target, source.target);
            assert!(source_root.join("uncheckpointed-work").exists());
            assert!(!destination_root.exists());
            let kept_move = mj_controller::database::load_move_operation(id)
                .unwrap()
                .unwrap();
            assert!(
                !kept_move.is_resolved(),
                "plain cancellation must keep Move retry authority"
            );
            assert!(matches!(
                kept_move.prepared_destination.unwrap().state,
                mj_core::state::PreparedDestinationState::Released
            ));
            continue;
        }
        let state = test_runtime_state_loading_the_store();
        let cloning = Arc::new(tokio::sync::Notify::new());
        let active_move = if restart {
            state.prepare_suspension(id).await.unwrap();
            // Even with every archive gone, GC must retain the durable owner
            // until close publishes its terminal outcome.
            let raw = rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
            let before_gc = mj_controller::database::load_move_operation(id)
                .unwrap()
                .unwrap();
            raw.execute(
                "UPDATE session_moves SET operation_json=json_set(operation_json, '$.checkpoint', NULL, '$.handoff', NULL, '$.prepared_destination', NULL) WHERE session_id=?1",
                [id],
            ).unwrap();
            mj_controller::database::reap_finished_move_intents().unwrap();
            assert!(
                mj_controller::database::load_move_operation(id)
                    .unwrap()
                    .is_some(),
                "pending close must retain its Move even without an archive"
            );
            mj_controller::database::save_move_operation(&before_gc).unwrap();
            let (operations, close_operations) =
                mj_controller::database::load_move_recovery().unwrap();
            let owned = state
                .recover_moves(operations, &close_operations)
                .await
                .unwrap();
            assert!(
                !owned.moves.contains(id),
                "startup must leave superseded Move to close"
            );
            assert!(owned.closes.contains(id));
            assert!(
                interrupted_suspend_session_ids(&Controller::load().unwrap())
                    .contains(&id.to_owned())
            );
            None
        } else {
            Some(
                // A close supersedes an automatic Move, such as quota
                // recovery's; it waits for a Move a person asked for.
                state
                    .start_automatic_lifecycle(id.into(), LifecycleKind::Move, {
                        let cloning = cloning.clone();
                        move |_, _, cancelled, _session_admission| async move {
                            // Hold the destination-cloning cut before any worker is installed.
                            cloning.notify_one();
                            while !cancelled.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                            operation.phase = mj_core::state::MovePhase::Cancelled;
                            operation.cancellation_requested = true;
                            mj_controller::database::save_move_operation(&operation)?;
                            bail!("clone cancelled by Suspend")
                        }
                    })
                    .await
                    .unwrap(),
            )
        };
        if !restart {
            cloning.notified().await;
        }
        let result =
            tokio::time::timeout(Duration::from_secs(30), state.suspend_session(id.into()))
                .await
                .unwrap();
        assert_eq!(result.is_err(), retained_workspace, "{result:?}");
        if let Some(active_move) = active_move {
            assert!(
                RuntimeState::wait_lifecycle_result(active_move)
                    .await
                    .is_err()
            );
        }
        let stored = mj_controller::database::load_state().unwrap();
        let record = &stored.sessions[id];
        assert_eq!(
            record.state,
            if retained_workspace {
                SessionState::Error
            } else {
                SessionState::Stopped
            }
        );
        assert!(checkpoint.archive_path.exists());
        assert_eq!(record.checkpoint.as_deref(), Some(&checkpoint));
        assert_eq!(
            destination_root.exists(),
            !sealed,
            "an unsealed source keeps every placement; a sealed source permits destination cleanup ({id})"
        );
        let settled = mj_controller::database::load_move_operation(id)
            .unwrap()
            .unwrap();
        assert!(
            !mj_controller::database::load_move_recovery()
                .unwrap()
                .1
                .contains(&settled.operation_id),
            "terminal outcome must consume Suspend intent"
        );
        if retained_workspace && sealed {
            assert!(matches!(
                settled.prepared_destination.as_ref().unwrap().state,
                mj_core::state::PreparedDestinationState::Released
            ));
            let calls =
                std::fs::read_to_string(std::env::var_os("MJ_TEST_MOVE_AWS_LOG").unwrap()).unwrap();
            assert!(
                calls
                    .lines()
                    .any(|call| call.contains("terminate-instances") && call.contains(id)),
                "{calls}"
            );
            assert!(
                calls
                    .lines()
                    .any(|call| call.contains("instance-terminated") && call.contains(id)),
                "{calls}"
            );
        }
        assert_eq!(settled.is_resolved(), sealed);
        assert!(
            !settled.holds_source_environment(),
            "Resume must not be blocked by Move"
        );
        if !sealed {
            assert_eq!(record.target, destination.target);
            assert!(source_root.join("uncheckpointed-work").exists());
            let message = record.public_error().unwrap();
            assert!(
                message.contains("could not establish a sealed Move checkpoint"),
                "{message}"
            );
            assert!(message.contains("Resume the session"), "{message}");
            assert!(message.contains("Reference "), "{message}");
        } else if retained_workspace {
            assert_eq!(record.target, source.target);
            assert_eq!(
                std::fs::read_to_string(source_root.join("uncheckpointed-work")).unwrap(),
                "keep source workspace"
            );
            let message = record.public_error().unwrap();
            assert!(message.contains("source workspace was kept"), "{message}");
            assert!(message.contains("Resume the session"), "{message}");
            assert!(message.contains("Reference "), "{message}");
        } else {
            assert!(record.target.is_none());
            assert!(!source_root.exists());
        }
    }
    // Historical Moves must not turn a workerless cleanup into interrupted
    // checkpoint recovery merely because their rows are still retained.
    let historical =
        mj_controller::database::load_move_operation("15420000000000000000000000000008")
            .unwrap()
            .unwrap();
    for (index, lifecycle) in [
        SessionState::Provisioning,
        SessionState::Parked,
        SessionState::StartupCleanup,
    ]
    .into_iter()
    .enumerate()
    {
        for phase in [
            mj_core::state::MovePhase::Completed,
            mj_core::state::MovePhase::Cancelled,
        ] {
            let root = tempfile::tempdir().unwrap();
            let id = format!(
                "1542{:028x}",
                16 + 2 * index + usize::from(phase == mj_core::state::MovePhase::Cancelled)
            );
            let worker_root = root.path().join(&id);
            std::fs::create_dir(&worker_root).unwrap();
            std::fs::write(worker_root.join("partial-checkout"), "unfinished startup").unwrap();
            let mut record = runtime_test_session(&id, &workspace.id, lifecycle);
            record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
                worker_root: worker_root.clone(),
            }));
            mj_controller::database::save_session(&record).unwrap();
            if index == 0 && phase == mj_core::state::MovePhase::Completed {
                for moving_state in [SessionState::Provisioning, SessionState::Closing] {
                    record.state = moving_state;
                    mj_controller::database::save_lifecycle_session(&record).unwrap();
                    let mut pending = retained_startup_test_move(&record);
                    pending.operation_id = format!("unaccepted-{id}");
                    mj_controller::database::save_move_operation(&pending).unwrap();
                    mj_controller::controller::move_session::restore_move_queue_hold(&pending);
                    let refused = test_runtime_state_loading_the_store();
                    let error = match refused.start_suspend_session(id.clone(), true, false).await {
                        Err(error) => error,
                        Ok(mut started) => started.confirm_acceptance().await.unwrap_err(),
                    };
                    assert!(error.to_string().contains("Move queue admission"));
                    let unchanged = mj_controller::database::load_session_record(&id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        unchanged.state, moving_state,
                        "an unaccepted close cannot change another lifecycle's state"
                    );
                    assert_eq!(unchanged.target, record.target);
                    assert_eq!(unchanged.last_error, record.last_error);
                    assert_eq!(unchanged.updated_at, record.updated_at);
                    assert!(worker_root.exists());
                    mj_controller::controller::move_session::release_move_queue_hold(&id);
                }
                record.state = lifecycle;
                mj_controller::database::save_lifecycle_session(&record).unwrap();
            }
            let mut completed = historical.clone();
            completed.selection.session_id = id.clone();
            completed.operation_id = format!("historical-{id}");
            completed.phase = phase;
            completed.queue_admission_started = false;
            completed.queue_admission_finished = true;
            completed.prepared_destination = None;
            mj_controller::database::save_move_operation(&completed).unwrap();
            // A crash during a later ordinary close must leave close recovery
            // in charge even while its historical Move row remains.
            record.state = SessionState::Closing;
            mj_controller::database::save_lifecycle_session(&record).unwrap();
            for exact_historical_intent in [false, true] {
                let recovering = test_runtime_state_loading_the_store();
                let close_ids = if exact_historical_intent {
                    BTreeSet::from([completed.operation_id.clone()])
                } else {
                    BTreeSet::new()
                };
                let owners = recovering
                    .recover_moves(vec![completed.clone()], &close_ids)
                    .await
                    .unwrap();
                assert!(!owners.moves.contains(&id));
                assert!(!owners.closes.contains(&id));
                assert!(
                    interrupted_suspend_session_ids(&Controller::load().unwrap()).contains(&id)
                );
            }
            record.state = lifecycle;
            mj_controller::database::save_lifecycle_session(&record).unwrap();
            let runtime = test_runtime_state_loading_the_store();
            if lifecycle == SessionState::Provisioning {
                // An older writer can preserve a now-historical exact marker.
                // It cannot authorize a rejected action to fail a new startup.
                let raw =
                    rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
                raw.execute("UPDATE session_moves SET close_requested_operation_id=operation_id WHERE session_id=?1", [&id]).unwrap();
                let before = mj_controller::database::load_session_record(&id)
                    .unwrap()
                    .unwrap();
                let cancelled = LifecycleControl::default();
                cancelled.cancelled.store(true, Ordering::Release);
                assert!(
                    runtime
                        .start_suspend_session_controlled(id.clone(), true, false, cancelled)
                        .await
                        .is_err()
                );
                let unchanged = mj_controller::database::load_session_record(&id)
                    .unwrap()
                    .unwrap();
                assert_eq!(unchanged.state, before.state);
                assert_eq!(unchanged.last_error, before.last_error);
                assert_eq!(unchanged.updated_at, before.updated_at);
                assert_eq!(raw.query_row("SELECT close_requested_operation_id FROM session_moves WHERE session_id=?1", [&id], |row| row.get::<_, String>(0)).unwrap(), completed.operation_id);
            }
            runtime.prepare_suspension(&id).await.unwrap();
            assert_eq!(
                mj_controller::database::load_session_record(&id)
                    .unwrap()
                    .unwrap()
                    .state,
                lifecycle,
                "resolved Move must preserve the existing cleanup route"
            );
            assert!(
                !mj_controller::database::load_move_recovery()
                    .unwrap()
                    .1
                    .contains(&completed.operation_id)
            );
            tokio::time::timeout(Duration::from_secs(30), runtime.suspend_session(id.clone()))
                .await
                .unwrap()
                .unwrap();
            let closed = mj_controller::database::load_session_record(&id)
                .unwrap()
                .unwrap();
            assert_eq!(closed.state, SessionState::Stopped);
            assert!(closed.target.is_none());
            assert!(!worker_root.exists());
        }
    }
}

/// #1191: `mj destroy` is answered "accepted" before the destroy finishes,
/// and a parent's destroy (its sub-agents first, then its own worker) can
/// still be running when `mj daemon restart` or `mj daemon stop` asks the
/// daemon to stop. Nothing about the destroy is durable, so the graceful stop
/// waits for it: the daemon stops only once the destroy has finished, and the
/// upgrade handoff names it as work it waits for.
#[cfg(unix)]
// Hard-won: bcbba06e: daemon restart during a non-durable destroy resurrected its parent session.
#[tokio::test]
async fn a_graceful_stop_waits_for_a_destroy_in_flight() {
    const NAME: &str = "a_graceful_stop_waits_for_a_destroy_in_flight";
    const CHILD: &str = "MJ_TEST_STOP_DRAINS_DESTROY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .env(
            mj_core::config::SESSION_INDEX_ENV,
            directory.path().join("sessionwiki"),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Destroy drain").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "11111111111111111111111111111111";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.project_directory = Some(root.path().join("project"));
    let reports = root.path().join("project/.mj/agents");
    std::fs::create_dir_all(reports.join(child_id)).unwrap();
    std::fs::write(
        reports.join(child_id).join("report.log"),
        "destroy evidence",
    )
    .unwrap();
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let child = runtime_test_session(child_id, &workspace.id, SessionState::Running);
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent(child_id, parent_id),
    )
    .unwrap();

    mj_controller::database::record_subagent_report_dir(
        child_id,
        &reports.join(child_id).to_string_lossy(),
    )
    .unwrap();
    let state = test_runtime_state_loading_the_store();
    let metadata = test_metadata("127.0.0.1:1".parse().unwrap());
    let shutdown = CancellationToken::new();
    // An operation on the parent that does not stop when cancelled holds the
    // destroy after its sub-agent is gone and before the parent's worker is
    // stopped, the stretch a Codex parent's destroy was in when the restart
    // came.
    let release = Arc::new(tokio::sync::Notify::new());
    state
        .start_or_join_lifecycle(parent_id.into(), LifecycleKind::Resume, {
            let release = release.clone();
            move |_state, _session_id, _cancelled, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let destroy = mj_core::runtime::spawn({
        let state = state.clone();
        async move {
            state
                .force_destroy_session(parent_id.to_owned(), BranchDisposition::Keep)
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if mj_controller::upgrade::gate()
            .active_labels()
            .iter()
            .any(|label| label.starts_with("session destroy"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Destroy must hold handoff admission while preempting earlier work"
        );
        tokio::task::yield_now().await;
    }
    assert!(
        mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(child_id),
        "preemption completes before the atomic child snapshot"
    );

    let blockers = match handle_action(DaemonAction::UpgradeBlockers, &metadata, &state, &shutdown)
        .await
        .unwrap()
    {
        DaemonReply::UpgradeBlockers(labels) => labels,
        other => panic!("expected named blockers, got {other:?}"),
    };
    assert!(
        blockers
            .iter()
            .any(|label| label.starts_with("session destroy")),
        "the upgrade handoff waits for the destroy: {blockers:?}"
    );
    assert!(matches!(
        handle_action(DaemonAction::Stop, &metadata, &state, &shutdown)
            .await
            .unwrap(),
        DaemonReply::Done
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !shutdown.is_cancelled(),
        "the daemon does not stop under a destroy it accepted"
    );
    assert!(!destroy.is_finished());

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(30), shutdown.cancelled())
        .await
        .expect("the daemon stops once the destroy is done");
    assert!(
        destroy.is_finished(),
        "the destroy finished before the stop"
    );
    destroy.await.unwrap().unwrap();
    assert!(
        !mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(parent_id)
    );
    assert!(
        !reports.exists(),
        "parent destroy removes its children's reports and empty root"
    );
}

/// Suspending a parent stops its sub-agents instead of suspending them, and
/// checkpoints only the parent. Each child is removed the way a destroy
/// removes one, after SessionWiki has taken its conversation, and the parent's
/// record lists what was stopped for its model. A child that cannot be
/// stopped retains its ownership and prevents shared-file retirement.
#[cfg(unix)]
#[tokio::test]
async fn suspending_a_parent_stops_its_sub_agents_and_lists_them_on_the_parent() {
    const NAME: &str = "suspending_a_parent_stops_its_sub_agents_and_lists_them_on_the_parent";
    const CHILD: &str = "MJ_TEST_SUSPEND_STOPS_SUBAGENTS";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        // SessionWiki walks every harness store under the home directory, so
        // the child process gets an empty home and an index of its own.
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .env(
            mj_core::config::SESSION_INDEX_ENV,
            directory.path().join("sessionwiki"),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .run();
        return;
    }
    use mj_core::subagent::StoppedSubagent;

    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Parent suspension").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let finished_id = "11111111111111111111111111111111";
    let stuck_id = "22222222222222222222222222222222";
    let oracle_id = "33333333333333333333333333333333";
    // A parent with nothing to checkpoint yet, so its own suspend settles
    // without a worker to talk to.
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Provisioning);
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();

    // A child that finished its task and handed back its report.
    let mut finished = runtime_test_session(finished_id, &workspace.id, SessionState::Running);
    finished.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(finished_id),
    }));
    finished.session_title_override = Some("Fix the parser".into());
    let mut relation = runtime_test_subagent(finished_id, parent_id);
    relation.initial_prompt = "Fix the off-by-one in the parser.".into();
    relation.handback_tool = true;
    mj_controller::database::save_subagent_session(&finished, &relation).unwrap();
    let mut conversation = mj_core::state::MaterializedSession::empty(finished_id);
    conversation.applied_event_ordinal = 3;
    conversation.applied_event_digest = format!("{:064x}", 3);
    conversation.last_activity_at_ms = Some(1_700_000_000_003);
    for (position, body) in [
        mj_core::transcript::TranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": "fix the parser"})],
        },
        mj_core::transcript::TranscriptBody::Agent {
            chunks: vec![serde_json::json!({"content": {"type": "text", "text": "fixed"}})],
            streaming: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let position = position as u64 + 1;
        let streamed = matches!(body, mj_core::transcript::TranscriptBody::Agent { .. });
        conversation
            .transcript
            .push(Arc::new(mj_core::transcript::TranscriptItem {
                stable_id: format!("item-{position}"),
                position,
                latest_content_event_ordinal: streamed.then_some(position),
                created_at_ms: 1_700_000_000_000 + position as i64,
                last_changed_at_ms: 1_700_000_000_000 + position as i64,
                body,
            }));
    }
    conversation.last_turn_outcome = Some(mj_core::state::MaterializedTurnOutcome {
        work_facts: None,
        answered_through: None,
        diagnostic: None,
        usage: None,
        command_id: "task-1".into(),
        accepted_ordinal: Some(1),
        turn_start_position: Some(1),
        completed_ordinal: 3,
        completed_at_ms: 1_700_000_000_003,
        outcome: mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: "end_turn".into(),
        },
    });
    mj_controller::database::save_materialized_session(&conversation).unwrap();
    assert!(
        mj_controller::database::record_subagent_handback(
            finished_id,
            &mj_core::subagent::SubagentHandback {
                command_id: "task-1".into(),
                message: "Fixed the parser.".into(),
                recorded_at_ms: 1_700_000_000_002,
            },
        )
        .unwrap()
    );

    // A child still at work on an SSH target whose settings are gone from
    // the configuration, so the ordinary stop cannot reach it.
    let mut stuck = runtime_test_session(stuck_id, &workspace.id, SessionState::Running);
    stuck.title = "Review the docs".into();
    stuck.target_template_id = "removed-target".into();
    stuck.target = Some(Box::new(mj_core::state::TargetLocator::SshBare {
        host: "builder.invalid".into(),
        workspace: PathBuf::from(format!(".local/share/hel/workspaces/{stuck_id}")),
        worker_id: Some(stuck_id.into()),
    }));
    mj_controller::database::save_subagent_session(
        &stuck,
        &runtime_test_subagent(stuck_id, parent_id),
    )
    .unwrap();
    // The primary's oracle, working like the child above. It is stopped with
    // the rest but never named to the primary.
    let mut oracle = runtime_test_session(oracle_id, &workspace.id, SessionState::Running);
    oracle.title = "oracle".into();
    oracle.target_template_id = "removed-target".into();
    oracle.target = Some(Box::new(mj_core::state::TargetLocator::SshBare {
        host: "builder.invalid".into(),
        workspace: PathBuf::from(format!(".local/share/hel/workspaces/{oracle_id}")),
        worker_id: Some(oracle_id.into()),
    }));
    let mut oracle_relation = runtime_test_subagent(oracle_id, parent_id);
    oracle_relation.kind = mj_core::subagent::SubagentKind::Oracle;
    mj_controller::database::save_subagent_session(&oracle, &oracle_relation).unwrap();
    // The index exists but has not seen either child yet.
    sessionwiki::index::open().unwrap();

    let state = test_runtime_state_loading_the_store();
    let error = tokio::time::timeout(
        Duration::from_secs(60),
        state.suspend_session(parent_id.to_owned()),
    )
    .await
    .unwrap()
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("stop sub-agent"), "{message}");
    assert!(message.contains(stuck_id), "{message}");
    assert!(message.contains(oracle_id), "{message}");

    let stored = mj_controller::database::load_state().unwrap();
    assert!(
        !stored.sessions.contains_key(finished_id) && !stored.subagents.contains_key(finished_id),
        "a succeeded child stop still settles and removes the child when siblings fail"
    );
    assert_ne!(stored.sessions[parent_id].state, SessionState::Stopped);
    for child in [stuck_id, oracle_id] {
        assert!(
            stored.sessions.contains_key(child) && stored.subagents.contains_key(child),
            "failed stops preserve retryable ownership"
        );
    }
    let owner = mj_controller::database::session_environment(parent_id)
        .unwrap()
        .unwrap();
    assert!(
        mj_controller::database::environment_workers(&owner.id)
            .unwrap()
            .iter()
            .any(|worker| worker.session_id == stuck_id)
    );
    assert_eq!(
        mj_controller::database::load_stopped_subagents(parent_id).unwrap(),
        [
            StoppedSubagent {
                child_session_id: finished_id.into(),
                title: "Fix the parser".into(),
                task: Some("Fix the off-by-one in the parser.".into()),
                handed_back: true,
            },
            StoppedSubagent {
                child_session_id: stuck_id.into(),
                title: "Review the docs".into(),
                task: Some("do the task".into()),
                handed_back: false,
            },
            // The oracle is told to the primary as one line, not by its ID.
            StoppedSubagent::oracle(),
        ]
    );
    // Child transcripts are deliberately excluded from SessionWiki.
    assert!(
        state
            .wiki_session(finished_id.to_owned())
            .await
            .unwrap()
            .is_none()
    );
}

/// A finished reviewer is destroyed, not left as a stopped sub-agent of the
/// session that asked for the review (the shape the earlier suspend-only close
/// left behind). Its usage stays in the accounting table.
#[cfg(unix)]
#[tokio::test]
async fn destroying_a_finished_reviewer_removes_it_and_keeps_its_accounting() {
    const NAME: &str = "destroying_a_finished_reviewer_removes_it_and_keeps_its_accounting";
    const CHILD: &str = "MJ_TEST_DESTROY_FINISHED_REVIEWER";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .env(
            mj_core::config::SESSION_INDEX_ENV,
            directory.path().join("sessionwiki"),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Reviewer destroy").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let reviewer_id = "44444444444444444444444444444444";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let reviewer = runtime_test_session(reviewer_id, &workspace.id, SessionState::Stopped);
    let mut relation = runtime_test_subagent(reviewer_id, parent_id);
    relation.kind = mj_core::subagent::SubagentKind::Reviewer;
    relation.task_name = "review".into();
    mj_controller::database::save_subagent_session(&reviewer, &relation).unwrap();

    let state = test_runtime_state_loading_the_store();
    tokio::time::timeout(
        Duration::from_secs(60),
        state.destroy_reviewer_child(reviewer_id.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();

    let stored = mj_controller::database::load_state().unwrap();
    assert!(!stored.sessions.contains_key(reviewer_id));
    assert!(!stored.subagents.contains_key(reviewer_id));
    assert!(stored.sessions.contains_key(parent_id));
    let connection = rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
    let accounted: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM subagent_accounting WHERE child_session_id=?1 AND task_name='review'",
            [reviewer_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accounted, 1);
}

/// Marks the isolated child a [`live_parent_with_a_working_sub_agent`] test
/// runs in.
#[cfg(unix)]
const LIVE_PARENT_CHILD: &str = "MJ_TEST_LIVE_PARENT_CHILD";

/// The sub-agent still at work in [`live_parent_with_a_working_sub_agent`].
#[cfg(unix)]
const WORKING_CHILD: &str = "33333333333333333333333333333333";

/// Run the named test alone in a child of this test binary, with its own
/// store and SessionWiki index, and with the stand-in relay advancing its own
/// Close, so a sealed parent needs no harness behind it. Returns whether this
/// process is that child.
#[cfg(unix)]
fn in_isolated_live_parent_test(name: &str) -> bool {
    if std::env::var_os(LIVE_PARENT_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    mj_controller::controller::test_support::IsolatedTest::new(
        mj_controller::controller::test_support::test_name(module_path!(), name),
    )
    .env(LIVE_PARENT_CHILD, "1")
    .env(
        mj_controller::controller::checkpoint::stand_in::LATCH_CHECKPOINT_ONLY,
        "1",
    )
    .isolated_store(directory.path())
    .env(
        mj_core::config::SESSION_INDEX_ENV,
        directory.path().join("sessionwiki"),
    )
    .env("HOME", &home)
    .env("XDG_DATA_HOME", home.join(".local/share"))
    .env("XDG_CONFIG_HOME", home.join(".config"))
    .run();
    false
}

/// Whether the parent in [`live_parent_with_a_working_sub_agent`] already has
/// a recovery copy.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentRecoveryCopy {
    /// The unchanged relay lets a close reuse it, so the checkpoint succeeds.
    Installed,
    /// A close has to export one, and the export fails.
    Missing,
}

#[cfg(unix)]
struct LiveParent {
    _directory: tempfile::TempDir,
    relay_root: PathBuf,
    channels: mj_controller::session_manager::SessionManagerChannels,
    state: Arc<RuntimeState>,
}

#[cfg(unix)]
impl LiveParent {
    fn relay_state(&self) -> String {
        std::fs::read_to_string(self.relay_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap()
    }

    /// The daemon's projected transcript. The worker collects journal files
    /// once the daemon acknowledges them (b370417ba), so the store is where
    /// a journaled notice lasts.
    fn transcript(&self) -> String {
        use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
        serde_json::to_string(
            &mj_controller::database::load_materialized_session(LATCH_RELAY_SESSION)
                .unwrap()
                .expect("the daemon keeps the parent's projection")
                .transcript,
        )
        .unwrap()
    }
}

/// Whether a close sealed the relay at `relay_root`: its own snapshot is
/// closing or closed. A refused or interrupted close returns it to idle. The
/// journal cannot answer this, because acknowledged files are collected.
#[cfg(unix)]
fn relay_sealed(relay_root: &Path) -> bool {
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(relay_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    [
        mj_core::relay::RelayExecutionState::Closing,
        mj_core::relay::RelayExecutionState::Closed,
    ]
    .iter()
    .any(|sealed| state["execution"] == serde_json::to_value(sealed).unwrap())
}

/// A live parent with one Mjolnir sub-agent at work ("Review the docs"),
/// served by the stand-in relay of the checkpoint suite. A suspend takes a
/// real checkpoint of it. Without a recovery copy to reuse, the export runs
/// `hel` in the parent's worker root, which fails the way a full disk does.
#[cfg(unix)]
async fn live_parent_with_a_working_sub_agent(recovery_copy: ParentRecoveryCopy) -> LiveParent {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    // A LocalBare worker root ends in its session id.
    let worker_root = directory.path().join(LATCH_RELAY_SESSION);
    let relay_root = directory.path().join("relay");
    let archives = directory.path().join("archives");
    let checkout = directory.path().join("checkout");
    for path in [&worker_root, &relay_root, &archives, &checkout] {
        std::fs::create_dir_all(path).unwrap();
    }
    seed_live_session(directory.path(), &relay_root);
    let hel = worker_root.join("hel");
    std::fs::write(
        &hel,
        format!(
            "#!/bin/sh\n[ \"$2\" = stop ] && exit 0\n\
             [ \"$2\" = proxy ] && {{ {proxy}; exit 0; }}\n\
             cat >/dev/null\necho 'No space left on device' >&2\nexit 1\n",
            proxy = mj_controller::controller::checkpoint::stand_in::latch_relay_proxy_command(
                &relay_root
            ),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hel, std::fs::Permissions::from_mode(0o755)).unwrap();

    let workspace = mj_controller::database::create_workspace("Parent suspension").unwrap();
    let mut parent =
        mj_controller::controller::test_support::checkpoint_test_session(LATCH_RELAY_SESSION);
    parent.workspace_id = workspace.id.clone();
    parent.target_template_id = "removed-local".into();
    parent.target_runtime = Some((&mj_core::config::TargetTemplate::LocalBare).into());
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    parent.project_directory = Some(checkout);
    if recovery_copy == ParentRecoveryCopy::Installed {
        parent.checkpoint = Some(Box::new(
            mj_controller::controller::test_support::write_checkpoint_gate_archive(
                &archives,
                LATCH_RELAY_SESSION,
                2,
            ),
        ));
    }
    mj_controller::database::save_session(&parent).unwrap();
    mj_controller::database::save_materialized_session(
        &mj_core::state::MaterializedSession::empty(LATCH_RELAY_SESSION),
    )
    .unwrap();

    let mut child = runtime_test_session(WORKING_CHILD, &workspace.id, SessionState::Running);
    child.title = "Review the docs".into();
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: directory.path().join(WORKING_CHILD),
    }));
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent(WORKING_CHILD, LATCH_RELAY_SESSION),
    )
    .unwrap();

    let reports = parent
        .project_directory
        .as_ref()
        .unwrap()
        .join(".mj/agents")
        .join(WORKING_CHILD);
    std::fs::create_dir_all(&reports).unwrap();
    std::fs::write(
        reports.join("evidence.log"),
        "child evidence survives capture failure",
    )
    .unwrap();
    mj_controller::database::record_subagent_report_dir(WORKING_CHILD, reports.to_str().unwrap())
        .unwrap();

    let (channels, state) = serve_live_session(&relay_root, false).await;
    LiveParent {
        _directory: directory,
        relay_root,
        channels,
        state,
    }
}

/// Seed the stand-in relay of a live session that has opened its native
/// session, and give the controller the Codex profile the session uses.
#[cfg(unix)]
fn seed_live_session(directory: &Path, relay_root: &Path) {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;

    let mut seed =
        mj_worker::relay::DurableRelay::open(relay_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionOpened {
        native_session_id: "native-session".into(),
        native_continuity_lost: false,
        replaced_unused_native_session_id: None,
        resumed: true,
    })
    .unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionConfigured {
        config_options: Vec::new(),
    })
    .unwrap();
    drop(seed);

    let profile_home = directory.join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    Config::update(|config| {
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    profile_home,
                )
            },
        );
        Ok(())
    })
    .unwrap();
}

/// Serve the live session from the stand-in relay and start a daemon runtime
/// over the store, which must already hold every record the test needs.
#[cfg(unix)]
async fn serve_live_session(
    relay_root: &Path,
    running: bool,
) -> (
    mj_controller::session_manager::SessionManagerChannels,
    Arc<RuntimeState>,
) {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_SESSION, ReleaseSupport, latch_relay_target,
    };

    let channels = mj_controller::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            relay_root,
            None,
            ReleaseSupport::Supported,
            running,
        )])
        .unwrap();
    channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    let recovery = channels.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(channels.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        channels.control.clone(),
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    (channels, state)
}

/// Marks the stand-in worker's run of [`stand_in_worker_exports_a_checkpoint`].
#[cfg(unix)]
const STAND_IN_EXPORT: &str = "MJ_TEST_STAND_IN_EXPORT";

/// The worker's `export-checkpoint`, run from this test binary for the
/// stand-in worker of [`live_clone_with_an_unpushed_commit`], because unit
/// tests have no worker binary. It does nothing unless that stand-in runs it.
#[cfg(unix)]
#[test]
fn stand_in_worker_exports_a_checkpoint() {
    if std::env::var_os(STAND_IN_EXPORT).is_none() {
        return;
    }
    // With `--nocapture` libtest writes `test <name> ... ` without a
    // trailing newline before the body runs. End that line first so the
    // result is a line of its own.
    println!();
    let exported =
        mj_worker::checkpoint::export_from_spec_reader(&mut std::io::stdin().lock()).unwrap();
    println!("{}", serde_json::to_string(&exported).unwrap());
}

#[cfg(unix)]
struct LiveClone {
    _source: tempfile::TempDir,
    _directory: tempfile::TempDir,
    checkout: PathBuf,
    worker_root: PathBuf,
    unpushed_commit: String,
    relay_root: PathBuf,
    channels: mj_controller::session_manager::SessionManagerChannels,
    state: Arc<RuntimeState>,
}

#[cfg(unix)]
impl LiveClone {
    /// Whether the session's relay was sealed by a close.
    fn sealed(&self) -> bool {
        relay_sealed(&self.relay_root)
    }
}

/// A live session on an isolated clone whose last commit is not on the
/// clone's origin, served by the stand-in relay. Its stand-in worker runs the
/// real checkpoint export, so a suspend checks the clone's publication against
/// a real archive and a real origin.
#[cfg(unix)]
async fn live_clone_with_an_unpushed_commit() -> LiveClone {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    use mj_controller::controller::test_support::{committed_repository, test_git};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    // A LocalBare worker root ends in its session id, and its harness home is
    // the worker root's `profile` directory.
    let worker_root = directory.path().join(LATCH_RELAY_SESSION);
    let relay_root = directory.path().join("relay");
    for path in [&worker_root.join("profile"), &relay_root] {
        std::fs::create_dir_all(path).unwrap();
    }
    seed_live_session(directory.path(), &relay_root);
    let hel = worker_root.join("hel");
    std::fs::write(
        &hel,
        format!(
            "#!/bin/sh\n\
             [ \"$2\" = stop ] && exit 0\n\
             [ \"$2\" = proxy ] && {{ {proxy}; exit 0; }}\n\
             [ \"$1 $2\" = 'worker export-checkpoint' ] || \
             {{ echo \"the stand-in worker only exports: $*\" >&2; exit 2; }}\n\
             {STAND_IN_EXPORT}=1 '{program}' --exact '{test}' --nocapture | grep '^{{'\n",
            proxy = mj_controller::controller::checkpoint::stand_in::latch_relay_proxy_command(
                &relay_root
            ),
            program = std::env::current_exe().unwrap().display(),
            test = mj_controller::controller::test_support::test_name(
                module_path!(),
                "stand_in_worker_exports_a_checkpoint",
            ),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hel, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The project's `master` is on its origin, and the session's clone has one
    // commit of its own that is not.
    let source = committed_repository();
    let origin = directory.path().join("origin.git");
    test_git(
        directory.path(),
        &["init", "--bare", "--initial-branch=master", "origin.git"],
    );
    test_git(
        source.path(),
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    test_git(source.path(), &["push", "origin", "master"]);
    let mut session = mj_controller::controller::test_support::managed_clone_session(
        source.path(),
        LATCH_RELAY_SESSION,
    );
    let checkout = session.project_directory.clone().unwrap();
    test_git(&checkout, &["config", "user.name", "Hel Tests"]);
    test_git(&checkout, &["config", "user.email", "hel@example.invalid"]);
    std::fs::write(checkout.join("session.txt"), "work\n").unwrap();
    test_git(&checkout, &["add", "."]);
    test_git(&checkout, &["commit", "-m", "session work"]);
    let unpushed_commit = test_git(&checkout, &["rev-parse", "HEAD"]);

    let workspace = mj_controller::database::create_workspace("Clone suspension").unwrap();
    session.workspace_id = workspace.id.clone();
    session.state = SessionState::Running;
    session.target_template_id = "removed-local".into();
    session.target_runtime = Some((&mj_core::config::TargetTemplate::LocalBare).into());
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    mj_controller::database::save_session(&session).unwrap();
    mj_controller::database::save_materialized_session(
        &mj_core::state::MaterializedSession::empty(LATCH_RELAY_SESSION),
    )
    .unwrap();

    let (channels, state) = serve_live_session(&relay_root, false).await;
    LiveClone {
        _source: source,
        _directory: directory,
        checkout,
        worker_root,
        unpushed_commit,
        relay_root,
        channels,
        state,
    }
}

/// R2-3 and R13-5: a suspend of a live clone with unpushed work, sent
/// without the acknowledgement, is refused by the controller's own check
/// after its checkpoint, not only by the API's early refusal. The daemon
/// closes a live session through the route that finishes an interrupted
/// close, which acknowledged on the caller's behalf. The refused session is
/// running again, not left `Closing` for the next start to close, with its
/// relay open and its checkout and target in place, and a suspend that
/// acknowledges then goes through.
#[cfg(unix)]
// Hard-won: f1145012: a live clone with unpushed work was suspended without acknowledgement.
#[tokio::test]
async fn a_suspend_without_the_acknowledgement_refuses_a_live_clone_with_unpushed_work() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    if !in_isolated_live_parent_test(
        "a_suspend_without_the_acknowledgement_refuses_a_live_clone_with_unpushed_work",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let clone = live_clone_with_an_unpushed_commit().await;

    let refusal = tokio::time::timeout(
        Duration::from_secs(60),
        clone
            .state
            .suspend_session_with_ack(LATCH_RELAY_SESSION.to_owned(), false),
    )
    .await
    .unwrap()
    .expect_err("a suspend without the acknowledgement must be refused");
    assert!(
        format!("{refusal:#}").contains("acknowledge_unpublished_work"),
        "the refusal names the acknowledgement: {refusal:#}"
    );
    let stored = mj_controller::database::load_state().unwrap();
    let record = &stored.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Running, "{record:?}");
    assert!(record.target.is_some(), "{record:?}");
    assert!(!clone.sealed(), "a refused suspend must not seal the relay");
    assert!(clone.worker_root.is_dir());
    assert_eq!(
        mj_controller::controller::test_support::test_git(&clone.checkout, &["rev-parse", "HEAD"]),
        clone.unpushed_commit
    );

    // An internal suspend (workspace close, recovery, a sub-agent's
    // teardown) acknowledges on its own, as a caller with the flag does.
    tokio::time::timeout(
        Duration::from_secs(60),
        clone.state.suspend_session(LATCH_RELAY_SESSION.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    let record = &stored.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Stopped, "{record:?}");
    assert!(clone.sealed());
    clone.channels.shutdown.shutdown().await.unwrap();
}

/// Mjolnir's own review refs are never pushed, and a review baseline names a
/// bare tree. A clone whose work is all published still suspends without the
/// acknowledgement when it carries them.
#[cfg(unix)]
// Hard-won: 02208d7b9: review refs saved in the checkpoint made every reviewed clone read as unpublished.
#[tokio::test]
async fn a_suspend_without_the_acknowledgement_ignores_review_refs_on_a_published_clone() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    use mj_controller::controller::test_support::test_git;
    if !in_isolated_live_parent_test(
        "a_suspend_without_the_acknowledgement_ignores_review_refs_on_a_published_clone",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let clone = live_clone_with_an_unpushed_commit().await;
    test_git(
        &clone.checkout,
        &["push", "origin", "HEAD:refs/heads/session"],
    );
    let tree = test_git(&clone.checkout, &["rev-parse", "HEAD^{tree}"]);
    test_git(
        &clone.checkout,
        &["update-ref", "refs/mj/review-baseline", &tree],
    );
    test_git(
        &clone.checkout,
        &[
            "update-ref",
            &format!("refs/mj/reviewed/{tree}"),
            &clone.unpushed_commit,
        ],
    );

    tokio::time::timeout(
        Duration::from_secs(60),
        clone
            .state
            .suspend_session_with_ack(LATCH_RELAY_SESSION.to_owned(), false),
    )
    .await
    .unwrap()
    .expect("published work needs no acknowledgement");
    let stored = mj_controller::database::load_state().unwrap();
    let record = &stored.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Stopped, "{record:?}");
    assert!(clone.sealed());
    clone.channels.shutdown.shutdown().await.unwrap();
}

/// A suspend whose checkpoint fails leaves the parent's sub-agents running,
/// and so has nothing to tell the parent or the person (R15-3). The children
/// stop only once the parent's checkpoint is verified.
#[cfg(unix)]
// Hard-won: d705260d: sub-agents were removed before the parent checkpoint had succeeded.
#[tokio::test]
async fn a_suspend_whose_checkpoint_fails_leaves_the_sub_agents_running() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    if !in_isolated_live_parent_test(
        "a_suspend_whose_checkpoint_fails_leaves_the_sub_agents_running",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Missing).await;

    let failure = tokio::time::timeout(
        Duration::from_secs(60),
        parent.state.suspend_session(LATCH_RELAY_SESSION.to_owned()),
    )
    .await
    .unwrap();
    assert!(failure.is_err());

    let stored = mj_controller::database::load_state().unwrap();
    let record = &stored.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Running, "{record:?}");
    assert!(
        record
            .last_checkpoint_error
            .as_deref()
            .is_some_and(|error| error.contains("No space left on device")),
        "the checkpoint's export is what failed: {record:?}"
    );
    assert_eq!(stored.sessions[WORKING_CHILD].state, SessionState::Running);
    assert!(stored.subagents.contains_key(WORKING_CHILD));
    assert!(
        parent
            ._directory
            .path()
            .join("checkout/.mj/agents")
            .join(WORKING_CHILD)
            .join("evidence.log")
            .is_file(),
        "a failed parent checkpoint must leave child reports untouched"
    );
    assert!(
        mj_controller::database::load_stopped_subagents(LATCH_RELAY_SESSION)
            .unwrap()
            .is_empty()
    );
    assert!(!parent.relay_state().contains("<mj-stopped-subagents>"));
    assert!(!parent.transcript().contains("Suspend stopped"));
    parent.channels.shutdown.shutdown().await.unwrap();
}

/// A suspend still stops the parent's sub-agents and lists them, once the
/// parent's checkpoint is verified, and then seals the parent's relay.
#[cfg(unix)]
#[tokio::test]
async fn a_suspend_stops_the_sub_agents_once_the_parents_checkpoint_is_verified() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    use mj_core::subagent::StoppedSubagent;
    if !in_isolated_live_parent_test(
        "a_suspend_stops_the_sub_agents_once_the_parents_checkpoint_is_verified",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Installed).await;

    let report_root = parent._directory.path().join("checkout/.mj/agents");
    let stopped_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let unrelated_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let parent_record =
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    let stopped = runtime_test_session(
        stopped_id,
        &parent_record.workspace_id,
        SessionState::Stopped,
    );
    mj_controller::database::save_subagent_session(
        &stopped,
        &runtime_test_subagent(stopped_id, LATCH_RELAY_SESSION),
    )
    .unwrap();
    mj_controller::database::record_subagent_report_dir(
        stopped_id,
        &report_root.join(stopped_id).to_string_lossy(),
    )
    .unwrap();
    for id in [stopped_id, unrelated_id] {
        std::fs::create_dir_all(report_root.join(id)).unwrap();
        std::fs::write(
            report_root.join(id).join("report.log"),
            "shared project evidence",
        )
        .unwrap();
    }
    // Reload so the pre-close relation snapshot includes an already-stopped child.
    parent.state.reload_controller().await.unwrap();

    tokio::time::timeout(
        Duration::from_secs(60),
        parent.state.suspend_session(LATCH_RELAY_SESSION.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();

    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert!(
        !stored.sessions.contains_key(WORKING_CHILD)
            && !stored.subagents.contains_key(WORKING_CHILD),
        "the sub-agent is removed, not suspended"
    );
    assert_eq!(
        mj_controller::database::load_stopped_subagents(LATCH_RELAY_SESSION).unwrap(),
        [StoppedSubagent {
            child_session_id: WORKING_CHILD.into(),
            title: "Review the docs".into(),
            task: Some("do the task".into()),
            handed_back: false,
        }]
    );
    assert!(!report_root.join(WORKING_CHILD).exists());
    assert!(
        !report_root.join(stopped_id).exists(),
        "the extra pass covers stopped children"
    );
    assert!(
        report_root.join(unrelated_id).join("report.log").is_file(),
        "another session's reports keep the shared project root alive"
    );

    // The parent went through a checkpointed close: its relay was sealed.
    assert!(relay_sealed(&parent.relay_root), "{}", parent.relay_state());
    parent.channels.shutdown.shutdown().await.unwrap();
}

/// Discarding the changes since a recovery copy stops the parent's
/// sub-agents the same way a suspend does. It is offered after a failed
/// suspend, which now leaves them running.
#[cfg(unix)]
#[tokio::test]
async fn discarding_changes_since_a_checkpoint_stops_the_sub_agents() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    if !in_isolated_live_parent_test("discarding_changes_since_a_checkpoint_stops_the_sub_agents") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Installed).await;
    let checkpoint = mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION]
        .checkpoint
        .clone()
        .unwrap();

    let bytes = std::fs::read(&checkpoint.archive_path).unwrap();
    std::fs::write(&checkpoint.archive_path, b"corrupt recovery archive").unwrap();
    let rejected = parent
        .state
        .start_session_lifecycle(SessionLifecycleRequest::Discard {
            session_id: LATCH_RELAY_SESSION.into(),
            checkpoint: *checkpoint.clone(),
        })
        .await
        .unwrap_err();
    assert!(format!("{rejected:#}").contains("SHA changed"));
    let unchanged = mj_controller::database::load_state().unwrap();
    assert_eq!(
        unchanged.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert!(unchanged.sessions.contains_key(WORKING_CHILD));
    std::fs::write(&checkpoint.archive_path, bytes).unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let child = parent
        .state
        .start_or_join_lifecycle(WORKING_CHILD.into(), LifecycleKind::StopSubagent, {
            let release = release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        parent
            .state
            .start_session_lifecycle(SessionLifecycleRequest::Discard {
                session_id: LATCH_RELAY_SESSION.into(),
                checkpoint: *checkpoint.clone(),
            }),
    )
    .await
    .expect("Discard acceptance waited for child teardown")
    .unwrap();
    let accepted = mj_controller::database::load_state().unwrap();
    assert_eq!(
        accepted.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Destroying
    );
    assert_eq!(
        accepted.sessions[LATCH_RELAY_SESSION].checkpoint.as_ref(),
        Some(&checkpoint)
    );
    assert!(accepted.sessions.contains_key(WORKING_CHILD));
    assert!(
        parent
            .state
            .cancel_lifecycle_with_intent(LATCH_RELAY_SESSION)
            .await
            .is_err()
    );
    let mut completion = Box::pin(parent.state.wait_session_lifecycle(receipt));
    assert!(futures::poll!(&mut completion).is_pending());
    release.notify_one();
    RuntimeState::wait_lifecycle_result(child).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(60), completion)
            .await
            .unwrap()
            .unwrap(),
        DaemonReply::Done
    ));

    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert!(
        !stored.sessions.contains_key(WORKING_CHILD)
            && !stored.subagents.contains_key(WORKING_CHILD)
    );
    let stopped = mj_controller::database::load_stopped_subagents(LATCH_RELAY_SESSION).unwrap();
    assert_eq!(
        stopped
            .iter()
            .map(|child| child.child_session_id.as_str())
            .collect::<Vec<_>>(),
        [WORKING_CHILD]
    );
    parent.channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1507: an accepted Discard failure disappeared when its observer left.
#[cfg(unix)]
#[tokio::test]
async fn accepted_discard_failure_is_published_without_an_observer() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    const NAME: &str = "accepted_discard_failure_is_published_without_an_observer";
    if !in_isolated_live_parent_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Installed).await;
    let checkpoint = mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION]
        .checkpoint
        .clone()
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let (acceptance, watch) = tokio::sync::watch::channel(Some(Ok(())));
    let child = parent
        .state
        .admit_prepared_lifecycle(
            WORKING_CHILD.into(),
            LifecycleKind::StopSubagent,
            super::lifecycle::LifecycleStart::request(
                serde_json::to_string(&(LifecycleKind::StopSubagent.label(), false)).unwrap(),
                None,
                None,
            ),
            Some(watch),
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    bail!("held child teardown diagnostic")
                }
            },
        )
        .await
        .unwrap();
    let child_result = child.result.clone();
    drop(child);
    let baseline = acceptance.receiver_count();
    let mut started = parent
        .state
        .admit_session_lifecycle(SessionLifecycleRequest::Discard {
            session_id: LATCH_RELAY_SESSION.into(),
            checkpoint: *checkpoint.clone(),
        })
        .await
        .unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(30), started.confirm_acceptance())
        .await
        .unwrap()
        .unwrap();
    let accepted =
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(accepted.state, SessionState::Destroying);
    assert_eq!(accepted.checkpoint.as_ref(), Some(&checkpoint));
    drop(started);
    tokio::time::timeout(Duration::from_secs(30), async {
        while acceptance.receiver_count() <= baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Discard never joined the held child stop");
    // No initiating result or acceptance observer remains when teardown fails.
    release.notify_one();
    assert!(
        RuntimeState::wait_lifecycle_result(child_result)
            .await
            .is_err()
    );
    let retained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let stored = mj_controller::database::load_state().unwrap();
            let record = &stored.sessions[LATCH_RELAY_SESSION];
            if record.public_error().is_some() {
                break record.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Discard failure was not persisted without an observer");
    let public = retained.public_error().unwrap();
    assert!(public.starts_with(mj_core::state::CLOSE_FAILURE_PREFIX));
    assert!(public.contains(&receipt.operation_id));
    assert!(!public.contains("held child teardown diagnostic"));
    assert_eq!(retained.state, SessionState::Destroying);
    assert_eq!(retained.checkpoint.as_ref(), Some(&checkpoint));
    assert_eq!(retained.target, accepted.target);
    // Stored timestamps have second precision; publication may share the ACK second.
    assert!(retained.updated_at >= accepted.updated_at);
    assert_eq!(
        Controller::load().unwrap().state.sessions[LATCH_RELAY_SESSION].last_error,
        retained.last_error
    );
    let outcome = parent.state.wait_session_lifecycle(receipt).await.unwrap();
    assert!(
        matches!(outcome, DaemonReply::LifecycleFailed(detail) if detail.contains("held child teardown diagnostic"))
    );
    assert_eq!(
        parent.state.owner().controller().state.sessions[LATCH_RELAY_SESSION].last_error,
        retained.last_error
    );
    parent.channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1507: an accepted Discard had no durable teardown intent before target removal.
#[cfg(unix)]
#[tokio::test]
async fn discard_intent_recovers_without_checkpointing_and_reconciles_children() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    const NAME: &str = "discard_intent_recovers_without_checkpointing_and_reconciles_children";
    if !in_isolated_live_parent_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Installed).await;
    parent
        .channels
        .control
        .session(LATCH_RELAY_SESSION)
        .await
        .unwrap()
        .submit(
            new_command_id("after-selected-checkpoint").unwrap(),
            RelayCommand::RecordNotice {
                text: "New work after the selected recovery copy".into(),
            },
        )
        .await
        .unwrap();
    let mut controller = Controller::load().unwrap();
    let checkpoint = controller.state.sessions[LATCH_RELAY_SESSION]
        .checkpoint
        .clone()
        .unwrap();
    mj_controller::controller::test_support::worker_test_scope(LATCH_RELAY_SESSION, || {
        controller.prepare_discard(
            LATCH_RELAY_SESSION,
            &checkpoint,
            &ProcessExecutor,
            || Ok(()),
        )
    })
    .unwrap();
    let durable = Controller::load().unwrap();
    assert_eq!(
        durable.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Destroying
    );
    assert!(interrupted_suspend_session_ids(&durable).contains(&LATCH_RELAY_SESSION.to_owned()));
    assert!(durable.state.sessions.contains_key(WORKING_CHILD));
    let recovery = parent.channels.control.session_gate.clone();
    let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        parent.channels.control.clone(),
        &recovery,
    );
    let restarted = Arc::new(RuntimeState::new_with_controller_loader(
        parent.channels.control.clone(),
        durable,
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    tokio::time::timeout(
        Duration::from_secs(60),
        restarted.suspend_session(LATCH_RELAY_SESSION.into()),
    )
    .await
    .unwrap()
    .unwrap();
    let restored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        restored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert_eq!(
        restored.sessions[LATCH_RELAY_SESSION].checkpoint.as_ref(),
        Some(&checkpoint)
    );
    assert!(restored.sessions[LATCH_RELAY_SESSION].target.is_none());
    assert!(!restored.sessions.contains_key(WORKING_CHILD));
    assert!(!restored.subagents.contains_key(WORKING_CHILD));
    assert_eq!(
        mj_controller::database::load_stopped_subagents(LATCH_RELAY_SESSION)
            .unwrap()
            .len(),
        1
    );
    parent.channels.shutdown.shutdown().await.unwrap();
}

/// When a suspend fails with sub-agents already stopped and the parent still
/// live, the parent is told at once: the relay holds the note for its next
/// prompt and the conversation has the line, instead of both waiting for a
/// later resume.
#[cfg(unix)]
// Hard-won: d705260d: the failed-suspend path did not tell a live parent which children had stopped.
#[tokio::test]
async fn a_failed_suspend_tells_a_live_parent_at_once_which_sub_agents_were_stopped() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    if !in_isolated_live_parent_test(
        "a_failed_suspend_tells_a_live_parent_at_once_which_sub_agents_were_stopped",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let parent = live_parent_with_a_working_sub_agent(ParentRecoveryCopy::Missing).await;
    // What a stop that failed partway leaves behind: a child already gone.
    mj_controller::database::record_stopped_subagents(
        LATCH_RELAY_SESSION,
        &[mj_core::subagent::StoppedSubagent {
            child_session_id: "44444444444444444444444444444444".into(),
            title: "Fix the parser".into(),
            task: Some("Fix the off-by-one in the parser.".into()),
            handed_back: false,
        }],
    )
    .unwrap();

    let failure = tokio::time::timeout(
        Duration::from_secs(60),
        parent.state.suspend_session(LATCH_RELAY_SESSION.to_owned()),
    )
    .await
    .unwrap();
    assert!(failure.is_err());

    let relay_state = parent.relay_state();
    assert!(
        relay_state.contains("<mj-stopped-subagents>") && relay_state.contains("Fix the parser"),
        "{relay_state}"
    );
    assert!(
        parent
            .transcript()
            .contains("Suspend stopped 1 working sub-agent: \\\"Fix the parser\\\"."),
        "{}",
        parent.transcript()
    );
    assert!(
        mj_controller::database::load_stopped_subagents(LATCH_RELAY_SESSION)
            .unwrap()
            .is_empty()
    );
    parent.channels.shutdown.shutdown().await.unwrap();
}

/// R15-2: while a parent's suspend stopped a child, the child's row and
/// header said "Destroying", and the dialog and the docs said "stopped".
/// The child's own operation is a stop, which surfaces show as "Stopping",
/// and it cannot be cancelled apart from the suspend that owns it.
#[cfg(unix)]
// Hard-won: 6bfe27a7: a child stopped by parent suspend appeared as Destroying and cancellable.
#[tokio::test]
async fn a_sub_agent_its_parents_suspend_stops_is_shown_stopping_not_destroying() {
    const NAME: &str = "a_sub_agent_its_parents_suspend_stops_is_shown_stopping_not_destroying";
    const CHILD: &str = "MJ_TEST_SUSPEND_SHOWS_STOPPING";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Parent suspension").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    // A parent with nothing to checkpoint yet, so its suspend settles
    // without a worker to talk to.
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Provisioning);
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = runtime_test_session(WORKING_CHILD, &workspace.id, SessionState::Running);
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(WORKING_CHILD),
    }));
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent(WORKING_CHILD, parent_id),
    )
    .unwrap();

    let (state, recovery, _upgrades) = test_runtime_state_loading_the_store_with_live_recovery();
    // A recovery copy in flight holds the child's stop at its start, where
    // the operation is already visible.
    let recovery_attempt = state
        .session_gate
        .try_start(
            WORKING_CHILD,
            mj_controller::recovery_gate::BackgroundWork::WorkerReplacement,
        )
        .unwrap();
    let suspend = mj_core::runtime::spawn({
        let state = state.clone();
        async move { state.suspend_session(parent_id.to_owned()).await }
    });
    let stopping = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(view) = state
                .active_lifecycles()
                .into_iter()
                .find(|view| view.session_id == WORKING_CHILD)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(recovery_attempt);

    assert_eq!(
        serde_json::to_value(stopping.kind).unwrap(),
        "stop_subagent",
        "{stopping:?}"
    );
    assert!(!stopping.cancellable, "{stopping:?}");
    tokio::time::timeout(Duration::from_secs(60), suspend)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !mj_controller::database::load_state()
            .unwrap()
            .sessions
            .contains_key(WORKING_CHILD)
    );
    recovery.close();
}

#[cfg(unix)]
#[tokio::test]
async fn suspending_a_parent_stops_sub_agents_concurrently_before_finishing_parent_close() {
    const NAME: &str =
        "suspending_a_parent_stops_sub_agents_concurrently_before_finishing_parent_close";
    const CHILD: &str = "MJ_TEST_SUSPEND_CONCURRENT_SUBAGENTS";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Concurrent sub-agent stop").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_a = "44444444444444444444444444444444";
    let child_b = "55555555555555555555555555555555";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Provisioning);
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    for child_id in [child_a, child_b] {
        let mut child = runtime_test_session(child_id, &workspace.id, SessionState::Running);
        child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: root.path().join(child_id),
        }));
        mj_controller::database::save_subagent_session(
            &child,
            &runtime_test_subagent(child_id, parent_id),
        )
        .unwrap();
    }

    let (state, recovery, _upgrades) = test_runtime_state_loading_the_store_with_live_recovery();
    let attempt_a = state
        .session_gate
        .try_start(
            child_a,
            mj_controller::recovery_gate::BackgroundWork::WorkerReplacement,
        )
        .unwrap();
    let attempt_b = state
        .session_gate
        .try_start(
            child_b,
            mj_controller::recovery_gate::BackgroundWork::WorkerReplacement,
        )
        .unwrap();

    let mut suspend = mj_core::runtime::spawn({
        let state = state.clone();
        async move { state.suspend_session(parent_id.to_owned()).await }
    });

    // Both children must enter StopSubagent concurrently while each is held.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let active = state.active_lifecycles();
            let has_a = active.iter().any(|view| view.session_id == child_a);
            let has_b = active.iter().any(|view| view.session_id == child_b);
            if has_a && has_b {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both sub-agent stops must be in flight concurrently");

    drop(attempt_a);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if !mj_controller::database::load_state()
                .unwrap()
                .sessions
                .contains_key(child_a)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first child stop should settle while sibling is still held");

    // Parent suspend must not complete while child_b is still in flight.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut suspend)
            .await
            .is_err(),
        "parent suspend must wait for every in-flight sub-agent stop"
    );
    assert_ne!(
        mj_controller::database::load_state().unwrap().sessions[parent_id].state,
        SessionState::Stopped
    );

    drop(attempt_b);
    tokio::time::timeout(Duration::from_secs(60), suspend)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(stored.sessions[parent_id].state, SessionState::Stopped);
    assert!(!stored.sessions.contains_key(child_b));
    recovery.close();
}

/// Run the named test alone, with a store and a SessionWiki index of its own
/// and an empty home. Returns whether this process is that run.
#[cfg(unix)]
fn in_isolated_parked_test(name: &str) -> bool {
    const PARKED_CHILD: &str = "MJ_TEST_PARKED_SUBAGENT_DAEMON_CHILD";
    if std::env::var_os(PARKED_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    mj_controller::controller::test_support::IsolatedTest::new(
        mj_controller::controller::test_support::test_name(module_path!(), name),
    )
    .env(PARKED_CHILD, "1")
    .isolated_store(directory.path())
    .env(
        mj_core::config::SESSION_INDEX_ENV,
        directory.path().join("sessionwiki"),
    )
    .env("HOME", &home)
    .env("XDG_DATA_HOME", home.join(".local/share"))
    .env("XDG_CONFIG_HOME", home.join(".config"))
    .run();
    false
}

/// A parked child on a bare target under `root`, whose worker root holds a
/// `hel` that records any attempt to start it in `root/started`.
#[cfg(unix)]
fn parked_child(root: &Path, child_id: &str, parent_id: &str, workspace_id: &str) -> SessionRecord {
    use std::os::unix::fs::PermissionsExt;
    let worker_root = root.join(child_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let hel = worker_root.join("hel");
    std::fs::write(
        &hel,
        format!(
            "#!/bin/sh\n[ \"$2\" = stop ] && exit 0\necho started >> {}\n",
            root.join("started").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hel, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = runtime_test_session(child_id, workspace_id, SessionState::Parked);
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root,
    }));
    child.session_title_override = Some("Map the parser".into());
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent(child_id, parent_id),
    )
    .unwrap();
    child
}

/// #1161: a parent's suspend ends its parked children the way it ends live
/// ones, without starting them first, and counts them as handed back, since
/// a child is parked only after its parent was told its turn ended.
#[cfg(unix)]
#[tokio::test]
async fn suspending_a_parent_removes_its_parked_sub_agents_without_starting_or_warning() {
    if !in_isolated_parked_test(
        "suspending_a_parent_removes_its_parked_sub_agents_without_starting_or_warning",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Parked children").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "44444444444444444444444444444444";
    // A parent with nothing to checkpoint yet, so its own suspend settles
    // without a worker to talk to.
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Provisioning);
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    // No handback tool and no finished turn on record: only being parked
    // says this child's parent already has what it will get from it.
    parked_child(root.path(), child_id, parent_id, &workspace.id);
    assert!(
        mj_controller::controller::subagent_has_handed_back(
            child_id,
            &mj_core::subagent::Liveness::Known(None)
        )
        .unwrap()
    );
    sessionwiki::index::open().unwrap();

    let state = test_runtime_state_loading_the_store();
    tokio::time::timeout(
        Duration::from_secs(60),
        state.suspend_session(parent_id.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();

    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(stored.sessions[parent_id].state, SessionState::Stopped);
    assert!(
        !stored.sessions.contains_key(child_id) && !stored.subagents.contains_key(child_id),
        "the parked child is removed with its parent's suspend"
    );
    let stopped = mj_controller::database::load_stopped_subagents(parent_id).unwrap();
    assert_eq!(stopped.len(), 1);
    assert!(stopped[0].handed_back, "a parked child is not warned about");
    assert!(
        !root.path().join("started").exists(),
        "the parked child's worker was not started"
    );
}

/// Closing a parked child ends it as closing an idle one does, so its
/// parent's `wait` reads "stopped", without starting its worker first.
#[cfg(unix)]
#[tokio::test]
async fn closing_a_parked_sub_agent_stops_it_without_starting_its_worker() {
    if !in_isolated_parked_test("closing_a_parked_sub_agent_stops_it_without_starting_its_worker") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Parked close").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "55555555555555555555555555555555";
    let parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    mj_controller::database::save_session(&parent).unwrap();
    let child = parked_child(root.path(), child_id, parent_id, &workspace.id);
    let worker_root = match child.target.as_deref() {
        Some(mj_core::state::TargetLocator::LocalBare { worker_root }) => worker_root.clone(),
        _ => unreachable!(),
    };

    let state = test_runtime_state_loading_the_store();
    tokio::time::timeout(
        Duration::from_secs(60),
        state.suspend_session(child_id.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();

    let stored = mj_controller::database::load_state().unwrap();
    let closed = &stored.sessions[child_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(
        closed.target.is_none(),
        "the child's private target state is gone"
    );
    assert!(
        stored.subagents.contains_key(child_id),
        "the relation stays"
    );
    assert!(
        worker_root.exists(),
        "the shared environment retains the child's private root"
    );
    assert!(
        !root.path().join("started").exists(),
        "the parked child's worker was not started"
    );
    assert_eq!(stored.sessions[parent_id].state, SessionState::Running);
}

// Hard-won: #1384: closing a live child exported unrelated nested worktrees in its parent's workspace.
#[cfg(unix)]
#[tokio::test]
async fn child_close_preserves_history_and_reports_without_exporting_the_workspace() {
    const NAME: &str = "child_close_preserves_history_and_reports_without_exporting_the_workspace";
    if !in_held_close_child(NAME, "unused") {
        return;
    }
    exercise_child_close_with_a_nested_workspace(false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn child_close_cancel_before_commit_leaves_it_running_and_allows_a_fresh_close() {
    const NAME: &str =
        "child_close_cancel_before_commit_leaves_it_running_and_allows_a_fresh_close";
    if !in_held_close_child(NAME, "begin_checkpoint") {
        return;
    }
    exercise_child_close_with_a_nested_workspace(true, false).await;
}

// Hard-won: #1448: failed reviewer starts left borrowed targets blocking parent replacement.
#[cfg(unix)]
#[tokio::test]
async fn failed_reviewer_recovery_releases_parent_upgrade_after_saving_its_journal() {
    const NAME: &str = "failed_reviewer_recovery_releases_parent_upgrade_after_saving_its_journal";
    if !in_held_close_child(NAME, "unused") {
        return;
    }
    exercise_child_close_with_a_nested_workspace(false, true).await;
}

// Hard-won: 413864386: a child close held daemon handoff for a nonanswering worker's five-minute hello.
#[cfg(unix)]
#[tokio::test]
async fn child_close_yields_handoff_and_replays_its_durable_request_with_the_final_journal() {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_HOLD_COMMAND, LATCH_RELAY_HOLD_PATH, LATCH_RELAY_JOURNALS_BEFORE_CLOSE,
        LATCH_RELAY_SESSION, LATE_HARNESS_NOTIFICATION,
    };
    use mj_core::subagent::{SubagentToolAction, SubagentToolRequest};
    const NAME: &str =
        "child_close_yields_handoff_and_replays_its_durable_request_with_the_final_journal";
    const PHASE: &str = "MJ_TEST_CHILD_CLOSE_HANDOFF_PHASE";
    const ROOT: &str = "MJ_TEST_CHILD_CLOSE_HANDOFF_ROOT";
    let Ok(phase) = std::env::var(PHASE) else {
        let directory = tempfile::tempdir().unwrap();
        for phase in ["blocked", "recovery"] {
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), NAME),
            )
            .env(PHASE, phase)
            .env(ROOT, directory.path())
            .env(LATCH_RELAY_HOLD_COMMAND, "begin_checkpoint")
            .env(
                LATCH_RELAY_HOLD_PATH,
                directory.path().join("recovery-hold"),
            )
            .env(LATCH_RELAY_JOURNALS_BEFORE_CLOSE, LATE_HARNESS_NOTIFICATION)
            .env("MJ_INSTANCE", "child-close-handoff")
            .isolated_store(directory.path())
            .run();
        }
        return;
    };
    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let worker_root = root.join(LATCH_RELAY_SESSION);
    let parent_id = "0123456789abcdef0123456789abcdef";
    let final_output = "journal output retained across daemon handoff ".repeat(4096);
    let request = SubagentToolRequest {
        request_id: "same-child-close-across-handoff".into(),
        originating_command_id: None,
        created_at_ms: 1234,
        action: SubagentToolAction::CloseAgent {
            child_session_id: LATCH_RELAY_SESSION.into(),
        },
    };
    if phase == "blocked" {
        seed_live_session(&root, &worker_root);
        let mut journal =
            mj_worker::relay::DurableRelay::open(&worker_root, LATCH_RELAY_SESSION, "1.0.0")
                .unwrap();
        journal
            .record_observation(mj_core::relay::RelayObservation::Warning {
                message: final_output,
            })
            .unwrap();
        drop(journal);
        let workspace = mj_controller::database::create_workspace("Child close handoff").unwrap();
        let parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
        mj_controller::database::save_session(&parent).unwrap();
        let mut child =
            runtime_test_session(LATCH_RELAY_SESSION, &workspace.id, SessionState::Running);
        child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));
        mj_controller::database::save_subagent_session(
            &child,
            &runtime_test_subagent(LATCH_RELAY_SESSION, parent_id),
        )
        .unwrap();
        let channels = mj_controller::session_manager::spawn_session_manager().unwrap();
        channels
            .targets
            .send(vec![mj_controller::session_manager::RelaySessionTarget {
                session_id: LATCH_RELAY_SESSION.into(),
                spec: CommandSpec::new("sleep", ["600"]),
                worker_recovery: None,
                project_memory: None,
            }])
            .unwrap();
        let recovery = channels.control.session_gate.clone();
        let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
            channels.control.clone(),
            &recovery,
        );
        let state = Arc::new(RuntimeState::new_with_controller_loader(
            channels.control.clone(),
            Controller::load().unwrap(),
            recovery.clone(),
            upgrades.observer(),
            Vec::new(),
            Controller::load,
        ));
        let backend = Arc::new(crate::server_runtime::api::ApiBackend::new(
            channels.control.client(),
            Arc::new({
                let state = state.clone();
                move |id| state.session_state(id)
            }),
            state.clone(),
        ));
        let pending = mj_core::runtime::spawn(async move {
            backend
                .execute_subagent_tool_durable(parent_id.into(), request)
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let yielded = state
                    .owner()
                    .lifecycle
                    .get(LATCH_RELAY_SESSION)
                    .is_some_and(|active| {
                        active.is_running() && active.upgrade_work.lock().unwrap().is_none()
                    });
                if yielded {
                    break;
                }
                assert!(
                    !pending.is_finished(),
                    "the unavailable worker must still be waiting"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the durable child close must release upgrade admission");
        assert!(
            mj_controller::upgrade::gate().try_close(),
            "{:?}",
            mj_controller::upgrade::active_labels()
        );
        // The daemon stops delegation delivery before interrupting lifecycles.
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), state.cancel_and_wait_lifecycles())
            .await
            .expect("shutdown must interrupt the pending relay lease")
            .unwrap();
        channels.shutdown.shutdown().await.unwrap();
        let stored = mj_controller::database::load_state().unwrap();
        let child = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(child.state, SessionState::Closing);
        assert_eq!(
            child.target,
            Some(Box::new(mj_core::state::TargetLocator::LocalBare {
                worker_root: worker_root.clone(),
            }))
        );
        assert!(
            child.public_error().is_none(),
            "handoff is not a failed close"
        );
        assert!(
            worker_root.exists(),
            "the final journal must survive handoff"
        );
        assert!(
            mj_controller::database::load_delegation(parent_id, "same-child-close-across-handoff")
                .unwrap()
                .unwrap()
                .1
                .is_none()
        );
    } else {
        let controller = Controller::load().unwrap();
        assert!(interrupted_suspend_session_ids(&controller).contains(&LATCH_RELAY_SESSION.into()));
        let (channels, state) = serve_live_session(&worker_root, false).await;
        // Startup recovery and replay can arrive in either order. The
        // lifecycle owner must coalesce the unkeyed recovery and keyed replay.
        let recovering = mj_core::runtime::spawn({
            let state = state.clone();
            async move { state.suspend_session(LATCH_RELAY_SESSION.into()).await }
        });
        let hold = root.join("recovery-hold");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !hold.with_extension("reached").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recovery must reach the worker barrier");
        let backend = Arc::new(crate::server_runtime::api::ApiBackend::new(
            channels.control.client(),
            Arc::new({
                let state = state.clone();
                move |id| state.session_state(id)
            }),
            state,
        ));
        let replaying = mj_core::runtime::spawn({
            let backend = backend.clone();
            let request = request.clone();
            async move {
                backend
                    .execute_subagent_tool_durable(parent_id.into(), request)
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !replaying.is_finished(),
            "replay must join recovery rather than refuse it"
        );
        std::fs::write(hold.with_extension("release"), b"go").unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(30), replaying)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        recovering.await.unwrap().unwrap();
        assert!(!closed.result.is_error, "{}", closed.result.message);
        let repeated = backend
            .execute_subagent_tool_durable(parent_id.into(), request)
            .await
            .unwrap();
        assert_eq!(closed.result, repeated.result);
        channels.shutdown.shutdown().await.unwrap();
        let stored = mj_controller::database::load_state().unwrap();
        assert_eq!(
            stored.sessions[LATCH_RELAY_SESSION].state,
            SessionState::Stopped
        );
        assert!(
            worker_root.exists(),
            "private roots remain owned until the environment retires"
        );
        let history = mj_controller::database::load_materialized_session(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert!(
            serde_json::to_string(&history)
                .unwrap()
                .contains(&final_output)
        );
    }
}

#[cfg(unix)]
async fn exercise_child_close_with_a_nested_workspace(
    cancel_before_commit: bool,
    failed_reviewer: bool,
) {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_HOLD_PATH, LATCH_RELAY_SESSION,
    };
    use mj_controller::controller::test_support::{committed_repository, test_git};
    let _writer = mj_controller::database::install_isolated_test_writer();
    let repository = committed_repository();
    let nested = ".mj/csharp-parity/bin-src";
    test_git(
        repository.path(),
        &["worktree", "add", "--detach", nested, "HEAD"],
    );
    let scratch_file = repository.path().join(nested).join("unfinished.txt");
    std::fs::write(&scratch_file, "scratch work must survive child close").unwrap();
    let root = tempfile::tempdir().unwrap();
    let relay_root = root.path().join("relay");
    seed_live_session(root.path(), &relay_root);
    let conversation = "child conversation ".repeat(8192);
    {
        let mut relay =
            mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0")
                .unwrap();
        relay
            .record_observation(mj_core::relay::RelayObservation::Warning {
                message: conversation.clone(),
            })
            .unwrap();
    }
    let workspace = mj_controller::database::create_workspace("Child close ownership").unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.project_directory = Some(repository.path().to_owned());
    let parent_root = root.path().join(parent_id);
    std::fs::create_dir_all(&parent_root).unwrap();
    std::fs::write(parent_root.join("private"), "parent worker").unwrap();
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: parent_root.clone(),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let worker_root = root.path().join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(worker_root.join("private"), "child worker").unwrap();
    let mut child = runtime_test_session(LATCH_RELAY_SESSION, &workspace.id, SessionState::Running);
    let mut relation = runtime_test_subagent(LATCH_RELAY_SESSION, parent_id);
    if failed_reviewer {
        child.state = SessionState::Error;
        relation.kind = mj_core::subagent::SubagentKind::Reviewer;
    }
    child.project_directory = parent.project_directory.clone();
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    mj_controller::database::save_subagent_session(&child, &relation).unwrap();
    let reports = repository
        .path()
        .join(".mj/agents")
        .join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&reports).unwrap();
    std::fs::write(reports.join("result.txt"), "child report").unwrap();
    mj_controller::database::record_subagent_report_dir(
        LATCH_RELAY_SESSION,
        reports.to_str().unwrap(),
    )
    .unwrap();
    if failed_reviewer {
        use mj_core::review::driver::TurnReviewSeed;
        use mj_review::driver::TurnReviewDriver;
        let (mut driver, _) = TurnReviewDriver::start(
            TurnReviewSeed {
                task: "review the changes".into(),
                user_messages: Vec::new(),
                declined: Vec::new(),
                brief: None,
            },
            "failed-start".into(),
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        driver.delta_captured(vec![mj_core::relay::RepoDelta {
            root: repository.path().into(),
            current_tree: "unreviewed".into(),
            patch: "+unfinished".into(),
            changed_lines: 1,
            ..Default::default()
        }]);
        driver.role_failed("reviewer-1", Some(LATCH_RELAY_SESSION), "startup failed");
        mj_controller::database::save_turn_review_state(
            parent_id,
            &mj_controller::database::TurnReviewState {
                open_reviews: vec![mj_core::storage::OpenReview {
                    active_group: Some(mj_core::storage::ActiveReviewGroup {
                        brief_writer: None,
                        driver: driver.snapshot(),
                        reviewers: vec![Default::default()],
                        registrations: vec![mj_core::storage::ReviewerRegistration {
                            role: "reviewer-1".into(),
                            request_key: relation.request_key.clone(),
                            child_session_id: Some(LATCH_RELAY_SESSION.into()),
                        }],
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .unwrap();
        mj_controller::database::begin_container_upgrade(
            parent_id,
            "upgrade-after-failed-review",
            Arc::new(AtomicBool::new(false)),
            mj_controller::targets::container_upgrade_test_plan(parent_id),
            None,
        )
        .unwrap();
        assert_eq!(
            mj_controller::database::try_commit_container_upgrade(
                parent_id,
                "upgrade-after-failed-review"
            )
            .unwrap(),
            mj_controller::database::ContainerUpgradeCommit::Waiting(vec![format!(
                "reviewer session {LATCH_RELAY_SESSION} (Error)"
            )]),
            "an Error reviewer with a retained target still blocks replacement"
        );
    }
    let (channels, state) = serve_live_session(&relay_root, !cancel_before_commit).await;
    if failed_reviewer {
        state.reviewer_runtime.set(Arc::downgrade(&state)).unwrap();
    }
    if cancel_before_commit {
        let closing = mj_core::runtime::spawn({
            let state = state.clone();
            async move { state.suspend_session(LATCH_RELAY_SESSION.into()).await }
        });
        let hold = PathBuf::from(std::env::var_os(LATCH_RELAY_HOLD_PATH).unwrap());
        tokio::time::timeout(Duration::from_secs(30), async {
            while !hold.with_extension("reached").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the child close did not reach its barrier");
        state
            .cancel_lifecycle_with_intent(LATCH_RELAY_SESSION)
            .await
            .unwrap();
        std::fs::write(hold.with_extension("release"), b"go").unwrap();
        let error = tokio::time::timeout(Duration::from_secs(30), closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        let stored = mj_controller::database::load_state().unwrap();
        assert_eq!(
            stored.sessions[LATCH_RELAY_SESSION].state,
            SessionState::Running
        );
        assert!(worker_root.join("private").exists());
    }
    if failed_reviewer {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                // A failed reviewer is destroyed once its role has failed.
                if mj_controller::database::load_session_record(LATCH_RELAY_SESSION)
                    .unwrap()
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("startup destroys a failed reviewer without any manual suspension");
        assert!(
            mj_controller::database::try_commit_container_upgrade(
                parent_id,
                "upgrade-after-failed-review"
            )
            .unwrap()
            .is_committed()
        );
        assert!(
            mj_controller::database::turn_review_state(parent_id)
                .unwrap()
                .push_findings
                .is_empty()
        );
        state.review_host.shutdown().await.unwrap();
    } else {
        tokio::time::timeout(
            Duration::from_secs(60),
            state.close_subagent_request(
                LATCH_RELAY_SESSION.into(),
                parent_id.into(),
                "close-child".into(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
    channels.shutdown.shutdown().await.unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    if !failed_reviewer {
        let closed = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(closed.state, SessionState::Stopped);
        assert!(closed.target.is_none() && closed.checkpoint.is_none());
        assert!(
            worker_root.exists(),
            "private roots remain owned until the environment retires"
        );
    }
    assert_eq!(stored.sessions[parent_id], parent);
    assert_eq!(
        std::fs::read_to_string(parent_root.join("private")).unwrap(),
        "parent worker"
    );
    assert_eq!(
        std::fs::read_to_string(&scratch_file).unwrap(),
        "scratch work must survive child close"
    );
    assert_eq!(
        std::fs::read_to_string(reports.join("result.txt")).unwrap(),
        "child report"
    );
    if failed_reviewer {
        assert!(!stored.subagents.contains_key(LATCH_RELAY_SESSION));
    } else {
        let history = mj_controller::database::load_materialized_session(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert!(
            serde_json::to_string(&history)
                .unwrap()
                .contains(&conversation)
        );
        assert_eq!(
            history.execution,
            mj_core::state::MaterializedExecutionState::Closed
        );
        assert!(stored.subagents.contains_key(LATCH_RELAY_SESSION));
    }
}

// Hard-won: #1444: an unreachable Closing child could lose its final journal when cleanup skipped relay recovery.
#[cfg(unix)]
#[tokio::test]
async fn unreachable_child_keeps_its_private_journal_until_worker_death_is_proven() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    const NAME: &str = "unreachable_child_keeps_its_private_journal_until_worker_death_is_proven";
    if !in_held_close_child(NAME, "unused") {
        return;
    }

    struct AliveWorker;
    impl CommandExecutor for AliveWorker {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            // The root exists (its journal is there); only the worker is unreachable.
            let stdout = match command.purpose.as_str() {
                "probe Mjolnir worker root" => b"recoverable\n".to_vec(),
                "probe Mjolnir worker daemon liveness" => b"alive\n".to_vec(),
                other => panic!("unexpected command: {other}"),
            };
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let child_root = root.path().join(LATCH_RELAY_SESSION);
    seed_live_session(root.path(), &child_root);
    use std::os::unix::fs::PermissionsExt;
    let proxy = child_root.join("hel");
    std::fs::write(&proxy, "#!/bin/sh\n[ \"$2\" = stop ] && exit 0\nexit 1\n").unwrap();
    std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let last_event = "journal-only child output ".repeat(4096);
    let mut journal =
        mj_worker::relay::DurableRelay::open(&child_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    journal
        .record_observation(mj_core::relay::RelayObservation::Warning {
            message: last_event,
        })
        .unwrap();
    drop(journal);

    let workspace = mj_controller::database::create_workspace("Dead child close").unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let parent_dir = root.path().join("parent-workspace");
    std::fs::create_dir(&parent_dir).unwrap();
    std::fs::write(parent_dir.join("parent-work"), "keep").unwrap();
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.project_directory = Some(parent_dir.clone());
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = runtime_test_session(LATCH_RELAY_SESSION, &workspace.id, SessionState::Closing);
    child.project_directory = Some(parent_dir.clone());
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: child_root.clone(),
    }));
    let mut relation = runtime_test_subagent(LATCH_RELAY_SESSION, parent_id);
    relation.working_directory = "deleted-child-cwd".into();
    mj_controller::database::save_subagent_session(&child, &relation).unwrap();
    assert!(!parent_dir.join(&relation.working_directory).exists());

    let mut controller = Controller::load().unwrap();
    let error = controller
        .suspend_subagent_controlled(
            &mj_controller::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &AliveWorker,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("worker is not confirmed dead"),
        "{error:#}"
    );
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Closing
    );
    assert_eq!(stored.sessions[LATCH_RELAY_SESSION].target, child.target);
    assert!(child_root.join(mj_core::relay::RELAY_STATE_FILE).exists());
    assert_eq!(stored.sessions[parent_id], parent);
    assert_eq!(
        std::fs::read_to_string(parent_dir.join("parent-work")).unwrap(),
        "keep"
    );
}

// Hard-won: #1444: a dead child's deleted cwd blocked close while its final journal remained recoverable.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires a matching built mj-worker via MJ_WORKER_BINARY"]
async fn dead_child_close_recovers_journal_without_recreating_its_cwd() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    const NAME: &str = "dead_child_close_recovers_journal_without_recreating_its_cwd";
    let worker_binary = PathBuf::from(
        std::env::var_os("MJ_WORKER_BINARY").expect("matching worker binary is required"),
    );
    mj_core::worker_build::verify_worker_build(&worker_binary).unwrap();
    const CHILD: &str = "MJ_TEST_DEAD_CHILD_CLOSE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .include_ignored()
        .env(CHILD, "1")
        .env("MJ_WORKER_BINARY", &worker_binary)
        .isolated_store(directory.path())
        .run();
        return;
    }

    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let child_root = root.path().join(LATCH_RELAY_SESSION);
    seed_live_session(root.path(), &child_root);
    std::fs::copy(&worker_binary, child_root.join("hel")).unwrap();
    Config::update(|config| {
        config
            .targets
            .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
        Ok(())
    })
    .unwrap();
    let final_event = "journal-only child output ".repeat(4096);
    let mut journal =
        mj_worker::relay::DurableRelay::open(&child_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    journal
        .record_observation(mj_core::relay::RelayObservation::Warning {
            message: final_event.clone(),
        })
        .unwrap();
    drop(journal);

    let workspace = mj_controller::database::create_workspace("Recover dead child close").unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let parent_dir = root.path().join("parent-workspace");
    std::fs::create_dir(&parent_dir).unwrap();
    std::fs::write(parent_dir.join("parent-work"), "keep").unwrap();
    let parent_root = root.path().join(parent_id);
    std::fs::create_dir(&parent_root).unwrap();
    std::fs::write(parent_root.join("private"), "parent worker").unwrap();
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.project_directory = Some(parent_dir.clone());
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: parent_root.clone(),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = runtime_test_session(LATCH_RELAY_SESSION, &workspace.id, SessionState::Closing);
    child.project_directory = Some(parent_dir.clone());
    child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: child_root.clone(),
    }));
    let mut relation = runtime_test_subagent(LATCH_RELAY_SESSION, parent_id);
    relation.working_directory = "deleted-child-cwd".into();
    mj_controller::database::save_subagent_session(&child, &relation).unwrap();
    let missing_cwd = parent_dir.join(&relation.working_directory);
    assert!(!missing_cwd.exists());

    let controller = Controller::load().unwrap();
    let backend = mj_controller::targets::TargetLocator::LocalBare {
        worker_root: child_root.to_string_lossy().into_owned(),
    };
    let reconnect = mj_controller::targets::reconnect_plan(&backend, LATCH_RELAY_SESSION)
        .unwrap()
        .commands
        .remove(0);
    let channels = mj_controller::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![mj_controller::session_manager::RelaySessionTarget {
            session_id: LATCH_RELAY_SESSION.into(),
            spec: reconnect,
            worker_recovery: None,
            project_memory: None,
        }])
        .unwrap();
    channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    let recovery = channels.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(channels.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        channels.control.clone(),
        controller,
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    tokio::time::timeout(
        Duration::from_secs(90),
        state.close_subagent_request(
            LATCH_RELAY_SESSION.into(),
            parent_id.into(),
            "recover-dead-child".into(),
        ),
    )
    .await
    .expect("child close timed out")
    .unwrap();

    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert!(stored.sessions[LATCH_RELAY_SESSION].target.is_none());
    assert!(
        !child_root.exists(),
        "private root removed only after final sync"
    );
    assert!(!missing_cwd.exists(), "child cwd was not recreated");
    assert_eq!(stored.sessions[parent_id], parent);
    assert_eq!(
        std::fs::read_to_string(parent_root.join("private")).unwrap(),
        "parent worker"
    );
    assert_eq!(
        std::fs::read_to_string(parent_dir.join("parent-work")).unwrap(),
        "keep"
    );
    let history = mj_controller::database::load_materialized_session(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(
        history.execution,
        mj_core::state::MaterializedExecutionState::Closed
    );
    assert!(
        serde_json::to_string(&history)
            .unwrap()
            .contains(&final_event)
    );
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1384: interrupted child cleanup wrongly required a workspace checkpoint and a live worker.
#[cfg(unix)]
#[tokio::test]
async fn child_close_finishes_interrupted_cleanup_without_a_worker_or_checkpoint() {
    const NAME: &str = "child_close_finishes_interrupted_cleanup_without_a_worker_or_checkpoint";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Interrupted child close").unwrap();
    let root = tempfile::tempdir().unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "55555555555555555555555555555555";
    let parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = parked_child(root.path(), child_id, parent_id, &workspace.id);
    child.state = SessionState::Destroying;
    mj_controller::database::save_session(&child).unwrap();
    let state = test_runtime_state_loading_the_store();
    state
        .close_subagent_request(
            child_id.into(),
            parent_id.into(),
            "retry-child-close".into(),
        )
        .await
        .unwrap();
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(stored.sessions[child_id].state, SessionState::Stopped);
    assert!(stored.sessions[child_id].checkpoint.is_none());
    assert!(stored.sessions[child_id].target.is_none());
    assert_eq!(stored.sessions[parent_id], parent);
    assert!(!root.path().join("started").exists());
}

#[tokio::test]
async fn late_stage_callbacks_cannot_change_a_replacement_lifecycle() {
    let state = test_runtime_state();
    let first_release = Arc::new(tokio::sync::Notify::new());
    let first = state
        .start_or_join_lifecycle("same-session".into(), LifecycleKind::Create, {
            let release = first_release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let old = DaemonStageReportingExecutor::new(
        RefusingExecutor("stage-only test"),
        state.clone(),
        "same-session".into(),
    );
    first_release.notify_one();
    RuntimeState::wait_lifecycle_result(first).await.unwrap();
    let second_release = Arc::new(tokio::sync::Notify::new());
    let second = state
        .start_or_join_lifecycle("same-session".into(), LifecycleKind::Resume, {
            let release = second_release.clone();
            move |_, _, _, _session_admission| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    let current = DaemonStageReportingExecutor::new(
        RefusingExecutor("stage-only test"),
        state.clone(),
        "same-session".into(),
    );
    old.stage_started(ProvisionStage::Cloning);
    old.notify_notice("obsolete operation");
    assert!(state.active_lifecycles()[0].active_stages.is_empty());
    assert!(state.active_lifecycles()[0].notice.is_none());
    current.stage_started(ProvisionStage::Cloning);
    old.stage_finished(ProvisionStage::Cloning);
    assert_eq!(state.active_lifecycles()[0].active_stages.len(), 1);
    second_release.notify_one();
    RuntimeState::wait_lifecycle_result(second).await.unwrap();
}

#[tokio::test]
async fn a_late_worker_view_cannot_recreate_a_deleted_session() {
    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(
            "session-1".into(),
            runtime_test_session("session-1", "workspace", SessionState::Running),
        );
    });
    state
        .owner()
        .install_relay_sessions(["session-1".to_owned()].into());
    state
        .publish_session("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    assert!(state.owner().sessions.contains_key("session-1"));
    state.owner().edit_sessions(|sessions| {
        sessions.remove("session-1");
    });
    assert!(!state.owner().sessions.contains_key("session-1"));
    state
        .publish_session("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    assert!(!state.owner().sessions.contains_key("session-1"));
}

/// A retired relay actor publishes no final view. Removing its session from
/// the manager's targets must drop the view it left, and a view it sends late
/// must not bring it back, so no client keeps a live-looking stopped session.
// Hard-won: 8b3a7286: a failed Move left the TUI unable to open a session because actor and feed ownership disagreed.
#[tokio::test]
async fn a_session_without_a_relay_actor_has_no_runtime_view() {
    if isolate_runtime_feed_test(
        module_path!(),
        "a_session_without_a_relay_actor_has_no_runtime_view",
    ) {
        return;
    }

    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(
            "session-1".into(),
            runtime_test_session("session-1", "workspace", SessionState::Running),
        );
    });
    state
        .publish_session("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    assert!(
        !state.owner().sessions.contains_key("session-1"),
        "a view arrived before its actor was installed"
    );

    state
        .owner()
        .install_relay_sessions(["session-1".to_owned()].into());
    let mut live = ready_startup_view();
    let materialized = &mut live.snapshot.as_mut().unwrap().materialized;
    materialized.last_activity_at_ms = Some(7_777);
    materialized
        .queued_prompts
        .push(mj_core::state::MaterializedQueuedPrompt {
            command_id: "queued".into(),
            kind: Default::default(),
            content: vec![serde_json::json!({"type": "text", "text": "queued prompt"})],
            queued_at_ms: 7_777,
            accepted_ordinal: None,
        });
    state
        .publish_session("session-1".into(), live)
        .await
        .unwrap();
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    assert!(replica.projection.sessions.contains_key("session-1"));
    let cursor = replica.cursor.clone();

    assert!(state.owner().install_relay_sessions(Default::default()));
    state.publish_revision();
    replica
        .apply(state.runtime_changes(cursor, false).await.unwrap())
        .unwrap();
    assert!(!replica.projection.sessions.contains_key("session-1"));
    assert!(state.owner().background_policies.is_empty());

    state
        .publish_session("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    let capture = state.runtime_publication().unwrap();
    let retained = &capture.sessions["session-1"];
    assert!(retained.materialized_only);
    assert!(retained.operational.is_none());
    let retained = retained.materialized.as_ref().unwrap();
    assert_eq!(retained.last_activity_at_ms, Some(7_777));
    assert_eq!(retained.queued_prompts[0].text, "queued prompt");
}

#[tokio::test]
async fn completed_lifecycle_retention_does_not_grow_with_history() {
    let state = test_runtime_state();
    for index in 0..300 {
        let result = state
            .start_or_join_lifecycle(
                format!("session-{index}"),
                LifecycleKind::Create,
                |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
            )
            .await
            .unwrap();
        RuntimeState::wait_lifecycle_result(result).await.unwrap();
    }
    assert_eq!(state.owner().lifecycle.len(), 256);
    assert!(state.active_lifecycles().is_empty());
    assert!(state.owner().lifecycle.contains_key("session-299"));
    assert!(!state.owner().lifecycle.contains_key("session-0"));
}

#[tokio::test]
async fn move_destination_ownership_is_typed_and_survives_cancellation() {
    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(
            "moving".into(),
            runtime_test_session("moving", "workspace", SessionState::Running),
        );
    });
    let (_, result) = tokio::sync::watch::channel(None);
    state.owner().lifecycle.insert(
        "moving".into(),
        ActiveLifecycle {
            origin: super::lifecycle::LifecycleOrigin::Person,
            acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
            move_operation_id: None,
            restart_intent: None,
            upgrade_work: Arc::new(Mutex::new(None)),
            phase: LifecyclePhase::Executing,
            operation_id: "current".into(),
            control: LifecycleControl::default(),
            kind: LifecycleKind::Move,
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            progress: LifecycleProgress::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            result,
        },
    );
    state.set_lifecycle_notice("moving", "current", "Preparing destination");
    assert!(!state.owner().worker_is_owned("moving"));
    state.reserve_move_destination("moving", "obsolete");
    assert!(!state.owner().worker_is_owned("moving"));
    state.reserve_move_destination("moving", "current");
    assert!(state.owner().worker_is_owned("moving"));
    state
        .owner()
        .lifecycle
        .get_mut("moving")
        .unwrap()
        .request_cancel("moving", "test cancellation");
    assert!(state.owner().worker_is_owned("moving"));
    state.owner().lifecycle.get_mut("moving").unwrap().phase =
        LifecyclePhase::Completed(Ok(DaemonLifecycleResult::Done));
    assert!(!state.owner().worker_is_owned("moving"));
}

#[tokio::test]
async fn runtime_publication_crosses_frame_limit_atomically_and_keeps_connection_usable() {
    use mj_client::runtime_feed::{RuntimeCursor, RuntimeFrame, RuntimeProjection};
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut projection = RuntimeProjection::default();
    projection
        .metadata
        .workspace_names
        .insert("large".into(), "x".repeat(MAX_FRAME_BYTES + 1024));
    let expected = projection.clone();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        for (request_id, result) in [
            (
                7,
                Ok(DaemonReply::RuntimeChanges(Box::new(
                    RuntimeFrame::Snapshot {
                        cursor: RuntimeCursor {
                            alert_sequence: 0,
                            incarnation: "test".into(),
                            sequence: 1,
                            subagent_parents: Default::default(),
                        },
                        projection: Box::new(projection),
                    },
                ))),
            ),
            (8, Ok(DaemonReply::Pong)),
        ] {
            super::serve::write_response(
                &mut stream,
                ResponseEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    result,
                },
            )
            .await
            .unwrap();
        }
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let response = mj_client::daemon::read_response(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 7);
    let Ok(DaemonReply::RuntimeChanges(frame)) = response.result else {
        panic!("expected complete publication")
    };
    let RuntimeFrame::Snapshot { projection, .. } = *frame else {
        panic!("expected snapshot")
    };
    assert_eq!(*projection, expected);
    let response = mj_client::daemon::read_response(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 8);
    assert!(matches!(response.result, Ok(DaemonReply::Pong)));
    sender.await.unwrap();
}

#[tokio::test]
async fn interrupted_runtime_fragments_never_publish_partial_state() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(
            &mut stream,
            &ResponseEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: 7,
                result: Ok(DaemonReply::ReplyChunk {
                    bytes: b"{\"Snapshot\":".to_vec(),
                    finished: false,
                }),
            },
        )
        .await
        .unwrap();
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    assert!(mj_client::daemon::read_response(&mut stream).await.is_err());
    sender.await.unwrap();
}

#[tokio::test]
async fn recovered_drafts_append_to_committed_edits_and_failed_writes_do_not_publish() {
    let Some(_writer) = startup_prompt_test_store(
        "recovered_drafts_append_to_committed_edits_and_failed_writes_do_not_publish",
    ) else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "original");
    mj_controller::database::set_session_draft_input("session-1", "newer client edit").unwrap();
    let (first, second) = tokio::join!(
        state.append_draft_input("session-1", "first"),
        state.append_draft_input("session-1", "second")
    );
    first.unwrap();
    second.unwrap();
    let saved = state.session_record("session-1").unwrap().draft_input;
    assert!(saved.starts_with("newer client edit\n\n"));
    assert!(saved.contains("first") && saved.contains("second"));
    assert!(
        state
            .append_draft_input("missing", "lost input")
            .await
            .is_err()
    );
    assert!(state.session_record("missing").is_none());
}

#[tokio::test]
async fn retry_admission_reserves_only_the_matching_move_destination() {
    let Some(_writer) =
        startup_prompt_test_store("retry_admission_reserves_only_the_matching_move_destination")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let session = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let operation = mj_core::state::MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "retained-move".into(),
        selection: MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            workspace: Default::default(),
            session_id: session.id.clone(),
            profile_id: Some(session.last_profile.clone()),
            target_template_id: Some(session.target_template_id.clone()),
            additional_mounts: None,
            resource_allocation: None,
            clear_resource_allocation: false,
        },
        source_profile_id: session.last_profile.clone(),
        source_target_template_id: session.target_template_id.clone(),
        source_target: None,
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "test".into(),
        checkpoint: None,
        recovery_session: Some(session.clone()),
        queue: mj_core::state::ResumeQueueDisposition::Start,
        phase: mj_core::state::MovePhase::StartingQueue,
        queue_admission_started: true,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
        error: None,
    };
    mj_controller::database::save_move_operation(&operation).unwrap();
    mj_controller::controller::move_session::restore_move_queue_hold(&operation);
    assert!(
        state
            .start_or_join_lifecycle(
                session.id.clone(),
                LifecycleKind::Resume,
                |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) }
            )
            .await
            .is_err()
    );
    for (id, owned) in [("retained-move", true), ("different-move", false)] {
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let watch = state
            .admit_lifecycle(
                session.id.clone(),
                LifecycleKind::Move,
                super::lifecycle::LifecycleStart {
                    origin: super::lifecycle::LifecycleOrigin::Automatic,
                    resume_workspace_id: None,
                    request_key: Some(id.into()),
                    control: None,
                    phase: LifecyclePhase::Executing,
                    move_operation_id: Some(id.into()),
                    deferrable_admission: None,
                    holds_through_drain: false,
                },
                move |_, _, _, _session_admission| async move {
                    released.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await
            .unwrap();
        assert_eq!(state.owner().worker_is_owned(&session.id), owned);
        release.notify_one();
        RuntimeState::wait_lifecycle_result(watch.clone())
            .await
            .unwrap();
        state.remove_completed_lifecycle(&watch);
    }
}

// Hard-won: 05d2c739: a wrong SQL column made vanished-session startup cleanup fail on every retry.
#[tokio::test]
async fn api_startup_of_a_vanished_session_is_marked_failed() {
    let Some(_writer) =
        startup_prompt_test_store("api_startup_of_a_vanished_session_is_marked_failed")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    state
        .queue_api_followup(
            "session-1",
            "api-test".into(),
            crate::server::api::StartFollowup {
                model: Some("model-a".into()),
                effort: None,
                fast_mode: false,
                prompt: None,
            },
        )
        .await
        .unwrap();
    // While the session is still starting, its pending input is kept.
    mj_controller::database::fail_unavailable_startup_groups("session-1", "gone").unwrap();
    let rows = mj_controller::database::load_latest_startup_group("session-1").unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.phase.as_str())
            .collect::<Vec<_>>(),
        ["pending"]
    );
    // Once the session row is gone, the pending input fails with the reason.
    mj_controller::database::delete_session("session-1").unwrap();
    mj_controller::database::fail_unavailable_startup_groups("session-1", "gone").unwrap();
    let rows = mj_controller::database::load_latest_startup_group("session-1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].phase, "failed");
    assert_eq!(rows[0].error.as_deref(), Some("gone"));
}

#[tokio::test]
async fn api_startup_persists_the_entire_ordered_followup_before_acknowledging() {
    let Some(_writer) = startup_prompt_test_store(
        "api_startup_persists_the_entire_ordered_followup_before_acknowledging",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let followup = crate::server::api::StartFollowup {
        model: Some("model-a".into()),
        effort: Some("high".into()),
        fast_mode: true,
        prompt: Some("accepted before readiness".into()),
    };
    state
        .queue_api_followup("session-1", "api-test".into(), followup.clone())
        .await
        .unwrap();
    state
        .queue_api_followup("session-1", "api-test".into(), followup.clone())
        .await
        .unwrap();
    let rows = mj_controller::database::load_latest_startup_group("session-1").unwrap();
    assert_eq!(
        rows.len(),
        4,
        "retry must reuse the entire existing request"
    );
    let commands: Vec<_> = rows.iter().map(|row| row.command_id.as_str()).collect();
    assert_eq!(
        commands,
        [
            "api-test-model",
            "api-test-effort",
            "api-test-fast-mode",
            "api-test-prompt"
        ]
    );
    assert!(
        matches!(serde_json::from_str::<StartupStep>(&rows.last().unwrap().step_json).unwrap(), StartupStep::ApiPrompt { text } if text == "accepted before readiness")
    );
    let mut changed = followup;
    changed.prompt = None;
    assert!(
        state
            .queue_api_followup("session-1", "api-test".into(), changed)
            .await
            .is_err(),
        "a request ID cannot change the saved workflow"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), manager.requests.recv())
            .await
            .is_err(),
        "nothing is submitted before readiness"
    );
    assert!(
        !mj_controller::upgrade::active_labels()
            .iter()
            .any(|label| label.contains("API startup followup"))
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
    assert_eq!(
        mj_controller::database::load_latest_startup_group("session-1")
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn initial_worker_receipts_settle_after_the_first_turn_has_already_parked() {
    const NAME: &str = "initial_worker_receipts_settle_after_the_first_turn_has_already_parked";
    const CHILD: &str = "child-initial";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, RelayCommand, RelayCommandOutcome};
    use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};
    let parent = runtime_test_session(
        "parent-initial",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&parent).unwrap();
    let child = runtime_test_session(
        CHILD,
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Parked,
    );
    let mut relation = runtime_test_subagent(CHILD, &parent.id);
    relation.model = Some("chosen-model".into());
    let group_id = format!("subagent-spawn-{}", relation.request_key);
    let steps = super::startup_followup::api_followup_steps(
        &group_id,
        crate::server::api::StartFollowup {
            model: relation.model.clone(),
            effort: None,
            fast_mode: false,
            prompt: Some(relation.initial_prompt.clone()),
        },
    );
    let rows = steps
        .into_iter()
        .map(
            |(command_id, step)| mj_controller::database::StartupDelivery {
                session_id: CHILD.into(),
                command_id,
                step_json: serde_json::to_string(&step).unwrap(),
                phase: "pending".into(),
                group_id: Some(group_id.clone()),
                accepted_ordinal: None,
                error: None,
            },
        )
        .collect();
    mj_controller::database::register_subagent_with_startup(&child, &relation, rows, None).unwrap();
    let root = tempfile::tempdir().unwrap();
    let model_id = format!("{group_id}-model");
    let prompt_id = format!("{group_id}-prompt");
    let input = vec![
        mj_core::worker_launch::InitialWorkerCommand {
            command_id: model_id.clone(),
            command: RelayCommand::SetConfig {
                key: "model".into(),
                value: "chosen-model".into(),
            },
            optional: false,
        },
        mj_core::worker_launch::InitialWorkerCommand {
            command_id: prompt_id.clone(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new(
                    relation.initial_prompt.clone(),
                ))],
            },
            optional: false,
        },
    ];
    let mut relay = mj_worker::relay::DurableRelay::open_for_harness(
        root.path(),
        CHILD,
        "test",
        mj_core::config::HarnessKind::Codex,
        &input,
    )
    .unwrap();
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(&model_id, RelayCommandOutcome::Configured)
        .unwrap();
    let prompt = relay.claim_pending_commands(true).unwrap().remove(0);
    relay
        .record_command_completed(
            &prompt_id,
            RelayCommandOutcome::Prompt {
                work_facts: None,
                stop_reason: "end_turn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    // Replay the actual worker journal through the controller's storage
    // boundary before attaching any daemon startup monitor.
    let events = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
    mj_controller::database::apply_projection_page(CHILD, move |page| {
        let mut current = mj_core::state::MaterializedSession::empty(CHILD);
        for event in events {
            let mutation = project_relay_event(&current, &event)?.mutation;
            page.apply(
                event.ordinal,
                &event.previous_digest,
                &event.digest,
                &mutation,
            )?;
            apply_committed_projection_event(&mut current, &event, mutation)?;
        }
        Ok(())
    })
    .unwrap();
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let first = mj_controller::database::next_startup_delivery(CHILD)
        .unwrap()
        .unwrap();
    state.start_persisted_startup_delivery(first, &CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mj_controller::database::load_startup_group(CHILD, &group_id)
                .unwrap()
                .iter()
                .all(|step| step.phase == "done")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parked worker receipts must settle without reconnecting");
    assert_eq!(
        mj_controller::database::load_subagent_report(CHILD)
            .unwrap()
            .awaited_ordinal,
        Some(prompt.accepted_ordinal)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), manager.requests.recv())
            .await
            .is_err(),
        "receipt monitoring must not contact or restart the parked worker"
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn cancelled_api_startup_is_not_submitted_after_daemon_reconstruction() {
    let Some(_writer) = startup_prompt_test_store(
        "cancelled_api_startup_is_not_submitted_after_daemon_reconstruction",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    state
        .queue_api_followup(
            "session-1",
            "cancelled-api".into(),
            crate::server::api::StartFollowup {
                prompt: Some("must never start".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state.cancel_api_followup("session-1").await.unwrap();
    state.cancel_and_join_startup_prompts().await.unwrap();
    let recovered = test_runtime_state_with_manager(&manager);
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    recovered
        .restore_startup_deliveries(&CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mj_controller::database::next_startup_delivery("session-1")
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled work should finish receipt cleanup");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), manager.requests.recv())
            .await
            .is_err(),
        "cancelled initial input must not become a new prompt on restart"
    );
    // The first daemon may already have dismissed the group, and startup
    // prunes dismissed groups, so an empty group also passes.
    assert!(
        mj_controller::database::load_latest_startup_group("session-1")
            .unwrap()
            .iter()
            .all(|step| step.phase == "dismissed")
    );
}

// Hard-won: 5165e5ba: cancelled missing-session steps retried and notified again after daemon restarts.
#[tokio::test]
async fn cancelled_api_startup_settles_without_a_managed_session() {
    let Some(_writer) =
        startup_prompt_test_store("cancelled_api_startup_settles_without_a_managed_session")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    state
        .queue_api_followup(
            "session-1",
            "cancelled-api".into(),
            crate::server::api::StartFollowup {
                prompt: Some("must never start".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state.cancel_api_followup("session-1").await.unwrap();
    state.cancel_and_join_startup_prompts().await.unwrap();
    // Removing the session also removes it from the manager.
    manager._targets.send_replace(Vec::new());
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.control.session("session-1").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the manager should drop the removed session");
    let recovered = test_runtime_state_with_manager(&manager);
    recovered
        .restore_startup_deliveries(&CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while mj_controller::database::next_startup_delivery("session-1")
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled work should settle without waiting for its session");
    // The first daemon may already have dismissed the group, and startup
    // prunes dismissed groups, so an empty group also passes.
    assert!(
        mj_controller::database::load_latest_startup_group("session-1")
            .unwrap()
            .iter()
            .all(|step| step.phase == "dismissed")
    );
}

#[tokio::test]
async fn startup_retry_keeps_earlier_uncertain_input_ahead_of_new_input() {
    let Some(_writer) =
        startup_prompt_test_store("startup_retry_keeps_earlier_uncertain_input_ahead_of_new_input")
    else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let cancellation = CancellationToken::new();
    state
        .queue_startup_step(
            "session-1",
            StartupStep::Prompt {
                move_operation_id: None,
                prompt: None,
                encoded_draft: None,
                text: "first".into(),
                inherited_draft: None,
            },
            &cancellation,
        )
        .await
        .unwrap();
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "first");
    reply
        .send(Err(unconfirmed("transient disconnect")))
        .unwrap();
    wait_for_startup_pause(&state).await;
    state
        .queue_startup_step(
            "session-1",
            StartupStep::Prompt {
                move_operation_id: None,
                prompt: None,
                encoded_draft: None,
                text: "second".into(),
                inherited_draft: None,
            },
            &cancellation,
        )
        .await
        .unwrap();
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(
        text, "first",
        "new input overtook the uncertain earlier command"
    );
    reply.send(Ok(4)).unwrap();
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "second");
    reply.send(Ok(8)).unwrap();
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[cfg(unix)]
// Hard-won: 352a87453: a review hold blocked foreground work and daemon handoff.
#[tokio::test]
async fn resumable_lifecycles_release_admission_and_reacquire_named_control() {
    const CHILD: &str = "MJ_TEST_RESUMABLE_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "resumable_lifecycles_release_admission_and_reacquire_named_control",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    for (id, kind, label) in [
        (
            "container-upgrade-admission",
            LifecycleKind::UpgradeContainer,
            "container worker swap",
        ),
        (
            "move-copy-admission",
            LifecycleKind::Move,
            "Move control transition",
        ),
        (
            "unpark-admission",
            LifecycleKind::Unpark,
            "subagent unpark launch",
        ),
    ] {
        let state = test_runtime_state();
        let review = state.session_gate.reserve_when_idle(id).await;
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let result = state
            .start_or_join_lifecycle(id.into(), kind, {
                let release = release.clone();
                let started = started.clone();
                move |state, session_id, _, _session_admission| async move {
                    let executor = DaemonStageReportingExecutor::new(
                        mj_controller::targets::ProcessExecutor,
                        state.clone(),
                        session_id.clone(),
                    );
                    assert_eq!(
                        state.owner().lifecycle[&session_id]
                            .upgrade_work
                            .lock()
                            .unwrap()
                            .is_some(),
                        kind != LifecycleKind::Unpark,
                        "Unpark releases handoff admission before waiting for foreground ownership",
                    );
                    executor.begin_resumable_work()?;
                    assert!(
                        state.owner().lifecycle[&session_id]
                            .upgrade_work
                            .lock()
                            .unwrap()
                            .is_none()
                    );
                    started.notify_one();
                    release.notified().await;
                    executor.end_resumable_work(label)?;
                    assert!(
                        state.owner().lifecycle[&session_id]
                            .upgrade_work
                            .lock()
                            .unwrap()
                            .is_some()
                    );
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("a worker-owned review must not hold up foreground lifecycle admission");
        assert!(
            state.owner().lifecycle[id].is_running(),
            "resumable work must keep lifecycle ownership while yielding upgrade admission"
        );
        release.notify_one();
        RuntimeState::wait_lifecycle_result(result).await.unwrap();
        assert!(
            state
                .session_gate
                .try_start(
                    id,
                    mj_controller::recovery_gate::BackgroundWork::WorkerReplacement
                )
                .is_err(),
            "the review continues across foreground work"
        );
        drop(review);
    }
}

/// Changing a live session's workspace is published to every dashboard: the
/// runtime row carries the new workspace id, and a dashboard that attaches
/// afterwards is given the session under the new workspace only.
#[tokio::test]
async fn changing_a_sessions_workspace_reaches_the_published_projection() {
    if isolate_runtime_feed_test(
        module_path!(),
        "changing_a_sessions_workspace_reaches_the_published_projection",
    ) {
        return;
    }

    let Some(_writer) =
        startup_prompt_test_store("changing_a_sessions_workspace_reaches_the_published_projection")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let destination = mj_controller::database::create_workspace("destination").unwrap();
    let mut live = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    live.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/worker/session-1".into(),
    }));
    mj_controller::database::save_session(&live).unwrap();
    assert!(state.session_record("session-1").is_some());
    let initial = state.runtime_changes(None, false).await.unwrap();
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica.apply(initial).unwrap();
    assert_eq!(
        replica
            .projection
            .records
            .get("session-1")
            .unwrap()
            .workspace_id,
        mj_core::workspace::DEFAULT_WORKSPACE_ID
    );

    handle_action(
        DaemonAction::SetSessionWorkspace {
            session_id: "session-1".into(),
            workspace_id: destination.id.clone(),
        },
        &test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        &state,
        &CancellationToken::new(),
    )
    .await
    .expect("change the workspace");

    let cursor = replica.cursor.clone();
    replica
        .apply(state.runtime_changes(cursor, false).await.unwrap())
        .unwrap();
    assert_eq!(
        replica
            .projection
            .records
            .get("session-1")
            .unwrap()
            .workspace_id,
        destination.id,
        "the attached dashboard kept the old workspace"
    );
    let fresh = state.runtime_changes(None, false).await.unwrap();
    let mj_client::runtime_feed::RuntimeFrame::Snapshot { projection, .. } = fresh else {
        panic!("a new dashboard is given a snapshot")
    };
    let members = |workspace: &str| {
        projection
            .records
            .iter()
            .filter(|(_, record)| record.workspace_id == workspace)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(members(&destination.id), ["session-1"]);
    assert!(members(mj_core::workspace::DEFAULT_WORKSPACE_ID).is_empty());
}

/// Toggling a session's notification mute persists it and reaches the
/// published projection an attached dashboard reads.
#[tokio::test]
async fn ignoring_notifications_persists_and_reaches_the_published_projection() {
    if isolate_runtime_feed_test(
        module_path!(),
        "ignoring_notifications_persists_and_reaches_the_published_projection",
    ) {
        return;
    }

    let Some(_writer) = startup_prompt_test_store(
        "ignoring_notifications_persists_and_reaches_the_published_projection",
    ) else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    mj_controller::database::save_session(&runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    ))
    .unwrap();
    handle_action(
        DaemonAction::SetSessionNotificationsIgnored {
            session_id: "session-1".into(),
            ignored: true,
        },
        &test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        &state,
        &CancellationToken::new(),
    )
    .await
    .expect("ignore notifications");
    let fresh = state.runtime_changes(None, false).await.unwrap();
    let mj_client::runtime_feed::RuntimeFrame::Snapshot { projection, .. } = fresh else {
        panic!("a new dashboard is given a snapshot")
    };
    assert!(projection.records["session-1"].notifications_ignored);
    assert!(
        mj_controller::database::load_state().unwrap().sessions["session-1"].notifications_ignored,
        "the flag is durable"
    );
    handle_action(
        DaemonAction::SetSessionNotificationsIgnored {
            session_id: "missing".into(),
            ignored: true,
        },
        &test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        &state,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
}

#[tokio::test]
async fn shutdown_joins_independent_startup_teardown_before_closing_the_store() {
    const TEST: &str = "shutdown_joins_independent_startup_teardown_before_closing_the_store";
    const CHILD: &str = "MJ_TEST_SHUTDOWN_JOINS_STARTUP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Failed startup cleanup is deferrable, and admission is process-wide,
        // so a handoff another test is draining refuses it here. Run alone with
        // a fresh gate so this measures the shutdown join and not that refusal.
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state();
    let (started, stopping) = tokio::sync::oneshot::channel();
    let result = state
        .start_or_join_lifecycle(
            "failed-startup".into(),
            LifecycleKind::StartupCleanup,
            |_state, _id, _cancelled, _session_admission| async move {
                started.send(()).unwrap();
                // A process-tree stop exceeds the former one-second join window.
                tokio::time::sleep(Duration::from_millis(1100)).await;
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await
        .unwrap();
    // Shutdown interrupts a teardown that still waits for its session, and
    // the next daemon runs it. This one is already stopping the process tree.
    stopping.await.unwrap();
    state.cancel_and_wait_lifecycles().await.unwrap();
    assert!(matches!(
        RuntimeState::wait_lifecycle_result(result).await.unwrap(),
        DaemonLifecycleResult::Done
    ));
}

pub(super) fn test_runtime_state_holding(state: mj_core::state::State) -> Arc<RuntimeState> {
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller {
            config: Config::default(),
            state,
        },
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        || {
            Ok(Controller {
                config: Config::default(),
                state: mj_core::state::State::default(),
            })
        },
    ))
}

#[tokio::test]
async fn resume_candidates_are_inactive_top_level_sessions_with_every_adopted_native_session() {
    let mut live = runtime_test_session("live", "workspace", SessionState::Running);
    live.native_session_id = Some("native-live".into());
    let mut stopped = runtime_test_session("stopped", "workspace", SessionState::Stopped);
    stopped.native_session_id = Some("native-stopped".into());
    stopped.acp_session_title = Some("long title ".repeat(10_000));
    let lost = runtime_test_session("lost", "workspace", SessionState::Lost);
    let child = runtime_test_session("child", "workspace", SessionState::Stopped);
    let mut state = mj_core::state::State::default();
    for record in [live, stopped, lost, child] {
        state.sessions.insert(record.id.clone(), record);
    }
    state.subagents.insert(
        "child".into(),
        runtime_test_subagent("child", "live").into(),
    );
    let candidates = test_runtime_state_holding(state).resume_candidates();
    let ids = candidates
        .candidates
        .iter()
        .map(|candidate| candidate.session_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["lost", "stopped"]));
    // A row carries what it shows, not the whole record.
    let stopped = candidates
        .candidates
        .iter()
        .find(|candidate| candidate.session_id == "stopped")
        .unwrap();
    assert_eq!(
        stopped.title.chars().count(),
        mj_client::daemon::ResumeCandidate::TITLE_CHARS
    );
    let adopted = candidates
        .adopted_native_sessions
        .iter()
        .map(|(_, id)| id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(adopted, BTreeSet::from(["native-live", "native-stopped"]));
}

#[tokio::test]
async fn go_startup_session_falls_back_when_the_remembered_session_is_archived() {
    let mut archived = runtime_test_session("remembered", "workspace", SessionState::Stopped);
    archived.archived = true;
    let mut older = runtime_test_session("older", "workspace", SessionState::Stopped);
    older.updated_at = "2026-09-04T00:00:00Z".into();
    let mut newest = runtime_test_session("newest", "workspace", SessionState::Stopped);
    newest.updated_at = "2026-09-05T00:00:00Z".into();
    let mut child = runtime_test_session("child", "workspace", SessionState::Stopped);
    child.updated_at = "2026-10-01T00:00:00Z".into();
    let mut elsewhere = runtime_test_session("elsewhere", "other-workspace", SessionState::Stopped);
    elsewhere.updated_at = "2026-10-02T00:00:00Z".into();
    let mut state = mj_core::state::State {
        sessions: [archived, older, newest, child.clone(), elsewhere]
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect(),
        ..mj_core::state::State::default()
    };
    state.subagents.insert(
        child.id.clone(),
        runtime_test_subagent(&child.id, "parent").into(),
    );
    let runtime = test_runtime_state_holding(state);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(
            stream,
            test_metadata(address),
            runtime,
            CancellationToken::new(),
        )
        .await
    });
    let mut client = mj_client::daemon::DaemonClient::connect(test_metadata(address))
        .await
        .expect("connect to daemon protocol test server");

    let selected = client
        .go_startup_session("workspace".into(), Some("remembered".into()))
        .await
        .expect("query startup session")
        .expect("eligible fallback session");

    assert_eq!(selected.id, "newest");
    drop(client);
    assert!(server.await.expect("daemon task").is_ok());
}

#[tokio::test]
async fn resume_candidates_larger_than_a_frame_arrive_whole() {
    let mut record = runtime_test_session("stopped", "workspace", SessionState::Stopped);
    record.acp_session_title = Some("t".repeat(500));
    let row = mj_client::daemon::ResumeCandidate::of(&record, &Config::default());
    // Enough rows that the list outgrows one frame, as a large store's does.
    let rows = MAX_FRAME_BYTES / serde_json::to_vec(&row).unwrap().len() + 1_000;
    let candidates = mj_client::daemon::ResumeCandidates {
        candidates: vec![row; rows],
        ..Default::default()
    };
    assert!(serde_json::to_vec(&candidates).unwrap().len() > MAX_FRAME_BYTES);
    let expected = candidates.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        for (request_id, result) in [
            (3, Ok(DaemonReply::ResumeCandidates(Box::new(candidates)))),
            (4, Ok(DaemonReply::Pong)),
        ] {
            super::serve::write_response(
                &mut stream,
                ResponseEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    result,
                },
            )
            .await
            .unwrap();
        }
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let response = mj_client::daemon::read_response(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 3);
    let Ok(DaemonReply::ResumeCandidates(received)) = response.result else {
        panic!("expected the whole candidate list")
    };
    assert_eq!(*received, expected);
    let response = mj_client::daemon::read_response(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 4);
    assert!(matches!(response.result, Ok(DaemonReply::Pong)));
    sender.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn archiving_old_children_needs_no_index_copy_and_preserves_the_parent() {
    const NAME: &str = "archiving_old_children_needs_no_index_copy_and_preserves_the_parent";
    if !in_isolated_live_parent_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Top-level archive").unwrap();
    let repository = mj_controller::controller::test_support::committed_repository();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "33333333333333333333333333333333";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.project_directory = Some(repository.path().to_owned());
    parent.updated_at = "2000-01-01T00:00:00Z".into();
    let archives = mj_core::config::sessions_dir();
    std::fs::create_dir_all(&archives).unwrap();
    let mut input = mj_controller::controller::test_support::checkpoint_archive_input(
        parent_id,
        1,
        Vec::new(),
        Vec::new(),
    );
    input
        .canonical_session
        .transcript
        .push(mj_core::archive::CanonicalTranscriptItem {
            stable_id: "prompt".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1_700_000_000_000,
            last_changed_at_ms: 1_700_000_000_000,
            body: mj_core::archive::CanonicalTranscriptBody::User {
                content: vec![
                    serde_json::json!({"type": "text", "text": "parent quokka conversation"}),
                ],
            },
        });
    parent.checkpoint = Some(Box::new(
        mj_controller::controller::test_support::write_checkpoint_archive_input(
            &archives, parent_id, &input,
        ),
    ));
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = parent.clone();
    child.state = SessionState::Stopped;
    child.id = child_id.into();
    child.checkpoint = None;
    mj_controller::database::save_subagent_session(
        &child,
        &runtime_test_subagent(child_id, parent_id),
    )
    .unwrap();
    parent.state = SessionState::Stopped;
    mj_controller::database::save_session(&parent).unwrap();
    let state = test_runtime_state_loading_the_store();
    let archived = tokio::time::timeout(Duration::from_secs(60), state.archive_aged_sessions(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        archived, 2,
        "the child must not be held back for having no index row"
    );
    let stored = mj_controller::database::load_state().unwrap();
    assert!(!stored.sessions.contains_key(parent_id));
    assert!(!stored.sessions.contains_key(child_id));
    let rows =
        mj_controller::sessionwiki::query_rows("quokka", 10, &Default::default(), true).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        [parent_id]
    );
    assert!(repository.path().join(".git").exists());
}

/// What a relay actor publishes reaches a terminal client as item changes:
/// a client that fetched the tail at its cursor and applies the frames after
/// it holds the tail the daemon serves at the newest cursor, and a session
/// whose actor retires takes its tail out of the feed.
#[tokio::test]
async fn published_transcripts_reach_a_client_as_item_changes() {
    if isolate_runtime_feed_test(
        module_path!(),
        "published_transcripts_reach_a_client_as_item_changes",
    ) {
        return;
    }

    let state = test_runtime_state();
    state.owner().edit_sessions(|sessions| {
        sessions.insert(
            "session-1".into(),
            runtime_test_session("session-1", "workspace", SessionState::Running),
        );
    });
    state
        .owner()
        .install_relay_sessions(["session-1".to_owned()].into());
    let item = |position: u64, text: &str| {
        Arc::new(mj_core::transcript::TranscriptItem {
            stable_id: format!("agent:{position}"),
            position,
            latest_content_event_ordinal: Some(position),
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: mj_core::transcript::TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content": {"type": "text", "text": text}})],
                streaming: true,
            },
        })
    };
    let view = |items: Vec<Arc<mj_core::transcript::TranscriptItem>>| {
        let mut view = ready_startup_view();
        let snapshot = view.snapshot.as_mut().unwrap();
        snapshot.materialized.applied_event_ordinal = items.len() as u64;
        snapshot.materialized.transcript = items;
        snapshot.window = mj_core::state::ProjectionWindow::of(&snapshot.materialized);
        view
    };
    let first = item(1, "first");
    state
        .publish_session("session-1".into(), view(vec![Arc::clone(&first)]))
        .await
        .unwrap();
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    let cursor = replica.cursor.clone().unwrap();
    let mj_client::runtime_feed::SessionTailReply::Tail {
        header,
        window,
        items,
    } = state.session_tail("session-1", &cursor)
    else {
        panic!("a published projection has a tail");
    };
    replica.projection.transcripts.insert(
        "session-1".into(),
        mj_client::runtime_feed::SessionTail::from_parts(*header, window, items),
    );

    state
        .publish_session(
            "session-1".into(),
            view(vec![first, item(2, "second, streaming")]),
        )
        .await
        .unwrap();
    let frame = state.runtime_changes(Some(cursor), false).await.unwrap();
    let mj_client::runtime_feed::RuntimeFrame::Delta { changes, .. } = &frame else {
        panic!("a held cursor gets a delta, not {frame:?}");
    };
    assert!(
        matches!(&changes.transcripts[..],
            [(id, mj_client::runtime_feed::TranscriptChange::Items(items))]
                if id == "session-1" && items.upserts.len() == 1 && items.removes.is_empty()),
        "only the new item travels: {:?}",
        changes.transcripts
    );
    replica.apply(frame).unwrap();
    let newest = replica.cursor.clone().unwrap();
    assert_eq!(
        mj_client::runtime_feed::SessionTailReply::of(&replica.projection.transcripts["session-1"]),
        state.session_tail("session-1", &newest),
    );

    state.owner().install_relay_sessions(BTreeSet::new());
    state.publish_revision();
    replica
        .apply(state.runtime_changes(Some(newest), false).await.unwrap())
        .unwrap();
    assert!(replica.projection.transcripts.is_empty());
}

/// Move admission and startup delivery share one reservation: an earlier
/// acceptance finishes before source work, and source readiness cannot admit
/// anything queued during Move ahead of its archived queue disposition.
#[tokio::test]
async fn startup_delivery_and_move_source_sealing_are_serialized() {
    const NAME: &str = "startup_delivery_and_move_source_sealing_are_serialized";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    handle_action(
        queued_prompt_action("before move"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    let (text, accepted) = next_submit(&mut manager).await;
    assert_eq!(text, "before move");
    manager
        .publisher
        .publish("session-1".into(), ManagedSessionView::default())
        .await
        .unwrap();
    handle_action(
        queued_prompt_action("waiting before move"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    let source_started = Arc::new(tokio::sync::Notify::new());
    let destination_ready = Arc::new(tokio::sync::Notify::new());
    let watch = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Move, {
            let source_started = source_started.clone();
            let destination_ready = destination_ready.clone();
            move |state, id, _, _session_admission| async move {
                source_started.notify_one();
                destination_ready.notified().await;
                // The Move owns archive disposition while holding the reservation.
                let handle = state.session_manager.session(&id).await?;
                handle
                    .submit(
                        "archived-move-command".into(),
                        RelayCommand::Prompt {
                            prompt: vec![ContentBlock::Text(TextContent::new("archived queue"))],
                        },
                    )
                    .await?;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .await
        .unwrap();
    handle_action(
        queued_prompt_action("during move"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), source_started.notified())
            .await
            .is_err(),
        "Move source sealing overtook an already admitted startup submit"
    );
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    accepted.send(Ok(1)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), source_started.notified())
        .await
        .unwrap();
    // A still-ready source never receives the newly queued prompt.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err()
    );
    destination_ready.notify_one();
    let (text, archived) = next_submit(&mut manager).await;
    assert_eq!(text, "archived queue");
    archived.send(Ok(2)).unwrap();
    RuntimeState::wait_lifecycle_result(watch).await.unwrap();
    let (text, delivered) = next_submit(&mut manager).await;
    assert_eq!(text, "waiting before move");
    delivered.send(Ok(3)).unwrap();
    let (text, delivered) = next_submit(&mut manager).await;
    assert_eq!(text, "during move");
    delivered.send(Ok(4)).unwrap();
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn startup_delivery_waits_for_durable_move_after_daemon_restart() {
    const NAME: &str = "startup_delivery_waits_for_durable_move_after_daemon_restart";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let session = state.session_record("session-1").unwrap();
    let mut operation = retained_startup_test_move(&session);
    mj_controller::database::save_move_operation(&operation).unwrap();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    handle_action(
        queued_prompt_action("queued before move"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    handle_action(
        queued_move_prompt_action("destination only", &operation.operation_id),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err()
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
    let restarted = test_runtime_state_with_manager(&manager);
    restarted
        .restore_startup_deliveries(&CancellationToken::new())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "restored startup work was sent before durable archived queue admission"
    );
    operation.destination_store_id = Some("destination-store".into());
    operation.phase = mj_core::state::MovePhase::Completed;
    operation.queue_admission_finished = true;
    mj_controller::database::save_move_operation(&operation).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "completed Move admitted a stale ready source actor"
    );
    let mut destination = ready_startup_view();
    destination.snapshot.as_mut().unwrap().operational.store_id = Some("destination-store".into());
    manager
        .publisher
        .publish("session-1".into(), destination.clone())
        .await
        .unwrap();
    let (text, delivered) = next_submit(&mut manager).await;
    assert_eq!(text, "queued before move");
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    delivered.send(Ok(9)).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
            .await
            .is_err(),
        "Move-origin input admitted a stale ready source actor"
    );
    manager
        .publisher
        .publish("session-1".into(), destination)
        .await
        .unwrap();
    let (text, delivered) = next_submit(&mut manager).await;
    assert_eq!(text, "destination only");
    delivered.send(Ok(10)).unwrap();
    restarted.cancel_and_join_startup_prompts().await.unwrap();
}

fn retained_startup_test_move(
    session: &mj_core::state::SessionRecord,
) -> mj_core::state::MoveOperation {
    mj_core::state::MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "startup-durable-move".into(),
        selection: MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            workspace: Default::default(),
            session_id: session.id.clone(),
            profile_id: Some(session.last_profile.clone()),
            target_template_id: Some(session.target_template_id.clone()),
            additional_mounts: None,
            resource_allocation: None,
            clear_resource_allocation: false,
        },
        source_profile_id: session.last_profile.clone(),
        source_target_template_id: session.target_template_id.clone(),
        source_target: None,
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "test".into(),
        checkpoint: None,
        recovery_session: Some(session.clone()),
        queue: mj_core::state::ResumeQueueDisposition::Start,
        phase: mj_core::state::MovePhase::StartingQueue,
        queue_admission_started: true,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
        error: None,
    }
}

fn startup_image_draft(label: &str, reference: &mj_core::attachment::AttachmentRef) -> String {
    let text = format!("{label}[image 1]");
    format!(
        "{}{}",
        mj_core::draft::CHAT_DRAFT_PREFIX,
        serde_json::json!({
            "text": text,
            "images": [{"range": {"start": label.len(), "end": text.len()}, "number": 1,
                "image": {"mime_type": reference.mime_type, "reference": reference}}],
            "retained_metadata": {"revision": 1}
        })
    )
}

#[tokio::test]
async fn startup_image_refusal_recovers_complete_drafts_and_invalid_merges_stay_durable() {
    const NAME: &str =
        "startup_image_refusal_recovers_complete_drafts_and_invalid_merges_stay_durable";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let reference =
        mj_core::attachment::AttachmentRef::new(b"\x89PNG\r\n\x1a\n", "image/png".into(), 1, 1)
            .unwrap();
    let recovered = startup_image_draft("queued ", &reference);
    let existing = startup_image_draft("existing 🐈 ", &reference);
    let mut saved: serde_json::Value = serde_json::from_str(
        existing
            .strip_prefix(mj_core::draft::CHAT_DRAFT_PREFIX)
            .unwrap(),
    )
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(
        recovered
            .strip_prefix(mj_core::draft::CHAT_DRAFT_PREFIX)
            .unwrap(),
    )
    .unwrap();
    let pending = |id: &str, startup: bool| {
        let mut submission = serde_json::json!({
            "id": id, "kind": "Prompt", "payload": payload,
            "status": "Delivery unconfirmed", "represented": false,
            "finished": true, "recorded_at_ms": 1
        });
        if startup {
            submission["startup_origin"] = serde_json::json!({
                "move_operation_id": null, "inherited_draft": null
            });
        }
        submission
    };
    saved["pending"] = serde_json::json!([
        pending("image-startup", true),
        pending("unrelated-native", false),
        pending("unrelated-startup", true)
    ]);
    let existing = format!("{}{}", mj_core::draft::CHAT_DRAFT_PREFIX, saved);
    mj_controller::database::set_session_draft_input("session-1", &existing).unwrap();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    let action = DaemonAction::QueueStartupPrompt {
        move_operation_id: None,
        session_id: "session-1".into(),
        command_id: "image-startup".into(),
        text: "queued [image 1]".into(),
        prompt: vec![reference.content_block()],
        inherited_draft: None,
        encoded_draft: recovered.clone(),
    };
    // Stable retries persist one structured row, rather than another text row.
    for _ in 0..2 {
        handle_action(action.clone(), &metadata, &state, &cancellation)
            .await
            .unwrap();
    }
    assert_eq!(
        mj_controller::database::load_startup_deliveries()
            .unwrap()
            .len(),
        1
    );
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let before_retry = session_draft_input(&state);
    handle_action(action.clone(), &metadata, &state, &cancellation)
        .await
        .unwrap();
    assert_eq!(
        session_draft_input(&state),
        before_retry,
        "settled enqueue retry restored input twice"
    );
    let mut changed = action.clone();
    if let DaemonAction::QueueStartupPrompt { text, .. } = &mut changed {
        text.push_str(" changed");
    }
    assert!(matches!(
        handle_action(changed, &metadata, &state, &cancellation)
            .await
            .unwrap(),
        DaemonReply::StartupPromptIdentityConflict
    ));
    let outcomes = handle_action(
        DaemonAction::StartupPromptOutcomes {
            session_id: "session-1".into(),
            command_ids: vec!["image-startup".into(), "missing".into()],
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    let DaemonReply::StartupPromptOutcomes(outcomes) = outcomes else {
        panic!("expected outcome receipts");
    };
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes["image-startup"], mj_client::daemon::StartupPromptOutcome::Refused { encoded_draft, reason }
        if encoded_draft == &recovered && reason.contains("does not support image prompts"))
    );
    let draft = session_draft_input(&state);
    let envelope: serde_json::Value = serde_json::from_str(
        draft
            .strip_prefix(mj_core::draft::CHAT_DRAFT_PREFIX)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        envelope["text"],
        "existing 🐈 [image 1]\n\nqueued [image 2]"
    );
    for image in envelope["images"].as_array().unwrap() {
        let text = envelope["text"].as_str().unwrap();
        let start = image["range"]["start"].as_u64().unwrap() as usize;
        let end = image["range"]["end"].as_u64().unwrap() as usize;
        assert_eq!(&text[start..end], format!("[image {}]", image["number"]));
        assert_eq!(
            image["image"]["reference"],
            serde_json::to_value(&reference).unwrap()
        );
    }
    assert_eq!(
        envelope["retained_metadata"],
        serde_json::json!({"revision": 1})
    );
    assert_eq!(
        envelope["pending"]
            .as_array()
            .unwrap()
            .iter()
            .map(|submission| submission["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["unrelated-native", "unrelated-startup"],
        "reopening a recovered draft must not retry its already restored startup payload"
    );
    assert!(
        notice_texts(&state)
            .iter()
            .any(|notice| notice.contains("does not support image prompts"))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
            .await
            .is_err(),
        "unsupported image reached the worker"
    );
    // Duplicate visible text has distinct caller identities. Recall only the
    // chosen message, then deliver the remaining installed image unchanged.
    manager
        .publisher
        .publish("session-1".into(), ManagedSessionView::default())
        .await
        .unwrap();
    for command_id in ["image-keep", "image-recall"] {
        handle_action(
            DaemonAction::QueueStartupPrompt {
                move_operation_id: None,
                session_id: "session-1".into(),
                command_id: command_id.into(),
                text: "queued [image 1]".into(),
                prompt: vec![reference.content_block()],
                encoded_draft: recovered.clone(),
                inherited_draft: None,
            },
            &metadata,
            &state,
            &cancellation,
        )
        .await
        .unwrap();
    }
    let withdrawn = handle_action(
        DaemonAction::WithdrawStartupPrompt {
            session_id: "session-1".into(),
            command_id: "image-recall".into(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(matches!(withdrawn, DaemonReply::PromptWithdrawn(true)));
    let mut supported = ready_startup_view();
    supported
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .agent_capabilities = Some(Box::new(
        serde_json::from_value(serde_json::json!({"promptCapabilities": {"image": true}})).unwrap(),
    ));
    manager
        .publisher
        .publish("session-1".into(), supported)
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit {
        command_id,
        command,
        reply,
        ..
    } = request
    else {
        panic!("expected structured prompt");
    };
    assert_eq!(command_id, "image-keep");
    assert_eq!(
        serde_json::to_value(command).unwrap(),
        serde_json::to_value(RelayCommand::Prompt {
            prompt: vec![reference.content_block()]
        })
        .unwrap()
    );
    reply.send(Ok(12)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    mj_controller::database::prune_settled_startup_deliveries().unwrap();
    let outcomes = mj_controller::database::startup_prompt_outcomes(
        "session-1",
        &["image-keep".into(), "image-recall".into()],
    )
    .unwrap();
    assert_eq!(
        outcomes["image-keep"],
        mj_client::daemon::StartupPromptOutcome::Accepted
    );
    assert_eq!(
        outcomes["image-recall"],
        mj_client::daemon::StartupPromptOutcome::Withdrawn
    );
    let recovered_before_conflict = session_draft_input(&state);
    let conflict = handle_action(
        DaemonAction::QueueStartupPrompt {
            move_operation_id: None,
            session_id: "session-1".into(),
            command_id: "image-keep".into(),
            text: "queued [image 1]".into(),
            prompt: vec![reference.content_block()],
            encoded_draft: recovered.clone(),
            inherited_draft: Some("different inherited draft".into()),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(matches!(
        conflict,
        DaemonReply::StartupPromptIdentityConflict
    ));
    assert_eq!(
        mj_controller::database::startup_prompt_outcomes("session-1", &["image-keep".into()])
            .unwrap()["image-keep"],
        mj_client::daemon::StartupPromptOutcome::Accepted,
        "a conflicting replay must retain the original accepted receipt"
    );
    assert_eq!(session_draft_input(&state), recovered_before_conflict);
    // Store integration also covers plain/encoded in both directions and an
    // invalid envelope retaining the entire refused durable payload.
    for (left, right) in [
        ("new typing", recovered.as_str()),
        (recovered.as_str(), "more typing"),
    ] {
        mj_controller::database::set_session_draft_input("session-1", left).unwrap();
        state.append_draft_input("session-1", right).await.unwrap();
        mj_core::draft::validate_reference_draft(&session_draft_input(&state)).unwrap();
    }
    mj_controller::database::set_session_draft_input("session-1", "mjolnir-chat-draft-v1:broken")
        .unwrap();
    mj_controller::database::enqueue_startup_deliveries(vec![
        mj_controller::database::StartupDelivery {
            session_id: "session-1".into(),
            command_id: "recover-invalid".into(),
            step_json: serde_json::to_string(&StartupStep::Prompt {
                move_operation_id: None,
                text: "queued [image 1]".into(),
                prompt: Some(vec![reference.content_block()]),
                encoded_draft: Some(recovered.clone()),
                inherited_draft: None,
            })
            .unwrap(),
            phase: "pending".into(),
            group_id: None,
            accepted_ordinal: None,
            error: None,
        },
    ])
    .unwrap();
    mj_controller::database::reject_startup_prompt("recover-invalid", "unsupported image").unwrap();
    assert!(
        mj_controller::database::recover_startup_prompt("recover-invalid", &recovered).is_err()
    );
    let rows = mj_controller::database::load_startup_deliveries().unwrap();
    assert_eq!(rows[0].phase, "rejecting");
    assert!(rows[0].step_json.contains("queued [image 1]"));
    assert_eq!(session_draft_input(&state), "mjolnir-chat-draft-v1:broken");
    state.cancel_and_join_startup_prompts().await.unwrap();
    mj_controller::database::delete_session("session-1").unwrap();
    assert!(
        mj_controller::database::startup_prompt_outcomes(
            "session-1",
            &[
                "image-startup".into(),
                "image-keep".into(),
                "image-recall".into()
            ]
        )
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn startup_delivery_reservation_wait_does_not_prevent_move_cancellation() {
    const NAME: &str = "startup_delivery_reservation_wait_does_not_prevent_move_cancellation";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let session_id = "session-1";
    let reservation = state.startup_delivery_gate(session_id).lock_owned().await;
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    handle_action(
        queued_move_prompt_action("cancelled before source", "pre-reservation-origin"),
        &metadata,
        &state,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let entered_source = Arc::new(AtomicBool::new(false));
    let control = LifecycleControl::default();
    let watch = state
        .admit_lifecycle(
            session_id.into(),
            LifecycleKind::Move,
            super::lifecycle::LifecycleStart {
                origin: super::lifecycle::LifecycleOrigin::Automatic,
                resume_workspace_id: None,
                request_key: None,
                control: Some(control.clone()),
                phase: LifecyclePhase::Executing,
                move_operation_id: Some("pre-reservation-origin".into()),
                deferrable_admission: None,
                holds_through_drain: false,
            },
            {
                let entered_source = entered_source.clone();
                move |_, _, _, _session_admission| async move {
                    entered_source.store(true, Ordering::Release);
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(
        state.active_lifecycles()[0].move_operation_id.as_deref(),
        Some("pre-reservation-origin")
    );
    assert!(control.request_cancel("test cancellation"));
    tokio::time::timeout(
        Duration::from_secs(2),
        RuntimeState::wait_lifecycle_result(watch),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !entered_source.load(Ordering::Acquire),
        "cancelled Move started source work"
    );
    assert_eq!(session_draft_input(&state), "cancelled before source");
    let outcomes = mj_controller::database::startup_prompt_outcomes(
        session_id,
        &["startup-cancelled before source".into()],
    )
    .unwrap();
    assert!(matches!(
        outcomes.values().next(),
        Some(mj_client::daemon::StartupPromptOutcome::Refused { .. })
    ));
    drop(reservation);
    state.cancel_and_join_startup_prompts().await.unwrap();
    // A stale frontend finishes enqueueing after cancellation and after the
    // ephemeral owner has disappeared. No durable Move ever existed.
    state.owner().lifecycle.remove(session_id);
    assert!(
        mj_controller::database::load_move_operation(session_id)
            .unwrap()
            .is_none()
    );
    manager
        .publisher
        .publish(session_id.into(), ready_startup_view())
        .await
        .unwrap();
    handle_action(
        queued_move_prompt_action("late destination draft", "pre-reservation-origin"),
        &metadata,
        &state,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                mj_controller::database::startup_prompt_outcomes(
                    session_id,
                    &["startup-late destination draft".into()]
                )
                .unwrap()
                .values()
                .next(),
                Some(mj_client::daemon::StartupPromptOutcome::Refused { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(session_draft_input(&state).contains("late destination draft"));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), manager.requests.recv())
            .await
            .is_err(),
        "late cancelled-origin enqueue reached the ready source"
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
    manager
        .publisher
        .publish(session_id.into(), ManagedSessionView::default())
        .await
        .unwrap();
    // A daemon handoff interrupts resumable work; it is not a user refusal.
    mj_controller::database::set_session_draft_input(session_id, "").unwrap();
    let reservation = state.startup_delivery_gate(session_id).lock_owned().await;
    handle_action(
        queued_prompt_action("keep through handoff"),
        &metadata,
        &state,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let control = LifecycleControl::default();
    // Clear the earlier completed operation as an ordinary caller does.
    state.owner().lifecycle.remove(session_id);
    let watch = state
        .start_or_join_lifecycle_controlled(
            session_id.into(),
            LifecycleKind::Move,
            None,
            None,
            Some(control.clone()),
            |_, _, _, _session_admission| async {
                panic!("handoff interrupted before source work")
            },
        )
        .await
        .unwrap();
    assert!(control.cancel_for_shutdown());
    RuntimeState::wait_lifecycle_result(watch).await.unwrap();
    assert_eq!(session_draft_input(&state), "");
    assert!(
        mj_controller::database::load_startup_deliveries()
            .unwrap()
            .iter()
            .any(
                |step| step.command_id == "startup-keep through handoff" && step.phase == "pending"
            )
    );
    drop(reservation);
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn startup_failed_and_cancelled_moves_restore_images_instead_of_using_retained_source() {
    const NAME: &str =
        "startup_failed_and_cancelled_moves_restore_images_instead_of_using_retained_source";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let reference =
        mj_core::attachment::AttachmentRef::new(b"\x89PNG\r\n\x1a\n", "image/png".into(), 1, 1)
            .unwrap();
    let draft = startup_image_draft("destination image ", &reference);
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    for (index, phase) in [
        mj_core::state::MovePhase::Failed,
        mj_core::state::MovePhase::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        mj_controller::database::set_session_draft_input("session-1", "").unwrap();
        let mut session = state.session_record("session-1").unwrap();
        session.state = SessionState::Running;
        mj_controller::database::save_session(&session).unwrap();
        let mut operation = retained_startup_test_move(&session);
        operation.in_place = true;
        operation.queue_admission_started = index != 0;
        operation.phase = mj_core::state::MovePhase::Preparing;
        mj_controller::database::save_move_operation(&operation).unwrap();
        manager
            .publisher
            .publish("session-1".into(), ready_startup_view())
            .await
            .unwrap();
        let command_id = format!("failed-move-image-{index}");
        handle_action(
            DaemonAction::QueueStartupPrompt {
                move_operation_id: Some(operation.operation_id.clone()),
                session_id: "session-1".into(),
                command_id: command_id.clone(),
                text: "destination image [image 1]".into(),
                prompt: vec![reference.content_block()],
                encoded_draft: draft.clone(),
                inherited_draft: None,
            },
            &metadata,
            &state,
            &cancellation,
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.requests.recv())
                .await
                .is_err()
        );
        operation.phase = phase;
        operation.error = Some("test destination failure".into());
        mj_controller::database::save_move_operation(&operation).unwrap();
        assert_eq!(session_draft_input(&state), draft);
        let outcomes =
            mj_controller::database::startup_prompt_outcomes("session-1", &[command_id]).unwrap();
        assert!(
            matches!(outcomes.values().next(), Some(mj_client::daemon::StartupPromptOutcome::Refused { encoded_draft, reason })
            if encoded_draft == &draft && reason.contains("test destination failure"))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), manager.requests.recv())
                .await
                .is_err(),
            "failed Move delivered destination input to source"
        );
    }
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn startup_retry_exhaustion_preserves_uncertain_command_identity() {
    const NAME: &str = "startup_retry_exhaustion_preserves_uncertain_command_identity";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    handle_action(
        queued_prompt_action("uncertain original"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    for attempt in 0..=super::state::STARTUP_STEP_ATTEMPTS {
        let request = tokio::time::timeout(Duration::from_secs(20), manager.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let RemoteSessionRequest::Submit {
            command_id, reply, ..
        } = request
        else {
            panic!("expected receipt reconciliation");
        };
        assert_eq!(command_id, "startup-uncertain original");
        assert_eq!(
            session_draft_input(&state),
            "",
            "uncertain acceptance became editable input"
        );
        if attempt == super::state::STARTUP_STEP_ATTEMPTS {
            reply.send(Ok(77)).unwrap();
        } else {
            reply
                .send(Err(unconfirmed(
                    "receipt lookup unavailable after original acceptance",
                )))
                .unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(session_draft_input(&state), "");
    assert_eq!(
        mj_controller::database::startup_prompt_outcomes(
            "session-1",
            &["startup-uncertain original".into()]
        )
        .unwrap()["startup-uncertain original"],
        mj_client::daemon::StartupPromptOutcome::Accepted
    );
    state.cancel_and_join_startup_prompts().await.unwrap();
}

#[tokio::test]
async fn startup_without_move_origin_ignores_historical_failed_move_after_resume() {
    const NAME: &str = "startup_without_move_origin_ignores_historical_failed_move_after_resume";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let mut session = state.session_record("session-1").unwrap();
    let mut historical = retained_startup_test_move(&session);
    historical.phase = mj_core::state::MovePhase::Failed;
    historical.queue_admission_started = false;
    historical.queue_admission_finished = false;
    historical.error = Some("historical destination failed".into());
    mj_controller::database::save_move_operation(&historical).unwrap();
    // Ordinary Resume installs its current state without erasing old Move history.
    session.state = SessionState::Running;
    session.native_session_id = Some("resumed-native".into());
    mj_controller::database::save_session(&session).unwrap();
    let mut ready = ready_startup_view();
    ready
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .native_session_id = Some("resumed-native".into());
    manager
        .publisher
        .publish("session-1".into(), ready)
        .await
        .unwrap();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();
    handle_action(
        queued_prompt_action("ordinary resumed startup"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    let (text, reply) = next_submit(&mut manager).await;
    assert_eq!(text, "ordinary resumed startup");
    reply.send(Ok(91)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                mj_controller::database::startup_prompt_outcomes(
                    "session-1",
                    &["startup-ordinary resumed startup".into()]
                )
                .unwrap()
                .values()
                .next(),
                Some(mj_client::daemon::StartupPromptOutcome::Accepted)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(session_draft_input(&state), "");
    assert_eq!(
        mj_controller::database::load_move_operation("session-1")
            .unwrap()
            .unwrap()
            .phase,
        mj_core::state::MovePhase::Failed
    );
    // Retrying that same Move is a current owner, even while its older
    // failure is still the committed durable snapshot.
    let control = LifecycleControl::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let entered = started.clone();
    let released = release.clone();
    let watch = state
        .admit_lifecycle(
            "session-1".into(),
            LifecycleKind::Move,
            super::lifecycle::LifecycleStart {
                origin: super::lifecycle::LifecycleOrigin::Automatic,
                resume_workspace_id: None,
                request_key: None,
                control: Some(control.clone()),
                phase: LifecyclePhase::Executing,
                move_operation_id: Some(historical.operation_id.clone()),
                deferrable_admission: None,
                holds_through_drain: false,
            },
            move |_, _, _, _session_admission| async move {
                entered.notify_one();
                released.notified().await;
                Err(anyhow!("matching Move retry cancelled"))
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    handle_action(
        queued_move_prompt_action("matching active retry", &historical.operation_id),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), manager.requests.recv())
            .await
            .is_err()
    );
    assert!(
        mj_controller::database::startup_prompt_outcomes(
            "session-1",
            &["startup-matching active retry".into()]
        )
        .unwrap()
        .is_empty(),
        "old durable failure overrode the matching current Move owner"
    );
    assert_eq!(session_draft_input(&state), "");
    assert!(control.request_cancel("test cancellation"));
    release.notify_one();
    assert!(RuntimeState::wait_lifecycle_result(watch).await.is_err());
    assert_eq!(session_draft_input(&state), "matching active retry");
    state.cancel_and_join_startup_prompts().await.unwrap();
}

/// Makes [`failing_controller_loader`] fail, as a store that cannot be read
/// after a registration committed would.
#[cfg(unix)]
static FAIL_CONTROLLER_RELOAD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
fn failing_controller_loader() -> Result<Controller> {
    anyhow::ensure!(
        !FAIL_CONTROLLER_RELOAD.load(std::sync::atomic::Ordering::SeqCst),
        "injected controller reload failure"
    );
    Controller::load()
}

/// A failure after the registration committed must not leave the child
/// `Provisioning` with nothing to start it: it becomes `Error`, for an
/// ordinary child and for the oracle, whose one-live-oracle rule would
/// otherwise refuse every later question.
#[cfg(unix)]
#[tokio::test]
async fn a_failure_after_a_sub_agent_registers_moves_it_to_error() {
    const CHILD: &str = "MJ_TEST_SUBAGENT_START_FAILURE";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "a_failure_after_a_sub_agent_registers_moves_it_to_error",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "subagent-start-failure")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let project = tempfile::tempdir().unwrap();
    mj_controller::controller::test_support::test_git(project.path(), &["init", "-b", "main"]);
    Config::update(|config| {
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    project.path().join("codex-home"),
                )
            },
        );
        config.targets.insert(
            "localhost".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        Ok(())
    })
    .unwrap();
    let parent_id = "0123456789abcdef0123456789abcdef";
    let workspace = mj_controller::database::create_workspace("Sub-agent start failure").unwrap();
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.target_template_id = "localhost".into();
    parent.project_directory = Some(project.path().to_path_buf());
    parent.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(parent_id),
    }));
    mj_controller::database::save_session(&parent).unwrap();

    let remote = spawn_remote_session_manager().unwrap();
    let recovery = remote.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(remote.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        failing_controller_loader,
    ));
    let request = |kind, request_key: &str| mj_controller::controller::RegisterSubagentRequest {
        no_github_credentials: false,
        kind,
        reviewer_launch: None,
        parent_session_id: parent_id.into(),
        task_name: "task".into(),
        profile_id: "codex".into(),
        model: Some("model".into()),
        effort: None,
        fast_mode: false,
        working_directory: PathBuf::new(),
        initial_prompt: "Question:\nWhich option?".into(),
        request_key: request_key.into(),
        report_root: None,
    };

    FAIL_CONTROLLER_RELOAD.store(true, std::sync::atomic::Ordering::SeqCst);
    for (kind, request_key) in [
        (mj_core::subagent::SubagentKind::Oracle, "first-question"),
        (mj_core::subagent::SubagentKind::Worker, "first-spawn"),
    ] {
        let error = tokio::time::timeout(
            Duration::from_secs(60),
            state.start_subagent_session(request(kind, request_key)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("injected controller reload failure"),
            "{error:#}"
        );
        let stored = mj_controller::database::load_state().unwrap();
        let child = stored
            .subagents
            .values()
            .find(|relation| relation.kind == kind)
            .unwrap_or_else(|| panic!("{kind:?} registration committed"));
        let session = &stored.sessions[&child.child_session_id];
        assert_eq!(session.state, SessionState::Error, "{kind:?}");
        assert!(
            session
                .last_error
                .as_deref()
                .is_some_and(|cause| cause.contains("injected controller reload failure")),
            "{:?}",
            session.last_error
        );
        // The report directory committed with the registration.
        assert!(
            mj_controller::database::load_subagent_report(&child.child_session_id)
                .unwrap()
                .report_dir
                .is_some()
        );
    }
    FAIL_CONTROLLER_RELOAD.store(false, std::sync::atomic::Ordering::SeqCst);

    // The failed oracle no longer blocks the next question's oracle.
    let mut controller = Controller::load().unwrap();
    controller
        .register_subagent(request(
            mj_core::subagent::SubagentKind::Oracle,
            "second-question",
        ))
        .expect("a fresh oracle may register once the failed one is in Error");
}

// Selection keys join running work; durable caller keys also replay completion.
#[tokio::test]
async fn fresh_lifecycle_retries_replace_completed_results_but_replays_keep_them() {
    let state = test_runtime_state();
    let key = "confirmed-selection".to_owned();
    let first = state
        .admit_prepared_lifecycle(
            "retry-session".into(),
            LifecycleKind::ForceStop,
            super::lifecycle::LifecycleStart::request(key.clone(), None, None),
            None,
            |_, _, _, _session_admission| async { bail!("injected operation failure") },
        )
        .await
        .unwrap();
    let old = first.receipt.clone();
    assert!(
        RuntimeState::wait_lifecycle_result(first.result)
            .await
            .is_err()
    );
    let replay = state
        .start_or_join_lifecycle_with_key(
            old.session_id.clone(),
            LifecycleKind::ForceStop,
            None,
            Some(key.clone()),
            |_, _, _, _session_admission| async { panic!("replay started duplicate work") },
        )
        .await
        .unwrap();
    assert!(RuntimeState::wait_lifecycle_result(replay).await.is_err());
    let release = Arc::new(tokio::sync::Notify::new());
    let next = state
        .admit_prepared_lifecycle(
            old.session_id.clone(),
            LifecycleKind::ForceStop,
            super::lifecycle::LifecycleStart::request(key, None, None),
            None,
            {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    assert_ne!(next.receipt.operation_id, old.operation_id);
    assert!(
        state
            .admit_prepared_lifecycle(
                old.session_id.clone(),
                LifecycleKind::ForceStop,
                super::lifecycle::LifecycleStart {
                    origin: super::lifecycle::LifecycleOrigin::Automatic,
                    ..super::lifecycle::LifecycleStart::request(
                        "different-checkpoint".into(),
                        None,
                        None
                    )
                },
                None,
                |_, _, _, _session_admission| async {
                    panic!("changed selections were admitted concurrently")
                },
            )
            .await
            .is_err()
    );
    assert!(matches!(
        state.wait_session_lifecycle(old).await.unwrap(),
        DaemonReply::LifecycleObservationUnavailable(_)
    ));
    release.notify_one();
    RuntimeState::wait_lifecycle_result(next.result)
        .await
        .unwrap();
}

// Hard-won: #1507: the initiating terminal waited through checkpoint and target retirement.
#[cfg(unix)]
#[tokio::test]
async fn suspend_acceptance_survives_client_disconnect_during_checkpoint() {
    use mj_controller::controller::checkpoint::stand_in::{
        LATCH_RELAY_HOLD_PATH, LATCH_RELAY_SESSION,
    };
    const NAME: &str = "suspend_acceptance_survives_client_disconnect_during_checkpoint";
    if !in_held_close_child(NAME, "begin_checkpoint") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let (controller, channels, _handle) =
        mj_controller::controller::checkpoint::stand_in::close_cut_controller(
            SessionState::Running,
        )
        .await;
    controller.config.save().unwrap();
    let recovery = channels.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(channels.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new_with_controller_loader(
        channels.control.clone(),
        Controller::load().unwrap(),
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        Controller::load,
    ));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn({
        let state = state.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, CancellationToken::new())
                .await
                .unwrap();
        }
    });
    let mut client = DaemonClient::connect(metadata).await.unwrap();
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        client.start_session_lifecycle(SessionLifecycleRequest::Suspend {
            session_id: LATCH_RELAY_SESSION.into(),
            acknowledge_unpublished_work: true,
            ignore_nested_repositories: false,
        }),
    )
    .await
    .expect("acceptance waited for the held checkpoint")
    .unwrap();
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].state,
        SessionState::Closing
    );
    assert!(state.close_is_requested(LATCH_RELAY_SESSION));
    drop(client);
    server.await.unwrap();
    let hold = PathBuf::from(std::env::var_os(LATCH_RELAY_HOLD_PATH).unwrap());
    tokio::time::timeout(Duration::from_secs(30), async {
        while !hold.with_extension("reached").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the daemon did not continue after acceptance");
    let mut completion = Box::pin(state.wait_session_lifecycle(receipt));
    assert!(futures::poll!(&mut completion).is_pending());
    std::fs::write(hold.with_extension("release"), b"continue").unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(60), completion)
            .await
            .unwrap()
            .unwrap(),
        DaemonReply::Done
    ));
    assert!(!state.close_is_requested(LATCH_RELAY_SESSION));
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert!(stored.sessions[LATCH_RELAY_SESSION].checkpoint.is_some());
}

/// Exercise the real authenticated control boundary: these directories exist
/// only in the server fixture, never in client-side interpretation.
// Hard-won: #1514: one archive metadata error discarded readable sizes in the batch.
#[tokio::test]
async fn daemon_api_exchange() {
    const CHILD: &str = "MJ_TEST_DAEMON_API_EXCHANGE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        // Keep probes isolated, but give macOS version detection a stable answer.
        // Apple container discovery is intentionally specific to the host OS.
        let bin = directory.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let probe = bin.join("sw_vers");
            fs::write(&probe, "#!/bin/sh\nprintf '25.0\\n'\n").unwrap();
            fs::set_permissions(probe, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut child = mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "daemon_api_exchange",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "machine-exchange")
        .env("PATH", &bin)
        .env("HOME", directory.path().join("home"))
        .env("USERPROFILE", directory.path().join("home"))
        .isolated_store(directory.path());
        for kind in mj_core::config::HarnessKind::ALL {
            child = child.env(kind.home_env(), directory.path().join("absent-home"));
            if let Some(key) = kind.api_key_variable() {
                child = child.env(key, "");
            }
        }
        child.run();
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let server_root = root.path().canonicalize().unwrap();
    let project = server_root.join("server-only-project");
    fs::create_dir(&project).unwrap();
    let archive = server_root.join("daemon-only.tar");
    fs::write(&archive, b"daemon archive bytes").unwrap();
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
    config.profiles.insert(
        "agent".into(),
        mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            server_root.as_path().join("profile"),
        ),
    );
    config.profiles.get_mut("agent").unwrap().environment = serde_json::from_value(
        serde_json::json!({"DAEMON_TEST_KEY": {"from_secret": "missing-test-key"}}),
    )
    .unwrap();
    let ready_home = server_root.join("ready-profile");
    fs::create_dir(&ready_home).unwrap();
    let mut ready =
        mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Codex, ready_home);
    ready.environment = serde_json::from_value(serde_json::json!({
        "DAEMON_TEST_KEY": {"from_secret": "daemon-only-key"}
    }))
    .unwrap();
    fs::create_dir_all(mj_core::config::config_dir()).unwrap();
    fs::write(
        mj_core::config::config_dir().join("secrets.toml"),
        r#"daemon-only-key = "daemon-secret-value""#,
    )
    .unwrap();
    config.profiles.insert("ready".into(), ready);
    config.save().unwrap();
    let state = test_runtime_state_with(Controller {
        config,
        state: Default::default(),
    });
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let unsupported_machine = mj_core::config::Machine::AwsEc2 {
        aws_profile: None,
        region: "test-region".into(),
        launch_template: "test-template".into(),
        launch_template_version: None,
        ssh_user: "test".into(),
        address_source: Default::default(),
        identity_file: None,
        ssh_args: vec![],
    };
    let client_root = tempfile::tempdir().unwrap();
    let client_config_path = client_root.path().join("config/config.toml");
    fs::create_dir_all(client_config_path.parent().unwrap()).unwrap();
    fs::create_dir(client_root.path().join("data")).unwrap();
    fs::write(&client_config_path, "").unwrap();
    assert!(
        Config::load_from(&client_config_path)
            .unwrap()
            .profiles
            .is_empty()
    );
    let actions = vec![
        DaemonAction::PreviewBuildCache {
            machine: unsupported_machine.clone(),
        },
        DaemonAction::InstallMbx {
            machine: unsupported_machine,
        },
        DaemonAction::CheckpointArchiveSizes {
            paths: [
                ("archived".into(), archive.clone()),
                ("unreadable".into(), archive.join("invalid\0archive")),
                ("missing".into(), server_root.join("missing.tar")),
            ]
            .into(),
        },
        DaemonAction::TestTarget {
            target_id: "missing".into(),
        },
        DaemonAction::ProfileLoginStatuses,
        DaemonAction::CheckAvailability,
        DaemonAction::DiscoverSetup {
            scope: mj_client::discovery::DetectScope::Profiles,
            machines: Vec::new(),
        },
        DaemonAction::DiscoverSetup {
            scope: mj_client::discovery::DetectScope::Runtimes,
            machines: Vec::new(),
        },
        DaemonAction::CheckProjectDirectories {
            paths: vec![project.clone(), server_root.as_path().join("absent")],
        },
        DaemonAction::CompletePath {
            host: mj_core::path_completion::CompletionHost::Local,
            // Typed with this host's separator: on Windows the root is a
            // verbatim `\\?\` path, which reads `/` as part of a name.
            prefix: server_root.join("server-").display().to_string(),
            kind: mj_core::path_completion::CompletionKind::Directories,
        },
        DaemonAction::DiscoverProjects {
            request: mj_core::project_picker::ProjectDiscoveryRequest::Directory {
                path: server_root.as_path().to_string_lossy().into_owned(),
                filter: String::new(),
            },
        },
        DaemonAction::ResolveSetupPath {
            machine: mj_core::config::Machine::Local { build_cache: None },
            source: project.clone(),
        },
        DaemonAction::ValidateMountSource {
            target_id: "missing".into(),
            source: project.clone(),
        },
        DaemonAction::ValidateSessionMounts {
            target_id: "missing".into(),
            mounts: vec![mj_core::targets::AdditionalMount {
                source: project.clone(),
                destination: "/project".into(),
                access: mj_core::targets::MountAccess::Ro,
            }],
        },
        DaemonAction::ResolveContainerPath {
            target_id: "local".into(),
            source: project.clone(),
        },
        DaemonAction::ValidateProjectDirectory {
            target_id: "local".into(),
            directory: project.clone(),
        },
        DaemonAction::PreflightNewSession {
            request: NewSessionPreflightRequest {
                profile_id: "agent".into(),
                workspace_id: String::new(),
                bundle_id: String::new(),
                target_id: "local".into(),
                project_directory: Some(project.clone()),
                additional_mounts: Vec::new(),
                remote_repairs: Vec::new(),
            },
        },
        DaemonAction::ResolveContainerPath {
            target_id: "missing".into(),
            source: project,
        },
        DaemonAction::SaveSpinnerStyle {
            style: mj_core::config::SpinnerStyle::Wave,
        },
    ];
    let actions = actions
        .into_iter()
        .map(|action| (PROTOCOL_VERSION, action))
        .chain([
            (
                PROTOCOL_VERSION,
                DaemonAction::CheckpointSession {
                    session_id: "removed".into(),
                },
            ),
            (
                PROTOCOL_VERSION - 1,
                DaemonAction::CheckpointSession {
                    session_id: "removed".into(),
                },
            ),
        ]);
    let mut exchange = Vec::new();
    for (index, (protocol_version, action)) in actions.enumerate() {
        let server_state = state.clone();
        let server_metadata = metadata.clone();
        let server_listener = &listener;
        let connect = TcpStream::connect(metadata.address);
        let (stream, accepted) = tokio::join!(connect, server_listener.accept());
        let (server_stream, _) = accepted.unwrap();
        let server = mj_core::runtime::spawn(async move {
            serve_client(
                server_stream,
                server_metadata,
                server_state,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        });
        let mut stream = stream.unwrap();
        let rendered_action = serde_json::to_value(&action).unwrap();
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version,
                request_id: index as u64,
                token: metadata.token.clone(),
                action,
            },
        )
        .await
        .unwrap();
        let wire: serde_json::Value = read_frame(&mut stream).await.unwrap();
        let mut response: ResponseEnvelope = mj_core::config::with_secret_resolver(
            mj_core::config::SecretResolver::beside(&client_config_path),
            || serde_json::from_value(wire).unwrap(),
        );
        // Embedded NUL is invalid on every host; only OS diagnostic wording varies.
        if let Ok(DaemonReply::CheckpointArchiveSizes(sizes)) = &mut response.result {
            let error = sizes.get_mut("unreadable").unwrap().as_mut().unwrap_err();
            assert!(error.starts_with("read "), "{error}");
            *error = "<archive metadata error>".into();
        }
        exchange.push(serde_json::json!({"request": rendered_action, "reply": response.result}));
        drop(stream);
        server.await.unwrap();
    }
    // Windows resolves a bare `docker` through its system directories before
    // PATH, so this fixture cannot hide a Docker the host installed. Leave the
    // host's Docker out there; Linux and macOS keep its isolated answer.
    if cfg!(windows) {
        for entry in &mut exchange {
            remove_host_docker(entry);
        }
    }
    // Normalize JSON's escaped Windows separators before replacing roots.
    let rendered = serde_json::to_string_pretty(&exchange)
        .unwrap()
        .replace("\\\\", "/")
        .replace(
            &mj_core::config::config_dir()
                .to_string_lossy()
                .replace('\\', "/"),
            "<daemon-config>",
        )
        .replace(
            &server_root.to_string_lossy().replace('\\', "/"),
            "<server>",
        )
        .replace(
            &server_root
                .parent()
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            "<server-parent>",
        );
    // Windows has its own runtime answers: no local bare target, and Podman
    // only through WSL2.
    if cfg!(windows) {
        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "daemon-api-exchange-windows",
            &rendered,
        );
    } else {
        mj_core::golden::assert_platform_golden(
            env!("CARGO_MANIFEST_DIR"),
            "daemon-api-exchange",
            &rendered,
        );
    }
}

/// Drop Docker discovery results that depend on this test host.
fn remove_host_docker(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::Array(rejected)) = object.get_mut("rejected_runtimes") {
                rejected.retain(|runtime| runtime["kind"] != "docker");
            }
            if let Some(serde_json::Value::Object(targets)) = object.get_mut("targets") {
                targets.retain(|_, target| target["kind"] != "local-docker");
            }
            object.values_mut().for_each(remove_host_docker);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(remove_host_docker),
        _ => {}
    }
}

/// Real OpenSSH is held in a proxy command. Closing a superseded completion
/// must reap that command even after it emits more than a pipe buffer.
#[cfg(unix)]
#[tokio::test]
async fn superseded_machine_completion_closes_connection_and_reaps_ssh() {
    let root = tempfile::tempdir().unwrap();
    let started = root.path().join("started");
    let script = root.path().join("proxy.sh");
    fs::write(
        &script,
        format!(
            "head -c 131072 /dev/zero >&2\nprintf '%s' $$ > {}\nexec sleep 600\n",
            mj_core::targets::posix_quote(&started.to_string_lossy())
        ),
    )
    .unwrap();
    let state = test_runtime_state();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server_metadata = metadata.clone();
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let mut server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(stream, server_metadata, state, server_shutdown)
            .await
            .unwrap();
    });
    let mut stream = TcpStream::connect(metadata.address).await.unwrap();
    let machine = mj_core::config::Machine::Ssh {
        ssh: mj_core::config::SshConnection {
            host: "machine-cancellation.invalid".into(),
            user: None,
            identity_file: None,
            extra_args: vec![
                "-o".into(),
                format!(
                    "ProxyCommand=sh {}",
                    mj_core::targets::posix_quote(&script.to_string_lossy())
                ),
                "-o".into(),
                "ControlMaster=no".into(),
                "-o".into(),
                "ControlPath=none".into(),
            ],
        },
        workspace_prefix: "work".into(),
        build_cache: None,
    };
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: metadata.token,
            action: DaemonAction::CompletePath {
                host: mj_core::path_completion::CompletionHost::Machine(Box::new(machine)),
                prefix: "~/projects".into(),
                kind: mj_core::path_completion::CompletionKind::Directories,
            },
        },
    )
    .await
    .unwrap();
    let started_result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(pid) = fs::read_to_string(&started)
                && let Ok(pid) = pid.parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    // Always cancel/reap before a failed assertion can abandon the fixture.
    drop(stream);
    match tokio::time::timeout(Duration::from_secs(30), &mut server).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            shutdown.cancel();
            server.await.unwrap();
            panic!("connection closure did not cancel completion");
        }
    }
    let pid = started_result.expect("OpenSSH proxy never started");
    assert!(
        !daemon_process_is_alive(pid),
        "superseded completion left its SSH proxy alive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_upload_exchange() {
    const CHILD: &str = "MJ_TEST_UPLOAD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "daemon_upload_exchange",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "upload-exchange")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::start_database_writer().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address: listener.local_addr().unwrap(),
        token: "upload-secret".into(),
        started_at: "now".into(),
        build_version: "test".into(),
        instance_id: "upload-exchange".into(),
        placement: DaemonPlacement::Host,
    };
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        for (index, authenticated) in [true, false, true, false, true].into_iter().enumerate() {
            if index == 4 {
                assert!(mj_controller::upgrade::gate().try_close());
            }
            let (stream, _) = listener.accept().await.unwrap();
            let result = serve_client(
                stream,
                server_metadata.clone(),
                test_runtime_state_loading_the_store(),
                CancellationToken::new(),
            )
            .await;
            assert_eq!(result.is_ok(), authenticated);
            assert_eq!(
                std::fs::read_dir(mj_core::config::data_dir().join("uploads"))
                    .unwrap()
                    .count(),
                0
            );
        }
    });
    let mut partial = TcpStream::connect(metadata.address).await.unwrap();
    write_frame(
        &mut partial,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: metadata.token.clone(),
            action: DaemonAction::UploadChunk {
                bytes: vec![0; 96 * 1024],
                sha256: None,
            },
        },
    )
    .await
    .unwrap();
    drop(partial);
    let mut unauthorized = TcpStream::connect(metadata.address).await.unwrap();
    write_frame(
        &mut unauthorized,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: "wrong-token".into(),
            action: DaemonAction::UploadChunk {
                bytes: vec![0; 96 * 1024],
                sha256: None,
            },
        },
    )
    .await
    .unwrap();
    assert!(
        read_frame::<ResponseEnvelope>(&mut unauthorized)
            .await
            .is_err()
    );
    assert!(
        !mj_core::attachment::AttachmentStore::controller("upload-session")
            .unwrap()
            .root()
            .exists()
    );
    // Exercise several transport fragments and the pipe-buffer boundary.
    let mut bytes = vec![37; 192 * 1024];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let reference =
        mj_core::attachment::AttachmentRef::new(&bytes, "image/png".into(), 256, 256).unwrap();
    let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
    let mut output = String::from("interrupted upload: no blob remains\n");
    for label in ["install 196608 bytes", "repeat content-addressed install"] {
        let blob = client.upload(&mut bytes.as_slice()).await.unwrap();
        let reply = client
            .request(DaemonAction::InstallAttachment {
                session_id: "upload-session".into(),
                reference: reference.clone(),
                blob,
            })
            .await
            .unwrap();
        output.push_str(&format!(
            "{label}: {}\n",
            serde_json::to_string(&reply).unwrap()
        ));
    }
    assert_eq!(
        mj_core::attachment::AttachmentStore::controller("upload-session")
            .unwrap()
            .read(&reference)
            .unwrap(),
        bytes
    );
    let mut corrupt = bytes;
    corrupt[100] ^= 1;
    let blob = client.upload(&mut corrupt.as_slice()).await.unwrap();
    let error = client
        .request(DaemonAction::InstallAttachment {
            session_id: "upload-session".into(),
            reference,
            blob,
        })
        .await
        .unwrap_err();
    output.push_str(&format!("corrupt payload: {error}\n"));
    let error = client
        .request(DaemonAction::ImportSession {
            blob: "another-connections-blob".into(),
            session: Box::new(runtime_test_session(
                "import-session",
                "default",
                SessionState::Stopped,
            )),
            bundle: None,
        })
        .await
        .unwrap_err();
    output.push_str(&format!("import unknown blob: {error}\n"));
    let staging = tempfile::tempdir().unwrap();
    let archive_path = staging.path().join("import.zip");
    let mut session = runtime_test_session("import-session", "default", SessionState::Stopped);
    let verified = mj_checkpoint::archive::write_archive_atomic(
        &archive_path,
        &mj_checkpoint::archive::ArchiveInput {
            session: mj_core::archive::SessionManifest {
                id: session.id.clone(),
                title: session.title.clone(),
                harness_kind: session.harness_kind,
                profile_id: session.last_profile.clone(),
                native_session_id: "native-import".into(),
                created_at: session.created_at.clone(),
                checkpointed_at: session.created_at.clone(),
                hel_version: "test".into(),
                relay_version: "test".into(),
                adapter_version: "test".into(),
            },
            target: mj_core::archive::TargetManifest {
                template_id: "local".into(),
                target_kind: "import".into(),
                details: Default::default(),
            },
            bundle: mj_core::archive::BundleManifest {
                id: session.bundle_id.clone(),
                primary_repository: "repo".into(),
            },
            canonical_session: empty_archive_snapshot(),
            native_artifacts: Vec::new(),
            repositories: Vec::new(),
        },
    )
    .unwrap();
    session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path: archive_path.clone(),
        sha256: verified.archive_sha256,
        created_at: session.created_at.clone(),
        event_frontier: 0,
    }));
    let blob = client
        .upload(&mut tokio::fs::File::open(&archive_path).await.unwrap())
        .await
        .unwrap();
    let reply = client
        .request(DaemonAction::ImportSession {
            session: Box::new(session),
            bundle: None,
            blob,
        })
        .await
        .unwrap();
    output.push_str(&format!(
        "import verified archive: {}\n",
        serde_json::to_string(&reply).unwrap()
    ));
    assert!(
        mj_core::config::sessions_dir()
            .join("import-session.hel.zip")
            .is_file()
    );
    let registered = client
        .session_record("import-session".into())
        .await
        .unwrap()
        .unwrap();
    let reply = client
        .request(DaemonAction::TranscriptionAvailable {
            preferred_profile: "missing".into(),
        })
        .await
        .unwrap();
    output.push_str(&format!(
        "transcription availability: {}\n",
        serde_json::to_string(&reply).unwrap()
    ));
    let audio_blob = client
        .upload(&mut vec![0; 196608].as_slice())
        .await
        .unwrap();
    let error = client
        .request(DaemonAction::TranscribeAudio {
            preferred_profile: "missing".into(),
            blob: audio_blob.clone(),
        })
        .await
        .unwrap_err();
    output.push_str(&format!("transcription without credential: {error}\n"));
    let error = client
        .request(DaemonAction::TranscribeAudio {
            preferred_profile: "missing".into(),
            blob: audio_blob,
        })
        .await
        .unwrap_err();
    output.push_str(&format!("transcription blob after refusal: {error}\n"));
    let reply = client.request(DaemonAction::Ping).await.unwrap();
    output.push_str(&format!(
        "connection after refusal: {}\n",
        serde_json::to_string(&reply).unwrap()
    ));
    client
        .upload(&mut b"unconsumed blob".as_slice())
        .await
        .unwrap();
    drop(client);
    let mut corrupt_digest = TcpStream::connect(metadata.address).await.unwrap();
    write_frame(
        &mut corrupt_digest,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: metadata.token.clone(),
            action: DaemonAction::UploadChunk {
                bytes: vec![1; 96 * 1024],
                sha256: Some("wrong".into()),
            },
        },
    )
    .await
    .unwrap();
    assert!(
        read_frame::<ResponseEnvelope>(&mut corrupt_digest)
            .await
            .is_err()
    );
    // Inspect raw replies: DaemonClient intentionally reconnects after UpgradePending.
    async fn exchange(
        stream: &mut TcpStream,
        metadata: &DaemonMetadata,
        action: DaemonAction,
    ) -> DaemonReply {
        write_frame(
            stream,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: 1,
                token: metadata.token.clone(),
                action,
            },
        )
        .await
        .unwrap();
        read_frame::<ResponseEnvelope>(stream)
            .await
            .unwrap()
            .result
            .unwrap()
    }
    // Hard-won: blob storage admission prevented management connections during handoff.
    let mut management = TcpStream::connect(metadata.address).await.unwrap();
    assert!(matches!(
        exchange(&mut management, &metadata, DaemonAction::Ping).await,
        DaemonReply::Pong
    ));
    use sha2::Digest as _;
    let bytes = std::fs::read(&archive_path).unwrap();
    let digest = mj_core::hex::lower_hex(sha2::Sha256::digest(&bytes));
    let DaemonReply::BlobUploaded(blob) = exchange(
        &mut management,
        &metadata,
        DaemonAction::UploadChunk {
            bytes,
            sha256: Some(digest),
        },
    )
    .await
    else {
        panic!("expected blob");
    };
    // Hard-won: #1514: archive preparation held daemon handoff admission.
    let reply = exchange(
        &mut management,
        &metadata,
        DaemonAction::ImportSession {
            session: Box::new(registered),
            bundle: None,
            blob,
        },
    )
    .await;
    output.push_str(&format!(
        "import during handoff: {}\n",
        serde_json::to_string(&reply).unwrap()
    ));
    let remaining = std::fs::read_dir(mj_core::config::sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        remaining,
        vec![std::ffi::OsString::from("import-session.hel.zip")]
    );
    assert_eq!(
        output,
        include_str!("../../tests/golden/daemon-upload-exchange.txt")
    );
    drop(management);
    server.await.unwrap();
}

// Hard-won: #1514: admitted mbx downloads kept daemon handoff waiting for minutes.
// The pinned mbx executable installs only on Linux container hosts.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn mbx_preparation_cancels_on_handoff_and_disconnect_before_removing_staging() {
    const CHILD: &str = "MJ_MBX_CANCELLATION_ROOT";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin = root.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/uname", bin.join("uname")).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "mbx_preparation_cancels_on_handoff_and_disconnect_before_removing_staging",
            ),
        )
        .env(CHILD, root.path())
        .env("MJ_INSTANCE", "mbx-cancellation")
        .env("HOME", &home)
        .env("PATH", &bin)
        .isolated_store(root.path())
        .run();
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let download_server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    mj_controller::controller::MBX_TEST_RELEASE_URL
        .set(format!(
            "http://{}/archive",
            download_server.local_addr().unwrap()
        ))
        .unwrap();
    for handoff in [true, false] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let metadata = test_metadata(listener.local_addr().unwrap());
        let server_metadata = metadata.clone();
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = mj_core::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(
                stream,
                server_metadata,
                test_runtime_state(),
                server_shutdown,
            )
            .await
        });
        let mut client = TcpStream::connect(metadata.address).await.unwrap();
        write_frame(
            &mut client,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: 1,
                token: metadata.token,
                action: DaemonAction::InstallMbx {
                    machine: mj_core::config::Machine::Local { build_cache: None },
                },
            },
        )
        .await
        .unwrap();
        let download =
            tokio::time::timeout(Duration::from_secs(30), download_server.accept()).await;
        if download.is_err() {
            shutdown.cancel();
            server.await.unwrap().unwrap();
            panic!("mbx download never reached HTTP fixture");
        }
        let (mut download, _) = download.unwrap().unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(download.read_u8().await.unwrap());
        }
        // The body remains incomplete. The real reqwest future must be dropped on cancellation.
        download
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await
            .unwrap();
        download.write_all(&vec![b'x'; 131072]).await.unwrap();
        assert!(
            !mj_controller::upgrade::active_labels()
                .iter()
                .any(|label| label.contains("mbx installation"))
        );
        if handoff {
            shutdown.cancel();
            let response: ResponseEnvelope = read_frame(&mut client).await.unwrap();
            assert!(response.result.unwrap_err().contains("cancel"));
        } else {
            drop(client);
        }
        server.await.unwrap().unwrap();
        let mut remaining = Vec::new();
        // A dropped response closes the incomplete HTTP body, instead of waiting for its deadline.
        tokio::time::timeout(
            Duration::from_secs(30),
            download.read_to_end(&mut remaining),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            fs::read_dir(mj_core::config::data_dir().join("mbx-download"))
                .unwrap()
                .count(),
            0
        );
        assert!(!root.join("home/.local/bin/mbx").exists());
    }
}

#[tokio::test]
async fn dismiss_notification_refuses_a_stale_target_before_dispatch() {
    let state = test_runtime_state();
    let error = state
        .dismiss_notification(
            "session-no-longer-current".into(),
            mj_core::attention::NotificationDismissal::Question(
                mj_core::assessment::InferredQuestion {
                    turn_id: "stale".into(),
                    completed_ordinal: 10,
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("notification is no longer current")
    );
}

/// The whole dismiss seam: a served question target, the daemon operation the
/// web and TUI menus call, and the next view the worker publishes once the
/// command lands. The attention must drop so the card glyph clears.
#[tokio::test]
async fn dismiss_notification_clears_the_served_question_attention() {
    use mj_core::attention::{
        EntryMessage, NotificationDismissal, NotificationKind, NotificationStack,
        NotificationStackEntry,
    };
    const CHILD: &str = "MJ_TEST_DISMISS_CLEARS_ATTENTION";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "dismiss_notification_clears_the_served_question_attention",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "dismiss-clears-attention")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let question = mj_core::assessment::InferredQuestion {
        turn_id: "turn-1".into(),
        completed_ordinal: 9,
    };
    let record = runtime_test_session(
        "session-1",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&record).unwrap();
    let mut applied = mj_core::state::MaterializedSession::empty("session-1");
    applied.applied_event_ordinal = question.completed_ordinal;
    applied.applied_event_digest = "11".repeat(32);
    mj_controller::database::save_materialized_session(&applied).unwrap();

    let mut manager = TestRemoteManager::new().await;
    let recovery = manager.control.session_gate.clone();
    let upgrades =
        crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(manager.control.clone(), &recovery);
    let state = Arc::new(RuntimeState::new(
        manager.control.clone(),
        Controller {
            config: runtime_test_config(),
            state: Default::default(),
        },
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
    ));
    state.owner().edit_sessions(|sessions| {
        sessions.insert(record.id.clone(), record);
    });
    state
        .owner()
        .install_relay_sessions(["session-1".to_owned()].into());

    let question_view = |dismissed: bool| {
        let mut view = ready_startup_view();
        let snapshot = view.snapshot.as_mut().unwrap();
        snapshot.materialized.applied_event_ordinal = question.completed_ordinal;
        snapshot.materialized.applied_event_digest = "11".repeat(32);
        snapshot.materialized.execution = mj_core::state::MaterializedExecutionState::Idle;
        snapshot.window = mj_core::state::ProjectionWindow::of(&snapshot.materialized);
        snapshot.operational.person_input_through = if dismissed { 10 } else { 0 };
        snapshot.operational.open_request =
            (!dismissed).then(|| mj_core::assessment::OpenRequest {
                question: question.clone(),
                question_message: None,
            });
        snapshot.operational.notification_stack = NotificationStack {
            revision: if dismissed { 10 } else { 9 },
            entries: if dismissed {
                vec![]
            } else {
                vec![
                    NotificationStackEntry {
                        completed_at_ms: 0,
                        completed_ordinal: 7,
                        kind: NotificationKind::Important,
                        message: EntryMessage::None,
                    },
                    NotificationStackEntry {
                        completed_at_ms: 0,
                        completed_ordinal: 9,
                        kind: NotificationKind::NeedsInput,
                        message: EntryMessage::None,
                    },
                ]
            },
        }
        .into();
        view
    };
    state
        .publish_session("session-1".into(), question_view(false))
        .await
        .unwrap();
    let served = state.runtime_publication().unwrap();
    assert_eq!(
        served.metadata.session_attention["session-1"],
        mj_core::attention::SessionAttention::NeedsInput
    );
    let target = served
        .metadata
        .notification_dismissals
        .get("session-1")
        .cloned()
        .expect("a needs-input session serves its dismissal target");
    assert_eq!(target, NotificationDismissal::Stack { revision: 9 });

    let answerer = mj_core::runtime::spawn(async move {
        let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
            .await
            .expect("the dismissal submit reached the worker bridge")
            .expect("request bridge open");
        let mj_controller::session_manager::RemoteSessionRequest::Submit {
            command_id,
            command,
            reply,
            ..
        } = request
        else {
            panic!("the dismissal is a submit");
        };
        assert_eq!(command_id, "clear-notifications-9");
        assert!(
            matches!(command, RelayCommand::ClearNotifications { revision: 9 }),
            "{command:?}"
        );
        let _ = reply.send(Ok(10));
    });
    let ordinal = state
        .dismiss_notification("session-1".into(), target)
        .await
        .unwrap();
    assert_eq!(ordinal, 10);
    answerer.await.unwrap();

    // The worker's next view closes the request; the publication must lower
    // the attention it serves, which is what clears the card glyph.
    state
        .publish_session("session-1".into(), question_view(true))
        .await
        .unwrap();
    let cleared = state.runtime_publication().unwrap();
    assert_eq!(cleared.records["session-1"].viewed_through_event_ordinal, 7);
    assert!(
        !cleared
            .metadata
            .notification_cards
            .as_ref()
            .unwrap()
            .contains_key("session-1")
    );
    assert_eq!(
        cleared.metadata.session_attention["session-1"],
        mj_core::attention::SessionAttention::Idle
    );
    assert!(
        !cleared
            .metadata
            .notification_dismissals
            .contains_key("session-1")
    );
    // Recorded failures use the same action without a worker: the database
    // writer publishes committed records before replying, and owner() observes
    // that publication before attention is projected.
    let mut failed = runtime_test_session(
        "resume-failure",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Stopped,
    );
    failed.last_error = Some("resume failed: operation cancelled".into());
    mj_controller::database::save_session(&failed).unwrap();
    let served = state.runtime_publication().unwrap();
    assert_eq!(
        served.metadata.session_attention[&failed.id],
        mj_core::attention::SessionAttention::Failed
    );
    let target = served.metadata.notification_dismissals[&failed.id].clone();
    // Hard-won: 0db309adc: a delayed dismissal accepted a repeated identical failure.
    failed.state = SessionState::Provisioning;
    mj_controller::database::save_session(&failed).unwrap();
    failed.state = SessionState::Stopped;
    mj_controller::database::save_session(&failed).unwrap();
    let error = state
        .dismiss_notification(failed.id.clone(), target)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("notification is no longer current")
    );
    let served = state.runtime_publication().unwrap();
    assert_eq!(
        served.metadata.session_attention[&failed.id],
        mj_core::attention::SessionAttention::Failed
    );
    let target = served.metadata.notification_dismissals[&failed.id].clone();

    assert!(matches!(
        target,
        mj_core::attention::NotificationDismissal::Failure(_)
    ));
    state
        .dismiss_notification(failed.id.clone(), target)
        .await
        .unwrap();
    let dismissed = state.runtime_publication().unwrap();
    assert_eq!(
        dismissed.metadata.session_attention[&failed.id],
        mj_core::attention::SessionAttention::Inactive
    );
    assert!(
        !dismissed
            .metadata
            .notification_dismissals
            .contains_key(&failed.id)
    );
    failed.title = "renamed failure".into();
    mj_controller::database::save_session(&failed).unwrap();
    assert_eq!(
        state
            .runtime_publication()
            .unwrap()
            .metadata
            .session_attention[&failed.id],
        mj_core::attention::SessionAttention::Inactive
    );
    failed.last_error = Some("resume failed: different failure".into());
    mj_controller::database::save_session(&failed).unwrap();
    let renewed = state.runtime_publication().unwrap();
    assert_eq!(
        renewed.metadata.session_attention[&failed.id],
        mj_core::attention::SessionAttention::Failed
    );
    assert!(
        renewed
            .metadata
            .notification_dismissals
            .contains_key(&failed.id)
    );
}

// Hard-won: 352a87453: queued Resume reused the Running state from before Suspend.
// Hard-won: f9e68d823: queued Resume skipped the deferred Podman cleanup produced by Suspend.
#[cfg(unix)]
#[tokio::test]
async fn queued_resume_rechecks_state_after_suspend_before_reserving_capacity() {
    const NAME: &str = "queued_resume_rechecks_state_after_suspend_before_reserving_capacity";
    const CHILD: &str = "MJ_TEST_QUEUED_RESUME_CLEANUP";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        mj_core::test_hooks::install_fake_command(
            &bin,
            "podman",
            r#"
printf '%s\n' "$*" >> "$MJ_TEST_QUEUED_RESUME_PODMAN_LOG"
case "$*" in
    *".State.Running"*) printf 'false\n' ;;
    "container inspect --format "*|"volume inspect --format "*) printf 'true|queued-resume\n' ;;
    "container inspect "*|"container exists "*) [ ! -f "$MJ_TEST_QUEUED_RESUME_REMOVED" ] ;;
    "rm "*) : > "$MJ_TEST_QUEUED_RESUME_REMOVED" ;;
    "stop "*|"volume rm "*|"info"|"exec "*) exit 0 ;;
    *) printf 'unexpected podman arguments: %s\n' "$*" >&2; exit 92 ;;
esac
"#,
        );
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .isolated_store(directory.path())
        .env(CHILD, "1")
        .env("PATH", path)
        .env(
            "MJ_TEST_QUEUED_RESUME_PODMAN_LOG",
            directory.path().join("podman-calls"),
        )
        .env(
            "MJ_TEST_QUEUED_RESUME_REMOVED",
            directory.path().join("removed"),
        )
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: serde_json::from_value(serde_json::json!({"image": "unused-test-image"}))
                .unwrap(),
        },
    );
    config.save().unwrap();
    let mut session = runtime_test_session(
        "queued-resume",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    session.target_template_id = "podman".into();
    let container = mj_core::targets::resource_name(&session.id).unwrap();
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: container.clone(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::ContainerLayer,
    }));
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state_loading_the_store();
    let release = Arc::new(tokio::sync::Notify::new());
    let close = state
        .start_or_join_lifecycle(session.id.clone(), LifecycleKind::Suspend, {
            let release = release.clone();
            move |_, id, _, _session_admission| async move {
                release.notified().await;
                blocking(move || {
                    let mut session =
                        mj_controller::database::read_durable_session_record(&id)?.unwrap();
                    session.state = SessionState::Stopped;
                    mj_controller::database::save_lifecycle_session(&session)
                })
                .await?;
                Ok(DaemonLifecycleResult::DeferredCleanup)
            }
        })
        .await
        .unwrap();
    let mut revisions = state.revisions.subscribe();
    let pending = mj_core::runtime::spawn({
        let state = state.clone();
        let id = session.id.clone();
        async move {
            state
                .start_resume_session(
                    ResumeSessionRequest {
                        session_id: id,
                        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                        profile_id: "unconfigured-profile".into(),
                        target_template_id: "unconfigured-target".into(),
                        additional_mounts: None,
                        resource_allocation: None,
                        discard_queue: false,
                        repository_preflight: None,
                    },
                    LifecycleControl::default(),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.owner().lifecycle_intents.contains_key(&session.id) {
                break;
            }
            revisions.changed().await.unwrap();
        }
    })
    .await
    .expect("Resume queues before Suspend changes the running record");
    assert_eq!(
        mj_controller::database::read_durable_session_record(&session.id)
            .unwrap()
            .unwrap()
            .state,
        SessionState::Running
    );
    release.notify_one();
    RuntimeState::wait_lifecycle_result(close).await.unwrap();
    let started = pending.await.unwrap().unwrap();
    let error = RuntimeState::wait_lifecycle_result(started.result)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("unknown target"),
        "queued Resume must attempt fresh capacity admission rather than report an already-running success: {error:#}"
    );
    let stored = mj_controller::database::read_durable_session_record(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, SessionState::Stopped);
    assert!(
        stored.target.is_none(),
        "Resume must finish Suspend's deferred cleanup before its capacity/provisioning decision"
    );
    let calls =
        std::fs::read_to_string(std::env::var_os("MJ_TEST_QUEUED_RESUME_PODMAN_LOG").unwrap())
            .unwrap();
    assert!(
        calls
            .lines()
            .any(|call| call == format!("rm --force --ignore {container}")),
        "{calls}"
    );
}

// Hard-won: 8481d8a51: an identical join could lose the owner lock to a queued differing request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_lifecycle_requests_do_not_conflict_during_fifo_turnover() {
    let state = test_runtime_state();
    for ordinal in 0..32 {
        let session = format!("fifo-turnover-{ordinal}");
        let release = Arc::new(tokio::sync::Notify::new());
        let first = state
            .start_or_join_lifecycle(session.clone(), LifecycleKind::Suspend, {
                let release = release.clone();
                move |_, _, _, _session_admission| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        let queued = mj_core::runtime::spawn({
            let state = state.clone();
            let session = session.clone();
            async move {
                state
                    .start_or_join_lifecycle(
                        session,
                        LifecycleKind::Resume,
                        |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.owner().lifecycle_intents.contains_key(&session) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("differing request queues behind the first operation");
        let identical = mj_core::runtime::spawn({
            let state = state.clone();
            let session = session.clone();
            async move {
                state
                    .start_or_join_lifecycle(
                        session,
                        LifecycleKind::Suspend,
                        |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
                    )
                    .await
            }
        });
        release.notify_one();
        RuntimeState::wait_lifecycle_result(first).await.unwrap();
        RuntimeState::wait_lifecycle_result(queued.await.unwrap().unwrap())
            .await
            .unwrap();
        RuntimeState::wait_lifecycle_result(identical.await.unwrap().unwrap())
            .await
            .unwrap();
    }
}

// Hard-won: 5b46f3845: queued client work held daemon handoff through a resumable image pull.
// Hard-won: 0e8fdc505: bounded destroy preemption bypassed daemon draining.
#[tokio::test]
async fn queued_lifecycle_requests_release_client_admission_for_handoff_and_stop() {
    const NAME: &str = "queued_lifecycle_requests_release_client_admission_for_handoff_and_stop";
    const PHASE: &str = "MJ_TEST_QUEUED_LIFECYCLE_HANDOFF_PHASE";
    let Ok(phase) = std::env::var(PHASE) else {
        for phase in ["handoff", "stop", "destroy-handoff", "destroy-stop"] {
            let directory = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), NAME),
            )
            .env(PHASE, phase)
            .env("MJ_INSTANCE", "queued-lifecycle-handoff")
            .isolated_store(directory.path())
            .run();
        }
        return;
    };
    let destroy = phase.starts_with("destroy-");
    let handoff = phase.ends_with("handoff");
    let _writer = mj_controller::database::install_isolated_test_writer();
    let state = test_runtime_state();
    let control = LifecycleControl::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let (started, ready) = tokio::sync::oneshot::channel();
    let upgrade = state
        .start_or_join_lifecycle_controlled(
            "queued-upgrade".into(),
            LifecycleKind::UpgradeContainer,
            None,
            None,
            Some(control.clone()),
            {
                let release = release.clone();
                move |state, id, cancelled, _session_admission| async move {
                    let executor = DaemonStageReportingExecutor::new(ProcessExecutor, state, id);
                    executor.begin_resumable_work()?;
                    executor.stage_started(ProvisionStage::Cloning);
                    started.send(()).unwrap();
                    tokio::select! {
                        () = release.notified() => {}
                        () = mj_controller::recovery_gate::wait_until_cancelled(&cancelled), if !destroy => {}
                    }
                    executor.stage_finished(ProvisionStage::Cloning);
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .await
        .unwrap();
    ready.await.unwrap();
    assert!(
        mj_controller::upgrade::active_labels().is_empty(),
        "resumable preparation holds no daemon admission"
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server = mj_core::runtime::spawn({
        let state = state.clone();
        let metadata = metadata.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, metadata, state, CancellationToken::new())
                .await
                .unwrap();
        }
    });
    let mut stream = TcpStream::connect(metadata.address).await.unwrap();
    let request = if destroy {
        SessionLifecycleRequest::ForceDestroy {
            session_id: "queued-upgrade".into(),
            delete_branch: false,
        }
    } else {
        SessionLifecycleRequest::Resume(Box::new(ResumeSessionRequest {
            session_id: "queued-upgrade".into(),
            workspace_id: "workspace".into(),
            profile_id: "unconfigured".into(),
            target_template_id: "unconfigured".into(),
            additional_mounts: None,
            resource_allocation: None,
            discard_queue: false,
            repository_preflight: None,
        }))
    };
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 77,
            token: metadata.token.clone(),
            action: DaemonAction::StartSessionLifecycle(request),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state
            .owner()
            .lifecycle_intents
            .contains_key("queued-upgrade")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the authenticated request queues behind resumable preparation");
    assert!(
        mj_controller::upgrade::active_labels()
            .iter()
            .any(|label| label.starts_with("client request"))
    );
    let shutdown = CancellationToken::new();
    let action = if handoff {
        DaemonAction::PrepareUpgrade
    } else {
        DaemonAction::Stop
    };
    let reply = handle_action(action, &metadata, &state, &shutdown)
        .await
        .unwrap();
    assert!(matches!(
        (handoff, reply),
        (true, DaemonReply::UpgradePending) | (false, DaemonReply::Done)
    ));
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame::<ResponseEnvelope>(&mut stream),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.request_id, 77);
    assert!(
        matches!(response.result.unwrap(), DaemonReply::UpgradePending),
        "the original request is explicitly unaccepted and safe to retry"
    );
    assert!(state.owner().lifecycle_intents.is_empty());
    assert!(
        control.cancelled.load(Ordering::Acquire) == destroy,
        "only destroy requests cancellation before waiting; draining does not cancel the upgrade"
    );
    drop(stream);
    server.await.unwrap();
    assert!(
        mj_controller::upgrade::active_labels().is_empty(),
        "client request admission is released after the refusal acknowledgement"
    );
    if handoff {
        assert!(matches!(
            handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
                .await
                .unwrap(),
            DaemonReply::Done
        ));
    }
    assert!(shutdown.is_cancelled());
    assert!(
        state.owner().lifecycle["queued-upgrade"].is_running(),
        "handoff never waited for the resumable preparation"
    );
    if destroy {
        release.notify_one();
    }
    state.cancel_and_wait_lifecycles().await.unwrap();
    release.notify_one();
    let outcome = RuntimeState::wait_lifecycle_result(upgrade).await;
    if let Err(error) = outcome {
        assert!(
            control.stopped_for_shutdown(),
            "unexpected lifecycle failure: {error:#}"
        );
    }
}

#[tokio::test]
async fn notification_item_read_crosses_the_daemon_client_boundary() {
    const NAME: &str = "notification_item_read_crosses_the_daemon_client_boundary";
    const CHILD: &str = "MJ_TEST_NOTIFICATION_ITEM_READ";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    mj_controller::database::save_session(&runtime_test_session(
        "notification-history",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    ))
    .unwrap();
    let mut history = mj_core::state::MaterializedSession::empty("notification-history");
    history.applied_event_ordinal = 600;
    history.applied_event_digest = format!("{:064x}", 600);
    for position in 1..=600 {
        history.transcript.push(Arc::new(mj_core::state::TranscriptItem {
            stable_id: format!("reply-{position}"), position,
            latest_content_event_ordinal: Some(position), created_at_ms: position as i64, last_changed_at_ms: position as i64,
            body: mj_core::state::TranscriptBody::Agent { chunks: vec![serde_json::json!({"content":{"type":"text","text":format!("Question {position}")}})], streaming: false },
        }));
    }
    history.last_turn_outcome = Some(mj_core::state::MaterializedTurnOutcome {
        command_id: "first-reply".into(),
        accepted_ordinal: None,
        turn_start_position: Some(0),
        completed_ordinal: 1,
        completed_at_ms: 1,
        outcome: mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: "EndTurn".into(),
        },
        answered_through: None,
        diagnostic: None,
        usage: None,
        work_facts: None,
    });
    mj_controller::database::save_materialized_session(&history).unwrap();
    let (tail, _) =
        mj_controller::database::load_materialized_projection_tail("notification-history", 512)
            .unwrap()
            .unwrap();
    assert!(
        !tail
            .transcript
            .iter()
            .any(|item| item.stable_id == "reply-1")
    );
    let state = test_runtime_state();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(
                stream,
                server_metadata.clone(),
                Arc::clone(&state),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        }
    });
    // Send the legacy action shape without new optional fields, in the
    // daemon's advertised dialect (as DaemonClient::request does). Reading a
    // single raw frame proves the reply never entered chunk transport.
    let mut legacy = TcpStream::connect(metadata.address).await.unwrap();
    write_frame(
        &mut legacy,
        &serde_json::json!({
            "protocol_version": metadata.protocol_version,
            "request_id": 41,
            "token": metadata.token.clone(),
            "action": {"action": "stored_projection_tail", "arguments": {
                "session_id": "notification-history", "items": 512
            }}
        }),
    )
    .await
    .unwrap();
    let response: ResponseEnvelope = read_frame(&mut legacy).await.unwrap();
    assert_eq!(response.request_id, 41);
    let Ok(DaemonReply::StoredProjectionTail(Some(legacy_tail))) = response.result else {
        panic!("legacy count requests must return an unchunked StoredProjectionTail frame");
    };
    assert_eq!(legacy_tail.transcript.first().unwrap().position, 89);
    assert_eq!(legacy_tail.transcript.last().unwrap().position, 600);
    drop(legacy);
    let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
    let StoredTranscriptItem::Found(item) = client
        .stored_transcript_item("notification-history".into(), "reply-1".into())
        .await
        .unwrap()
    else {
        panic!("old question must be found outside the tail");
    };
    assert_eq!(*item, *history.transcript[0]);
    let StoredTranscriptItem::Found(bounded) = client
        .stored_agent_message_through("notification-history".into(), 1)
        .await
        .unwrap()
    else {
        panic!("completion-bound read must find the original reply outside the tail");
    };
    assert_eq!(bounded, item);

    assert!(matches!(
        client
            .stored_transcript_item("notification-history".into(), "missing".into())
            .await
            .unwrap(),
        StoredTranscriptItem::Missing
    ));
    let count_tail = client
        .stored_projection_tail("notification-history".into(), 512)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count_tail.transcript.first().unwrap().position, 89);
    assert_eq!(count_tail.transcript.last().unwrap().position, 600);
    // A single stored item can exceed the transport frame boundary.
    let large_text = "x".repeat(MAX_FRAME_BYTES + 128 * 1024);
    for position in 601..=610 {
        history.transcript.push(Arc::new(mj_core::state::TranscriptItem {
            stable_id: format!("reply-{position}"), position,
            latest_content_event_ordinal: Some(position), created_at_ms: position as i64, last_changed_at_ms: position as i64,
            body: mj_core::state::TranscriptBody::Agent { chunks: vec![serde_json::json!({"content":{"type":"text", "text": if position == 610 { large_text.clone() } else { format!("Question {position}") }}})], streaming: false },
        }));
    }
    history.applied_event_ordinal = 610;
    history.applied_event_digest = format!("{:064x}", 610);
    mj_controller::database::save_materialized_session(&history).unwrap();
    assert!(
        serde_json::to_vec(&DaemonReply::StoredTranscriptItem(Some(Box::new(
            history.transcript.last().unwrap().as_ref().clone()
        ))))
        .unwrap()
        .len()
            > MAX_FRAME_BYTES
    );
    let StoredTranscriptItem::Found(large) = client
        .stored_transcript_item("notification-history".into(), "reply-610".into())
        .await
        .unwrap()
    else {
        panic!("large item must be returned");
    };
    assert!(large.body == history.transcript.last().unwrap().body);
    drop(client);
    let mut old_metadata = metadata;
    old_metadata.protocol_version = mj_client::daemon::STORED_TRANSCRIPT_READ_PROTOCOL - 1;
    let mut older = DaemonClient::connect(old_metadata).await.unwrap();
    assert!(matches!(
        older
            .stored_transcript_item("notification-history".into(), "reply-1".into())
            .await
            .unwrap(),
        StoredTranscriptItem::Unsupported
    ));
    assert!(matches!(
        older
            .stored_agent_message_through("notification-history".into(), 1)
            .await
            .unwrap(),
        StoredTranscriptItem::Unsupported
    ));
    // No unknown action was sent and no error/reconnect loop was entered.
    // The frozen management operation still works on the same connection.
    older.status().await.unwrap();
    drop(older);
    server.await.unwrap();
}

// Hard-won: 41e5b2ddd: a phone Resume could outlive cancellation while queued for workspace admission.
#[tokio::test]
async fn cancelling_resume_retires_its_workspace_wait_without_admission() {
    let state = test_runtime_state();
    let workspace_id = "resume-cancellation";
    let _closing = state
        .workspace_resume_gate(workspace_id)
        .write_owned()
        .await;
    let control = LifecycleControl::default();
    let resume = state.start_resume_session(
        ResumeSessionRequest {
            session_id: "waiting-resume".into(),
            workspace_id: workspace_id.into(),
            profile_id: "profile".into(),
            target_template_id: "target".into(),
            additional_mounts: None,
            resource_allocation: None,
            discard_queue: false,
            repository_preflight: None,
        },
        control.clone(),
    );
    tokio::pin!(resume);
    assert!(futures::poll!(&mut resume).is_pending());
    assert!(control.request_cancel("phone cancellation"));
    let Err(error) = tokio::time::timeout(Duration::from_secs(5), resume)
        .await
        .unwrap()
    else {
        panic!("cancelled Resume admitted a lifecycle");
    };
    assert!(
        error
            .to_string()
            .contains("cancelled before workspace admission"),
        "{error:#}"
    );
    assert!(state.owner().lifecycle.is_empty());
}

#[cfg(unix)]
// Hard-won: d5c0c25: adoption closed the server socket although the client retained it for reuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_refusal_keeps_the_daemon_connection_reusable() {
    let Some(_writer) =
        startup_prompt_test_store("adoption_refusal_keeps_the_daemon_connection_reusable")
    else {
        return;
    };
    let session = runtime_test_session(
        "already-adopted",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(
            stream,
            server_metadata,
            test_runtime_state(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    });
    let mut client = mj_client::daemon::DaemonClient::connect(metadata)
        .await
        .unwrap();
    let error = client
        .adopt_recovery(
            mj_client::daemon::RecoveryAdoptionRequest {
                session_id: session.id,
                target_id: "unused".into(),
                profile: None,
                bundle: None,
                all_instances: false,
            },
            |_| panic!("tracked session must refuse without progress"),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("already tracked"));
    client
        .request(DaemonAction::Ping)
        .await
        .expect("same connection remains usable after adoption's final reply");
    drop(client);
    server.await.unwrap();
}

#[cfg(unix)]
// Hard-won: d5c0c25: a blocked discovery command prevented the adoption connection from observing EOF or drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_discovery_stops_its_subprocess_on_disconnect_and_handoff() {
    use mj_controller::controller::move_session::test_fixtures::TestProxyGroupGuard;
    const NAME: &str = "adoption_discovery_stops_its_subprocess_on_disconnect_and_handoff";
    const ROOT: &str = "MJ_ADOPTION_SCAN_TEST_ROOT";
    if std::env::var_os(ROOT).is_none() {
        for mode in ["disconnect", "handoff"] {
            let root = tempfile::tempdir().unwrap();
            let bin = root.path().join("bin");
            fs::create_dir(&bin).unwrap();
            let groups = TestProxyGroupGuard::new(root.path());
            let script = format!(
                "#!/bin/sh\n{}head -c 131072 /dev/zero >&2\nprintf ready > {}\n/bin/sleep 600 &\nwait\n",
                groups.registration_script(),
                mj_core::targets::posix_quote(&root.path().join("scan-started").to_string_lossy())
            );
            mj_core::test_hooks::install_fake_command(&bin, "podman", &script);
            let path = std::env::join_paths(
                std::iter::once(bin)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), NAME),
            )
            .env(ROOT, root.path())
            .env("MJ_ADOPTION_SCAN_MODE", mode)
            .env("PATH", path)
            .isolated_store(root.path())
            .run();
            drop(groups);
        }
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let mut config = Config::default();
    config.targets.clear();
    config.targets.insert(
        "blocked".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: serde_json::from_value(serde_json::json!({"image":"unused-test-image"}))
                .unwrap(),
        },
    );
    config.save().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let metadata = test_metadata(listener.local_addr().unwrap());
    let server_metadata = metadata.clone();
    let server = mj_core::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_client(
            stream,
            server_metadata,
            test_runtime_state(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    });
    let mut stream = TcpStream::connect(metadata.address).await.unwrap();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: metadata.token,
            action: DaemonAction::AdoptRecovery(mj_client::daemon::RecoveryAdoptionRequest {
                session_id: "missing-orphan".into(),
                target_id: "blocked".into(),
                profile: None,
                bundle: None,
                all_instances: false,
            }),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.join("scan-started").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("discovery reaches the real blocking subprocess without blocking EOF supervision");
    let group: i32 = fs::read_dir(root.join("proxy-groups"))
        .unwrap()
        .find_map(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
        })
        .unwrap();
    if std::env::var("MJ_ADOPTION_SCAN_MODE").unwrap() == "handoff" {
        mj_controller::upgrade::gate().begin_draining();
        let response: ResponseEnvelope =
            tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream))
                .await
                .unwrap()
                .unwrap();
        assert!(response.result.is_err());
    }
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("cancelled discovery must stop and reap its subprocess")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while TestProxyGroupGuard::group_exists(group).unwrap() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no process in the discovery group may survive cancellation");
    assert!(
        mj_controller::database::read_durable_session_record("missing-orphan")
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
// Hard-won: d9dcf7c: a backpressured adoption frame stopped polling daemon drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_backpressured_response_releases_admission_on_handoff() {
    use std::os::fd::AsRawFd;
    const NAME: &str = "adoption_backpressured_response_releases_admission_on_handoff";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let client = tokio::net::TcpSocket::new_v4().unwrap();
    // These fixture capacities force the peer to stop accepting a frame much
    // larger than a pipe buffer. They do not constrain production sockets.
    client.set_recv_buffer_size(4096).unwrap();
    let client = client
        .connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let capacity: libc::c_int = 4096;
    // SAFETY: a live socket descriptor and an initialized integer of the
    // documented SO_SNDBUF size; setsockopt copies it before returning.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&capacity as *const libc::c_int).cast(),
                std::mem::size_of_val(&capacity) as libc::socklen_t,
            )
        },
        0
    );
    let admission = mj_controller::upgrade::activity_unless_draining("adoption install").unwrap();
    let server = mj_core::runtime::spawn(async move {
        let result = super::serve::write_adoption_response(
            &mut stream,
            ResponseEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: 1,
                result: Ok(DaemonReply::AdoptionWaiting {
                    blockers: vec!["x".repeat(256 * 1024)],
                    destroy_command: None,
                }),
            },
            &CancellationToken::new(),
        )
        .await;
        drop(admission);
        result
    });
    tokio::time::timeout(Duration::from_secs(5), client.readable())
        .await
        .unwrap()
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        !server.is_finished(),
        "the peer is not consuming the blocked progress frame"
    );
    mj_controller::upgrade::gate().begin_draining();
    let error = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("socket backpressure must not retain adoption admission")
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("handoff"));
    assert!(mj_controller::upgrade::active_labels().is_empty());
    drop(client);
}

// Hard-won: 1f0ee89: a retry's orphan-destroy command could not reach its tracked worker.
#[test]
fn adoption_destroy_advice_follows_durable_worker_ownership() {
    const NAME: &str = "adoption_destroy_advice_follows_durable_worker_ownership";
    if std::env::var_os("MJ_ADOPTION_ADVICE_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), NAME),
        )
        .env("MJ_ADOPTION_ADVICE_CHILD", "1")
        .env("MJ_INSTANCE", "adoption-advice-test")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let mut record = runtime_test_session(
        "owned-adoption",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Disconnected,
    );
    record.native_session_id = None;
    record.target_template_id = "bare".into();
    record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: root.path().join(&record.id),
    }));
    mj_controller::database::save_session(&record).unwrap();
    assert_eq!(
        mj_controller::controller::adoption_destroy_command(&record.id, "bare", true).unwrap(),
        "mj --instance 'adoption-advice-test' destroy --session 'owned-adoption'"
    );
    // The row alone is not ownership: a lost session without a locator remains an orphan.
    record.state = SessionState::Lost;
    record.target = None;
    mj_controller::database::save_session(&record).unwrap();
    assert_eq!(
        mj_controller::controller::adoption_destroy_command(&record.id, "bare", true).unwrap(),
        "mj --instance 'adoption-advice-test' recover destroy --session 'owned-adoption' --target 'bare' --confirm 'owned-adoption' --all-instances"
    );
}

// Hard-won: b370417: concurrent adoption waits exhausted the daemon blocking pool.
#[test]
fn adoption_waits_leave_the_blocking_pool_available() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let release = CancellationToken::new();
        let mut waits = Vec::new();
        for _ in 0..3 {
            let cancelled = release.clone();
            let (entered, started) = tokio::sync::oneshot::channel();
            waits.push(super::serve::spawn_adoption_operation(async move {
                entered.send(()).unwrap();
                cancelled.cancelled().await;
                Ok(())
            }));
            // Make every operation enter its wait before asking for the next.
            // Cancellation below also releases waits if the regression returns.
            if tokio::time::timeout(Duration::from_secs(5), started)
                .await
                .is_err()
            {
                release.cancel();
                for wait in waits {
                    let _ = wait.await;
                }
                panic!("adoption waits occupied the blocking pool");
            }
        }
        let progress = tokio::time::timeout(
            Duration::from_secs(5),
            mj_core::runtime::spawn_blocking(|| 42),
        )
        .await;
        release.cancel();
        for wait in waits {
            wait.await.unwrap().unwrap();
        }
        assert_eq!(
            progress
                .expect("blocking pool remains available for controller loads and progress")
                .unwrap(),
            42
        );
    });
}

pub(super) fn isolate_runtime_feed_test(module: &str, name: &str) -> bool {
    const KEY: &str = "MJ_TEST_RUNTIME_FEED_ISOLATED";
    if std::env::var(KEY).ok().as_deref() == Some(name) {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    mj_controller::controller::test_support::IsolatedTest::new(
        mj_controller::controller::test_support::test_name(module, name),
    )
    .env(KEY, name)
    .isolated_store(root.path())
    .run();
    true
}

// Hard-won: 48811463b: a completed sync flag could label pre-sync rows as final.
#[cfg(unix)]
#[tokio::test]
async fn wiki_search_generations_cover_commits_and_running_requests_coalesce() {
    const TEST: &str = "wiki_search_generations_cover_commits_and_running_requests_coalesce";
    const CHILD: &str = "MJ_TEST_WIKI_GENERATIONS";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let hooks = root.path().join("hooks");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module_path!(), TEST),
        )
        .isolated_store(root.path())
        .env(CHILD, "1")
        .env(
            mj_core::config::SESSION_INDEX_ENV,
            root.path().join("sessionwiki"),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "sessionwiki_sync_pass")
        .env("MJ_TEST_HOOK_DIR", &hooks)
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let id = "0123456789abcdef0123456789abcdef";
    let session = runtime_test_session(
        id,
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let mut conversation = mj_core::state::MaterializedSession::empty(id);
    conversation.applied_event_ordinal = 1;
    conversation.applied_event_digest = format!("{:064x}", 1);
    conversation.last_activity_at_ms = Some(1_700_000_000_001);
    conversation
        .transcript
        .push(Arc::new(mj_core::transcript::TranscriptItem {
            stable_id: "prompt".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1_700_000_000_001,
            last_changed_at_ms: 1_700_000_000_001,
            body: mj_core::transcript::TranscriptBody::User {
                content: vec![
                    serde_json::json!({"type":"text", "text":"swsyncquokka fresh conversation"}),
                ],
            },
        }));
    mj_controller::database::save_materialized_session(&conversation).unwrap();
    sessionwiki::index::open().unwrap();
    let state = test_runtime_state_loading_the_store();
    let waiting_state = Arc::clone(&state);
    let waiting =
        mj_core::runtime::spawn(async move { waiting_state.wiki().sync_now(false).await });
    let hooks = PathBuf::from(std::env::var_os("MJ_TEST_HOOK_DIR").unwrap());
    tokio::time::timeout(Duration::from_secs(60), async {
        while !hooks.join("sessionwiki_sync_pass.reached").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sync reaches barrier");
    let first = state.wiki().state().started;
    let pending = state.wiki_search("swsyncquokka".into(), 10).await.unwrap();
    assert!(pending.rows.is_empty());
    let live_pending = state
        .session_text_search("swsyncquokka".into())
        .await
        .unwrap();
    assert!(live_pending.matches.is_empty());
    let next = state.wiki().request_sync(false);
    assert_eq!(next, first + 1);
    for full in [false, true, false, true] {
        assert_eq!(state.wiki().request_sync(full), next);
    }
    let mut changes = state.wiki().watch();
    std::fs::write(hooks.join("sessionwiki_sync_pass.continue"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if changes.borrow_and_update().finished >= next {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("both passes finish");
    waiting.await.unwrap().unwrap();
    let synced = state.wiki().state();
    assert_eq!(
        synced.started, next,
        "bursts coalesce into exactly one successor"
    );
    assert_eq!(synced.successful, next, "{:?}", synced.outcome);
    let page = state.wiki_search("swsyncquokka".into(), 10).await.unwrap();
    assert_eq!(
        page.rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        [id]
    );
    let live = state
        .session_text_search("swsyncquokka".into())
        .await
        .unwrap();
    assert_eq!(
        live.matches
            .iter()
            .map(|hit| hit.session_id.as_str())
            .collect::<Vec<_>>(),
        [id]
    );
}
