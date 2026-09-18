use super::*;
use crate::controller::test_support::RefusingExecutor;
use tokio::io::AsyncWriteExt;

#[test]
fn newer_daemon_protocol_requires_updating_the_client() {
    assert!(ensure_supported_daemon_protocol(PROTOCOL_VERSION).is_ok());
    assert!(ensure_supported_daemon_protocol(PROTOCOL_VERSION - 1).is_ok());
    let error = ensure_supported_daemon_protocol(PROTOCOL_VERSION + 1).unwrap_err();
    assert!(error.to_string().contains("restart this client"));
}

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
        LifecycleKind::Close,
        Some(SessionState::Running)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Close,
        Some(SessionState::Checkpointing)
    ));
    assert!(!lifecycle_owns_worker_target(
        LifecycleKind::Close,
        Some(SessionState::Closing)
    ));
    assert!(lifecycle_owns_worker_target(
        LifecycleKind::Close,
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
fn a_close_past_its_verified_checkpoint_cannot_be_cancelled() {
    assert!(lifecycle_cancellable(
        LifecycleKind::Close,
        Some(SessionState::Running)
    ));
    assert!(lifecycle_cancellable(
        LifecycleKind::Close,
        Some(SessionState::Checkpointing)
    ));
    assert!(lifecycle_cancellable(
        LifecycleKind::Close,
        Some(SessionState::Closing)
    ));
    assert!(lifecycle_cancellable(LifecycleKind::Close, None));
    assert!(!lifecycle_cancellable(
        LifecycleKind::Close,
        Some(SessionState::Destroying)
    ));
    // Only a graceful close has this gate; a forced teardown keeps none.
    assert!(lifecycle_cancellable(
        LifecycleKind::ForceStop,
        Some(SessionState::Destroying)
    ));
    assert!(lifecycle_cancellable(
        LifecycleKind::Create,
        Some(SessionState::Destroying)
    ));
    assert!(lifecycle_cancellable(
        LifecycleKind::Move,
        Some(SessionState::Destroying)
    ));
}

/// After a restart, every in-flight lifecycle state is either owned by
/// something that resumes it or reported to the user. Nothing stays in flight
/// with nobody behind it (#1070).
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
        session.target = session_target;
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

#[test]
fn a_stop_on_a_record_left_mid_close_routes_to_recovery() {
    let target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "a".repeat(64),
        workspace_storage: Default::default(),
    });
    for state in [SessionState::Closing, SessionState::Destroying] {
        let mut session = runtime_test_session("session", "workspace", state);
        session.target = target.clone();
        assert_eq!(close_route(Some(&session)), CloseRoute::RecoverInterrupted);
        // Without a target there is nothing left for recovery to finish, and
        // nothing to checkpoint either: the close settles instead of waiting
        // on a relay that does not exist.
        session.target = None;
        assert_eq!(
            close_route(Some(&session)),
            CloseRoute::SettleWithoutCheckpoint
        );
    }

    let mut running = runtime_test_session("session", "workspace", SessionState::Running);
    running.target = target.clone();
    assert_eq!(close_route(Some(&running)), CloseRoute::Graceful);
    assert_eq!(close_route(None), CloseRoute::Graceful);

    // A session wedged in provisioning has no harness state and no relay, with
    // or without the container it managed to create (#1059).
    let mut provisioning = runtime_test_session("session", "workspace", SessionState::Provisioning);
    assert_eq!(
        close_route(Some(&provisioning)),
        CloseRoute::SettleWithoutCheckpoint
    );
    provisioning.target = target.clone();
    assert_eq!(
        close_route(Some(&provisioning)),
        CloseRoute::SettleWithoutCheckpoint
    );

    // The same session once startup reconciliation has failed it (#1070).
    let failed = runtime_test_session("session", "workspace", SessionState::Error);
    assert_eq!(
        close_route(Some(&failed)),
        CloseRoute::SettleWithoutCheckpoint
    );
    // A failed session that still names its target keeps a workspace worth
    // checkpointing, so it takes the graceful close.
    let mut failed_with_target = failed.clone();
    failed_with_target.target = target.clone();
    assert_eq!(close_route(Some(&failed_with_target)), CloseRoute::Graceful);

    let mut stopped = runtime_test_session("session", "workspace", SessionState::Stopped);
    assert_eq!(close_route(Some(&stopped)), CloseRoute::Done);
    stopped.target = target;
    assert_eq!(close_route(Some(&stopped)), CloseRoute::DeferredCleanup);
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

#[tokio::test]
async fn client_presence_is_global_and_detach_and_prune_remove_it() {
    let state = test_runtime_state();
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    handle_action(
        DaemonAction::Attach {
            client_id: "client-a".into(),
            pid: std::process::id(),
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect("attach presence");
    state
        .attachments()
        .insert("dead-client".into(), Attachment { pid: u32::MAX });

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
async fn workspace_deletion_guard_ignores_global_client_presence() {
    let state = test_runtime_state();
    state.attachments().insert(
        "client-a".into(),
        Attachment {
            pid: std::process::id(),
        },
    );
    assert!(!state.workspace_has_active_resume("workspace-a"));

    let (_completed, result) = tokio::sync::watch::channel(None);
    state.lifecycle.lock().unwrap().insert(
        "session-a".into(),
        ActiveLifecycle {
            operation_id: "resume-operation".into(),
            create_control: None,
            kind: LifecycleKind::Resume,
            cancelled: Arc::new(AtomicBool::new(false)),
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            resume_workspace_id: Some("workspace-a".into()),
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            move_source_closed: false,
            result,
        },
    );
    assert!(state.workspace_has_active_resume("workspace-a"));
}

#[tokio::test]
async fn checkpoint_lifecycle_guard_is_per_session() {
    let state = test_runtime_state();
    let (_completed, result) = tokio::sync::watch::channel(None);
    state.lifecycle.lock().unwrap().insert(
        "session-b".into(),
        ActiveLifecycle {
            operation_id: "resume-operation".into(),
            create_control: None,
            kind: LifecycleKind::Resume,
            cancelled: Arc::new(AtomicBool::new(false)),
            started_at_epoch_seconds: epoch_seconds().saturating_sub(45),
            active_stages: BTreeMap::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            move_source_closed: false,
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

fn test_runtime_state() -> Arc<RuntimeState> {
    let remote = spawn_remote_session_manager().unwrap();
    let recovery = crate::recovery::RecoveryCoordinator::spawn(remote.control.clone());
    let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        remote.control.clone(),
        &recovery.observer(),
    );
    Arc::new(RuntimeState::new_with_controller_loader(
        remote.control,
        Controller {
            config: Config::default(),
            state: mj_core::state::State::default(),
        },
        recovery.observer(),
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

struct TestRemoteManager {
    control: SessionManagerControl,
    requests: RemoteSessionRequests,
    publisher: RemoteSessionPublisher,
    _shutdown: SessionManagerShutdown,
    _targets: tokio::sync::watch::Sender<Vec<RelaySessionTarget>>,
}

impl TestRemoteManager {
    async fn new() -> Self {
        let channels = spawn_remote_session_manager().expect("remote manager");
        let session_id = "session-1";
        channels.targets.send_replace(vec![RelaySessionTarget {
            session_id: session_id.to_owned(),
            spec: CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let manager = Self {
            control: channels.control,
            requests: channels.requests,
            publisher: channels.publisher,
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
        manager
    }
}

fn test_runtime_state_with_manager(manager: &TestRemoteManager) -> Arc<RuntimeState> {
    let recovery = crate::recovery::RecoveryCoordinator::spawn(manager.control.clone());
    let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        manager.control.clone(),
        &recovery.observer(),
    );
    Arc::new(RuntimeState::new_with_controller_loader(
        manager.control.clone(),
        Controller {
            config: Config::default(),
            state: mj_core::state::State::default(),
        },
        recovery.observer(),
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

fn test_metadata(address: SocketAddr) -> DaemonMetadata {
    DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: 1,
        address,
        token: "right-token".into(),
        started_at: "now".into(),
        build_version: "test".into(),
    }
}

#[tokio::test]
async fn in_process_reviewer_forwarding_stops_when_the_caller_goes_away() {
    let mut manager = TestRemoteManager::new().await;
    let (reply, response) = tokio::sync::oneshot::channel();
    let forwarding = tokio::spawn(forward_in_process_session_request(
        RemoteSessionRequest::Reviewer {
            session_id: "session-1".into(),
            role: None,
            action: crate::session_manager::ReviewerAction::Status,
            reply,
        },
        manager.control.clone(),
    ));
    let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
        .await
        .expect("reviewer forwarding did not reach the manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Reviewer { mut reply, .. } = request else {
        panic!("expected the forwarded reviewer request")
    };
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), reply.closed())
        .await
        .expect("in-process forwarding kept the actor reply alive");
    forwarding.await.expect("forwarding task panicked");
}

#[tokio::test]
async fn daemon_drops_in_flight_reviewer_work_when_the_client_eof_arrives() {
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
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
            action: DaemonAction::ReviewerAction {
                session_id: "session-1".into(),
                role: None,
                action: crate::session_manager::ReviewerAction::Status,
            },
        },
    )
    .await
    .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
        .await
        .expect("reviewer request did not reach the manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Reviewer { mut reply, .. } = request else {
        panic!("expected a reviewer request")
    };
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), reply.closed())
        .await
        .expect("daemon kept reviewer work alive after client EOF");
    assert!(server.await.expect("daemon task panicked").is_ok());
}

#[tokio::test]
async fn daemon_peek_keeps_a_pipelined_request_for_the_next_loop() {
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
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
    for (request_id, action) in [
        (
            1,
            DaemonAction::ReviewerAction {
                session_id: "session-1".into(),
                role: None,
                action: crate::session_manager::ReviewerAction::Pause,
            },
        ),
        (2, DaemonAction::Ping),
    ] {
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id,
                token: "right-token".into(),
                action,
            },
        )
        .await
        .unwrap();
    }
    let request = tokio::time::timeout(Duration::from_secs(5), manager.requests.recv())
        .await
        .expect("reviewer request did not reach the manager")
        .expect("manager request stream ended");
    let RemoteSessionRequest::Reviewer { reply, .. } = request else {
        panic!("expected a reviewer request")
    };
    reply
        .send(Ok(crate::session_manager::ReviewerOutcome::Paused))
        .expect("daemon still awaits the reviewer reply");

    let first: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert!(matches!(
        first.result,
        Ok(DaemonReply::Reviewer(outcome))
            if matches!(*outcome, crate::session_manager::ReviewerOutcome::Paused)
    ));
    let second: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
    assert!(matches!(second.result, Ok(DaemonReply::Pong)));
    drop(stream);
    let _ = server.await.expect("daemon task panicked");
}

#[tokio::test]
async fn daemon_client_eof_does_not_cancel_a_submitted_mutation() {
    let mut manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
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

fn runtime_test_session(id: &str, workspace_id: &str, state: SessionState) -> SessionRecord {
    SessionRecord {
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        id: id.into(),
        workspace_id: workspace_id.into(),
        title: id.into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "local".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        container_cpus: None,
        container_memory: None,
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
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

#[test]
fn runtime_records_include_global_history_but_only_local_active_sessions() {
    let local = runtime_test_session("local", "workspace-a", SessionState::Running);
    let remote = runtime_test_session("remote", "workspace-b", SessionState::Running);
    let history = runtime_test_session("history", "deleted-workspace", SessionState::Stopped);
    let controller = Controller {
        config: Config::default(),
        state: mj_core::state::State {
            sessions: [local, remote, history]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            ..mj_core::state::State::default()
        },
    };
    let records = runtime_records_for_workspace(&controller, &BTreeSet::from(["local".to_owned()]));
    let ids = records
        .iter()
        .map(|session| session.id.as_str())
        .collect::<BTreeSet<_>>();

    assert_eq!(ids, BTreeSet::from(["history", "local"]));
}

fn runtime_test_subagent(child_session_id: &str, parent_session_id: &str) -> SubagentRecord {
    SubagentRecord {
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
    }
}

#[test]
fn runtime_subagents_include_only_relations_whose_child_is_in_the_returned_records() {
    let parent_a = runtime_test_session("parent-a", "workspace-a", SessionState::Running);
    let child_a1 = runtime_test_session("child-a1", "workspace-a", SessionState::Running);
    let child_a2 = runtime_test_session("child-a2", "workspace-a", SessionState::Running);
    let parent_b = runtime_test_session("parent-b", "workspace-b", SessionState::Running);
    let child_b1 = runtime_test_session("child-b1", "workspace-b", SessionState::Running);
    let mut state = mj_core::state::State {
        sessions: [
            parent_a.clone(),
            child_a1.clone(),
            child_a2.clone(),
            parent_b.clone(),
            child_b1.clone(),
        ]
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect(),
        ..mj_core::state::State::default()
    };
    state.subagents = [
        runtime_test_subagent(&child_a1.id, &parent_a.id),
        runtime_test_subagent(&child_a2.id, &parent_a.id),
        runtime_test_subagent(&child_b1.id, &parent_b.id),
    ]
    .into_iter()
    .map(|subagent| (subagent.child_session_id.clone(), subagent))
    .collect();
    let controller = Controller {
        config: Config::default(),
        state,
    };

    let workspace_a_ids = BTreeSet::from([
        "parent-a".to_owned(),
        "child-a1".to_owned(),
        "child-a2".to_owned(),
    ]);
    let workspace_a_records = runtime_records_for_workspace(&controller, &workspace_a_ids);
    let workspace_a_subagents = runtime_subagents_for_workspace(&controller, &workspace_a_records);
    let mut workspace_a_child_ids = workspace_a_subagents
        .iter()
        .map(|subagent| subagent.child_session_id.as_str())
        .collect::<Vec<_>>();
    workspace_a_child_ids.sort_unstable();
    assert_eq!(workspace_a_child_ids, ["child-a1", "child-a2"]);

    let all_ids = BTreeSet::from([
        "parent-a".to_owned(),
        "child-a1".to_owned(),
        "child-a2".to_owned(),
        "parent-b".to_owned(),
        "child-b1".to_owned(),
    ]);
    let all_records = runtime_records_for_workspace(&controller, &all_ids);
    let all_subagents = runtime_subagents_for_workspace(&controller, &all_records);
    let mut all_child_ids = all_subagents
        .iter()
        .map(|subagent| subagent.child_session_id.as_str())
        .collect::<Vec<_>>();
    all_child_ids.sort_unstable();
    assert_eq!(all_child_ids, ["child-a1", "child-a2", "child-b1"]);
}

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
    .map(|subagent| (subagent.child_session_id.clone(), subagent))
    .collect();

    let children = active_child_session_ids(&state, &parent.id);

    assert_eq!(children, vec!["running-child".to_owned()]);
}

#[tokio::test]
async fn daemon_records_definitive_missing_workspaces_without_an_attached_surface() {
    let state = test_runtime_state();
    let session = runtime_test_session("missing", "workspace", SessionState::Running);
    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .insert(session.id.clone(), session.clone());
    assert!(state.attachments.lock().unwrap().is_empty());
    let mut view = ManagedSessionView {
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
    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .state = SessionState::Closing;
    assert!(state.missing_target_record(&session.id, &view).is_none());
    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .state = SessionState::Error;
    assert!(state.missing_target_record(&session.id, &view).is_none());
}

#[tokio::test]
async fn review_host_notifier_wakes_runtime_revision_subscribers() {
    let revisions = RuntimeRevisions::new(40);
    let mut subscriber = revisions.subscribe();
    // TurnReviewHost's behavior tests prove that view insert/change/remove
    // invokes this callback. This proves the production callback wired by
    // RuntimeState wakes the daemon and phone revision feed.
    let notify_review_publication = revisions.notifier();

    notify_review_publication();
    tokio::time::timeout(Duration::from_secs(1), subscriber.changed())
        .await
        .expect("review publication did not wake runtime subscribers")
        .expect("runtime revision publisher stopped");

    assert_eq!(*subscriber.borrow_and_update(), 41);
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
async fn workspace_publication_reaches_existing_phone_subscriber() {
    let state = test_runtime_state();
    let mut workspaces = state.workspaces();
    let expected = WorkspaceRecord {
        id: "workspace-1".into(),
        name: "Reliability".into(),
        created_at: "2026-08-30T00:00:00Z".into(),
        last_opened_at: "2026-08-30T00:00:00Z".into(),
        session_count: 0,
    };

    state.publish_workspaces(vec![expected.clone()]);
    tokio::time::timeout(Duration::from_secs(1), workspaces.changed())
        .await
        .expect("workspace publication timed out")
        .expect("workspace publisher stopped");

    assert_eq!(workspaces.borrow_and_update().as_slice(), &[expected]);
}

#[tokio::test]
async fn framing_round_trips_payloads_larger_than_a_pipe_buffer() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let sender = tokio::spawn(async move {
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
    let sender = tokio::spawn(async move {
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
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let server_metadata = metadata.clone();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
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
    };
    let mut client = ManagementClient::new(DaemonClient::connect(metadata).await.unwrap());
    let (mut peer, _) = listener.accept().await.unwrap();
    let stop = tokio::spawn(async move { client.stop().await });
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
    };
    let server_metadata = metadata.clone();
    let server = tokio::spawn(async move {
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
    };
    let server_metadata = metadata.clone();
    let server = tokio::spawn(async move {
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

#[test]
fn client_presence_and_workspace_listing_use_global_wire_shapes() {
    let attach = serde_json::to_value(DaemonAction::Attach {
        client_id: "client-a".into(),
        pid: 4242,
    })
    .unwrap();
    assert_eq!(
        attach,
        serde_json::json!({
            "action": "attach",
            "arguments": {"client_id": "client-a", "pid": 4242},
        })
    );

    let listing = serde_json::to_value(WorkspaceListing {
        workspace: WorkspaceRecord {
            id: "workspace-a".into(),
            name: "Workspace A".into(),
            created_at: "2026-09-01T00:00:00Z".into(),
            last_opened_at: "2026-09-01T00:00:00Z".into(),
            session_count: 0,
        },
    })
    .unwrap();
    assert!(listing.get("attached_pids").is_none());
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
        };
        let server_metadata = metadata.clone();
        let server = tokio::spawn(async move {
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
    ]
}

#[tokio::test]
async fn management_client_talks_to_every_released_protocol_version() {
    for transcript in released_protocol_transcripts() {
        let protocol_version = transcript.protocol_version;
        let daemon_build = transcript.daemon_build;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
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
    let first = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Close, {
            let starts = starts.clone();
            let release = release.clone();
            move |_state, _session_id, _cancelled| async move {
                starts.fetch_add(1, Ordering::AcqRel);
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    tokio::task::yield_now().await;
    let second = state
        .start_or_join_lifecycle(
            "session-1".into(),
            LifecycleKind::Close,
            |_state, _session_id, _cancelled| async move {
                panic!("joined lifecycle request started duplicate work")
            },
        )
        .unwrap();
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert!(
        state
            .start_or_join_lifecycle(
                "session-1".into(),
                LifecycleKind::Resume,
                |_state, _session_id, _cancelled| async move { Ok(DaemonLifecycleResult::Done) },
            )
            .is_err()
    );

    // The daemon task is independent of either client waiter.
    drop(first);
    release.notify_one();
    assert!(matches!(
        RuntimeState::wait_lifecycle_result(second).await.unwrap(),
        DaemonLifecycleResult::Done
    ));
    assert_eq!(starts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn close_keeps_worker_target_available_for_checkpoint_lease() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Close, {
            let release = release.clone();
            move |_state, _session_id, _cancelled| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();

    assert!(
        state
            .worker_poll_exclusion_session_ids(
                &state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            )
            .is_empty()
    );

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
                move |_, _, _| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .unwrap();
    assert!(crate::controller::move_session::move_owns_session(
        "move-join-one"
    ));
    let duplicate = state
        .start_or_join_lifecycle_with_key(
            "move-join-one".into(),
            LifecycleKind::Move,
            None,
            Some("profile-a/target-a/discard".into()),
            |_, _, _| async move { panic!("duplicate move launched a second writer") },
        )
        .unwrap();
    assert!(
        state
            .start_or_join_lifecycle_with_key(
                "move-join-one".into(),
                LifecycleKind::Move,
                None,
                Some("profile-b/target-a/discard".into()),
                |_, _, _| async move { Ok(DaemonLifecycleResult::Done) },
            )
            .is_err()
    );
    let unrelated = state
        .start_or_join_lifecycle_with_key(
            "move-join-two".into(),
            LifecycleKind::Move,
            None,
            Some("profile-b/target-b/start".into()),
            |_, _, _| async move { Ok(DaemonLifecycleResult::Done) },
        )
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
    assert!(!crate::controller::move_session::move_owns_session(
        "move-join-two"
    ));
    drop(first); // An initiating client can disappear without cancelling.
    release.notify_one();
    let channel = duplicate.clone();
    RuntimeState::wait_lifecycle_result(duplicate)
        .await
        .unwrap();
    state.remove_completed_lifecycle(&channel);
    assert!(!crate::controller::move_session::move_owns_session(
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
            |_, _, _| async move { panic!("injected move task panic") },
        )
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
    assert!(!crate::controller::move_session::move_owns_session(
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
            move |_state, _session_id, _cancelled| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    assert_eq!(
        state.worker_poll_exclusion_session_ids(
            &state
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        ),
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
        let lifecycle = state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stages = &lifecycle.get("session-1").unwrap().active_stages;
        assert_eq!(stages.get(&ProvisionStage::Cloning).unwrap().0, 1);
        assert_eq!(stages.get(&ProvisionStage::Syncing).unwrap().0, 1);
    }
    executor.stage_finished(ProvisionStage::Cloning);
    executor.stage_finished(ProvisionStage::Syncing);
    assert!(
        state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
            move |state, session_id, cancelled| async move {
                let executor = DaemonStageReportingExecutor::new(
                    crate::targets::ProcessExecutor,
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
fn force_destruction_enumerates_only_the_workspaces_active_sessions_oldest_first() {
    let mut oldest = runtime_test_session("oldest", "workspace-a", SessionState::Provisioning);
    oldest.created_at = "2026-09-01T00:00:00Z".into();
    let newest = runtime_test_session("newest", "workspace-a", SessionState::Error);
    let elsewhere = runtime_test_session("elsewhere", "workspace-b", SessionState::Running);
    let history = runtime_test_session("history", "workspace-a", SessionState::Stopped);
    let controller = Controller {
        config: Config::default(),
        state: mj_core::state::State {
            sessions: [oldest, newest, elsewhere, history]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            ..mj_core::state::State::default()
        },
    };

    assert_eq!(
        active_sessions_for_force_destruction(&controller, "workspace-a"),
        vec!["oldest".to_owned(), "newest".to_owned()]
    );
    assert_eq!(
        active_sessions_for_force_destruction(&controller, "workspace-b"),
        vec!["elsewhere".to_owned()]
    );
}

#[test]
fn force_destroy_serializes_as_its_own_lifecycle_kind() {
    assert_eq!(
        serde_json::to_string(&RuntimeLifecycleKind::ForceDestroy).unwrap(),
        "\"force_destroy\""
    );
}

#[test]
fn create_cancellation_and_commit_have_one_winner() {
    for _ in 0..32 {
        let control = CreateSessionControl::default();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let canceller = {
            let control = control.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                control.request_cancel()
            })
        };
        barrier.wait();
        let committed = control.grant_commit();
        let cancelled = canceller.join().expect("canceller panicked");
        assert_ne!(committed, cancelled);
        assert_eq!(control.cancelled.load(Ordering::Acquire), cancelled);
        assert!(!control.is_cancellable());
        assert!(!control.request_cancel());
        assert!(!control.grant_commit());
    }
}

#[tokio::test]
async fn completed_stop_stays_visible_until_cleanup_takes_ownership() {
    let state = test_runtime_state();
    let (complete, result) = tokio::sync::watch::channel(None);
    state.lifecycle.lock().unwrap().insert(
        "cleanup-gap".into(),
        ActiveLifecycle {
            operation_id: "closing-operation".into(),
            create_control: None,
            kind: LifecycleKind::Close,
            cancelled: Arc::new(AtomicBool::new(false)),
            started_at_epoch_seconds: 1,
            active_stages: BTreeMap::new(),
            resume_workspace_id: None,
            resume_destination: None,
            notice: None,
            request_key: None,
            _move_guard: None,
            move_source_closed: false,
            result: result.clone(),
        },
    );
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
            move |_, _, _| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
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
#[tokio::test]
async fn session_projection_reads_lifecycles_without_relocking_the_controller() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    let running = state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Close, {
            let release = release.clone();
            move |_, _, _| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
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
            move |_, _, _| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    let first_id = state.active_lifecycles()[0].operation_id.clone();
    let joined = state
        .start_or_join_lifecycle("identity".into(), LifecycleKind::Resume, |_, _, _| async {
            panic!("joined operation must not run twice")
        })
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
            move |_, _, _| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    let second_id = state.active_lifecycles()[0].operation_id.clone();
    assert_ne!(second_id, first_id);
    state.remove_completed_lifecycle(&first);
    assert_eq!(state.active_lifecycles()[0].operation_id, second_id);
    release.notify_one();
    RuntimeState::wait_lifecycle_result(second).await.unwrap();
}

#[tokio::test]
async fn close_waits_for_cancelled_or_committed_provisioning_to_release_ownership() {
    for committed in [false, true] {
        let state = test_runtime_state();
        let control = CreateSessionControl::default();
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
                    move |_, _, _| async move {
                        release.notified().await;
                        Ok(DaemonLifecycleResult::Done)
                    }
                },
            )
            .unwrap();
        if committed {
            assert!(control.grant_commit());
        }
        state.request_close("close-race");
        let waiter = {
            let state = state.clone();
            tokio::spawn(async move { state.wait_before_close("close-race").await })
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
        assert!(!state.lifecycle.lock().unwrap().contains_key("close-race"));
    }
}

#[tokio::test]
async fn committed_creation_cannot_be_cancelled_by_another_surface() {
    let state = test_runtime_state();
    let control = CreateSessionControl::default();
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
                move |_, _, _| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            },
        )
        .unwrap();
    assert!(state.active_lifecycles()[0].cancellable);
    assert!(control.grant_commit());
    assert!(!state.active_lifecycles()[0].cancellable);
    assert!(state.cancel_lifecycle("committed").is_err());
    state.cancel_lifecycle_if_active("committed");
    assert!(!control.cancelled.load(Ordering::Acquire));
    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

#[tokio::test]
async fn a_close_removing_the_target_stops_offering_cancellation() {
    let state = test_runtime_state();
    let mut session = runtime_test_session("destroying", "workspace", SessionState::Closing);
    session.target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "a".repeat(64),
        workspace_storage: Default::default(),
    });
    state
        .controller
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .state
        .sessions
        .insert(session.id.clone(), session.clone());
    let release = Arc::new(tokio::sync::Notify::new());
    let result = state
        .start_or_join_lifecycle("destroying".into(), LifecycleKind::Close, {
            let release = release.clone();
            move |_state, _session_id, _cancelled| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();

    // Before the checkpoint gate the stop is still cancellable.
    assert!(state.active_lifecycles()[0].cancellable);

    session.state = SessionState::Destroying;
    state
        .controller
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .state
        .sessions
        .insert(session.id.clone(), session);

    assert!(!state.active_lifecycles()[0].cancellable);
    let error = state.cancel_lifecycle("destroying").unwrap_err();
    assert!(
        error.to_string().contains("cannot be cancelled"),
        "unexpected error: {error:#}"
    );
    assert!(
        !state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get("destroying")
            .expect("lifecycle entry")
            .cancelled
            .load(Ordering::Acquire),
        "a refused cancel must not reach the running teardown"
    );

    release.notify_one();
    RuntimeState::wait_lifecycle_result(result).await.unwrap();
}

#[tokio::test]
async fn force_destruction_preempts_a_running_lifecycle_and_waits_for_it() {
    let state = test_runtime_state();
    let release = Arc::new(tokio::sync::Notify::new());
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            let release = release.clone();
            move |_state, _session_id, _cancelled| async move {
                release.notified().await;
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    tokio::task::yield_now().await;

    let preempt_state = state.clone();
    let preempted =
        tokio::spawn(async move { preempt_state.preempt_active_lifecycle("session-1").await });
    tokio::task::yield_now().await;
    {
        let lifecycle = state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            lifecycle
                .get("session-1")
                .expect("lifecycle entry")
                .cancelled
                .load(Ordering::Acquire),
            "preemption must cancel the running operation"
        );
    }

    release.notify_one();
    preempted
        .await
        .expect("preempt task")
        .expect("a cancelled-and-finished lifecycle lets force destruction proceed");
}

#[tokio::test(start_paused = true)]
async fn force_destruction_preemption_times_out_without_destroying() {
    let state = test_runtime_state();
    state
        .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
            |_state, _session_id, _cancelled| async move {
                // A lifecycle that ignores cancellation forever.
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(DaemonLifecycleResult::Done)
            }
        })
        .unwrap();
    tokio::task::yield_now().await;

    let error = state
        .preempt_active_lifecycle("session-1")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("did not stop after cancellation"),
        "{error:#}"
    );
}

/// A background image download belongs to the daemon, not to a session, so
/// whichever workspace the person is looking at shows it.
#[test]
fn a_daemon_owned_notice_reaches_every_workspace_snapshot() {
    let session_ids = BTreeSet::from(["018f9dd2-a3b4".to_owned()]);
    let daemon_notice = RuntimeNotice {
        id: 1,
        session_id: String::new(),
        text: "Downloading image ghcr.io/example/dev:latest for local podman\u{2026}".to_owned(),
    };
    let own_session = RuntimeNotice {
        id: 2,
        session_id: "018f9dd2-a3b4".to_owned(),
        text: "Mounted /data read-only.".to_owned(),
    };
    let other_session = RuntimeNotice {
        id: 3,
        session_id: "018f9dd2-cccc".to_owned(),
        text: "Mounted /data read-only.".to_owned(),
    };

    assert!(snapshot::notice_reaches_workspace(
        &daemon_notice,
        &session_ids
    ));
    assert!(snapshot::notice_reaches_workspace(
        &own_session,
        &session_ids
    ));
    assert!(!snapshot::notice_reaches_workspace(
        &other_session,
        &session_ids
    ));
}

/// A view of `session-1` whose harness is ready for its first prompt.
fn ready_startup_view() -> ManagedSessionView {
    let materialized = mj_core::state::MaterializedSession::empty("session-1");
    let operational = mj_core::relay::RelayOperationalState {
        goal: Default::default(),
        capacity_retry: None,
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
        checkpoint_only: false,
        acp_ready: Some(true),
        agent_capabilities: None,
        agent_info: None,
        steering_supported: None,
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
        background_work_known: None,
        tools_in_flight: Vec::new(),
        activity: None,
    };
    ManagedSessionView {
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational,
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
    let mut session = runtime_test_session("session-1", "workspace", SessionState::Provisioning);
    draft.clone_into(&mut session.draft_input);
    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .insert(session.id.clone(), session);
}

/// The smallest archived snapshot a hand-off step can carry.
fn empty_archive_snapshot() -> mj_core::archive::CanonicalSessionSnapshot {
    mj_core::archive::CanonicalSessionSnapshot {
        event_frontier: 0,
        event_frontier_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        session: mj_core::archive::CanonicalSessionState {
            execution: mj_core::archive::CanonicalExecutionState::Idle,
            last_activity_at_ms: None,
            session_title: None,
            configuration: BTreeMap::new(),
        },
        transcript: Vec::new(),
        queued_prompts: Vec::new(),
    }
}

fn queued_prompt_action(text: &str) -> DaemonAction {
    DaemonAction::QueueStartupPrompt {
        session_id: "session-1".into(),
        text: text.into(),
        inherited_draft: None,
    }
}

fn submitted_prompt_text(command: &RelayCommand) -> String {
    let RelayCommand::Prompt { prompt } = command else {
        panic!("expected a prompt command, got {command:?}")
    };
    prompt
        .iter()
        .map(|block| match block {
            agent_client_protocol::schema::v1::ContentBlock::Text(text) => text.text.clone(),
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

fn notice_texts(state: &RuntimeState) -> Vec<String> {
    state
        .notices
        .lock()
        .unwrap()
        .iter()
        .map(|notice| notice.text.clone())
        .collect()
}

async fn next_submit(
    manager: &mut TestRemoteManager,
) -> (
    String,
    tokio::sync::oneshot::Sender<std::result::Result<u64, String>>,
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

/// A restored session's hand-off is queued ahead of any prompt, so the prompt
/// is never submitted before the context it is meant to read. Here the
/// hand-off cannot be installed -- a remote session manager refuses it -- so
/// the prompt behind it is put back into the draft instead of going out
/// without its context.
#[tokio::test]
async fn a_queued_hand_off_runs_before_the_prompt_behind_it() {
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
        .expect("queue the hand-off");
    state
        .queue_startup_step(
            "session-1",
            StartupStep::Prompt {
                text: "after the hand-off".into(),
                inherited_draft: None,
            },
            &cancellation,
        )
        .expect("queue the prompt behind the hand-off");
    manager
        .publisher
        .publish("session-1".into(), ready_startup_view())
        .await
        .expect("publish the ready view");

    let restored = wait_for_draft(&state, "after the hand-off").await;
    assert_eq!(restored, "after the hand-off");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), manager.requests.recv())
            .await
            .is_err(),
        "the prompt was submitted even though its hand-off never was"
    );
    let notices = notice_texts(&state);
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("without its archived hand-off")),
        "the dropped hand-off was not reported: {notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("it is back in the composer draft")),
        "the restored prompt was not reported: {notices:?}"
    );
}

/// A session that stops while its prompt waits ends the wait at once, and the
/// text joins whatever draft the session already had.
#[tokio::test]
async fn a_session_that_stops_returns_its_queued_prompt_to_the_draft() {
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
    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .state = SessionState::Stopped;

    let draft = wait_for_draft(&state, "undelivered prompt").await;
    assert_eq!(draft, "half-written note\n\nundelivered prompt");
    let notices = notice_texts(&state);
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("it is back in the composer draft")),
        "the undelivered prompt was not reported: {notices:?}"
    );
}

/// A refused submit puts the refused text back, and the prompts still waiting
/// behind it with it, in the order they were typed.
#[tokio::test]
async fn a_refused_submit_restores_the_prompt_and_the_rest_of_the_queue() {
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
        .send(Err("the harness refused the prompt".into()))
        .expect("the drain awaits the submit reply");

    let draft = wait_for_draft(&state, "prompt behind it").await;
    assert_eq!(draft, "refused prompt\n\nprompt behind it");
}

/// Shutdown cancels a drain that is still waiting for a harness and returns
/// well inside its own bound, so quitting the daemon stays responsive.
#[tokio::test]
async fn cancelling_startup_prompts_returns_while_a_drain_is_waiting() {
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
    assert_eq!(session_draft_input(&state), "never delivered");
}

/// Queueing is refused when the text is empty, when the session can no longer
/// become ready, and when the daemon has no such session at all.
#[tokio::test]
async fn queueing_a_startup_prompt_is_refused_for_blank_text_and_unusable_sessions() {
    let manager = TestRemoteManager::new().await;
    let state = test_runtime_state_with_manager(&manager);
    insert_starting_session(&state, "");
    let metadata = test_metadata(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let cancellation = CancellationToken::new();

    let blank = handle_action(
        queued_prompt_action("   "),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect_err("a blank startup prompt was accepted");
    assert!(blank.to_string().contains("needs text"), "{blank:#}");

    let unknown = handle_action(
        DaemonAction::QueueStartupPrompt {
            session_id: "no-such-session".into(),
            text: "hello".into(),
            inherited_draft: None,
        },
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect_err("a prompt for an unknown session was accepted");
    assert!(
        unknown.to_string().contains("unknown session"),
        "{unknown:#}"
    );

    state
        .controller
        .lock()
        .unwrap()
        .state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .state = SessionState::Stopped;
    let stopped = handle_action(
        queued_prompt_action("hello"),
        &metadata,
        &state,
        &cancellation,
    )
    .await
    .expect_err("a prompt for a stopped session was accepted");
    assert!(
        stopped.to_string().contains("cannot take a queued prompt"),
        "{stopped:#}"
    );
}

/// Wait for a failed delivery to put text back into the in-memory record.
/// Unit tests run without a database writer, so the persisted copy fails and
/// is reported; the in-memory record is what a surface would read.
async fn wait_for_draft(state: &Arc<RuntimeState>, expected: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let draft = session_draft_input(state);
        if draft.contains(expected) {
            return draft;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no queued prompt was restored into the session draft"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
