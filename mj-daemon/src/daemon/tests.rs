use super::*;
use crate::server::api::TerminalOperations;
use mj_client::http::{
    ImportSessionMetadata, LifecycleCompletion, SessionCommandRequest,
    StartupPromptOutcomesRequest, StartupPromptRequest, SuspendPreflightRequest,
};
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
    for kind in [LifecycleKind::Create] {
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
            .start_suspend_session_controlled(
                LATCH_RELAY_SESSION.into(),
                mj_core::suspend_consent::SuspendConsent::default(),
                cancelled,
            )
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
                .start_suspend_session_controlled(
                    LATCH_RELAY_SESSION.into(),
                    mj_core::suspend_consent::SuspendConsent::default(),
                    control,
                )
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
        assert_eq!(close_route(&session, true), CloseRoute::RecoverInterrupted);
        // Without a target there is nothing left for recovery to finish, and
        // nothing to checkpoint either: the close settles instead of waiting
        // on a relay that does not exist.
        session.target = None;
        assert_eq!(
            close_route(&session, true),
            CloseRoute::SettleWithoutCheckpoint
        );
    }

    let mut running = runtime_test_session("session", "workspace", SessionState::Running);
    running.target = target.clone().map(Box::new);
    assert_eq!(close_route(&running, true), CloseRoute::Graceful);

    // A session wedged in provisioning has no harness state and no relay, with
    // or without the container it managed to create (#1059).
    let mut provisioning = runtime_test_session("session", "workspace", SessionState::Provisioning);
    assert_eq!(
        close_route(&provisioning, true),
        CloseRoute::SettleWithoutCheckpoint
    );
    provisioning.target = target.clone().map(Box::new);
    assert_eq!(
        close_route(&provisioning, true),
        CloseRoute::SettleWithoutCheckpoint
    );

    // The same session once startup reconciliation has failed it (#1070).
    let failed = runtime_test_session("session", "workspace", SessionState::Error);
    assert_eq!(
        close_route(&failed, true),
        CloseRoute::SettleWithoutCheckpoint
    );
    // A failed session that still names its target keeps a workspace worth
    // checkpointing, so it takes the graceful close.
    let mut failed_with_target = failed.clone();
    failed_with_target.target = target.clone().map(Box::new);
    assert_eq!(close_route(&failed_with_target, true), CloseRoute::Graceful);
    // Every child takes the ownership-aware close, including recovery of an
    // interrupted teardown; none needs its parent's workspace checkpoint.
    assert_eq!(
        close_route(&failed_with_target, false),
        CloseRoute::Borrower
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
        assert_eq!(close_route(&child, false), CloseRoute::Borrower);
    }

    let mut stopped = runtime_test_session("session", "workspace", SessionState::Stopped);
    assert_eq!(close_route(&stopped, true), CloseRoute::Done);
    stopped.target = target.map(Box::new);
    assert_eq!(close_route(&stopped, true), CloseRoute::DeferredCleanup);
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
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    http.set_operator_away(true).await.unwrap();
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
                answers: None,
                permission_decision: None,
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
    http.set_operator_away(false).await.unwrap();
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn client_presence_is_global_and_detach_and_prune_remove_it() {
    let Some(_writer) =
        startup_prompt_test_store("client_presence_is_global_and_detach_and_prune_remove_it")
    else {
        return;
    };
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    http.attach("client-a".into()).await.unwrap();
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

    http.detach("client-a".into()).await.unwrap();
    assert!(state.attachments().is_empty());
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_quota_refresh_request_wakes_the_daemons_poller_and_fails_without_one() {
    let state = test_runtime_state();
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    http.refresh_quota()
        .await
        .expect_err("no quota poller is attached, so nothing can probe");

    let (wake, mut woken) = tokio::sync::mpsc::unbounded_channel();
    state.attach_quota_refresh(wake);
    for _ in 0..3 {
        http.refresh_quota().await.expect("refresh quota");
    }
    for _ in 0..3 {
        assert_eq!(
            woken.try_recv().expect("the poller receives every intent"),
            QuotaRefreshIntent::Force
        );
    }
    assert!(woken.try_recv().is_err());
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_probe_quota_request_targets_named_profiles_even_disabled_ones() {
    let state = test_runtime_state();
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
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
    let error = http
        .probe_quota_profiles(vec!["missing".into()], false)
        .await
        .expect_err("an unconfigured profile is refused");
    assert!(error.to_string().contains("no profile named"), "{error:#}");

    let (wake, mut woken) = tokio::sync::mpsc::unbounded_channel();
    state.attach_quota_refresh(wake);
    http.probe_quota_profiles(vec!["dsh".into()], true)
        .await
        .expect("probe the disabled profile");
    let Some(QuotaRefreshIntent::ProbeProfiles { requests, forced }) = woken.try_recv().ok() else {
        panic!("the poller receives the probe");
    };
    assert!(forced, "the force flag reaches the poller");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].profile_id, "dsh");
    shutdown.cancel();
    server.await.unwrap().unwrap();
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

pub(crate) fn test_runtime_state() -> Arc<RuntimeState> {
    test_runtime_state_with(Controller {
        config: Config::default(),
        state: mj_core::state::State::default(),
    })
}

pub(super) fn test_runtime_state_with(controller: Controller) -> Arc<RuntimeState> {
    test_runtime_state_with_live_recovery(controller, empty_test_controller).0
}

fn empty_test_controller() -> Result<Controller> {
    Ok(Controller {
        config: Config::default(),
        state: mj_core::state::State::default(),
    })
}

fn test_runtime_state_with_live_recovery(
    controller: Controller,
    controller_loader: fn() -> Result<Controller>,
) -> (
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
        controller,
        recovery.clone(),
        upgrades.observer(),
        Vec::new(),
        controller_loader,
    ));
    (state, recovery, upgrades)
}

#[cfg(unix)]
pub(super) fn test_runtime_state_loading_the_store_with_live_recovery() -> (
    Arc<RuntimeState>,
    Arc<RecoveryGate>,
    crate::worker_upgrade::WorkerUpgradeCoordinator,
) {
    test_runtime_state_with_live_recovery(Controller::load().unwrap(), Controller::load)
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
    let (http, shutdown, server) = start_test_dashboard_http_api(state).await;
    let call = mj_core::runtime::spawn(async move {
        http.submit_session_command(
            "session-1".into(),
            SessionCommandRequest {
                command_id: "command-1".into(),
                inherited_draft: None,
                command: RelayCommand::ClearQueuedPrompts,
            },
        )
        .await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
        .await
        .expect("submit request did not reach the manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Submit { reply, .. } = request else {
        panic!("expected a submitted mutation")
    };
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    reply
        .send(Ok(7))
        .expect("daemon incorrectly cancelled a non-reviewer mutation");
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
        staged_aws_profile: None,
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

pub(crate) fn runtime_test_subagent(
    child_session_id: &str,
    parent_session_id: &str,
) -> SubagentRecord {
    SubagentRecord {
        resume_with_parent: false,
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
        orchestrator: false,
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

#[test]
fn tree_wide_lifecycle_finds_live_depth_two_worker_under_parked_orchestrator() {
    let primary = runtime_test_session("primary", "workspace", SessionState::Running);
    let parked_sup = runtime_test_session("parked-sup", "workspace", SessionState::Parked);
    let mut live_d2 = runtime_test_session("live-d2", "workspace", SessionState::Running);
    live_d2.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/tmp/live-d2"),
    }));
    let mut state = mj_core::state::State {
        sessions: [primary.clone(), parked_sup.clone(), live_d2.clone()]
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        ..mj_core::state::State::default()
    };
    let mut sup_record = runtime_test_subagent(&parked_sup.id, &primary.id);
    sup_record.orchestrator = true;
    let d2_record = runtime_test_subagent(&live_d2.id, &parked_sup.id);
    state.subagents = [sup_record, d2_record]
        .into_iter()
        .map(|subagent| (subagent.child_session_id.clone(), subagent.into()))
        .collect();
    state.environment_owners = state
        .sessions
        .keys()
        .map(|id| (id.clone(), primary.id.clone()))
        .collect();

    let children = state.active_child_session_ids(&primary.id);
    assert!(
        children.contains(&"live-d2".to_owned()),
        "active_child_session_ids must include live depth-2 worker under parked orchestrator: {children:?}"
    );
    assert!(
        state.subagents_occupy_container(&primary.id),
        "live depth-2 worker under parked orchestrator occupies the primary's container"
    );
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

    let refused = ask(&mut stream, DaemonAction::Identity).await;
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
            action: DaemonAction::Identity,
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
        ProtocolTranscript {
            protocol_version: 123,
            daemon_build: "2.74.0",
            expected_requests: requests(123),
            responses: [
                r#"{"protocol_version":123,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-10-27T00:00:00Z","build_version":"2.74.0","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                r#"{"protocol_version":123,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
            ],
        },
    ]
}

// Hard-won: 848b4781f: direct CLI removal saved config before failing without the daemon database writer.
#[cfg(unix)]
#[tokio::test]
async fn remove_project_http_removes_missing_directory_from_config_and_runtime_feed() {
    const NAME: &str = "remove_project_http_removes_missing_directory_from_config_and_runtime_feed";
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

    let (http, shutdown, server) = start_test_http_api(state.clone()).await;
    http.remove_project(bundle_id.into()).await.unwrap();
    shutdown.cancel();
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
    let without_checkpoint = serde_json::to_string(&mj_core::suspend_consent::SuspendConsent {
        without_checkpoint: true,
        ..Default::default()
    })
    .unwrap();
    let mut cancelled_wait = Box::pin(state.start_or_join_lifecycle_controlled(
        "session-1".into(),
        LifecycleKind::Suspend,
        None,
        Some(without_checkpoint),
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

#[cfg(unix)]
const DEFERRED_CLEANUP_TEST_SESSION: &str = "0123456789abcdef0123456789abcdef";

#[cfg(unix)]
fn deferred_cleanup_test(name: &str, phases: &[&str]) -> Option<(PathBuf, String)> {
    use mj_controller::controller::test_support::{IsolatedTest, test_name};

    const ROOT: &str = "MJ_DEFERRED_CLEANUP_TEST_ROOT";
    const PHASE: &str = "MJ_DEFERRED_CLEANUP_TEST_PHASE";
    if let Some(root) = std::env::var_os(ROOT) {
        return Some((root.into(), std::env::var(PHASE).unwrap()));
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let session = DEFERRED_CLEANUP_TEST_SESSION;
    let container = mj_controller::targets::resource_name(session).unwrap();
    let script = format!(
        r#"#!/bin/sh
set -eu
case "$1 ${{2-}}" in
    'container inspect'|'container exists') exit 1 ;;
    'rm --force'|'info ') exit 0 ;;
    'volume inspect')
        case "$5" in
            '{container}-workspace') file=volume ;;
            '{container}-replacement') file=replacement-volume ;;
            *) exit 1 ;;
        esac
        [ -f "$MJ_DEFERRED_CLEANUP_TEST_ROOT/$file" ] || exit 1
        printf '%s\n' 'true|{session}'
        ;;
    'volume rm')
        echo "$4" >> "$MJ_DEFERRED_CLEANUP_TEST_ROOT/removals"
        case "$4" in
            '{container}-workspace') ;;
            '{container}-replacement') rm "$MJ_DEFERRED_CLEANUP_TEST_ROOT/replacement-volume"; exit ;;
            *) exit 125 ;;
        esac
        if test "$MJ_DEFERRED_CLEANUP_TEST_PHASE" != recovery; then
            head -c 131072 /dev/zero >&2
            printf '%s %s\n' "$PPID" "$$" > "$MJ_DEFERRED_CLEANUP_TEST_ROOT/removal-started.pending"
            mv "$MJ_DEFERRED_CLEANUP_TEST_ROOT/removal-started.pending" "$MJ_DEFERRED_CLEANUP_TEST_ROOT/removal-started"
            exec /bin/sleep 60
        fi
        rm "$MJ_DEFERRED_CLEANUP_TEST_ROOT/volume"
        ;;
    *) exit 125 ;;
esac
"#,
    );
    mj_core::test_hooks::install_fake_command(&bin, "podman", &script);
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    for phase in phases {
        IsolatedTest::new(test_name(module_path!(), name))
            .env(ROOT, root.path())
            .env(PHASE, phase)
            .env("PATH", &path)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env(
                mj_core::config::SESSION_INDEX_ENV,
                root.path().join("sessionwiki"),
            )
            .isolated_store(root.path())
            .run();
    }
    None
}

#[cfg(unix)]
fn start_deferred_cleanup_test(
    root: &Path,
) -> (
    Arc<RuntimeState>,
    crate::worker_upgrade::WorkerUpgradeCoordinator,
) {
    use mj_controller::controller::lifecycle::test_fixtures::{
        persist_controller_fixture, stopped_podman_cleanup_controller,
    };
    let mut controller = stopped_podman_cleanup_controller(DEFERRED_CLEANUP_TEST_SESSION);
    controller.config.save().unwrap();
    persist_controller_fixture(&mut controller);
    fs::write(root.join("volume"), "").unwrap();
    fs::write(root.join("replacement-volume"), "").unwrap();
    let (state, _, upgrades) = test_runtime_state_loading_the_store_with_live_recovery();
    (state, upgrades)
}

#[cfg(unix)]
async fn wait_for_cleanup_removal(root: &Path, result: LifecycleWatch) -> Vec<u32> {
    tokio::select! {
        pids = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pids) = fs::read_to_string(root.join("removal-started")) {
                    return pids.split_whitespace().map(|pid| pid.parse().unwrap()).collect();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }) => pids.expect("deferred cleanup reached its exact removal command"),
        outcome = RuntimeState::wait_lifecycle_result(result) => {
            panic!("deferred cleanup settled before its held removal command: {outcome:?}");
        }
    }
}

#[cfg(unix)]
// Hard-won: #1628: daemon handoff waited for deferred storage removal before cancelling it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_cleanup_yields_handoff_reaps_removal_and_retries_on_startup() {
    const NAME: &str = "deferred_cleanup_yields_handoff_reaps_removal_and_retries_on_startup";
    let Some((root, phase)) = deferred_cleanup_test(NAME, &["handoff", "recovery"]) else {
        return;
    };
    let _writer = mj_controller::database::install_isolated_test_writer();
    let id = DEFERRED_CLEANUP_TEST_SESSION;
    if phase == "recovery" {
        let (state, _, mut upgrades) = test_runtime_state_loading_the_store_with_live_recovery();
        state.resume_retained_cleanups().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while mj_controller::database::read_durable_session_record(id)
                .unwrap()
                .unwrap()
                .target
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("startup must finish cleaning the retained locator");
        upgrades.shutdown().await.unwrap();
        state.cancel_and_wait_lifecycles().await.unwrap();
        assert!(!root.join("volume").exists());
        assert!(root.join("replacement-volume").exists());
        assert_eq!(
            fs::read_to_string(root.join("removals"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        return;
    }

    let (state, mut upgrades) = start_deferred_cleanup_test(&root);
    let result = state.start_deferred_cleanup(id.to_owned()).await.unwrap();
    let pids = wait_for_cleanup_removal(&root, result.clone()).await;
    assert!(state.active_lifecycles().iter().any(|active| {
        active.kind == RuntimeLifecycleKind::Cleanup
            && active
                .active_stages
                .iter()
                .any(|(stage, _)| *stage == ProvisionStage::RemovingStorage)
    }));
    let metadata = test_metadata("127.0.0.1:0".parse().unwrap());
    let shutdown = CancellationToken::new();
    assert!(matches!(
        handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
            .await
            .unwrap(),
        DaemonReply::Done
    ));
    assert!(shutdown.is_cancelled());
    upgrades.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), state.cancel_and_wait_lifecycles())
        .await
        .expect("shutdown must interrupt the command without a cleanup drain")
        .unwrap();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
    assert!(
        pids.iter()
            .all(|&pid| !mj_client::daemon::process_is_alive(pid))
    );
    // SAFETY: signal zero is an existence probe. The recorded parent is the
    // local removal command owned by the cancellable executor.
    assert_eq!(unsafe { libc::kill(pids[0].try_into().unwrap(), 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let stopped = mj_controller::database::read_durable_session_record(id)
        .unwrap()
        .unwrap();
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.target.is_some());
    assert!(stopped.last_error.is_none());
    assert!(root.join("volume").exists());
}

#[cfg(unix)]
// Hard-won: #1628: pre-admission teardown could make handoff wait on automatic cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_cleanup_serializes_queued_destroy_and_archive_without_holding_handoff() {
    use mj_controller::recovery_gate::LifecycleOrigin;

    const NAME: &str =
        "deferred_cleanup_serializes_queued_destroy_and_archive_without_holding_handoff";
    let Some((root, _)) = deferred_cleanup_test(NAME, &["queue"]) else {
        return;
    };
    let _writer = mj_controller::database::install_isolated_test_writer();
    let id = DEFERRED_CLEANUP_TEST_SESSION;
    let (state, mut upgrades) = start_deferred_cleanup_test(&root);
    let mut stopped = mj_controller::database::read_durable_session_record(id)
        .unwrap()
        .unwrap();
    let mut input = mj_controller::controller::test_support::checkpoint_archive_input(
        id,
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
                content: vec![serde_json::json!({"type": "text", "text": "archive this session"})],
            },
        });
    let archives = mj_core::config::sessions_dir();
    fs::create_dir_all(&archives).unwrap();
    stopped.checkpoint = Some(Box::new(
        mj_controller::controller::test_support::write_checkpoint_archive_input(
            &archives, id, &input,
        ),
    ));
    let repository = mj_controller::controller::test_support::committed_repository();
    stopped.project_directory = Some(repository.path().to_owned());
    stopped.updated_at = "2000-01-01T00:00:00Z".into();
    mj_controller::database::save_session(&stopped).unwrap();
    let foreground = state
        .session_gate
        .try_enter_foreground(id, "replacement", LifecycleOrigin::Person)
        .unwrap();
    let cleanup = state.start_deferred_cleanup(id.into()).await.unwrap();
    assert!(mj_controller::upgrade::active_labels().is_empty());
    assert!(!root.join("removal-started").exists());
    drop(foreground);
    wait_for_cleanup_removal(&root, cleanup.clone()).await;
    assert!(
        state
            .session_gate
            .try_enter_foreground(id, "replacement", LifecycleOrigin::Person)
            .is_err()
    );

    let metadata = test_metadata("127.0.0.1:0".parse().unwrap());
    let shutdown = CancellationToken::new();
    let destroy = TerminalOperations::terminal_destroy_stopped_session(state.clone(), id.into());
    let archive = state.archive_aged_sessions(1);
    tokio::pin!(destroy, archive);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(futures::poll!(&mut destroy).is_pending());
            assert!(futures::poll!(&mut archive).is_pending());
            if state
                .owner()
                .lifecycle_intents
                .get(id)
                .is_some_and(|queue| queue.len() == 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both teardown requests enter the same lifecycle queue");
    assert!(state.owner().lifecycle[id].result.same_channel(&cleanup));
    assert!(matches!(
        handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
            .await
            .unwrap(),
        DaemonReply::Done
    ));
    assert!(super::actions::is_upgrade_handoff_refusal(
        &destroy.await.unwrap_err()
    ));
    assert_eq!(archive.await.unwrap(), 0);
    upgrades.shutdown().await.unwrap();
    state.cancel_and_wait_lifecycles().await.unwrap();
    assert_eq!(
        fs::read_to_string(root.join("removals"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(root.join("replacement-volume").exists());
}

#[cfg(unix)]
// Hard-won: #1628: archive running retained storage cleanup before automatic cleanup held handoff admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_cleanup_inside_archive_yields_handoff_without_destroying_session() {
    const NAME: &str = "deferred_cleanup_inside_archive_yields_handoff_without_destroying_session";
    let Some((root, _)) = deferred_cleanup_test(NAME, &["archive"]) else {
        return;
    };
    let _writer = mj_controller::database::install_isolated_test_writer();
    let id = DEFERRED_CLEANUP_TEST_SESSION;
    let (state, mut upgrades) = start_deferred_cleanup_test(&root);
    let mut stopped = mj_controller::database::read_durable_session_record(id)
        .unwrap()
        .unwrap();
    let mut input = mj_controller::controller::test_support::checkpoint_archive_input(
        id,
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
                content: vec![serde_json::json!({"type": "text", "text": "archive this session"})],
            },
        });
    let archives = mj_core::config::sessions_dir();
    fs::create_dir_all(&archives).unwrap();
    stopped.checkpoint = Some(Box::new(
        mj_controller::controller::test_support::write_checkpoint_archive_input(
            &archives, id, &input,
        ),
    ));
    let repository = mj_controller::controller::test_support::committed_repository();
    stopped.project_directory = Some(repository.path().to_owned());
    stopped.updated_at = "2000-01-01T00:00:00Z".into();
    mj_controller::database::save_session(&stopped).unwrap();

    let archive = state.archive_aged_sessions(1);
    tokio::pin!(archive);
    let pids: Vec<u32> = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(futures::poll!(&mut archive).is_pending());
            if let Ok(pids) = fs::read_to_string(root.join("removal-started")) {
                return pids
                    .split_whitespace()
                    .map(|pid| pid.parse().unwrap())
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("archive reached the stopped session's retained removal command");

    let metadata = test_metadata("127.0.0.1:0".parse().unwrap());
    let shutdown = CancellationToken::new();
    assert!(matches!(
        handle_action(DaemonAction::PrepareUpgrade, &metadata, &state, &shutdown)
            .await
            .unwrap(),
        DaemonReply::Done
    ));
    assert!(shutdown.is_cancelled());
    upgrades.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), state.cancel_and_wait_lifecycles())
        .await
        .expect("shutdown must interrupt archive's retained storage cleanup")
        .unwrap();
    assert_eq!(archive.await.unwrap(), 0);
    assert!(
        pids.iter()
            .all(|&pid| !mj_client::daemon::process_is_alive(pid))
    );
    let retained = mj_controller::database::read_durable_session_record(id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, SessionState::Stopped);
    assert!(retained.target.is_some());
    assert!(root.join("volume").exists());
    assert!(root.join("replacement-volume").exists());
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
    let (state, _, _upgrades) = test_runtime_state_with_live_recovery(
        empty_test_controller().unwrap(),
        empty_test_controller,
    );
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
    let state = create_watchdog_test_runtime(Some(mj_core::subagent::SubagentKind::Reviewer));
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
        assert_create_progress(&state, LIFECYCLE_STALL_BOUND);
        executor.stage_finished(ProvisionStage::Syncing);
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        assert_create_progress(&state, LIFECYCLE_STALL_BOUND);
    }
}

fn check_progress(state: &Arc<RuntimeState>, bound: Duration) -> views::LifecycleStallProgress {
    state.cancel_stalled_lifecycle("session-1", LifecycleKind::Create, bound)
}

fn assert_create_progress(state: &Arc<RuntimeState>, bound: Duration) {
    assert!(matches!(
        check_progress(state, bound),
        views::LifecycleStallProgress::Running(_)
    ));
}

fn create_watchdog_test_runtime(
    kind: Option<mj_core::subagent::SubagentKind>,
) -> Arc<RuntimeState> {
    let mut controller = empty_test_controller().unwrap();
    let session = runtime_test_session("session-1", "workspace", SessionState::Provisioning);
    if let Some(kind) = kind {
        let mut relation = runtime_test_subagent(&session.id, "parent");
        relation.kind = kind;
        controller
            .state
            .subagents
            .insert(session.id.clone(), relation.into());
    }
    controller
        .state
        .sessions
        .insert(session.id.clone(), session);
    test_runtime_state_with(controller)
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
    let bound = LIFECYCLE_STALL_BOUND / 2;
    for queue in [
        ProvisionStage::WaitingForImagePull,
        ProvisionStage::WaitingForStartSlot,
        ProvisionStage::WaitingForWorkerStore,
        ProvisionStage::WaitingForSshConnection,
    ] {
        executor.stage_started(queue.clone());
        tokio::time::sleep(bound * 3).await;
        assert_create_progress(&state, bound);
        executor.stage_finished(queue);
    }
    // Admitted: the clock starts again from admission, and the next stage can
    // stall like any other.
    executor.stage_started(ProvisionStage::Starting);
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    assert_create_progress(&state, bound);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(matches!(
        check_progress(&state, bound),
        views::LifecycleStallProgress::Stalled { .. }
    ));
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
    let bound = LIFECYCLE_STALL_BOUND / 2;
    let half = bound / 2;

    // The stream has to queue for SSH first. Its idle clock begins only after
    // admission, even when queueing takes more than ten minutes.
    report(
        profile,
        send_profile,
        CommandActivity::StageStarted(ProvisionStage::WaitingForSshConnection),
    );
    tokio::time::advance(bound * 2).await;
    assert_create_progress(&state, bound);
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
        assert_create_progress(&state, bound);
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
    assert_create_progress(&state, bound);
    report(
        profile,
        send_profile,
        CommandActivity::StageStarted(ProvisionStage::WaitingForWorkerStore),
    );
    tokio::time::advance(half).await;
    assert!(matches!(
        check_progress(&state, bound),
        views::LifecycleStallProgress::Stalled {
            stream: Some(stream),
            phase: views::StalledLifecycle::Cancelled,
            ..
        } if stream == upload_worker
    ));
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
    let bound = LIFECYCLE_STALL_BOUND / 2;
    let preparing = ProvisionStage::PreparingHarness {
        harness: mj_core::config::HarnessKind::Codex,
        step: "Downloading".into(),
        since_ms: 0,
    };
    executor.stage_started(preparing.clone());
    tokio::time::sleep(bound * 3).await;
    assert_create_progress(&state, bound);
    executor.stage_finished(preparing);
    // The clock starts again when preparation ends.
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    assert_create_progress(&state, bound);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(matches!(
        check_progress(&state, bound),
        views::LifecycleStallProgress::Stalled { .. }
    ));
}

// Hard-won: 9c928ddc2: a stalled Create only failed the reviewer start and kept running with its lifecycle admission held, which blocks daemon handoff.
#[tokio::test(start_paused = true)]
async fn a_stalled_create_is_cancelled_and_releases_its_admission() {
    let state = create_watchdog_test_runtime(Some(mj_core::subagent::SubagentKind::Reviewer));
    let cancel_seen = Arc::new(AtomicBool::new(false));
    let (admitted, started) = tokio::sync::oneshot::channel();
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let cancel_seen = cancel_seen.clone();
            move |_state, _session_id, cancelled, _session_admission| async move {
                admitted.send(()).unwrap();
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                cancel_seen.store(true, Ordering::Release);
                anyhow::bail!("create cancelled")
            }
        })
        .await
        .unwrap();
    started.await.unwrap();
    assert!(
        state.owner().lifecycle["session-1"]
            .progress
            .stall_progress_changed
            .is_some()
    );
    let executor = DaemonStageReportingExecutor::new(
        RefusingExecutor("a stage notification"),
        state.clone(),
        "session-1".into(),
    );
    executor.stage_started(ProvisionStage::Syncing);
    let bound = LIFECYCLE_STALL_BOUND;
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    assert_create_progress(&state, bound);
    assert!(!cancel_seen.load(Ordering::Acquire));
    tokio::time::sleep(Duration::from_secs(2)).await;
    // The cancel reached the Create's own flag, and the Create ended, which
    // is what drops its admission.
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        RuntimeState::wait_lifecycle_result(result),
    )
    .await
    .expect("the cancelled Create ends");
    let error = outcome.unwrap_err().to_string();
    assert!(error.contains("stalled in: Sync"), "{error}");
    assert!(cancel_seen.load(Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn a_primary_or_worker_create_can_run_a_stage_past_the_stall_bound() {
    for kind in [None, Some(mj_core::subagent::SubagentKind::Worker)] {
        let state = create_watchdog_test_runtime(kind);
        let (admitted, started) = tokio::sync::oneshot::channel();
        let (finish, running) = tokio::sync::oneshot::channel();
        let result = state
            .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
                move |_, _, cancelled, _| async move {
                    admitted.send(()).unwrap();
                    running.await.unwrap();
                    assert!(!cancelled.load(Ordering::Acquire));
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .await
            .unwrap();
        started.await.unwrap();
        assert!(
            state.owner().lifecycle["session-1"]
                .progress
                .stall_progress_changed
                .is_none()
        );
        let executor = DaemonStageReportingExecutor::new(
            RefusingExecutor("a stage notification"),
            state.clone(),
            "session-1".into(),
        );
        executor.stage_started(ProvisionStage::Syncing);
        tokio::time::sleep(LIFECYCLE_STALL_BOUND * 2).await;
        executor.stage_finished(ProvisionStage::Syncing);
        finish.send(()).unwrap();
        RuntimeState::wait_lifecycle_result(result).await.unwrap();
    }
}

/// A background image download belongs to the daemon, not to a session, so
/// whichever workspace the person is looking at shows it.
/// A view of `session-1` whose harness is ready for its first prompt.
pub(super) fn ready_startup_view() -> ManagedSessionView {
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let operational = mj_core::relay::RelayOperationalState {
        in_place_move_hold: None,
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
    isolated_test_store(module_path!(), name)
}

pub(crate) fn isolated_test_store(
    module: &str,
    name: &str,
) -> Option<mj_controller::database::DatabaseWriterOwner> {
    if std::env::var_os("MJ_TEST_DURABLE_STARTUP").is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(module, name),
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
    // retained agent-dev tag.
    let recorded = mj_controller::database::load_session_record("session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        mj_core::state::ContainerSettings::requested(&recorded).default_image,
        Some(Some(
            mj_core::config::LEGACY_DEFAULT_CONTAINER_IMAGE.to_owned()
        ))
    );
    // Hard-won: review of a13f351: upgrading an agent-dev session onto buildpack-deps lost its hel account, home and control directory.
    assert_eq!(
        mj_core::state::ContainerSettings::upgrade_image(
            &recorded,
            mj_core::config::DEFAULT_SESSION_IMAGE
        ),
        mj_core::config::LEGACY_DEFAULT_CONTAINER_IMAGE
    );
    let mut refreshed = mj_core::state::ContainerSettings::requested(&recorded);
    refreshed.record_upgrade_image(mj_core::config::LEGACY_DEFAULT_CONTAINER_IMAGE);
    assert_eq!(
        refreshed.default_image,
        Some(Some(
            mj_core::config::LEGACY_DEFAULT_CONTAINER_IMAGE.to_owned()
        ))
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
            mj_controller::database::begin_subagent_unpark("child-1", &wake, None, None).unwrap();
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
                mj_controller::database::begin_subagent_unpark("child-1", &wake, None, None)
                    .is_err()
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
        native_use_evidence: None,
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

fn queued_prompt_action(text: &str) -> StartupPromptRequest {
    StartupPromptRequest {
        move_operation_id: None,
        command_id: format!("startup-{text}"),
        text: text.into(),
        prompt: vec![ContentBlock::Text(TextContent::new(text))],
        encoded_draft: text.into(),
        inherited_draft: None,
    }
}

fn queued_move_prompt_action(text: &str, origin: &str) -> StartupPromptRequest {
    let mut action = queued_prompt_action(text);
    action.move_operation_id = Some(origin.to_owned());
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
    let cancellation = CancellationToken::new();

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("first prompt"),
        cancellation.clone(),
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

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("second prompt"),
        cancellation.clone(),
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

#[tokio::test]
async fn startup_prompt_delivers_installed_file_without_image_capability() {
    use mj_chat::chat::{AttachedFile, PromptFile, PromptPayload};
    use mj_core::attachment::{AttachmentStore, FileAttachmentRef};
    let Some(_writer) = startup_prompt_test_store(
        "startup_prompt_delivers_installed_file_without_image_capability",
    ) else {
        return;
    };
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");

    let bytes = b"example file";
    let reference = FileAttachmentRef::new(bytes, "report.txt", "text/plain".into()).unwrap();
    AttachmentStore::controller("session-1")
        .unwrap()
        .install_file(&reference, bytes)
        .unwrap();
    let text = mj_core::draft::file_marker(1, &reference.filename);
    let payload = PromptPayload {
        files: vec![PromptFile {
            range: 0..text.len(),
            number: 1,
            file: AttachedFile::from_reference(reference.clone()).unwrap(),
        }],
        text,
        images: Vec::new(),
    };
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        StartupPromptRequest {
            move_operation_id: None,
            command_id: "startup-file".into(),
            text: payload.text.clone(),
            prompt: payload.content_blocks(),
            encoded_draft: payload.encode_draft(),
            inherited_draft: None,
        },
        CancellationToken::new(),
    )
    .await
    .expect("queue file prompt");
    let view = ready_startup_view();
    assert!(
        !view
            .snapshot
            .as_ref()
            .unwrap()
            .operational
            .accepts_prompt_images()
    );
    manager
        .publisher
        .publish("session-1".into(), view)
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(10), manager.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit {
        command: RelayCommand::Prompt { prompt },
        reply,
        ..
    } = request
    else {
        panic!("expected a submitted prompt");
    };
    assert_eq!(prompt, vec![reference.content_block()]);
    reply.send(Ok(1)).unwrap();
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
    let cancellation = CancellationToken::new();

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("undelivered prompt"),
        cancellation.clone(),
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
    let cancellation = CancellationToken::new();

    for text in ["refused prompt", "prompt behind it"] {
        TerminalOperations::terminal_queue_startup_prompt(
            state.clone(),
            "session-1".into(),
            queued_prompt_action(text),
            cancellation.clone(),
        )
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
    let cancellation = CancellationToken::new();

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("never delivered"),
        cancellation.clone(),
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
    let cancellation = CancellationToken::new();
    for text in ["refused prompt", "prompt behind it"] {
        TerminalOperations::terminal_queue_startup_prompt(
            state.clone(),
            "session-1".into(),
            queued_prompt_action(text),
            cancellation.clone(),
        )
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
    let cancellation = CancellationToken::new();

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("edit me"),
        cancellation.clone(),
    )
    .await
    .expect("queue the prompt");
    let reply = TerminalOperations::terminal_withdraw_startup_prompt(
        state.clone(),
        "session-1".into(),
        "startup-edit me".into(),
    )
    .await
    .expect("withdraw the queued prompt");
    assert!(reply, "{reply:?}");

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

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("the edited prompt"),
        cancellation.clone(),
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
    let cancellation = CancellationToken::new();

    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("already going"),
        cancellation.clone(),
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

    let answer = TerminalOperations::terminal_withdraw_startup_prompt(
        state.clone(),
        "session-1".into(),
        "startup-already going".into(),
    )
    .await
    .expect("the daemon answers a late withdrawal");
    assert!(
        !answer,
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
    let started = state
        .start_destroy_stopped_session(session.id.clone(), BranchDisposition::Keep)
        .await
        .unwrap();
    RuntimeState::wait_lifecycle_result(started.result)
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

pub(crate) async fn start_test_http_api(
    state: Arc<RuntimeState>,
) -> (
    mj_client::http::HttpDaemonClient,
    CancellationToken,
    tokio::task::JoinHandle<Result<()>>,
) {
    let remote = spawn_remote_session_manager().unwrap();
    let (updates, delegation_updates) = mj_controller::session_manager::delegation_channel();
    let shutdown = CancellationToken::new();
    let (services, delegation) = super::delegation::spawn(
        state.clone(),
        remote.control.clone(),
        delegation_updates,
        shutdown.clone(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let token = "direct-test-token";
    let (_options, task) = crate::server_runtime::start_api(
        listener,
        token.into(),
        shutdown.clone(),
        remote.control.clone(),
        state,
        services,
    )
    .await
    .unwrap();
    let server = mj_core::runtime::spawn(async move {
        let _remote = remote;
        let _updates = updates;
        let result = task.await?;
        delegation.await??;
        result
    });
    let http = mj_client::http::HttpDaemonClient::new(format!("http://{address}"), token).unwrap();
    (http, shutdown, server)
}

// Exercise the HTTP routes and the real runtime without starting unrelated
// viewer maintenance jobs that also publish runtime revisions.
pub(super) async fn start_test_dashboard_http_api(
    state: Arc<RuntimeState>,
) -> (
    mj_client::http::HttpDaemonClient,
    CancellationToken,
    tokio::task::JoinHandle<Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_test_dashboard_http_api_on(state, listener).await
}

pub(crate) async fn start_test_dashboard_http_api_on(
    state: Arc<RuntimeState>,
    listener: TcpListener,
) -> (
    mj_client::http::HttpDaemonClient,
    CancellationToken,
    tokio::task::JoinHandle<Result<()>>,
) {
    let address = listener.local_addr().unwrap();
    let token = "direct-test-token";
    let shutdown = CancellationToken::new();
    let (action_tx, _actions) = tokio::sync::mpsc::channel(1);
    let mut options = crate::server::ServerOptions::new(
        address,
        state.subscribe_browser_views().unwrap(),
        state.browser_conversations(),
        crate::server::ServerRequests { action_tx },
        crate::server::BrowserSource {
            revisions: state.revisions(),
            views: Arc::new({
                let state = state.clone();
                move |cursor| state.browser_views(cursor)
            }),
        },
    )
    .unwrap();
    options.shutdown = shutdown.clone();
    options.set_loopback_token(token.into());
    options.set_api_token("public-test-token".into());
    options.set_subagent_backend(Arc::new(crate::server_runtime::api::ApiBackend::new(
        state.session_manager.client(),
        Arc::new(|_| None),
        state,
    )));
    let task = mj_core::runtime::spawn(crate::server::run_server_on_listener(options, listener));
    let http = mj_client::http::HttpDaemonClient::new(format!("http://{address}"), token).unwrap();
    (http, shutdown, task)
}

/// A runtime whose controller comes from the real store, for the tests that
/// exercise a daemon operation end to end in an isolated data directory.
pub(crate) fn test_runtime_state_loading_the_store() -> Arc<RuntimeState> {
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
            create::CreateInputs {
                child: None,
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

#[tokio::test]
async fn a_reviewer_create_failure_settles_its_open_review() {
    use mj_core::review::driver::TurnReviewSeed;
    use mj_review::driver::TurnReviewDriver;

    const NAME: &str = "a_reviewer_create_failure_settles_its_open_review";
    const CHILD: &str = "MJ_TEST_REVIEWER_CREATE_FAILURE";
    let Ok(failure) = std::env::var(CHILD) else {
        for failure in ["create", "configure", "rejecting"] {
            let directory = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), NAME),
            )
            .env(CHILD, failure)
            .env("MJ_INSTANCE", "slice-b-reviewer-failure")
            .isolated_store(directory.path())
            .run();
        }
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let create_failed = failure == "create";
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Reviewer create failure").unwrap();
    let parent = runtime_test_session("review-parent", &workspace.id, SessionState::Running);
    let mut child = runtime_test_session(
        "review-child",
        &workspace.id,
        if create_failed {
            SessionState::Provisioning
        } else {
            SessionState::Disconnected
        },
    );
    let mut relation = runtime_test_subagent(&child.id, &parent.id);
    relation.kind = mj_core::subagent::SubagentKind::Reviewer;
    relation.model = Some("chosen-model".into());
    mj_controller::database::save_session(&parent).unwrap();
    // A missing target makes Create fail before launching a worker.
    mj_controller::database::register_subagent_with_startup(
        &child,
        &relation,
        vec![],
        None,
        None,
        false,
    )
    .unwrap();
    let review_id = "review-create-failed";
    let (mut driver, _) = TurnReviewDriver::start(
        TurnReviewSeed {
            task: "review the changes".into(),
            user_messages: Vec::new(),
            declined: Vec::new(),
            brief: None,
        },
        review_id.into(),
        std::num::NonZeroUsize::new(1).unwrap(),
    );
    driver.delta_captured(vec![mj_core::relay::RepoDelta {
        root: directory.path().into(),
        current_tree: "newtree".into(),
        patch: "+change".into(),
        changed_lines: 1,
        ..Default::default()
    }]);
    mj_controller::database::save_turn_review_state(
        &parent.id,
        &mj_controller::database::TurnReviewState {
            open_reviews: vec![mj_core::storage::OpenReview {
                active: Some(review_id.into()),
                active_group: Some(mj_core::storage::ActiveReviewGroup {
                    brief_writer: None,
                    driver: driver.snapshot(),
                    reviewers: vec![Default::default()],
                    registrations: vec![mj_core::storage::ReviewerRegistration {
                        role: "reviewer-1".into(),
                        request_key: relation.request_key.clone(),
                        child_session_id: Some(child.id.clone()),
                    }],
                }),
            }],
            ..Default::default()
        },
    )
    .unwrap();

    let state = test_runtime_state_loading_the_store();
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.review_host().view(&parent.id).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the open review is restored");
    if create_failed {
        state
            .start_create_lifecycle(
                child.id.clone(),
                LifecycleControl::default(),
                create::CreateInputs {
                    child: Some(relation),
                    publication: None,
                    remember_launch_choices: false,
                },
                None,
            )
            .await
            .unwrap();
    } else if failure == "rejecting" {
        mj_controller::controller::recover_registered_subagent_startup(&relation, false).unwrap();
        let configure = mj_controller::database::next_startup_delivery(&child.id)
            .unwrap()
            .unwrap();
        let connection =
            rusqlite::Connection::open(mj_controller::database::database_path()).unwrap();
        connection
            .execute(
                "UPDATE startup_steps SET phase='rejecting',error='provider refused the selected model' WHERE command_id=?1",
                [&configure.command_id],
            )
            .unwrap();
        let rejecting = mj_controller::database::next_startup_delivery(&child.id)
            .unwrap()
            .unwrap();
        assert_eq!(rejecting.phase, "rejecting");
        state.start_persisted_startup_delivery(rejecting, &CancellationToken::new());
        // A pre-upgrade `rejecting` row must still confirm worker exit before
        // settling the open review.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            mj_controller::database::finished_review(&parent.id, Some(review_id))
                .unwrap()
                .is_none(),
            "persisted rejecting row must not settle a reviewer before worker exit is confirmed"
        );
        child.state = SessionState::Error;
        child.last_error = Some("provider refused the selected model".into());
        mj_controller::database::save_session(&child).unwrap();
    } else {
        use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, RelayCommand};
        use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};

        mj_controller::controller::recover_registered_subagent_startup(&relation, false).unwrap();
        let configure = mj_controller::database::next_startup_delivery(&child.id)
            .unwrap()
            .unwrap();
        let mut worker = mj_worker::relay::DurableRelay::open_for_harness(
            directory.path(),
            &child.id,
            "test",
            mj_core::config::HarnessKind::Codex,
            &[
                mj_core::worker_launch::InitialWorkerCommand {
                    command_id: configure.command_id.clone(),
                    command: RelayCommand::SetConfig {
                        key: "model".into(),
                        value: "chosen-model".into(),
                    },
                    optional: false,
                },
                mj_core::worker_launch::InitialWorkerCommand {
                    command_id: mj_controller::database::startup_step_id(
                        configure.group_id.as_deref().unwrap(),
                        "prompt",
                    ),
                    command: RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(
                            relation.initial_prompt,
                        ))],
                    },
                    optional: false,
                },
            ],
        )
        .unwrap();
        worker.claim_pending_commands(true).unwrap();
        worker
            .record_command_rejected(
                &configure.command_id,
                mj_core::event_outcome::OutcomeReason::CommandFailed,
                "provider refused the selected model",
            )
            .unwrap_err();
        let events = worker.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        let child_id = child.id.clone();
        mj_controller::database::apply_projection_page(&child.id, move |page| {
            let mut current = mj_core::state::MaterializedSession::empty(&child_id);
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
        state.start_persisted_startup_delivery(configure, &CancellationToken::new());
        // While the reviewer worker is disconnected and not confirmed stopped,
        // startup rejection stays pending instead of settling the open review.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            mj_controller::database::finished_review(&parent.id, Some(review_id))
                .unwrap()
                .is_none(),
            "disconnected reviewer must not settle its review before worker exit is confirmed"
        );
        child.state = SessionState::Error;
        child.last_error = Some("provider refused the selected model".into());
        mj_controller::database::save_session(&child).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(review) =
                mj_controller::database::finished_review(&parent.id, Some(review_id)).unwrap()
            {
                assert_eq!(
                    review.resolution,
                    mj_core::storage::FinishedReviewResolution::Failed
                );
                assert_eq!(review.roles.len(), 1);
                assert!(review.roles[0].handback.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{failure} failure must reach the review host and settle its role"));
    let record = mj_controller::database::load_session_record(&child.id)
        .unwrap()
        .unwrap();
    if create_failed {
        assert_eq!(record.state, SessionState::Error);
        assert!(record.last_error.is_some());
    } else {
        let steps = mj_controller::database::load_latest_startup_group(&child.id).unwrap();
        assert!(steps.iter().all(|step| step.phase == "failed"));
        state.cancel_and_join_startup_prompts().await.unwrap();
    }
    state.review_host().shutdown().await.unwrap();
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
    let create = |title: &str| mj_client::daemon::CreateSessionRequest {
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
        provider: None,
        bundle_id: "project".into(),
        project_directory: Some(project.clone()),
        target_template_id: "localhost".into(),
        additional_mounts: Vec::new(),
        resource_allocation: None,
        title: title.into(),
        session_title_override: None,
    };
    let registered = |reply: mj_client::daemon::RegisteredSession| reply.session.id;
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
            state.start_create_session(create(title)).await.unwrap(),
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
        state.start_create_session(create("fourth")).await.unwrap(),
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
    let (http, http_shutdown, http_server) = start_test_dashboard_http_api(state.clone()).await;
    mj_controller::upgrade::gate().begin_draining();
    http.cancel_lifecycle(sessions[0].clone()).await.unwrap();
    let outcome = tokio::time::timeout(BOUND, http.wait_create_session(sessions[0].clone()))
        .await
        .expect("a cancelled launch finishes promptly");
    let error = outcome.expect_err("a cancelled launch must not succeed");
    assert!(error.is::<mj_client::daemon::DaemonRefusal>(), "{error:#}");
    mj_controller::upgrade::gate().expire_drain_for_test();
    http_shutdown.cancel();
    http_server.await.unwrap().unwrap();
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
    seed_legacy_detached_draft(
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
    state.close_workspace(workspace.id.clone()).await.unwrap();
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
    let (http, shutdown, server) = start_test_http_api(state.clone()).await;
    let workspace = http.create_workspace("Before".into()).await.unwrap();
    assert_eq!(state.workspaces().borrow()[0].id, workspace.id);

    http.rename_workspace(workspace.id.clone(), "After".into())
        .await
        .unwrap();
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    seed_legacy_detached_draft(
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
    seed_legacy_detached_draft(&workspace.id, None, "terminal", Some(7), "keep me").unwrap();
    let state = test_runtime_state_loading_the_store();
    // Each stop stands in for a close the workspace close joins: one already
    // committed and still removing its target, one still checkpointing.
    let committed = LifecycleControl::default();
    let finish_teardown = Arc::new(tokio::sync::Notify::new());
    let key = serde_json::to_string(&mj_core::suspend_consent::SuspendConsent::default()).unwrap();
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
    let (http, shutdown, server) = start_test_http_api(state.clone()).await;
    let http = Arc::new(http);
    let mut closing = mj_core::runtime::spawn({
        let http = http.clone();
        let id = workspace.id.clone();
        async move { http.close_workspace(id).await }
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
    assert!(
        !mj_controller::upgrade::gate()
            .blockers()
            .iter()
            .any(|blocker| blocker.label == "workspace close"),
        "workspace close must release its handoff admission once member suspensions are dispatched"
    );
    http.cancel_workspace_close(workspace.id.clone())
        .await
        .unwrap();
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
        error
            .downcast_ref::<mj_client::http::HttpApiError>()
            .unwrap()
            .message,
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let (http, shutdown, server) = start_test_dashboard_http_api(state).await;
    // No periodic config refresher runs in this fixture: the save itself must
    // install the preference before acknowledging it to the terminal.
    for size in [
        mj_core::workspace::PaneSize::Minimized,
        mj_core::workspace::PaneSize::Standard,
    ] {
        http.save_sidebar_size(size).await.unwrap();
        assert_eq!(Config::load().unwrap().sidebar_size, size);
        let startup = http
            .dashboard_startup("reopened-dashboard".into(), "default".into())
            .await
            .unwrap();
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

// Hard-won: 0bd61a289: dashboard startup failed when live session state exceeded one frame.
#[tokio::test]
async fn dashboard_startup_larger_than_a_frame_arrives_whole() {
    let Some(_writer) =
        startup_prompt_test_store("dashboard_startup_larger_than_a_frame_arrives_whole")
    else {
        return;
    };
    let mut session = runtime_test_session("large-draft", "default", SessionState::Running);
    session.draft_input = "draft 🙂\n".repeat(MAX_FRAME_BYTES / 8);
    let expected = session.draft_input.clone();
    assert!(expected.len() > MAX_FRAME_BYTES);
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state_loading_the_store();
    let (http, shutdown, server) = start_test_dashboard_http_api(state).await;
    let startup = http
        .dashboard_startup("large-dashboard".into(), "default".into())
        .await
        .unwrap();
    assert_eq!(
        startup.runtime.projection.records["large-draft"].draft_input,
        expected
    );
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn oversized_response_reports_its_size_and_keeps_connection_usable() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = mj_core::runtime::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        for (request_id, result) in [
            (
                7,
                Ok(DaemonReply::UpgradeBlockers(vec![
                    "x".repeat(MAX_FRAME_BYTES),
                ])),
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
    let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert_eq!(response.request_id, 7);
    let error = response.result.unwrap_err();
    assert!(
        error.contains("upgrade_blockers response for request 7"),
        "{error}"
    );
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
    let pending = LifecycleFailure::of(
        &anyhow::anyhow!("private SSH diagnostic")
            .context(mj_controller::controller::LifecycleFailureReason::SuspendStopPending),
    );
    state
        .record_failed_close(&session.id, "lifecycle-pending-stop", &pending)
        .await;
    let retained = mj_controller::database::read_durable_session_record(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, SessionState::Closing);
    let notice = retained.public_error().unwrap();
    assert!(notice.contains("Retry Suspend"), "{notice}");
    assert!(notice.contains("lifecycle-pending-stop"), "{notice}");
    assert!(!notice.contains("private SSH diagnostic"), "{notice}");
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
    for (index, (in_place, restart, sealed, has_prepared_destination, checkout_only, origin)) in [
        (false, false, true, false, false, Origin::Suspend),
        (true, false, true, false, false, Origin::Suspend),
        (false, true, true, true, false, Origin::Suspend),
        (true, true, true, false, false, Origin::Suspend),
        (false, false, false, false, false, Origin::Suspend),
        (false, true, false, false, false, Origin::Suspend),
        (false, true, true, true, false, Origin::Cancel),
        (false, true, true, true, false, Origin::StaleCancel),
        (false, false, true, true, false, Origin::Suspend),
        // Crash after creating a destination clone and binding its ledger
        // environment, before runtime provisioning sets `destination.target`.
        (false, true, true, false, true, Origin::Suspend),
    ]
    .into_iter()
    .enumerate()
    {
        let requested_close = origin == Origin::Suspend;
        let root = tempfile::tempdir().unwrap();
        let id = format!("1542{:028}", index + 1);
        let id = id.as_str();
        let source_root = root.path().join("source").join(id);
        let destination_repo = root.path().join("destination-repo");
        let mut destination_root = if checkout_only {
            destination_repo.join(".mj").join("clones").join(id)
        } else {
            root.path().join("destination").join(id)
        };
        std::fs::create_dir_all(&source_root).unwrap();
        if checkout_only {
            std::fs::create_dir_all(&destination_repo).unwrap();
        }
        if !in_place {
            std::fs::create_dir_all(&destination_root).unwrap();
            std::fs::write(
                destination_root.join("clone.partial"),
                "unfinished checkout",
            )
            .unwrap();
        }
        std::fs::write(
            source_root.join("uncheckpointed-work"),
            "keep source workspace",
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
            prepared_destination: has_prepared_destination.then(|| {
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
            transfer_route: None,
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
            granted_keys: Vec::new(),
            granted_ec2_rules: Vec::new(),
            in_place_expected_store_id: None,
            in_place_continuation_command_id: None,
            in_place_held_command_ids: Vec::new(),
            in_place_harness_restarted: false,
            deferred_environment_changes: Vec::new(),
            source_snapshot: None,
            source_checkpoint_only: false,
            in_place,
            operation_id: format!("mid-clone-{id}"),
            selection: mj_core::state::MoveSelection {
                session_id: id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("local".into()),
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
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
            phase: mj_core::state::MovePhase::Restoring,
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
        if checkout_only {
            destination.target = None;
            destination.project_directory = Some(destination_root.clone());
            destination.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
                kind: mj_core::state::ManagedCheckoutKind::Clone,
                source_project_directory: destination_repo.clone(),
                source_repository: destination_repo.clone(),
                worktree_root: destination_root.clone(),
                branch: format!("mj/{id}"),
                target: mj_core::state::ManagedWorktreeTarget::Local,
                base_commit: None,
            }));
        } else if !in_place {
            destination.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
                worker_root: destination_root.clone(),
            }));
        }
        mj_controller::database::save_resumed_session(&destination, None).unwrap();
        if restart {
            operation.phase = mj_core::state::MovePhase::Cancelled;
            operation.cancellation_requested = true;
            mj_controller::database::save_move_operation(&operation).unwrap();
            destination.state = SessionState::Closing;
            mj_controller::database::save_lifecycle_session(&destination).unwrap();
        }
        if requested_close && restart && sealed && !in_place && !checkout_only {
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
            let mut committed = saved_move.clone();
            committed.phase = mj_core::state::MovePhase::DestinationOwns;
            mj_controller::database::save_move_operation(&committed).unwrap();
            assert!(
                !controller
                    .settle_superseded_move_for_close(
                        id,
                        &RefusingExecutor("a committed destination must never be rolled back"),
                        None,
                        mj_controller::controller::lifecycle::SuspendSettlement::Suspend,
                    )
                    .await
                    .unwrap()
            );
            mj_controller::database::save_move_operation(&saved_move).unwrap();
            assert!(
                !controller
                    .settle_superseded_move_for_close(
                        id,
                        &RefusingExecutor("a ready destination must never be rolled back"),
                        None,
                        mj_controller::controller::lifecycle::SuspendSettlement::Suspend,
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
                    None,
                )
                .unwrap();
            let teardown = test_runtime_state_loading_the_store();
            let (teardown_moves, close_operations) =
                mj_controller::database::load_move_recovery().unwrap();
            let owners = teardown
                .recover_moves(teardown_moves, &close_operations, &BTreeSet::new())
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
        if requested_close && restart && has_prepared_destination {
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
                    &operation.operation_id,
                    true,
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
                .recover_moves(operations, &close_operations, &BTreeSet::new())
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
            assert_ne!(kept.state, SessionState::Stopped);
            assert_eq!(kept.checkpoint.as_deref(), Some(&checkpoint));
            assert!(source_root.join("uncheckpointed-work").exists());
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
                .recover_moves(operations, &close_operations, &BTreeSet::new())
                .await
                .unwrap();
            assert!(
                !owned.moves.contains(id),
                "startup must leave superseded Move to close"
            );
            assert!(owned.closes.contains(id));
            if !checkout_only {
                assert!(
                    interrupted_suspend_session_ids(&Controller::load().unwrap())
                        .contains(&id.to_owned())
                );
            }
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
        assert!(
            result.is_err(),
            "the source needs repair before it can suspend ({id})"
        );
        if let Some(active_move) = active_move {
            assert!(
                RuntimeState::wait_lifecycle_result(active_move)
                    .await
                    .is_err()
            );
        }
        let stored = mj_controller::database::load_state().unwrap();
        let record = &stored.sessions[id];
        assert_eq!(record.state, SessionState::Error, "{id}");
        assert!(checkpoint.archive_path.exists());
        assert_eq!(record.checkpoint.as_deref(), Some(&checkpoint));
        assert_eq!(
            destination_root.exists(),
            !sealed && !in_place,
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
        if has_prepared_destination {
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
        } else {
            assert_eq!(record.target, source.target);
            assert_eq!(
                std::fs::read_to_string(source_root.join("uncheckpointed-work")).unwrap(),
                "keep source workspace"
            );
            let message = record.public_error().unwrap();
            assert!(message.contains("source workspace was kept"), "{message}");
            assert!(message.contains("Resume the session"), "{message}");
            assert!(message.contains("Reference "), "{message}");
            let environment = mj_controller::database::session_environment(id)
                .unwrap()
                .unwrap();
            assert_eq!(
                environment.placement.target,
                source.target.as_deref().cloned()
            );
            assert_eq!(
                environment.phase,
                mj_controller::database::EnvironmentPhase::Active
            );
            if !in_place {
                let environments = mj_controller::database::worker_environments(id).unwrap();
                let provisional = environments
                    .iter()
                    .find(|environment| {
                        environment.placement.target.as_ref() == destination.target.as_deref()
                    })
                    .expect("the provisional destination has a ledger entry");
                assert_eq!(
                    provisional.phase,
                    mj_controller::database::EnvironmentPhase::Reclaimed
                );
            }
        }
    }
    // Historical Moves must not turn a workerless cleanup into interrupted
    // checkpoint recovery merely because their rows are still retained.
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
                    let error = match refused
                        .start_suspend_session(
                            id.clone(),
                            mj_core::suspend_consent::SuspendConsent::default(),
                        )
                        .await
                    {
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
            let historical_archive =
                mj_controller::controller::test_support::write_checkpoint_gate_archive(
                    root.path(),
                    &id,
                    7,
                );
            let mut completed = retained_startup_test_move(&record);
            completed.operation_id = format!("historical-{id}");
            completed.phase = phase;
            completed.queue_admission_started = false;
            completed.queue_admission_finished = true;
            completed.cancellation_requested = phase == mj_core::state::MovePhase::Cancelled;
            completed.prepared_destination = None;
            completed.checkpoint = Some(historical_archive);
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
                    .recover_moves(vec![completed.clone()], &close_ids, &BTreeSet::new())
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
                        .start_suspend_session_controlled(
                            id.clone(),
                            mj_core::suspend_consent::SuspendConsent::default(),
                            cancelled,
                        )
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
            if lifecycle == SessionState::Parked {
                // Placed Parked owners now checkpoint on ordinary suspend instead of
                // settling workerlessly, while explicit without-checkpoint suspend still
                // cleans up without tripping interrupted Move recovery.
                let parked = mj_controller::database::load_session_record(&id)
                    .unwrap()
                    .unwrap();
                assert_eq!(close_route(&parked, true), CloseRoute::Graceful);
                tokio::time::timeout(
                    Duration::from_secs(30),
                    runtime.suspend_session_with_consent(
                        id.clone(),
                        mj_core::suspend_consent::SuspendConsent {
                            without_checkpoint: true,
                            confirm_no_checkpoint: true,
                            ..Default::default()
                        },
                    ),
                )
                .await
                .unwrap()
                .unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(30), runtime.suspend_session(id.clone()))
                    .await
                    .unwrap()
                    .unwrap();
            }
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
    parent.project = Some(Arc::new(mj_core::repository::ProjectBundleSnapshot {
        bundle: mj_controller::controller::test_support::local_bundle(&checkout),
        identities: BTreeMap::from([(
            "project".into(),
            mj_core::repository::RepositoryIdentity::Local(checkout.clone()),
        )]),
        network_sources: BTreeMap::new(),
        plain_directories: BTreeSet::from(["project".into()]),
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
/// stand-in worker of the quiesced clone fixture, because unit
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
    origin: PathBuf,
    unpushed_commit: String,
    relay_root: PathBuf,
    channels: mj_controller::session_manager::SessionManagerChannels,
    state: Arc<RuntimeState>,
}

#[cfg(unix)]
impl LiveClone {
    /// Recover suspension, then restore the checkpoint into a clean origin seed.
    async fn suspend_and_restore(&self, workspace: &Path) -> PathBuf {
        use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
        use mj_controller::controller::test_support::test_git;

        tokio::time::timeout(
            Duration::from_secs(60),
            self.state.recover_suspension(LATCH_RELAY_SESSION.into()),
        )
        .await
        .unwrap()
        .unwrap();
        let record = mj_controller::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Stopped, "{record:?}");
        assert!(record.target.is_none());
        assert!(relay_sealed(&self.relay_root));
        assert!(
            !self.checkout.exists(),
            "suspend must release the original clone"
        );

        let checkpoint = record.checkpoint.unwrap();
        let archive =
            mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path).unwrap();
        let [repository] = archive.repositories.as_slice() else {
            panic!("expected a single archived repository");
        };
        assert!(repository.metadata.origin_delta.is_some());
        let restored = workspace.join(&repository.metadata.relative_destination);
        std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
        test_git(
            workspace,
            &[
                "clone",
                self.origin.to_str().unwrap(),
                restored.to_str().unwrap(),
            ],
        );
        mj_checkpoint::checkpoint::restore_repositories(
            &checkpoint.archive_path,
            workspace,
            &mj_checkpoint::archive::SystemGit,
        )
        .unwrap();
        restored
    }
}

/// A managed clone with an unpublished commit. Suspend recovery starts after
/// the durable quiescence receipt and checkpoint-only worker boot. The relay
/// is a stand-in, while capture runs the real worker exporter.
#[cfg(unix)]
async fn quiesced_clone_with_an_unpushed_commit() -> LiveClone {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    use mj_controller::controller::test_support::{committed_repository, test_git};
    use mj_core::worker_suspend as wire;
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
             [ \"$2\" = liveness ] && {{ printf 'alive\\n'; exit 0; }}\n\
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
    session.state = SessionState::Closing;
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

    let controller = Controller::load().unwrap();
    let backend = mj_controller::targets::backend_locator(
        session.target.as_deref().unwrap(),
        &session,
        &controller.config,
    )
    .unwrap();
    let mut launch = mj_controller::worker_launch::current_worker_launch_config(
        &controller.config,
        &controller.state,
        LATCH_RELAY_SESSION,
        &backend,
    )
    .unwrap();
    launch.run_mode = mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
    std::fs::write(
        worker_root.join("launch.json"),
        serde_json::to_vec(&launch).unwrap(),
    )
    .unwrap();
    let operation = new_command_id("test-suspend").unwrap();
    wire::write(&worker_root, wire::ACCEPTED, &operation).unwrap();
    wire::write(
        &worker_root,
        wire::REQUEST,
        &wire::SuspendRequest {
            operation: operation.clone(),
            // A process with a different birth identity cannot be the
            // original quiesced worker, even when its PID has been reused.
            worker: wire::WorkerIdentity {
                pid: std::process::id(),
                start: 0,
            },
            checkout: checkout.clone(),
            child_sessions: Vec::new(),
        },
    )
    .unwrap();
    wire::write(
        &worker_root,
        wire::RECEIPT,
        &wire::SuspendReceipt {
            operation,
            error: None,
        },
    )
    .unwrap();

    let (channels, state) = serve_live_session(&relay_root, false).await;
    LiveClone {
        _source: source,
        _directory: directory,
        checkout,
        origin,
        unpushed_commit,
        relay_root,
        channels,
        state,
    }
}

/// Suspend must preserve commits available only in the session clone.
#[cfg(unix)]
#[tokio::test]
async fn suspend_recovery_preserves_a_clones_unpushed_work() {
    use mj_controller::controller::test_support::test_git;
    if !in_isolated_live_parent_test("suspend_recovery_preserves_a_clones_unpushed_work") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let clone = quiesced_clone_with_an_unpushed_commit().await;
    let workspace = tempfile::tempdir().unwrap();
    let restored = clone.suspend_and_restore(workspace.path()).await;
    assert_eq!(
        test_git(&restored, &["rev-parse", "HEAD"]),
        clone.unpushed_commit
    );
    assert_eq!(
        std::fs::read_to_string(restored.join("session.txt")).unwrap(),
        "work\n"
    );
    clone.channels.shutdown.shutdown().await.unwrap();
}

/// A published clone can suspend with private review refs. Durable review
/// refs return on restore, while the obsolete review-baseline pin is omitted.
#[cfg(unix)]
#[tokio::test]
async fn suspend_recovery_preserves_a_published_clones_durable_review_refs() {
    use mj_controller::controller::test_support::test_git;
    if !in_isolated_live_parent_test(
        "suspend_recovery_preserves_a_published_clones_durable_review_refs",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let clone = quiesced_clone_with_an_unpushed_commit().await;
    test_git(
        &clone.checkout,
        &["push", "origin", "HEAD:refs/heads/session"],
    );
    let tree = test_git(&clone.checkout, &["rev-parse", "HEAD^{tree}"]);
    let reviewed = format!("refs/mj/reviewed/{tree}");
    test_git(
        &clone.checkout,
        &["update-ref", "refs/mj/review-baseline", &tree],
    );
    test_git(
        &clone.checkout,
        &["update-ref", &reviewed, &clone.unpushed_commit],
    );

    let workspace = tempfile::tempdir().unwrap();
    let restored = clone.suspend_and_restore(workspace.path()).await;
    assert_eq!(
        test_git(&restored, &["rev-parse", &reviewed]),
        clone.unpushed_commit
    );
    assert!(test_git(&restored, &["for-each-ref", "refs/mj/review-baseline"]).is_empty());
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

    let closed_id = "closed-borrower";
    let workspace = &parent
        .state
        .session_record(LATCH_RELAY_SESSION)
        .unwrap()
        .workspace_id;
    let closed = runtime_test_session(closed_id, workspace, SessionState::Stopped);
    mj_controller::database::save_subagent_session(
        &closed,
        &runtime_test_subagent(closed_id, LATCH_RELAY_SESSION),
    )
    .unwrap();
    let data = mj_core::config::data_dir();
    mj_controller::database::tool_details::append_tool_detail(&data, closed_id, "detail", "{}")
        .unwrap();

    let failure = tokio::time::timeout(
        Duration::from_secs(60),
        parent.state.suspend_session(LATCH_RELAY_SESSION.to_owned()),
    )
    .await
    .unwrap();
    assert!(failure.is_err());

    let stored = mj_controller::database::load_state().unwrap();
    assert!(stored.sessions.contains_key(closed_id));
    assert!(data.join("tool-details/closed-borrower.mjtd").exists());
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
    assert!(!stored.sessions.contains_key(stopped_id));
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
        .start_session_lifecycle(SessionLifecycleRequest::Suspend {
            session_id: LATCH_RELAY_SESSION.into(),
            discard_untracked: mj_core::suspend_consent::UntrackedConsent::None,
            abort_operations: mj_core::suspend_consent::OperationConsent::None,
            without_checkpoint: true,
            confirm_no_checkpoint: false,
        })
        .await
        .unwrap_err();
    assert!(format!("{rejected:#}").contains("No verified checkpoint exists"));
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
            .start_session_lifecycle(SessionLifecycleRequest::Suspend {
                session_id: LATCH_RELAY_SESSION.into(),
                discard_untracked: mj_core::suspend_consent::UntrackedConsent::None,
                abort_operations: mj_core::suspend_consent::OperationConsent::None,
                without_checkpoint: true,
                confirm_no_checkpoint: false,
            }),
    )
    .await
    .expect("Discard acceptance waited for child teardown")
    .unwrap();
    let accepted = mj_controller::database::load_state().unwrap();
    assert_eq!(
        accepted.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Closing
    );
    assert_eq!(
        mj_controller::database::load_closing_consent(LATCH_RELAY_SESSION).unwrap(),
        Some(mj_core::suspend_consent::SuspendConsent {
            without_checkpoint: true,
            ..Default::default()
        })
    );
    assert_eq!(
        accepted.sessions[LATCH_RELAY_SESSION].checkpoint.as_ref(),
        Some(&checkpoint)
    );
    assert!(accepted.sessions.contains_key(WORKING_CHILD));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !parent.state.owner().lifecycle[LATCH_RELAY_SESSION]
            .control
            .is_committed()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Suspend never committed to stopping the held child");
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
        LifecycleCompletion::Done
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
async fn accepted_suspend_failure_is_published_without_an_observer() {
    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION;
    const NAME: &str = "accepted_suspend_failure_is_published_without_an_observer";
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
                serde_json::to_string(&(LifecycleKind::StopSubagent.label(), false, false))
                    .unwrap(),
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
        .admit_session_lifecycle(SessionLifecycleRequest::Suspend {
            session_id: LATCH_RELAY_SESSION.into(),
            discard_untracked: mj_core::suspend_consent::UntrackedConsent::None,
            abort_operations: mj_core::suspend_consent::OperationConsent::None,
            without_checkpoint: false,
            confirm_no_checkpoint: false,
        })
        .await
        .unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(30), started.confirm_acceptance())
        .await
        .unwrap()
        .unwrap();
    let accepted =
        mj_controller::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(accepted.state, SessionState::Closing);
    assert_eq!(accepted.checkpoint.as_ref(), Some(&checkpoint));
    drop(started);
    tokio::time::timeout(Duration::from_secs(30), async {
        while acceptance.receiver_count() <= baseline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Suspend never joined the held child stop");
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
    .expect("Suspend failure was not persisted without an observer");
    let public = retained.public_error().unwrap();
    assert!(public.starts_with(mj_core::state::CLOSE_FAILURE_PREFIX));
    assert!(public.contains(&receipt.operation_id));
    assert!(!public.contains("held child teardown diagnostic"));
    assert_eq!(retained.state, SessionState::Running);
    assert!(!relay_sealed(&parent.relay_root));
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
        matches!(outcome, LifecycleCompletion::LifecycleFailed(detail) if detail.contains("held child teardown diagnostic"))
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
    let mut closing = mj_controller::database::load_session_record(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    let checkpoint = closing.checkpoint.clone().unwrap();
    let consent = mj_core::suspend_consent::SuspendConsent {
        without_checkpoint: true,
        ..Default::default()
    };
    closing.state = SessionState::Closing;
    mj_controller::database::save_suspension_intent(&closing, None, Some(&consent)).unwrap();
    let durable = Controller::load().unwrap();
    assert_eq!(
        durable.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Closing
    );
    assert_eq!(
        mj_controller::database::load_closing_consent(LATCH_RELAY_SESSION).unwrap(),
        Some(consent)
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
        restarted.recover_suspension(LATCH_RELAY_SESSION.into()),
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
    let closed_id = "closed-borrower";
    let closed = runtime_test_session(closed_id, &workspace.id, SessionState::Stopped);
    mj_controller::database::save_subagent_session(
        &closed,
        &runtime_test_subagent(closed_id, parent_id),
    )
    .unwrap();
    let item = Arc::new(mj_core::transcript::TranscriptItem {
        stable_id: "message".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: mj_core::transcript::TranscriptBody::System {
            text: "closed child history".into(),
        },
    });
    let mut projection = mj_core::state::MaterializedSession::empty(closed_id);
    projection.applied_event_ordinal = 1;
    projection.applied_event_digest = format!("{:064x}", 1);
    projection.transcript.push(item.clone());
    mj_controller::database::save_materialized_session(&projection).unwrap();
    let data_dir = mj_core::config::data_dir();
    mj_controller::database::tool_details::append_tool_detail(
        &data_dir,
        closed_id,
        &item.stable_id,
        &serde_json::to_string(&item.body).unwrap(),
    )
    .unwrap();
    let detail = data_dir
        .join("tool-details")
        .join(format!("{closed_id}.mjtd"));
    sessionwiki::index::open().unwrap();

    let state = test_runtime_state_loading_the_store();
    let mut view = ready_startup_view();
    let snapshot = view.snapshot.as_mut().unwrap();
    snapshot.window = mj_core::state::ProjectionWindow::of(&projection);
    snapshot.materialized = projection;
    state.owner().publish_view(closed_id.into(), view);
    let mut replica = mj_client::runtime_feed::RuntimeReplica::default();
    replica
        .apply(state.runtime_changes(None, false).await.unwrap())
        .unwrap();
    let cursor = replica.cursor.clone().unwrap();
    assert!(matches!(
        state.session_tail(closed_id, &cursor),
        mj_client::runtime_feed::SessionTailReply::Tail { .. }
    ));
    assert!(detail.is_file());
    tokio::time::timeout(
        Duration::from_secs(60),
        state.suspend_session(parent_id.to_owned()),
    )
    .await
    .unwrap()
    .unwrap();

    replica
        .apply(state.runtime_changes(Some(cursor), false).await.unwrap())
        .unwrap();
    assert!(matches!(
        state.session_tail(closed_id, replica.cursor.as_ref().unwrap()),
        mj_client::runtime_feed::SessionTailReply::NoTail
    ));
    assert!(state.session_record(closed_id).is_none());
    assert!(!state.owner().sessions.contains_key(closed_id));
    assert!(!state.owner().transcripts.contains_key(closed_id));
    assert!(
        mj_controller::database::load_materialized_session(closed_id)
            .unwrap()
            .is_none()
    );
    assert!(!detail.exists());

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
    assert!(!worker_root.exists());
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
        assert!(!worker_root.exists());
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
    let profile = mj_controller::targets::local_nested_profile_root(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&profile).unwrap();
    let detail_data = mj_core::config::data_dir();
    let detail_body = mj_core::transcript::TranscriptBody::System {
        text: "spilled child detail".into(),
    };
    let (offset, len) = mj_controller::database::tool_details::append_tool_detail(
        &detail_data,
        LATCH_RELAY_SESSION,
        "detail",
        &serde_json::to_string(&detail_body).unwrap(),
    )
    .unwrap();
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
        assert!(!worker_root.exists());
        assert!(!profile.exists());
        assert_eq!(
            mj_controller::database::tool_details::read_tool_detail(
                &detail_data,
                LATCH_RELAY_SESSION,
                "detail",
                offset,
                len,
            )
            .unwrap(),
            detail_body,
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
        transfer_route: None,
        consent: Default::default(),
        skip_keep_paths: Vec::new(),
        granted_keys: Vec::new(),
        granted_ec2_rules: Vec::new(),
        in_place_expected_store_id: None,
        in_place_continuation_command_id: None,
        in_place_held_command_ids: Vec::new(),
        in_place_harness_restarted: false,
        deferred_environment_changes: Vec::new(),
        source_snapshot: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "retained-move".into(),
        selection: MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
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
        phase: mj_core::state::MovePhase::DestinationOwns,
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
    let mut child = runtime_test_session(
        CHILD,
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
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
    mj_controller::database::register_subagent_with_startup(
        &child, &relation, rows, None, None, false,
    )
    .unwrap();
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
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let first = mj_controller::database::next_startup_delivery(CHILD)
        .unwrap()
        .unwrap();
    state.start_persisted_startup_delivery(first, &CancellationToken::new());
    // Let the drain see a live child with no projected receipts, then
    // advance beyond the old deadline while those receipts are still absent.
    tokio::time::timeout(Duration::from_secs(5), async {
        while mj_controller::database::load_startup_delivery(&model_id)
            .unwrap()
            .unwrap()
            .phase
            != "delivering"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    for _ in 0..65 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tokio::time::resume();
    assert_eq!(
        mj_controller::database::load_startup_delivery(&model_id)
            .unwrap()
            .unwrap()
            .phase,
        "delivering"
    );
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    child.state = SessionState::Parked;
    mj_controller::database::save_session(&child).unwrap();
    // Replay the actual worker journal through the controller's storage
    // boundary after the child has parked.
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

#[cfg(unix)]
#[tokio::test]
async fn child_startup_failure_waits_for_confirmed_worker_death() {
    use std::os::unix::fs::PermissionsExt;

    use mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_SESSION as CHILD;
    use mj_controller::session_manager::{
        WorkerRecoveryOutcome, WorkerRecoveryPlan, recover_worker_controlled,
    };

    const NAME: &str = "child_startup_failure_waits_for_confirmed_worker_death";
    const PARENT: &str = "parent-startup";
    const CAUSE: &str = "provider refused required seeded model";
    let Some(_writer) = startup_prompt_test_store(NAME) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(CHILD);
    seed_live_session(temp.path(), &root);
    let workspace = mj_core::workspace::DEFAULT_WORKSPACE_ID;
    let parent = runtime_test_session(PARENT, workspace, SessionState::Running);
    mj_controller::database::save_session(&parent).unwrap();
    let target = mj_core::state::TargetLocator::LocalBare {
        worker_root: root.clone(),
    };
    let mut child = runtime_test_session(CHILD, workspace, SessionState::Running);
    child.target = Some(Box::new(target.clone()));
    let relation = runtime_test_subagent(CHILD, PARENT);
    let group = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(&relation.request_key);
    let (command_id, step) = super::startup_followup::api_followup_steps(
        &group,
        crate::server::api::StartFollowup {
            model: Some("requested-model".into()),
            ..Default::default()
        },
    )
    .remove(0);
    mj_controller::database::register_subagent_with_startup(
        &child,
        &relation,
        vec![mj_controller::database::StartupDelivery {
            session_id: CHILD.into(),
            command_id: command_id.clone(),
            step_json: serde_json::to_string(&step).unwrap(),
            phase: "pending".into(),
            group_id: Some(group.clone()),
            accepted_ordinal: None,
            error: None,
        }],
        None,
        None,
        false,
    )
    .unwrap();
    let (channels, mut state) = serve_live_session(&root, true).await;
    Arc::get_mut(&mut state).unwrap().committed =
        Some(mj_controller::database::subscribe_committed_state().unwrap());
    let backend = Arc::new(crate::server_runtime::api::ApiBackend::new(
        channels.control.client(),
        Arc::new({
            let state = state.clone();
            move |id| state.session_state(id)
        }),
        state.clone(),
    ));
    state.install_report_backend(&backend);
    let row = || {
        mj_controller::database::load_startup_delivery(&command_id)
            .unwrap()
            .unwrap()
    };
    let reports = || mj_controller::database::pending_subagent_finished_children(PARENT).unwrap();

    assert!(
        !state
            .fail_subagent_start(CHILD, &group, CAUSE)
            .await
            .unwrap()
    );
    assert_eq!(row().phase, "pending");
    assert!(reports().is_empty());

    // A legacy readiness failure can set Error with the worker still running.
    let hel = root.join("hel");
    std::fs::write(
        &hel,
        r#"#!/bin/sh
if [ "$3" = --help ]; then exit 0; fi
printf '%s\n' "$2" >> "$4/control.log"
case "$2" in
    liveness) if [ -e "$4/worker-live" ]; then echo alive; else echo dead; fi ;;
    stop) echo "stop refused by fixture" >&2; exit 1 ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&hel, std::fs::Permissions::from_mode(0o755)).unwrap();
    let live = root.join("worker-live");
    std::fs::write(&live, "").unwrap();

    let followup_group = "later-followup";
    let followup_command = "later-followup-model";
    let followup_cause = "provider refused the follow-up model";
    mj_controller::database::enqueue_startup_deliveries(vec![
        mj_controller::database::StartupDelivery {
            session_id: CHILD.into(),
            command_id: followup_command.into(),
            step_json: serde_json::to_string(&step).unwrap(),
            phase: "pending".into(),
            group_id: Some(followup_group.into()),
            accepted_ordinal: None,
            error: None,
        },
    ])
    .unwrap();
    assert!(
        state
            .reject_startup(CHILD, followup_group, followup_cause)
            .await
            .unwrap()
    );
    let followup = mj_controller::database::load_startup_delivery(followup_command)
        .unwrap()
        .unwrap();
    assert_eq!(followup.phase, "failed");
    assert_eq!(followup.error.as_deref(), Some(followup_cause));
    assert_eq!(row().phase, "pending");
    assert!(reports().is_empty());
    assert!(!root.join("control.log").exists());

    let child = mj_controller::database::read_durable_session_record(CHILD)
        .unwrap()
        .unwrap();
    assert_eq!(child.state, SessionState::Running);
    assert!(child.last_error.is_none());
    mj_controller::database::fail_unready_session(CHILD, CAUSE, child.state, &child.updated_at)
        .unwrap()
        .unwrap();
    let error = state
        .fail_subagent_start(CHILD, &group, CAUSE)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("stop refused by fixture"),
        "{error:#}"
    );
    assert_eq!(row().phase, "pending");
    assert!(reports().is_empty());

    let exit_record = root.join(mj_core::relay::WORKER_EXIT_FILE);
    channels.shutdown.shutdown().await.unwrap();
    std::fs::remove_file(live).unwrap();
    // Restore Running so the typed-death recovery path can commit its own Error.
    mj_controller::database::save_lifecycle_session(&child).unwrap();
    std::fs::write(
        &exit_record,
        serde_json::json!({"reason": "worker exited", "harness_startup_failure": {"message": CAUSE}})
            .to_string(),
    )
    .unwrap();
    state.start_persisted_startup_delivery(row(), &CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), async {
        while row().phase != "delivering" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let outcome = blocking(move || {
        recover_worker_controlled(
            WorkerRecoveryPlan {
                source_target: target,
                target: None,
                workspace: None,
                liveness_probe: CommandSpec::new("printf", ["dead"]).into(),
                exit_record: Some(CommandSpec::new("cat", [exit_record.to_str().unwrap()])),
                binary_refresh: None,
                launch_refresh: None,
                restart: CommandSpec::new("false", Vec::<String>::new()).into(),
            },
            false,
            Some(CHILD),
            &mj_controller::targets::ProcessExecutor,
        )
    })
    .await
    .unwrap();
    assert_eq!(outcome, WorkerRecoveryOutcome::HarnessStartupFailed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while row().phase != "failed" || !reports().contains(CHILD) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.session_state(CHILD), Some(SessionState::Error));
    assert!(row().error.unwrap().contains(CAUSE));
    assert_eq!(
        std::fs::read_to_string(root.join("control.log")).unwrap(),
        "liveness\nstop\nliveness\n"
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

    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let move_owner = MoveMutationGuard::reserve("session-1").unwrap();
    let refused = http
        .set_session_workspace("session-1".into(), destination.id.clone())
        .await
        .unwrap_err();
    let refused = refused
        .downcast_ref::<mj_client::http::HttpApiError>()
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    assert!(refused.message.contains("after Move finishes"));
    drop(move_owner);
    http.set_session_workspace("session-1".into(), destination.id.clone())
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    http.set_session_notifications_ignored("session-1".into(), true)
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
    http.set_session_notifications_ignored("missing".into(), true)
        .await
        .unwrap_err();
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let Some(_writer) = startup_prompt_test_store(
        "go_startup_session_falls_back_when_the_remembered_session_is_archived",
    ) else {
        return;
    };
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
    state.environment_owners = state
        .sessions
        .keys()
        .map(|id| (id.clone(), id.clone()))
        .collect();
    let runtime = test_runtime_state_holding(state);

    let (client, shutdown, server) = start_test_dashboard_http_api(runtime).await;
    let selected = client
        .go_startup_session("workspace".into(), Some("remembered".into()))
        .await
        .expect("query startup session")
        .expect("eligible fallback session");

    assert_eq!(selected.id, "newest");
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let cancellation = CancellationToken::new();
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .unwrap();
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("before move"),
        cancellation.clone(),
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("waiting before move"),
        cancellation.clone(),
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("during move"),
        cancellation.clone(),
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
    let cancellation = CancellationToken::new();
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("queued before move"),
        cancellation.clone(),
    )
    .await
    .unwrap();
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_move_prompt_action("destination only", &operation.operation_id),
        cancellation.clone(),
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

pub(super) fn retained_startup_test_move(
    session: &mj_core::state::SessionRecord,
) -> mj_core::state::MoveOperation {
    mj_core::state::MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        transfer_route: None,
        consent: Default::default(),
        skip_keep_paths: Vec::new(),
        granted_keys: Vec::new(),
        granted_ec2_rules: Vec::new(),
        in_place_expected_store_id: None,
        in_place_continuation_command_id: None,
        in_place_held_command_ids: Vec::new(),
        in_place_harness_restarted: false,
        deferred_environment_changes: Vec::new(),
        source_snapshot: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "startup-durable-move".into(),
        selection: MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
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
        phase: mj_core::state::MovePhase::DestinationOwns,
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
async fn oversized_startup_prompt_is_refused_before_persistence() {
    let Some(_writer) =
        startup_prompt_test_store("oversized_startup_prompt_is_refused_before_persistence")
    else {
        return;
    };
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let mut action = queued_prompt_action("oversized");
    action.prompt = vec![ContentBlock::Text(TextContent::new(
        "x".repeat(mj_core::relay::RELAY_COMMAND_BYTE_BUDGET),
    ))];
    let result = TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        action,
        CancellationToken::new(),
    )
    .await;
    let Err(error) = result else {
        panic!("oversized startup prompt was accepted");
    };
    assert!(error.is::<mj_core::relay::RelayBudgetExceeded>());
    assert!(
        mj_controller::database::load_startup_deliveries()
            .unwrap()
            .is_empty()
    );
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
    mj_controller::database::test_fixtures::seed_session_draft_input("session-1", &existing)
        .unwrap();
    let cancellation = CancellationToken::new();
    let action = StartupPromptRequest {
        move_operation_id: None,
        command_id: "image-startup".into(),
        text: "queued [image 1]".into(),
        prompt: vec![reference.content_block()],
        inherited_draft: None,
        encoded_draft: recovered.clone(),
    };
    // Stable retries persist one structured row, rather than another text row.
    for _ in 0..2 {
        TerminalOperations::terminal_queue_startup_prompt(
            state.clone(),
            "session-1".into(),
            action.clone(),
            cancellation.clone(),
        )
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        action.clone(),
        cancellation.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        session_draft_input(&state),
        before_retry,
        "settled enqueue retry restored input twice"
    );
    let mut changed = action.clone();
    changed.text.push_str(" changed");
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let error = http
        .queue_startup_prompt("session-1".into(), changed)
        .await
        .unwrap_err();
    assert!(error.is::<mj_client::daemon::StartupPromptIdentityConflict>());
    shutdown.cancel();
    server.await.unwrap().unwrap();
    let outcomes = TerminalOperations::terminal_startup_prompt_outcomes(
        state.clone(),
        "session-1".into(),
        StartupPromptOutcomesRequest {
            command_ids: vec!["image-startup".into(), "missing".into()],
        },
    )
    .await
    .unwrap();
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
        TerminalOperations::terminal_queue_startup_prompt(
            state.clone(),
            "session-1".into(),
            StartupPromptRequest {
                move_operation_id: None,
                command_id: command_id.into(),
                text: "queued [image 1]".into(),
                prompt: vec![reference.content_block()],
                encoded_draft: recovered.clone(),
                inherited_draft: None,
            },
            cancellation.clone(),
        )
        .await
        .unwrap();
    }
    let withdrawn = TerminalOperations::terminal_withdraw_startup_prompt(
        state.clone(),
        "session-1".into(),
        "image-recall".into(),
    )
    .await
    .unwrap();
    assert!(withdrawn);
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
    let conflict = TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        StartupPromptRequest {
            move_operation_id: None,
            command_id: "image-keep".into(),
            text: "queued [image 1]".into(),
            prompt: vec![reference.content_block()],
            encoded_draft: recovered.clone(),
            inherited_draft: Some("different inherited draft".into()),
        },
        cancellation.clone(),
    )
    .await
    .unwrap_err();
    assert!(conflict.is::<mj_client::daemon::StartupPromptIdentityConflict>());
    assert_eq!(
        mj_controller::database::startup_prompt_outcomes("session-1", &["image-keep".into()])
            .unwrap()["image-keep"],
        mj_client::daemon::StartupPromptOutcome::Accepted,
        "a conflicting replay must retain the original accepted receipt"
    );
    assert_eq!(session_draft_input(&state), recovered_before_conflict);
    mj_controller::database::test_fixtures::seed_session_draft_input(
        "session-1",
        "mjolnir-chat-draft-v1:broken",
    )
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_move_prompt_action("cancelled before source", "pre-reservation-origin"),
        CancellationToken::new(),
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_move_prompt_action("late destination draft", "pre-reservation-origin"),
        CancellationToken::new(),
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
    mj_controller::database::test_fixtures::seed_session_draft_input(session_id, "").unwrap();
    let reservation = state.startup_delivery_gate(session_id).lock_owned().await;
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("keep through handoff"),
        CancellationToken::new(),
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
    let cancellation = CancellationToken::new();
    for (index, phase) in [
        mj_core::state::MovePhase::Failed,
        mj_core::state::MovePhase::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        mj_controller::database::test_fixtures::seed_session_draft_input("session-1", "").unwrap();
        let mut session = state.session_record("session-1").unwrap();
        session.state = SessionState::Running;
        mj_controller::database::save_session(&session).unwrap();
        let mut operation = retained_startup_test_move(&session);
        operation.in_place = true;
        operation.queue_admission_started = index != 0;
        operation.phase = mj_core::state::MovePhase::Capturing;
        mj_controller::database::save_move_operation(&operation).unwrap();
        manager
            .publisher
            .publish("session-1".into(), ready_startup_view())
            .await
            .unwrap();
        let command_id = format!("failed-move-image-{index}");
        TerminalOperations::terminal_queue_startup_prompt(
            state.clone(),
            "session-1".into(),
            StartupPromptRequest {
                move_operation_id: Some(operation.operation_id.clone()),
                command_id: command_id.clone(),
                text: "destination image [image 1]".into(),
                prompt: vec![reference.content_block()],
                encoded_draft: draft.clone(),
                inherited_draft: None,
            },
            cancellation.clone(),
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
    let cancellation = CancellationToken::new();
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("uncertain original"),
        cancellation.clone(),
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
    let cancellation = CancellationToken::new();
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_prompt_action("ordinary resumed startup"),
        cancellation.clone(),
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
    TerminalOperations::terminal_queue_startup_prompt(
        state.clone(),
        "session-1".into(),
        queued_move_prompt_action("matching active retry", &historical.operation_id),
        cancellation.clone(),
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
        orchestrators: false,
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
        orchestrator: false,
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
        category: match kind {
            mj_core::subagent::SubagentKind::Oracle => "oracle",
            mj_core::subagent::SubagentKind::Reviewer => "reviewer",
            mj_core::subagent::SubagentKind::Worker => "straightforward",
        }
        .into(),
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
            LifecycleKind::Suspend,
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
            LifecycleKind::Suspend,
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
            LifecycleKind::Suspend,
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
                LifecycleKind::Suspend,
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
        LifecycleCompletion::LifecycleObservationUnavailable(_)
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
    let (client, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        client.start_session_lifecycle(SessionLifecycleRequest::Suspend {
            session_id: LATCH_RELAY_SESSION.into(),
            discard_untracked: mj_core::suspend_consent::UntrackedConsent::Any,
            abort_operations: mj_core::suspend_consent::OperationConsent::None,
            without_checkpoint: false,
            confirm_no_checkpoint: false,
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
        LifecycleCompletion::Done
    ));
    assert!(!state.close_is_requested(LATCH_RELAY_SESSION));
    let stored = mj_controller::database::load_state().unwrap();
    assert_eq!(
        stored.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
    assert!(stored.sessions[LATCH_RELAY_SESSION].checkpoint.is_some());
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let (http, shutdown, server) = start_test_dashboard_http_api_on(state, listener).await;
    let actions = [
        (
            "/api/v1/machines/build-cache/preview",
            Some(
                serde_json::to_value(mj_client::http::machines::PreviewBuildCacheRequest {
                    machine: unsupported_machine.clone(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/machines/build-cache/install",
            Some(
                serde_json::to_value(mj_client::http::machines::InstallMbxRequest {
                    machine: unsupported_machine,
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/archives/sizes",
            Some(
                serde_json::to_value(mj_client::http::machines::CheckpointArchiveSizesRequest {
                    paths: [
                        ("archived".into(), archive.clone()),
                        ("unreadable".into(), archive.join("invalid\0archive")),
                        ("missing".into(), server_root.join("missing.tar")),
                    ]
                    .into(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/machines/targets/test",
            Some(
                serde_json::to_value(mj_client::http::machines::TestTargetRequest {
                    target_id: "missing".into(),
                })
                .unwrap(),
            ),
        ),
        ("/api/v1/profiles/login-statuses", None),
        ("/api/v1/terminal/availability/check", None),
        (
            "/api/v1/setup/discover",
            Some(
                serde_json::to_value(mj_client::http::machines::DiscoverSetupRequest {
                    scope: mj_client::discovery::DetectScope::Profiles,
                    machines: Vec::new(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/setup/discover",
            Some(
                serde_json::to_value(mj_client::http::machines::DiscoverSetupRequest {
                    scope: mj_client::discovery::DetectScope::Runtimes,
                    machines: Vec::new(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/projects/directories/check",
            Some(
                serde_json::to_value(mj_client::http::machines::CheckProjectDirectoriesRequest {
                    paths: vec![project.clone(), server_root.as_path().join("absent")],
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/setup/path/resolve",
            Some(
                serde_json::to_value(mj_client::http::machines::ResolveSetupPathRequest {
                    machine: mj_core::config::Machine::Local { build_cache: None },
                    source: project.clone(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/mounts/source/validate",
            Some(
                serde_json::to_value(mj_client::http::machines::ValidateMountSourceRequest {
                    target_id: "missing".into(),
                    source: project.clone(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/mounts/session/validate",
            Some(
                serde_json::to_value(mj_client::http::machines::ValidateSessionMountsRequest {
                    target_id: "missing".into(),
                    mounts: vec![mj_core::targets::AdditionalMount {
                        source: project.clone(),
                        destination: "/project".into(),
                        access: mj_core::targets::MountAccess::Ro,
                    }],
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/machines/container-path/resolve",
            Some(
                serde_json::to_value(mj_client::http::machines::ResolveContainerPathRequest {
                    target_id: "local".into(),
                    source: project.clone(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/projects/directory/validate",
            Some(
                serde_json::to_value(mj_client::http::machines::ValidateProjectDirectoryRequest {
                    target_id: "local".into(),
                    directory: project.clone(),
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/machines/container-path/resolve",
            Some(
                serde_json::to_value(mj_client::http::machines::ResolveContainerPathRequest {
                    target_id: "missing".into(),
                    source: project,
                })
                .unwrap(),
            ),
        ),
        (
            "/api/v1/terminal/spinner-style",
            Some(
                serde_json::to_value(mj_client::http::setup::SaveSpinnerStyleRequest {
                    style: mj_core::config::SpinnerStyle::Wave,
                })
                .unwrap(),
            ),
        ),
    ];
    let mut exchange = Vec::new();
    for (path, body) in actions {
        let reply = match &body {
            Some(body) => http.post_json::<serde_json::Value>(path, body).await,
            None => http.post_empty::<serde_json::Value>(path).await,
        };
        let mut reply = reply.map_err(|error| {
            error
                .downcast_ref::<mj_client::http::HttpApiError>()
                .unwrap()
                .message
                .clone()
        });
        // Embedded NUL is invalid on every host; only OS diagnostic wording varies.
        if path.ends_with("/sizes") {
            let error = &mut reply.as_mut().unwrap()["unreadable"]["Err"];
            assert!(error.as_str().unwrap().starts_with("read "));
            *error = serde_json::json!("<archive metadata error>");
        }
        exchange.push(serde_json::json!({"path": path, "request": body, "reply": reply}));
    }
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let Some(_writer) =
        startup_prompt_test_store("superseded_machine_completion_closes_connection_and_reaps_ssh")
    else {
        return;
    };
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
    let (http, shutdown, server) = start_test_http_api(test_runtime_state()).await;
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
    let request = mj_core::runtime::spawn(async move {
        http.resolve_setup_path(machine, "~/projects".into()).await
    });
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
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let pid = started_result.expect("OpenSSH proxy never started");
    let reaped = tokio::time::timeout(Duration::from_secs(30), async {
        while daemon_process_is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    shutdown.cancel();
    server.await.unwrap().unwrap();
    reaped.expect("superseded completion left its SSH proxy alive");
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_import_streams_cleans_up_and_refuses_registration_during_drain() {
    const CHILD: &str = "MJ_TEST_UPLOAD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "terminal_import_streams_cleans_up_and_refuses_registration_during_drain",
            ),
        )
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "upload-exchange")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::start_database_writer().unwrap();
    let state = test_runtime_state_loading_the_store();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (http, shutdown, server) = start_test_dashboard_http_api_on(state, listener).await;
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
            native_artifacts: vec![mj_core::archive::NativeArtifact {
                relative_path: "history.jsonl".into(),
                data: {
                    use sha2::Digest as _;
                    (0..4096_u32)
                        .flat_map(|index| sha2::Sha256::digest(index.to_be_bytes()))
                        .collect::<Vec<_>>()
                        .into()
                },
                mode: 0o600,
            }],
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
    assert!(archive_path.metadata().unwrap().len() > 64 * 1024);
    let mut file = tokio::fs::File::open(&archive_path).await.unwrap();
    let metadata = ImportSessionMetadata {
        session: Box::new(session),
        bundle: None,
    };
    // A disconnect mid-upload must remove the partial file and leave the gate idle.
    let mut partial = TcpStream::connect(address).await.unwrap();
    use tokio::io::AsyncWriteExt as _;
    let header = serde_json::to_vec(&metadata).unwrap();
    let archive = fs::read(&archive_path).unwrap();
    partial.write_all(format!(
        "POST /api/v1/sessions/import HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer direct-test-token\r\nContent-Length: {}\r\n\r\n",
        4 + header.len() + archive.len(),
    ).as_bytes()).await.unwrap();
    partial
        .write_all(&u32::try_from(header.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    partial.write_all(&header).await.unwrap();
    partial.write_all(&archive[..80 * 1024]).await.unwrap();
    let uploads = mj_core::config::data_dir().join("uploads");
    let upload_count = || {
        fs::read_dir(&uploads)
            .map(|files| files.count())
            .unwrap_or(0)
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while upload_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(mj_controller::upgrade::active_labels().is_empty());
    drop(partial);
    tokio::time::timeout(Duration::from_secs(5), async {
        while upload_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    mj_controller::upgrade::gate().begin_draining();
    let error = http
        .import_session_observed(
            metadata.clone(),
            &mut file,
            |_| async { Ok(()) },
            std::future::pending(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<mj_client::http::HttpApiError>()
            .is_some_and(|failure| failure.is_handoff())
    );
    assert!(mj_controller::upgrade::active_labels().is_empty());
    assert_eq!(
        std::fs::read_dir(mj_core::config::sessions_dir())
            .unwrap()
            .count(),
        0
    );
    assert!(
        !http
            .transcription_available("missing".into())
            .await
            .unwrap()
    );
    let error = http
        .transcribe_audio("missing".into(), vec![0; 196608])
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires a signed-in ChatGPT Codex profile")
    );
    assert!(mj_controller::upgrade::active_labels().is_empty());
    mj_controller::upgrade::gate().expire_drain_for_test();
    let phases = std::cell::RefCell::new(Vec::new());
    http.import_session_observed(
        metadata,
        &mut file,
        |uploading| {
            phases.borrow_mut().push(uploading);
            async { Ok(()) }
        },
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(*phases.borrow(), [true, false]);
    assert!(
        http.session_record("import-session".into())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        mj_core::config::sessions_dir()
            .join("import-session.hel.zip")
            .is_file()
    );
    assert_eq!(
        std::fs::read_dir(mj_core::config::data_dir().join("uploads"))
            .unwrap()
            .count(),
        0
    );
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    for handoff in [false, true] {
        let (http, shutdown, server) = start_test_dashboard_http_api(test_runtime_state()).await;
        let client = mj_core::runtime::spawn(async move {
            http.install_mbx(mj_core::config::Machine::Local { build_cache: None })
                .await
        });
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
            assert!(mj_controller::upgrade::gate().try_close());
            shutdown.cancel();
            let error = client.await.unwrap().unwrap_err();
            assert!(
                error
                    .downcast_ref::<mj_client::http::HttpApiError>()
                    .is_some_and(|failure| failure.is_handoff()),
                "{error:#}"
            );
        } else {
            client.abort();
            assert!(client.await.unwrap_err().is_cancelled());
        }
        // The client response and shutdown need not wait for the disposable
        // blocking probe. Wait for it to close the download and clean staging.
        let staging = mj_core::config::data_dir().join("mbx-download");
        tokio::time::timeout(Duration::from_secs(30), async {
            download.read_to_end(&mut Vec::new()).await.unwrap();
            while fs::read_dir(&staging).unwrap().count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        server.await.unwrap().unwrap();
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
    let metadata = test_metadata("127.0.0.1:0".parse().unwrap());
    let (http, http_shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let request = if destroy {
        SessionLifecycleRequest::ForceDestroy {
            session_id: "queued-upgrade".into(),
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
    let response =
        mj_core::runtime::spawn(async move { http.start_session_lifecycle(request).await });
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
    assert!(mj_controller::upgrade::active_labels().is_empty());
    let shutdown = CancellationToken::new();
    let action = if handoff {
        DaemonAction::PrepareUpgrade
    } else {
        DaemonAction::Stop
    };
    handle_action(action, &metadata, &state, &shutdown)
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<mj_client::http::HttpApiError>()
            .is_some_and(|failure| failure.is_handoff())
    );
    assert!(state.owner().lifecycle_intents.is_empty());
    assert!(
        control.cancelled.load(Ordering::Acquire) == destroy,
        "only destroy requests cancellation before waiting; draining does not cancel the upgrade"
    );
    http_shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let state = test_runtime_state_loading_the_store();
    let (client, shutdown, server) = start_test_dashboard_http_api(state).await;
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
        serde_json::to_vec(history.transcript.last().unwrap().as_ref())
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
    assert!(matches!(
        client
            .stored_agent_message_through("missing".into(), 0)
            .await
            .unwrap(),
        StoredTranscriptItem::Missing
    ));
    assert!(
        client
            .session_record("missing".into())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        client
            .session_record("notification-history".into())
            .await
            .unwrap()
            .unwrap()
            .id,
        "notification-history"
    );
    shutdown.cancel();
    server.await.unwrap().unwrap();
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

#[tokio::test]
async fn http_adoption_delivers_final_failure_for_a_tracked_session() {
    let Some(_writer) =
        startup_prompt_test_store("http_adoption_delivers_final_failure_for_a_tracked_session")
    else {
        return;
    };
    let session = runtime_test_session(
        "already-adopted",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Running,
    );
    mj_controller::database::save_session(&session).unwrap();
    let (http, shutdown, server) = start_test_dashboard_http_api(test_runtime_state()).await;
    let error = http
        .adopt_recovery(
            RecoveryAdoptionRequest {
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (_, shutdown, server) =
        start_test_dashboard_http_api_on(test_runtime_state(), listener).await;
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/api/v1/recovery/adopt"))
        .bearer_auth("direct-test-token")
        .json(&RecoveryAdoptionRequest {
            session_id: "missing-orphan".into(),
            target_id: "blocked".into(),
            profile: None,
            bundle: None,
            all_instances: false,
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
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
        let line = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .unwrap()
            .unwrap();
        assert!(line.is_none(), "handoff must close the adoption stream");
    }
    drop(response);
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
    shutdown.cancel();
    server.await.unwrap().unwrap();
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
    let mut record = runtime_test_session(
        "owned-adoption",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        SessionState::Disconnected,
    );
    record.native_session_id = None;
    record.target_template_id = "bare".into();
    record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from(format!("/workers/{}", record.id)),
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
            waits.push(super::terminal_streams::spawn_adoption_operation(
                async move {
                    entered.send(()).unwrap();
                    cancelled.cancelled().await;
                    Ok(())
                },
            ));
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
    mj_core::test_hooks::enable_test_hooks();
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

#[tokio::test]
async fn close_without_checkpoint_refusal_leaves_session_running() {
    const CHILD: &str = "MJ_TEST_CLOSE_WITHOUT_CHECKPOINT_REFUSAL";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "close_without_checkpoint_refusal_leaves_session_running",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let session_id = "daemon-without-checkpoint-refusal";
    let mut record = runtime_test_session(session_id, "default", SessionState::Running);
    record.checkpoint = None;
    mj_controller::database::save_session(&record).unwrap();

    let state = test_runtime_state_loading_the_store();
    let preflight = TerminalOperations::terminal_suspend_preflight(
        state.clone(),
        session_id.into(),
        SuspendPreflightRequest {
            dry_run: false,
            without_checkpoint: true,
        },
    )
    .await
    .unwrap();
    assert!(preflight.existing_checkpoint.is_none());

    let consent = mj_core::suspend_consent::SuspendConsent {
        without_checkpoint: true,
        confirm_no_checkpoint: false,
        ..Default::default()
    };
    let err = state
        .suspend_session_with_consent(session_id.into(), consent)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("No verified checkpoint exists"),
        "expected refusal, got: {err:#}"
    );
    // `suspend_session_with_consent` awaited `started.result`, which only
    // resolves after lifecycle completion (and any failure recording).
    assert!(
        state
            .owner()
            .lifecycle
            .get(session_id)
            .is_none_or(|active| !active.is_running()),
        "refused suspend lifecycle must be finished once wait_lifecycle_result returns"
    );
    let loaded = Controller::load().unwrap();
    assert_eq!(
        loaded.state.sessions[session_id].state,
        SessionState::Running,
        "refused without_checkpoint must not transition session to Closing"
    );
    assert_eq!(
        loaded.state.sessions[session_id].last_error, None,
        "pre-acceptance suspend refusal must not stamp last_error onto a live session"
    );
}

#[tokio::test]
async fn terminal_suspend_preflight_runs_during_drain_without_admission() {
    const CHILD: &str = "MJ_TEST_SUSPEND_PREFLIGHT_MACHINE_DISPATCH";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        mj_controller::controller::test_support::IsolatedTest::new(
            mj_controller::controller::test_support::test_name(
                module_path!(),
                "terminal_suspend_preflight_runs_during_drain_without_admission",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let session_id = "daemon-suspend-preflight-machine";
    let record = runtime_test_session(session_id, "default", SessionState::Running);
    mj_controller::database::save_session(&record).unwrap();

    let state = test_runtime_state_loading_the_store();
    let (client, shutdown, server) = start_test_dashboard_http_api(state).await;
    mj_controller::upgrade::gate().begin_draining();
    let preflight = client
        .suspend_preflight(
            session_id.into(),
            SuspendPreflightRequest {
                dry_run: false,
                without_checkpoint: false,
            },
        )
        .await
        .unwrap();
    assert!(mj_controller::upgrade::active_labels().is_empty());
    assert_eq!(preflight.session_id, session_id);
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn parent_suspend_records_only_children_it_stops_and_resume_keeps_failed_children_eligible() {
    const NAME: &str =
        "parent_suspend_records_only_children_it_stops_and_resume_keeps_failed_children_eligible";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Owned children").unwrap();
    let parent_id = "11111111111111111111111111111111";
    let live_id = "22222222222222222222222222222222";
    let manual_id = "33333333333333333333333333333333";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Provisioning);
    mj_controller::database::save_session(&parent).unwrap();
    for (id, status) in [
        (live_id, SessionState::Provisioning),
        (manual_id, SessionState::Stopped),
    ] {
        let session = runtime_test_session(id, &workspace.id, status);
        mj_controller::database::register_subagent_with_startup(
            &session,
            &runtime_test_subagent(id, parent_id),
            vec![],
            None,
            None,
            true,
        )
        .unwrap();
    }
    let failed_id = "44444444444444444444444444444444";
    let mut failed = runtime_test_session(failed_id, &workspace.id, SessionState::Parked);
    let unbootable_root = mj_core::config::data_dir().join(failed_id);
    std::fs::create_dir(&unbootable_root).unwrap();
    failed.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: unbootable_root,
    }));
    mj_controller::database::register_subagent_with_startup(
        &failed,
        &runtime_test_subagent(failed_id, parent_id),
        vec![],
        None,
        None,
        true,
    )
    .unwrap();
    let state = test_runtime_state_loading_the_store();
    state.suspend_session(parent_id.into()).await.unwrap();
    let load_flag = |id| {
        mj_controller::database::load_subagent(id)
            .unwrap()
            .unwrap()
            .resume_with_parent
    };
    assert!(load_flag(live_id));
    assert!(!load_flag(manual_id));
    assert!(!load_flag(failed_id));
    assert_ne!(
        mj_controller::database::load_session_state(failed_id).unwrap(),
        Some(SessionState::Stopped)
    );
    assert_eq!(
        mj_controller::database::load_session_state(parent_id).unwrap(),
        Some(SessionState::Stopped)
    );
    assert_eq!(
        mj_controller::database::load_session_state(live_id).unwrap(),
        Some(SessionState::Stopped)
    );

    // An already-running parent must retry a child's Resume. With no recovery
    // archive, the failure leaves its eligibility available on the next retry.
    parent.state = SessionState::Running;
    mj_controller::database::save_lifecycle_session(&parent).unwrap();
    let request = ResumeSessionRequest {
        session_id: parent_id.into(),
        workspace_id: workspace.id.clone(),
        profile_id: "codex".into(),
        target_template_id: "local".into(),
        additional_mounts: None,
        resource_allocation: None,
        discard_queue: false,
        repository_preflight: None,
    };
    for _ in 0..2 {
        state.resume_session(request.clone()).await.unwrap();
        assert!(load_flag(live_id));
        assert!(!load_flag(manual_id));
    }
    state.suspend_session(live_id.into()).await.unwrap();
    assert!(
        !load_flag(live_id),
        "explicit suspend cleared eligibility on a stopped child"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn archive_refuses_parent_with_live_or_resume_eligible_owned_child() {
    const NAME: &str = "archive_refuses_parent_with_live_or_resume_eligible_owned_child";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Archive protection").unwrap();
    let parent_id = "11111111111111111111111111111111";
    let child_id = "22222222222222222222222222222222";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    mj_controller::database::save_session(&parent).unwrap();
    let mut child = runtime_test_session(child_id, &workspace.id, SessionState::Running);
    mj_controller::database::register_subagent_with_startup(
        &child,
        &runtime_test_subagent(child_id, parent_id),
        vec![],
        None,
        None,
        true,
    )
    .unwrap();
    parent.state = SessionState::Stopped;
    mj_controller::database::save_lifecycle_session(&parent).unwrap();
    let state = test_runtime_state_loading_the_store();
    assert!(
        state
            .archive_stopped_session(parent_id.into())
            .await
            .is_err()
    );
    child.state = SessionState::Stopped;
    mj_controller::database::save_lifecycle_session(&child).unwrap();
    mj_controller::database::set_subagent_resume_with_parent(child_id, true).unwrap();
    assert!(
        state
            .archive_stopped_session(parent_id.into())
            .await
            .is_err()
    );
    assert!(
        state
            .archive_stopped_session(child_id.into())
            .await
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_state(parent_id).unwrap(),
        Some(SessionState::Stopped)
    );
    assert_eq!(
        mj_controller::database::load_session_state(child_id).unwrap(),
        Some(SessionState::Stopped)
    );
    // Even after the owning child is manually stopped (resume_with_parent = false),
    // its foreign-key subagent_sessions row still references the parent, so archive
    // must keep the parent intact until the child is removed first.
    mj_controller::database::set_subagent_resume_with_parent(child_id, false).unwrap();
    assert!(
        state
            .archive_stopped_session(parent_id.into())
            .await
            .is_err()
    );
    assert_eq!(
        mj_controller::database::load_session_state(parent_id).unwrap(),
        Some(SessionState::Stopped)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn interrupted_parent_suspend_recovery_cascades_to_owned_children_and_keeps_resume_flag() {
    const NAME: &str =
        "interrupted_parent_suspend_recovery_cascades_to_owned_children_and_keeps_resume_flag";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Recovery cascade").unwrap();
    let parent_id = "11111111111111111111111111111111";
    let pending_child_id = "22222222222222222222222222222222";
    let closing_child_id = "33333333333333333333333333333333";

    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.target = None;
    mj_controller::database::save_session(&parent).unwrap();
    for (child_id, child_state, resume_flag) in [
        (pending_child_id, SessionState::Provisioning, false),
        (closing_child_id, SessionState::Closing, true),
    ] {
        let mut child = runtime_test_session(child_id, &workspace.id, child_state);
        child.target = None;
        let mut relation = runtime_test_subagent(child_id, parent_id);
        relation.resume_with_parent = resume_flag;
        mj_controller::database::register_subagent_with_startup(
            &child,
            &relation,
            vec![],
            None,
            None,
            true,
        )
        .unwrap();
    }
    parent.state = SessionState::Closing;
    mj_controller::database::save_lifecycle_session(&parent).unwrap();

    let state = test_runtime_state_loading_the_store();
    state
        .recover_suspension(closing_child_id.into())
        .await
        .unwrap();
    state.recover_suspension(parent_id.into()).await.unwrap();

    for id in [parent_id, pending_child_id, closing_child_id] {
        assert_eq!(
            mj_controller::database::load_session_state(id).unwrap(),
            Some(SessionState::Stopped),
            "{id} should finish stopping during recovery"
        );
    }
    for id in [pending_child_id, closing_child_id] {
        assert!(
            mj_controller::database::load_subagent(id)
                .unwrap()
                .unwrap()
                .resume_with_parent,
            "{id} should preserve resume_with_parent after recovery"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_orchestrator_checks_destination_and_persists_only_explicit_repository_bundles() {
    use mj_core::config::{Config, TargetTemplate};
    use mj_core::subagent::{SubagentPolicy, SubagentToolAction, SubagentToolRequest};

    const NAME: &str =
        "spawn_orchestrator_checks_destination_and_persists_only_explicit_repository_bundles";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    for id in ["container", "disabled"] {
        config.targets.insert(
            id.into(),
            serde_json::from_value(serde_json::json!({"kind": "local-podman"})).unwrap(),
        );
    }
    config.disabled_targets.insert("disabled".into());
    // Loading adds the default local targets, and on macOS `apple-container`
    // sorts before "container". Save them turned off so the refusal below
    // names the container target this test configures on every platform.
    let mut config = config.with_local_targets();
    let defaults = config.default_targets.clone();
    config.disabled_targets.extend(defaults);
    config.save().unwrap();
    let workspace = mj_controller::database::create_workspace("Orchestrator placement").unwrap();
    let parent_id = "11111111111111111111111111111111";
    let mut parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    parent.subagents = Some(SubagentPolicy::Mjolnir {
        orchestrators: true,
        straightforward: None,
        subtle: None,
    });
    mj_controller::database::save_session(&parent).unwrap();
    let runtime = test_runtime_state_loading_the_store();
    let urls = vec!["https://github.com/example/p1.git".into()];
    for (target, repos, error) in [
        (Some("unknown"), None, "unknown target"),
        (Some("disabled"), None, "turned off"),
        (
            None,
            Some(urls.clone()),
            "use container target \"container\"",
        ),
        (Some("container"), None, "no cloneable project bundle"),
        (
            Some("container"),
            Some(Vec::new()),
            "at least one repository URL",
        ),
        // Reaching selection with no attached backend proves both project
        // creation and duplicate resolution ran on the explicit container.
        (
            Some("container"),
            Some(urls.clone()),
            "placement backend is unavailable",
        ),
        (
            Some("container"),
            Some(urls),
            "placement backend is unavailable",
        ),
    ] {
        let request = SubagentToolRequest {
            request_id: "spawn-orchestrator-preflight".into(),
            originating_command_id: None,
            created_at_ms: 0,
            action: SubagentToolAction::SpawnOrchestrator {
                task_name: "orchestrate".into(),
                instructions: "coordinate".into(),
                target: target.map(str::to_owned),
                repos,
                no_github_credentials: false,
            },
        };
        let failure = runtime
            .start_orchestrator_session(parent_id.into(), request, None)
            .await
            .unwrap_err();
        assert!(format!("{failure:#}").contains(error), "{failure:#}");
    }
    assert_eq!(
        mj_controller::database::read_project_catalog()
            .unwrap()
            .projects
            .len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_orchestrator_close_clears_resume_for_an_already_stopped_owner() {
    const NAME: &str = "spawn_orchestrator_close_clears_resume_for_an_already_stopped_owner";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Explicit child close").unwrap();
    let parent_id = "11111111111111111111111111111111";
    let child_id = "22222222222222222222222222222222";
    let parent = runtime_test_session(parent_id, &workspace.id, SessionState::Running);
    mj_controller::database::save_session(&parent).unwrap();
    let child = runtime_test_session(child_id, &workspace.id, SessionState::Stopped);
    let mut relation = runtime_test_subagent(child_id, parent_id);
    relation.orchestrator = true;
    relation.resume_with_parent = true;
    mj_controller::database::register_subagent_with_startup(
        &child,
        &relation,
        vec![],
        None,
        None,
        true,
    )
    .unwrap();
    let runtime = test_runtime_state_loading_the_store();
    let response = runtime.close_orchestrator(child_id.into()).await.unwrap();
    assert_eq!(response["closed"], true);
    assert_eq!(response["status"], "already_stopped");
    assert!(
        !mj_controller::database::load_subagent(child_id)
            .unwrap()
            .unwrap()
            .resume_with_parent
    );
}

#[cfg(unix)]
#[tokio::test]
async fn close_of_parked_owning_child_rolls_back_after_checkpoint_only_start_fails() {
    use std::os::unix::fs::PermissionsExt;

    const NAME: &str = "close_of_parked_owning_child_rolls_back_after_checkpoint_only_start_fails";
    if !in_isolated_parked_test(NAME) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let workspace = mj_controller::database::create_workspace("Already quiescent").unwrap();
    let parent = "77777777777777777777777777777777";
    let child = "88888888888888888888888888888888";
    let mut config = mj_core::config::Config::default();
    config
        .targets
        .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
    config.profiles.insert(
        "codex".into(),
        mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            mj_core::config::data_dir().join("profile"),
        ),
    );
    config.save().unwrap();
    mj_controller::database::save_session(&runtime_test_session(
        parent,
        &workspace.id,
        SessionState::Running,
    ))
    .unwrap();
    let mut record = runtime_test_session(child, &workspace.id, SessionState::Parked);
    let root = tempfile::tempdir().unwrap();
    let worker_root = root.path().join(child);
    std::fs::create_dir(&worker_root).unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    record.project_directory = Some(project);
    let worker = worker_root.join("hel");
    let start_marker = root.path().join("started");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\n[ \"$2\" = stop ] && exit 0\necho started >> {}\n",
            start_marker.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    mj_controller::database::register_subagent_with_startup(
        &record,
        &runtime_test_subagent(child, parent),
        vec![],
        None,
        None,
        true,
    )
    .unwrap();
    // Fail before spawning any worker, while attempting to read the installed launch.
    std::fs::write(worker_root.join("launch.json"), b"{").unwrap();
    let state = test_runtime_state_loading_the_store();
    assert!(state.suspend_session(child.into()).await.is_err());
    let saved = mj_controller::database::read_durable_session_record(child)
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, SessionState::Parked);
    assert_eq!(saved.target, record.target);
    assert!(!start_marker.exists());
}

pub(crate) fn seed_legacy_detached_draft(
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> rusqlite::Result<()> {
    let connection = rusqlite::Connection::open(mj_controller::database::database_path())?;
    connection.execute(
        "INSERT INTO detached_drafts
            (draft_id, workspace_id, session_id, source, owner_pid, saved_at, text)
         VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?5)",
        rusqlite::params![workspace_id, session_id, source, owner_pid, text],
    )?;
    Ok(())
}

#[tokio::test]
async fn retired_harness_requires_explicit_destroy() {
    use mj_controller::database::{self, test_fixtures};
    use mj_controller::test_isolation::{IsolatedTest, test_name};

    const CHILD: &str = "MJ_TEST_RETIRED_HARNESS_DESTROY";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "retired_harness_requires_explicit_destroy",
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "retired-harness-destroy")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = database::install_isolated_test_writer();
    let mut session = test_fixtures::checkpoint_test_session("retired-destroy");
    session.state = SessionState::Destroying;
    database::save_session(&session).unwrap();
    test_fixtures::retire_test_harness(&session.id).unwrap();
    let mut state = test_runtime_state_loading_the_store();
    Arc::get_mut(&mut state).unwrap().committed =
        Some(database::subscribe_committed_state().unwrap());

    let snapshots = state.subscribe_browser_views().unwrap();
    {
        let snapshot = snapshots.borrow();
        assert!(snapshot.sessions.get(&session.id).is_none());
        assert!(snapshot.retired_session_ids.contains(&session.id));
    }

    state.recover_destruction(session.id.clone()).await.unwrap();
    let retained = database::load_retired_harness_session(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.owned_environment.unwrap().phase,
        database::EnvironmentPhase::Active
    );

    let started = state
        .admit_session_lifecycle(SessionLifecycleRequest::DestroyStopped {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    RuntimeState::wait_lifecycle_result(started.result)
        .await
        .unwrap();
    assert!(
        database::load_retired_harness_session(&session.id)
            .unwrap()
            .is_none()
    );
    state.browser_views(None).unwrap();
    assert!(!snapshots.borrow().retired_session_ids.contains(&session.id));
}

#[tokio::test]
async fn retired_harness_child_is_destroyed_during_parent_destruction_recovery() {
    use mj_controller::database::{self, test_fixtures};
    use mj_controller::test_isolation::{IsolatedTest, test_name};

    const TEST: &str = "retired_harness_child_is_destroyed_during_parent_destruction_recovery";
    const CHILD: &str = "MJ_TEST_RETIRED_HARNESS_CHILD_DESTROY";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), TEST))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "retired-harness-child-destroy")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = database::install_isolated_test_writer();
    let parent = test_fixtures::checkpoint_test_session("supported-parent");
    let child = test_fixtures::checkpoint_test_session("retired-child");
    database::save_session(&parent).unwrap();
    database::save_subagent_session(&child, &runtime_test_subagent(&child.id, &parent.id)).unwrap();
    database::test_fixtures::retire_test_harness(&child.id).unwrap();
    assert_eq!(
        database::begin_session_destroy(&parent.id)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&child.id)
    );

    let state = test_runtime_state_loading_the_store();
    state.recover_destruction(parent.id.clone()).await.unwrap();

    assert!(
        database::read_durable_session_record(&parent.id)
            .unwrap()
            .is_none()
    );
    assert!(
        database::load_retired_harness_session(&child.id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retired_harness_child_is_stopped_before_its_parent_suspends() {
    use mj_controller::database::{self, test_fixtures};
    use mj_controller::test_isolation::{IsolatedTest, test_name};

    const TEST: &str = "retired_harness_child_is_stopped_before_its_parent_suspends";
    const CHILD: &str = "MJ_TEST_RETIRED_HARNESS_CHILD_SUSPEND";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), TEST))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "retired-harness-child-suspend")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = database::install_isolated_test_writer();
    let parent = test_fixtures::checkpoint_test_session("supported-parent");
    let child = test_fixtures::checkpoint_test_session("retired-child");
    database::save_session(&parent).unwrap();
    database::save_subagent_session(&child, &runtime_test_subagent(&child.id, &parent.id)).unwrap();
    database::test_fixtures::retire_test_harness(&child.id).unwrap();

    let state = test_runtime_state_loading_the_store();
    assert!(
        state
            .owner()
            .controller()
            .state
            .subagents
            .get(&child.id)
            .is_none()
    );
    state
        .stop_subagents_before_close(&parent.id, None)
        .await
        .unwrap();

    assert!(
        database::load_retired_harness_session(&child.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        database::session_environment(&parent.id)
            .unwrap()
            .unwrap()
            .phase,
        database::EnvironmentPhase::Active
    );
}

#[cfg(unix)]
#[tokio::test]
async fn retired_harness_stop_keeps_independent_descendants() {
    use mj_controller::database::{self, test_fixtures};
    use mj_controller::test_isolation::{IsolatedTest, test_name};
    use mj_core::state::TargetLocator;
    use std::os::unix::fs::PermissionsExt as _;

    const TEST: &str = "retired_harness_stop_keeps_independent_descendants";
    const CHILD: &str = "MJ_TEST_RETIRED_HARNESS_INDEPENDENT_DESCENDANTS";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), TEST))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "retired-harness-independent-descendants")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();

    for variant in ["supported", "retired"] {
        let session = |role| {
            let id = format!("{variant}-{role}");
            let mut session = runtime_test_session(
                &id,
                mj_core::workspace::DEFAULT_WORKSPACE_ID,
                SessionState::Running,
            );
            session.target = Some(Box::new(TargetLocator::LocalBare {
                worker_root: root.path().join(id),
            }));
            session
        };
        let parent = session("parent");
        let child = session("borrower");
        let sibling = session("sibling");
        let bound = session("bound-grandchild");
        let independent = session("independent-grandchild");
        let worker_root = |id: &str| root.path().join(id);
        let stopped_marker = |id: &str| root.path().join(format!("{id}.stopped"));
        for id in [&child.id, &independent.id] {
            let dir = worker_root(id);
            std::fs::create_dir_all(&dir).unwrap();
            let worker = dir.join("hel");
            std::fs::write(
                &worker,
                "#!/bin/sh\n[ \"$3\" != --help ] || exit 0\ntouch \"$4.stopped\"\n",
            )
            .unwrap();
            std::fs::set_permissions(worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let data = worker_root(&independent.id).join("work");
        std::fs::write(&data, b"independent work").unwrap();

        database::save_session(&parent).unwrap();
        let mut borrower = runtime_test_subagent(&child.id, &parent.id);
        borrower.orchestrator = true;
        database::save_subagent_session(&child, &borrower).unwrap();
        database::save_subagent_session(&sibling, &runtime_test_subagent(&sibling.id, &parent.id))
            .unwrap();
        database::save_subagent_session(&bound, &runtime_test_subagent(&bound.id, &child.id))
            .unwrap();
        database::register_subagent_with_startup(
            &independent,
            &runtime_test_subagent(&independent.id, &child.id),
            Vec::new(),
            None,
            None,
            true,
        )
        .unwrap();
        let shared = database::session_environment(&parent.id).unwrap().unwrap();
        let before = database::session_environment(&independent.id)
            .unwrap()
            .unwrap();
        assert_ne!(before.id, shared.id);
        test_fixtures::retire_test_harness(&child.id).unwrap();
        if variant == "retired" {
            test_fixtures::retire_test_harness(&bound.id).unwrap();
            test_fixtures::retire_test_harness(&independent.id).unwrap();
        }

        let state = test_runtime_state_loading_the_store();
        state
            .stop_subagents_before_close(&sibling.id, None)
            .await
            .unwrap();
        assert!(!stopped_marker(&child.id).exists());
        assert!(worker_root(&child.id).exists());
        assert_eq!(
            database::session_environment(&child.id)
                .unwrap()
                .unwrap()
                .id,
            shared.id
        );
        assert_eq!(
            database::load_retired_harness_session(&child.id)
                .unwrap()
                .unwrap()
                .state,
            SessionState::Running
        );

        state
            .stop_subagents_before_close(&parent.id, None)
            .await
            .unwrap();

        assert!(stopped_marker(&child.id).exists());
        assert!(!worker_root(&child.id).exists());
        assert!(database::session_environment(&child.id).unwrap().is_none());
        assert!(
            database::load_retired_harness_session(&child.id)
                .unwrap()
                .is_some()
        );
        assert!(
            database::environment_workers(&shared.id)
                .unwrap()
                .iter()
                .any(|lease| lease.session_id == child.id)
        );
        assert!(
            database::load_retired_harness_session(&bound.id)
                .unwrap()
                .is_none()
        );
        assert!(
            database::read_durable_session_record(&bound.id)
                .unwrap()
                .is_none()
        );

        let after = database::session_environment(&independent.id)
            .unwrap()
            .unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.phase, database::EnvironmentPhase::Active);
        assert_eq!(
            database::load_subagent(&independent.id)
                .unwrap()
                .unwrap()
                .parent_session_id,
            child.id
        );
        let independent_state = if variant == "retired" {
            database::load_retired_harness_session(&independent.id)
                .unwrap()
                .unwrap()
                .state
        } else {
            database::read_durable_session_record(&independent.id)
                .unwrap()
                .unwrap()
                .state
        };
        assert_eq!(independent_state, SessionState::Running);
        assert!(!stopped_marker(&independent.id).exists());
        assert_eq!(std::fs::read(&data).unwrap(), b"independent work");
    }
}

#[tokio::test]
async fn terminal_dashboard_routes_round_trip_and_reads_continue_during_drain() {
    let Some(_writer) = startup_prompt_test_store(
        "terminal_dashboard_routes_round_trip_and_reads_continue_during_drain",
    ) else {
        return;
    };
    use mj_core::workspace::{ConversationLayout, PaneSize, PaneSizes};
    let mut config = Config::default();
    config.targets.insert(
        "container".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: serde_json::from_value(serde_json::json!({})).unwrap(),
        },
    );
    config.save().unwrap();
    let workspace = mj_controller::database::create_workspace("Dashboard HTTP").unwrap();
    let id = "stopped-dashboard-session";
    let mut session = runtime_test_session(id, &workspace.id, SessionState::Stopped);
    session.target_template_id = "container".into();
    mj_controller::database::save_session(&session).unwrap();
    let state = test_runtime_state_loading_the_store();
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;

    let sizes = PaneSizes {
        sessions: PaneSize::Minimized,
        ..Default::default()
    };
    let mut layout = ConversationLayout::default();
    layout.sessions.insert(layout.focus, id.into());
    http.save_workspace_pane_sizes(workspace.id.clone(), sizes)
        .await
        .unwrap();
    http.save_workspace_layout(workspace.id.clone(), layout.clone())
        .await
        .unwrap();
    let mount_history = vec![PathBuf::from("/retained/mount")];
    assert!(
        http.set_session_container_settings(id.into(), None, vec![], mount_history.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        http.stored_session_metadata()
            .await
            .unwrap()
            .mount_history
            .get("local"),
        Some(&mount_history)
    );

    let gate = mj_controller::upgrade::gate();
    gate.begin_draining();
    assert!(gate.is_draining());
    let layouts = http
        .workspace_layouts(vec![workspace.id.clone()])
        .await
        .unwrap();
    assert_eq!(layouts.pane_sizes[&workspace.id], sizes);
    assert_eq!(layouts.conversation_layouts[&workspace.id], layout);
    let startup = http
        .dashboard_startup("terminal".into(), workspace.id)
        .await
        .unwrap();
    assert!(matches!(
        http.session_tail("missing".into(), startup.runtime.cursor)
            .await
            .unwrap(),
        mj_client::runtime_feed::SessionTailReply::NoTail
    ));
    http.record_operator_activity().await.unwrap();
    assert!(gate.is_draining());
    let refused = http.attach("terminal".into()).await.unwrap_err();
    assert!(
        refused
            .downcast_ref::<mj_client::http::HttpApiError>()
            .unwrap()
            .is_handoff()
    );
    assert!(state.attachments().is_empty());

    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn container_settings_late_admission_refuses_both_http_paths_without_writing() {
    let Some(_writer) = startup_prompt_test_store(
        "container_settings_late_admission_refuses_both_http_paths_without_writing",
    ) else {
        return;
    };
    let mut config = Config::default();
    config.targets.insert(
        "container".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: serde_json::from_value(serde_json::json!({})).unwrap(),
        },
    );
    config.save().unwrap();
    let workspace = mj_controller::database::create_workspace("Container settings").unwrap();
    let mut session =
        runtime_test_session("settings-session", &workspace.id, SessionState::Stopped);
    session.target_template_id = "container".into();
    mj_controller::database::save_session(&session).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let state = test_runtime_state_loading_the_store();
    let (_http, shutdown, server) = start_test_dashboard_http_api_on(state, listener).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let allocation = mj_core::state::SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 4 << 30,
    };
    let terminal = client
        .put(format!(
            "{base_url}/api/v1/sessions/{}/container-settings",
            session.id
        ))
        .json(&mj_client::http::ContainerSettingsRequest {
            resource_allocation: Some(allocation.clone()),
            mounts: Vec::new(),
            mount_history: vec![PathBuf::from("/refused/mount")],
        });
    let web = client.post(format!("{base_url}/api/v1/actions")).json(
        &crate::server::ControllerAction::SetContainerSettings {
            session_id: session.id.clone(),
            resource_allocation: Some(allocation),
            mounts: Vec::new(),
        },
    );
    let gate = mj_controller::upgrade::gate();
    gate.begin_draining();
    for request in [terminal, web] {
        let response = request
            .bearer_auth("direct-test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        assert_eq!(response.headers()[crate::server::UPGRADE_HEADER], "pending");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["code"], "daemon_handoff");
        let stored =
            mj_controller::database::load_state_from(&mj_controller::database::database_path())
                .unwrap();
        assert_eq!(stored.sessions[&session.id].resource_allocation, None);
        assert_eq!(stored.sessions[&session.id].updated_at, session.updated_at);
        assert!(stored.mount_history.is_empty());
    }
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn container_settings_mount_probe_is_cancellable_without_upgrade_admission() {
    let Some(_writer) = startup_prompt_test_store(
        "container_settings_mount_probe_is_cancellable_without_upgrade_admission",
    ) else {
        return;
    };
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
    let mut config = Config::default();
    config.targets.insert(
        "container".into(),
        mj_core::config::TargetTemplate::SshPodman {
            ssh: mj_core::config::SshConnection {
                host: "container-settings-cancellation.invalid".into(),
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
            container: serde_json::from_value(serde_json::json!({})).unwrap(),
        },
    );
    config.save().unwrap();
    let workspace = mj_controller::database::create_workspace("Container probe").unwrap();
    let mut session = runtime_test_session("settings-probe", &workspace.id, SessionState::Stopped);
    session.target_template_id = "container".into();
    mj_controller::database::save_session(&session).unwrap();

    let state = test_runtime_state_loading_the_store();
    let (http, shutdown, server) = start_test_dashboard_http_api(state).await;
    let id = session.id.clone();
    let request = mj_core::runtime::spawn(async move {
        http.set_session_container_settings(
            id,
            None,
            vec![mj_core::targets::AdditionalMount {
                source: "/remote/settings-source".into(),
                destination: "/settings".into(),
                access: mj_core::targets::MountAccess::Ro,
            }],
            Vec::new(),
        )
        .await
    });
    let pid = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(pid) = fs::read_to_string(&started)
                && let Ok(pid) = pid.parse::<u32>()
            {
                break pid;
            }
            assert!(
                !request.is_finished(),
                "mount validation finished before reaching SSH"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("OpenSSH proxy never started");
    assert!(daemon_process_is_alive(pid));
    let gate = mj_controller::upgrade::gate();
    let blockers = gate.blockers();
    assert!(
        blockers.is_empty(),
        "mount probe held upgrade admission: {:?}",
        blockers.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    assert!(
        gate.try_close(),
        "the in-flight SSH probe blocked the handoff"
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(30), async {
        while daemon_process_is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnect left the mount probe's SSH proxy alive");
    let stored =
        mj_controller::database::load_state_from(&mj_controller::database::database_path())
            .unwrap();
    assert!(stored.sessions[&session.id].additional_mounts.is_empty());
    assert!(stored.mount_history.is_empty());
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn terminal_setup_and_viewer_round_trip_and_only_writes_refuse_during_drain() {
    let Some(_writer) = startup_prompt_test_store(
        "terminal_setup_and_viewer_round_trip_and_only_writes_refuse_during_drain",
    ) else {
        return;
    };
    Config::default().save().unwrap();
    let state = test_runtime_state_loading_the_store();
    state
        .api_credential
        .set("external-api-test-credential".into())
        .unwrap();
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let original = http.config().await.unwrap();
    let mut updated = original.clone();
    updated.spinner = mj_core::config::SpinnerStyle::Wave;
    let original_json = serde_json::to_string(&original).unwrap();
    let updated_json = serde_json::to_string(&updated).unwrap();
    let saved = http
        .save_setup(original_json.clone(), updated_json.clone())
        .await
        .unwrap();
    assert_eq!(saved.spinner, updated.spinner);
    assert_eq!(http.config().await.unwrap().spinner, updated.spinner);
    let access = serde_json::to_value(state.web_viewer.access()).unwrap();
    assert_eq!(
        serde_json::to_value(http.web_access().await.unwrap()).unwrap(),
        access
    );
    assert_eq!(
        http.api_credential().await.unwrap(),
        "external-api-test-credential"
    );

    let gate = mj_controller::upgrade::gate();
    gate.begin_draining();
    assert!(http.first_run_setup_due().await.is_ok());
    assert_eq!(http.config().await.unwrap().spinner, updated.spinner);
    let paths = vec![mj_core::config::data_dir()];
    assert_eq!(
        http.check_project_directories(paths.clone()).await.unwrap(),
        vec![(paths[0].clone(), true)]
    );
    assert!(gate.blockers().is_empty());
    for error in [
        http.save_setup(updated_json, original_json)
            .await
            .unwrap_err(),
        http.save_spinner_style(original.spinner).await.unwrap_err(),
        http.refresh_quota().await.unwrap_err(),
    ] {
        let failure = error
            .downcast_ref::<mj_client::http::HttpApiError>()
            .unwrap();
        assert_eq!(failure.status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert!(failure.is_handoff(), "{failure}");
    }

    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_recovery_destroy_holds_bounded_admission_after_disconnect() {
    let Some(_writer) = startup_prompt_test_store(
        "terminal_recovery_destroy_holds_bounded_admission_after_disconnect",
    ) else {
        return;
    };
    let (state, recovery, _upgrades) = test_runtime_state_loading_the_store_with_live_recovery();
    let session_id = "held-orphan";
    // Hold the session slot so the HTTP backend cannot finish its teardown.
    let session_hold = recovery
        .try_enter_foreground(
            session_id,
            "test session reservation",
            mj_controller::recovery_gate::LifecycleOrigin::Person,
        )
        .unwrap();
    let (http, shutdown, server) = start_test_dashboard_http_api(state).await;
    let gate = mj_controller::upgrade::gate();
    let destroy = mj_core::runtime::spawn(async move {
        http.destroy_recovery(session_id.into(), "bare".into(), session_id.into(), false)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.active_labels().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the HTTP destroy must acquire admission");
    assert_eq!(gate.active_labels(), ["session destroy"]);
    assert!(!gate.try_close());
    assert!(gate.destroys_hold_the_stop());

    destroy.abort();
    assert!(destroy.await.unwrap_err().is_cancelled());
    // Wait for the HTTP server to drop the handler before checking ownership.
    shutdown.cancel();
    server.await.unwrap().unwrap();
    assert!(!gate.try_close());
    assert!(gate.destroys_hold_the_stop());
    drop(session_hold);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !gate.active_labels().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the detached destroy must finish and release admission");
    assert!(!gate.destroys_hold_the_stop());
    assert!(gate.try_close());
}

#[tokio::test]
async fn http_runtime_changes_serves_large_frames_and_waits_without_upgrade_admission() {
    use mj_client::runtime_feed::RuntimeFrame;
    let Some(_writer) = startup_prompt_test_store(
        "http_runtime_changes_serves_large_frames_and_waits_without_upgrade_admission",
    ) else {
        return;
    };
    let state = test_runtime_state();
    let text = "x".repeat(MAX_FRAME_BYTES + 1024);
    let mut config = Config::default();
    config.bundles.insert(
        "large".into(),
        mj_core::config::ProjectBundle {
            primary_repo: text.clone(),
            repositories: Vec::new(),
        },
    );
    state.owner().install_config(config);
    let (http, shutdown, server) = start_test_dashboard_http_api(state.clone()).await;
    let RuntimeFrame::Snapshot { cursor, projection } =
        http.runtime_changes(None, false).await.unwrap()
    else {
        panic!("initial runtime response must be a snapshot");
    };
    assert_eq!(
        projection.metadata.installed_config().bundles["large"].primary_repo,
        text
    );
    let RuntimeFrame::Delta { from, cursor, .. } = http
        .runtime_changes(Some(cursor.clone()), false)
        .await
        .unwrap()
    else {
        panic!("a current cursor must get a delta");
    };
    assert_eq!(from, cursor);
    let mut expired = cursor.clone();
    expired.incarnation = "expired-daemon".into();
    assert!(matches!(
        http.runtime_changes(Some(expired), false).await.unwrap(),
        RuntimeFrame::Snapshot { .. }
    ));

    let gate = mj_controller::upgrade::gate();
    gate.begin_draining();
    let wait = http.runtime_changes(Some(cursor), true);
    tokio::pin!(wait);
    tokio::select! {
        result = &mut wait => panic!("long poll must remain pending during drain: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert!(
        gate.try_close(),
        "the long poll must not hold admission: {:?}",
        gate.blockers()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    shutdown.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is::<DaemonTransportLost>(), "{error:#}");
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn streaming_operations_require_terminal_auth_and_invalid_login_refuses_before_streaming() {
    let state = test_runtime_state();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (http, shutdown, server) = start_test_dashboard_http_api_on(state, listener).await;
    let client = reqwest::Client::new();
    let adoption = serde_json::json!({
        "session_id": "orphan", "target_id": "bare", "all_instances": false,
    });
    for (path, body) in [
        (
            "runtime/changes",
            serde_json::json!({"wait": false, "subagent_parents": []}),
        ),
        ("recovery/adopt", adoption),
        (
            "login-terminal",
            serde_json::json!({"profile_id": "missing", "cols": 80, "rows": 24}),
        ),
        (
            "login-terminal/missing/input",
            serde_json::json!({"Resize": {"cols": 80, "rows": 24}}),
        ),
    ] {
        let response = client
            .post(format!("http://{address}/api/v1/{path}"))
            .bearer_auth("public-test-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{path}");
    }
    let Err(error) = http.open_login_terminal("missing".into(), 80, 24).await else {
        panic!("an unknown profile must be rejected before response headers");
    };
    let error = error.downcast::<mj_client::http::HttpApiError>().unwrap();
    assert_eq!(error.status, 400);
    assert!(error.message.contains("missing"));
    shutdown.cancel();
    server.await.unwrap().unwrap();
}
