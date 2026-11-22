pub(crate) use super::stand_in::*;
use super::*;
use crate::targets::{CommandPlan, TargetRecoveryPlan};
#[cfg(unix)]
use crate::test_isolation::{IsolatedTest, test_name};
use mj_core::hex::lower_hex;

/// I1-12: a relay's final rejection reaches the user as the relay's reason
/// alone, not "relay 2.20.0 could not perform submit: relay rejected request
/// (InvalidState): …", and is not reported as unconfirmed.
// Hard-won: 8bf21ad0: A definite relay refusal must not return as an unconfirmed delivery or leak its error chain.
#[test]
fn a_final_rejection_tells_the_submitter_only_the_relays_reason() {
    let rejected = anyhow::Error::new(crate::worker_client::RelayRejected(
        mj_core::relay::relay_protocol_error(
            mj_core::relay::RelayErrorCode::InvalidState,
            "/clear requires an idle session",
            false,
            None,
        ),
    ))
    .context("relay 2.20.0 could not perform submit");
    let failure = actor::submit_failure(&rejected);
    assert!(!failure.unconfirmed);
    assert_eq!(
        failure.code,
        Some(mj_core::relay::RelayErrorCode::InvalidState)
    );
    assert_eq!(failure.message, "/clear requires an idle session");

    let lost = anyhow::anyhow!("connection reset").context("relay 2.20.0 could not perform submit");
    let failure = actor::submit_failure(&lost);
    assert!(failure.unconfirmed);
    assert!(failure.message.contains("connection reset"));
}

fn recovery_source_target() -> mj_core::state::TargetLocator {
    mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/test-worker").join(LEASED_RELAY_SESSION),
    }
}

// Hard-won: 9317fe87c: recovery exhausted its 30-second deadline during staging.
#[tokio::test]
#[cfg_attr(windows, ignore = "worker recovery commands require Unix")]
async fn recovery_allows_preparation_longer_than_the_previous_thirty_second_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("sh", ["-c", "sleep 31; printf dead"])
            .purpose("probe slowly staged recovery")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart after slow preparation".into(),
            commands: vec![CommandSpec::new(
                "touch",
                [directory
                    .path()
                    .join("restarted")
                    .to_string_lossy()
                    .into_owned()],
            )],
        }
        .into(),
    };
    assert_eq!(
        recover_worker(plan, false).await.unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert!(directory.path().join("restarted").exists());
}

#[tokio::test]
async fn session_adoption_deadline_also_bounds_an_unanswered_manager_request() {
    let (commands, mut requests) = mpsc::channel(1);
    let control = SessionManagerControl {
        session_gate: Arc::default(),
        credential_wakes: Arc::default(),
        harness_failure: Default::default(),
        guardian_stage1: Default::default(),
        model_policies: watch::channel(BTreeMap::new()).0,
        commands,
        launches: mpsc::unbounded_channel().0,
        session_cpu: watch::channel(SessionCpuTable::new()).1,
    };
    let request = mj_core::runtime::spawn(async move {
        control
            .wait_for_session("muse", Duration::from_millis(20))
            .await
    });
    let ManagerCommand::Session { mut reply, .. } = requests.recv().await.unwrap() else {
        panic!("expected Session lookup command");
    };
    // Retain the reply without answering, like an unresponsive manager.
    let error = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("the adoption deadline must bound an individual request")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("did not become available"));
    reply.closed().await;
}
#[cfg(unix)]
use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use sha2::Digest;

fn ordering_request(session_id: &str, command_id: &str) -> RemoteSessionRequest {
    let (reply, _response) = oneshot::channel();
    RemoteSessionRequest::Submit {
        session_id: session_id.into(),
        command_id: command_id.into(),
        command: RelayCommand::SetConfig {
            key: "effort".into(),
            value: "high".into(),
        },
        reply,
    }
}

/// `/effort` followed by a prompt has to reach the relay that way round,
/// or the prompt runs under the old setting. A bridge that spawns every
/// request concurrently loses that, so the order is pinned here: the
/// first request is held up, and the second must not overtake it.
// Hard-won: 0db1a867: A prompt must not overtake its preceding /effort update on the same relay.
#[tokio::test]
async fn one_session_keeps_its_requests_in_the_order_they_were_made() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut order = SessionRequestOrder::new();

    for command_id in ["first", "second", "third"] {
        let observed = Arc::clone(&observed);
        let release = Arc::clone(&release);
        order.dispatch(ordering_request("session-a", command_id), move |request| {
            let RemoteSessionRequest::Submit { command_id, .. } = request else {
                unreachable!("the fixture only submits")
            };
            async move {
                // Only the first request waits. If the order were lost,
                // the other two would finish while it is held.
                if command_id == "first" {
                    release.notified().await;
                }
                observed.lock().unwrap().push(command_id);
            }
        });
    }

    // Nothing may run while the first request is held. Yield generously:
    // the point is that the later requests never get to run, not that
    // they have not been polled yet.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        observed.lock().unwrap().is_empty(),
        "a later request overtook the one being held: {:?}",
        observed.lock().unwrap()
    );

    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while observed.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every request ran");
    assert_eq!(*observed.lock().unwrap(), ["first", "second", "third"]);
}

/// Ordering is per session: one session waiting on a slow relay must not
/// hold up another session's prompt.
#[tokio::test]
async fn different_sessions_still_overlap() {
    let finished = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut order = SessionRequestOrder::new();

    let held = Arc::clone(&release);
    let recorder = Arc::clone(&finished);
    order.dispatch(ordering_request("session-a", "slow"), move |_| async move {
        held.notified().await;
        recorder.lock().unwrap().push("slow");
    });
    let recorder = Arc::clone(&finished);
    order.dispatch(ordering_request("session-b", "fast"), move |_| async move {
        recorder.lock().unwrap().push("fast");
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while finished.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the other session ran while the first was held");
    assert_eq!(*finished.lock().unwrap(), ["fast"]);

    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while finished.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the held request ran once released");
}

#[tokio::test]
async fn request_queue_refuses_overload_without_blocking_another_session() {
    let mut order = SessionRequestOrder::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    order.dispatch(ordering_request("slow", "first"), move |_| async move {
        held.notified().await;
    });
    for _ in 1..32 {
        order.dispatch(ordering_request("slow", "queued"), |_| async {});
    }
    let (reply, refused) = oneshot::channel();
    let mut request = ordering_request("slow", "excess");
    if let RemoteSessionRequest::Submit { reply: sender, .. } = &mut request {
        *sender = reply;
    }
    order.dispatch(request, |_| async { panic!("refused request ran") });
    let error = refused.await.unwrap().unwrap_err();
    assert!(!error.unconfirmed);
    assert!(error.message.contains("queue is full"));
    let (done, finished) = oneshot::channel();
    order.dispatch(ordering_request("other", "independent"), |_| async move {
        done.send(()).unwrap();
    });
    tokio::time::timeout(Duration::from_secs(2), finished)
        .await
        .unwrap()
        .unwrap();
    release.notify_one();
    order.drain().await.unwrap();
}

#[tokio::test]
async fn dispatcher_drains_after_disconnect_and_releases_panicked_streams() {
    let mut order = SessionRequestOrder::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    order.dispatch(ordering_request("one", "panics"), |_| async move {
        held.notified().await;
        panic!("request panic fixture");
    });
    let (done, finished) = oneshot::channel();
    order.dispatch(ordering_request("one", "following"), |_| async move {
        done.send(()).unwrap();
    });
    // Closing the bridge must not abandon already admitted mutations.
    drop(order);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), finished)
        .await
        .unwrap()
        .unwrap();
}

// Hard-won: be7c009d: A failed worker must back off so reconnect logs cannot flood the live terminal.
#[test]
fn reconnect_delay_backs_off_and_stops_at_the_ceiling() {
    assert_eq!(reconnect_delay(1), RECONNECT_INTERVAL);
    assert_eq!(reconnect_delay(2), Duration::from_secs(2));
    assert_eq!(reconnect_delay(4), Duration::from_secs(8));
    assert_eq!(reconnect_delay(6), RECONNECT_BACKOFF_CEILING);
    assert_eq!(reconnect_delay(u32::MAX), RECONNECT_BACKOFF_CEILING);
}

#[test]
fn only_dead_worker_connection_failures_request_a_restart() {
    // The wording is deliberately unlike anything a matcher could have
    // been written against: the marker, not the message, decides.
    let reworded = anyhow::Error::new(RelayTransportDead::new(
        "the session proxy vanished mid-conversation",
    ))
    .context("connect to the session worker for checkpoint");
    assert!(worker_connect_needs_restart(&reworded), "{reworded:#}");
    assert!(!worker_connect_allows_live_restart(&reworded));

    // Text alone proves nothing now, not even the exact text the producing
    // sites still use: an unmarked failure must never restart a worker.
    for detail in [
        "relay proxy disconnected during hello",
        "Connection refused (os error 111)",
        "relay negotiated unsupported protocol 9",
        "controller projection is corrupt",
    ] {
        assert!(!worker_connect_needs_restart(&anyhow::anyhow!(detail)));
    }
}

/// The producing side of the same contract: a proxy that dies without
/// serving the handshake must ask for a worker restart, whatever its
/// failure happens to read like.
#[cfg(unix)]
#[tokio::test]
async fn a_proxy_that_dies_before_hello_requests_a_worker_restart() {
    let mut dead = target("sh");
    dead.spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("dead relay proxy fixture");

    let error = StandaloneSession::connect(&dead)
        .await
        .err()
        .expect("a proxy that exits cannot serve a session");

    assert!(worker_connect_needs_restart(&error), "{error:#}");
    assert!(worker_connect_allows_live_restart(&error));
}

/// A lease answer crosses a channel. Formatting the failure into a string
/// there would strip the cause and silently cost the checkpoint path its
/// restart decision, so prove the typed cause survives the handoff.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_lease_keeps_the_cause_that_decides_a_worker_restart() {
    let (commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, _view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    let mut dead = target("sh");
    dead.spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("dead relay proxy fixture");
    mj_core::runtime::spawn(run_session_actor(
        dead,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    let (reply, response) = oneshot::channel();
    commands_tx
        .send(ActorCommand::Lease {
            returns: _releases_tx.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            reply,
            idle_harness: None,
        })
        .await
        .unwrap();
    let error = response
        .await
        .expect("actor answered the lease request")
        .err()
        .expect("a dead proxy cannot be leased");

    assert!(worker_connect_needs_restart(&error), "{error:#}");
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_restarts_a_live_worker_only_after_a_failed_handshake() {
    let directory = tempfile::tempdir().unwrap();
    let restarted = directory.path().join("restarted");
    let recovery = |liveness: &str| WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: Some(WorkerWorkspace {
            target: mj_core::state::ManagedWorktreeTarget::Local,
            directory: directory.path().to_path_buf(),
        }),
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", [format!("{liveness}\n")])
            .purpose("probe test worker liveness")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test worker"),
            ],
        }
        .into(),
    };

    assert_eq!(
        recover_worker(recovery("alive"), false).await.unwrap(),
        WorkerRecoveryOutcome::Alive
    );
    assert!(!restarted.exists(), "a live worker must not be restarted");

    assert_eq!(
        recover_worker(recovery("starting"), true).await.unwrap(),
        WorkerRecoveryOutcome::Starting
    );
    assert!(
        !restarted.exists(),
        "a worker recovering its journal must not be restarted"
    );

    assert_eq!(
        recover_worker(recovery("alive"), true).await.unwrap(),
        WorkerRecoveryOutcome::RestartedUnresponsive
    );
    assert!(
        restarted.exists(),
        "a worker that cannot serve a fresh handshake is restarted"
    );
    std::fs::remove_file(&restarted).unwrap();

    assert_eq!(
        recover_worker(recovery("dead"), false).await.unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert!(restarted.exists(), "a confirmed dead worker is restarted");
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_reports_a_missing_bare_workspace_without_restarting() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("removed-worktree");
    let restarted = directory.path().join("worker-restarted");
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: Some(WorkerWorkspace {
            target: mj_core::state::ManagedWorktreeTarget::Local,
            directory: missing.clone(),
        }),
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["dead\n"])
            .purpose("probe test worker liveness")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "must not restart missing workspace worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test worker"),
            ],
        }
        .into(),
    };

    assert_eq!(
        recover_worker(plan, false).await.unwrap(),
        WorkerRecoveryOutcome::WorkspaceMissing(missing.clone())
    );
    assert!(
        !restarted.exists(),
        "a missing workspace must not be restarted"
    );
}

#[cfg(unix)]
// Hard-won: #1444: a missing workspace kept producing alternating relay errors.
#[tokio::test]
async fn missing_workspace_actor_parks_until_its_target_changes() {
    const CHILD: &str = "MJ_TEST_MISSING_WORKSPACE_ACTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "missing_workspace_actor_parks_until_its_target_changes",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut record = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    record.target = Some(Box::new(recovery_source_target()));
    crate::database::save_session(&record).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("worktree");
    let restarted = directory.path().join("worker-restarted");
    let mut target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.into(),
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
        worker_recovery: Some(WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: None,
            workspace: Some(WorkerWorkspace {
                target: mj_core::state::ManagedWorktreeTarget::Local,
                directory: missing.clone(),
            }),
            exit_record: None,
            liveness_probe: CommandSpec::new("printf", ["dead\n"]).into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart after workspace restore".into(),
                commands: vec![CommandSpec::new(
                    "touch",
                    [restarted.to_string_lossy().into_owned()],
                )],
            }
            .into(),
        }),
        project_memory: None,
    };
    let manager = spawn_session_manager().unwrap();
    manager.targets.send_replace(vec![target.clone()]);
    let handle = manager
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(5))
        .await
        .unwrap();
    let mut view = handle.view.clone();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            view.changed().await.unwrap();
            if matches!(
                view.borrow_and_update().error.as_ref(),
                Some(ViewError::TargetMissing(_))
            ) {
                break;
            }
        }
    })
    .await
    .expect("missing workspace was not reported");
    assert!(
        handle
            .sync_now()
            .await
            .unwrap_err()
            .to_string()
            .contains("working directory"),
        "a parked actor must reject new relay work with its definitive cause"
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(35)).await;
    assert!(!view.has_changed().unwrap(), "definitive view was replaced");
    std::fs::create_dir(&missing).unwrap();
    tokio::time::advance(Duration::from_secs(35)).await;
    assert!(!restarted.exists(), "parked actor restarted the worker");
    tokio::time::resume();

    target.spec = CommandSpec::new("sh", ["-c", "exit 1 # revised target"]);
    manager.targets.send_replace(vec![target]);
    tokio::time::timeout(Duration::from_secs(15), async {
        while !restarted.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new target did not resume recovery");
    manager.shutdown.shutdown().await.unwrap();
}

/// precision-3260: the worker stopped with "No space left on device", and
/// every recovery attempt uploaded a replacement into the same full disk.
/// Recovery now reads the dead worker's exit record, tells the storage owner,
/// and restarts nothing until a later measurement finds room.
// Hard-won: 540c9202: Full-disk recovery must not upload another worker onto the same full filesystem.
// Hard-won: e579584b9: a harness that failed to open on a full disk went to Error once space returned.
// Hard-won: #1620: a room-for-the-worker-root sample restarted a worker whose workspace mount was still full.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_waits_on_a_full_disk_and_restarts_once_space_is_measured() {
    // The storage board is per process; this host name is this test's own.
    let host = "full-disk-recovery-host";
    let directory = tempfile::tempdir().unwrap();
    let restarted = directory.path().join("worker-restarted");
    let stopped_at = chrono::Utc::now() - chrono::Duration::seconds(60);
    let exit_record = format!(
        // The marker says the harness never opened; the full disk is why,
        // so space returning must restart it rather than end in Error.
        r#"{{"reason":"relay coordinator failed: append journal: No space left on device (os error 28)","refusal":null,"at":"{}","version":"test","recent_log":["failed diagnostic\n"],"harness_startup_failure":{{"message":"ACP runtime failed: No space left on device (os error 28)"}}}}    "#,
        stopped_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    let plan = WorkerRecoveryPlan {
        source_target: mj_core::state::TargetLocator::SshBare {
            host: host.into(),
            // The workspace is on a mount of its own, apart from the worker
            // root, and the exit record does not say which one was full.
            workspace: "/home/jonathan/Projects/app".into(),
            worker_id: None,
        },
        target: None,
        workspace: None,
        exit_record: Some(
            CommandSpec::new("printf", ["%s", exit_record.as_str()])
                .purpose("read test worker exit record"),
        ),
        liveness_probe: CommandSpec::new("printf", ["dead\n"])
            .purpose("probe test worker liveness")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test worker"),
            ],
        }
        .into(),
    };

    let outcome = recover_worker(plan.clone(), false).await.unwrap();
    let WorkerRecoveryOutcome::StorageFull {
        host: full_host,
        problem,
    } = outcome
    else {
        panic!("expected a full-disk wait, got {outcome:?}");
    };
    assert_eq!(full_host, host);
    assert!(problem.starts_with("disk full: "), "{problem}");
    assert!(problem.contains("No space left on device"), "{problem}");
    assert!(!restarted.exists(), "a worker on a full disk was restarted");

    // Still full by measurement: still no restart.
    let now = chrono::Utc::now().timestamp() as u64;
    crate::target_storage::record_samples(
        &[crate::target_storage::test_storage_sample(
            host,
            0,
            40 << 30,
        )],
        now,
    );
    assert!(matches!(
        recover_worker(plan.clone(), false).await.unwrap(),
        WorkerRecoveryOutcome::StorageFull { .. }
    ));
    assert!(!restarted.exists());

    // The worker root has room, but the workspace's mount is still full: a
    // restart would fail the same way.
    crate::target_storage::record_samples(
        &[crate::target_storage::test_storage_sample(
            host,
            40 << 30,
            0,
        )],
        now,
    );
    let outcome = recover_worker(plan.clone(), false).await.unwrap();
    let WorkerRecoveryOutcome::StorageFull { problem, .. } = &outcome else {
        panic!("expected a wait on the workspace's mount, got {outcome:?}");
    };
    assert!(problem.contains("/home/jonathan/Projects"), "{problem}");
    assert!(!restarted.exists());

    // Space was freed. The old exit record predates this measurement, so it
    // no longer holds recovery back.
    crate::target_storage::record_samples(
        &[crate::target_storage::test_storage_sample(
            host,
            40 << 30,
            40 << 30,
        )],
        now + 1,
    );
    assert_eq!(
        recover_worker(plan, false).await.unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert!(
        restarted.exists(),
        "recovery did not resume once space was free"
    );
}

#[cfg(unix)]
// Hard-won: 58e37a647: A disconnected session with a marked exit kept polling.
#[test]
fn recovery_stops_at_a_marked_harness_startup_failure() {
    const CHILD: &str = "MJ_STARTUP_FAILURE_RECOVERY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(CHILD, "recovery_stops_at_a_marked_harness_startup_failure");
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let id = LEASED_RELAY_SESSION;
    let mut record = crate::database::read_durable_session_record(id)
        .unwrap()
        .unwrap();
    record.target = Some(Box::new(recovery_source_target()));

    let directory = tempfile::tempdir().unwrap();
    let worker_root = directory.path().join("worker root");
    let exit_path = worker_root.join(mj_core::relay::WORKER_EXIT_FILE);
    let worker_root_text = worker_root.to_str().unwrap();
    let backend = mj_core::targets::TargetLocator::LocalBare {
        worker_root: worker_root_text.to_owned(),
    };
    let exit_record =
        mj_worker_staging::worker_binary::worker_exit_record_command(&backend, worker_root_text);
    let executor = crate::targets::ProcessExecutor;
    let absent = executor.execute(&exit_record).unwrap();
    assert_eq!(absent.status, 0);
    assert!(
        mj_worker_staging::worker_binary::read_worker_exit_record(&absent.stdout)
            .unwrap()
            .is_none()
    );
    std::fs::create_dir(&worker_root).unwrap();

    let restarted = directory.path().join("worker-restarted");
    let cause = "ACP runtime failed: fixture transient load failure";
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        exit_record: Some(exit_record),
        liveness_probe: CommandSpec::new("printf", ["dead"]).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![CommandSpec::new(
                "touch",
                [restarted.to_string_lossy().into_owned()],
            )],
        }
        .into(),
    };
    let recover = || recover_worker_controlled(plan.clone(), false, Some(id), &executor);
    use mj_core::state::SessionState;
    for state in [SessionState::Running, SessionState::Disconnected] {
        record.state = state;
        crate::database::save_session(&record).unwrap();

        std::fs::create_dir(&exit_path).unwrap();
        assert!(recover().is_err(), "a failed read must defer recovery");
        std::fs::remove_dir(&exit_path).unwrap();
        assert!(!restarted.exists());

        // An interrupted write cannot repair itself, and a legacy record
        // with the same cause supplies no typed startup marker.
        for content in [
            r#"{"reason":"interrupted"#.to_owned(),
            serde_json::json!({"reason": cause}).to_string(),
        ] {
            std::fs::write(&exit_path, content).unwrap();
            assert_eq!(recover().unwrap(), WorkerRecoveryOutcome::RestartedDead);
            assert!(crate::database::load_worker_restart(id).unwrap().is_some());
            std::fs::remove_file(&restarted).unwrap();
        }

        let marked = serde_json::json!({
            "reason": "worker exited",
            "harness_startup_failure": {"message": cause},
        });
        std::fs::write(&exit_path, marked.to_string()).unwrap();
        assert_eq!(
            recover().unwrap(),
            WorkerRecoveryOutcome::HarnessStartupFailed
        );
        assert!(!restarted.exists());
        let failed = crate::database::read_durable_session_record(id)
            .unwrap()
            .unwrap();
        assert_eq!(failed.state, SessionState::Error);
        assert_eq!(failed.last_error.as_deref(), Some(cause));
        assert!(!session_target_is_pollable(&failed));
        assert!(crate::database::load_worker_restart(id).unwrap().is_none());
        assert_eq!(recover().unwrap(), WorkerRecoveryOutcome::Suppressed);
        assert!(!restarted.exists());
        std::fs::remove_file(&exit_path).unwrap();
    }
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_replaces_only_a_stale_worker_binary_before_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("current-worker");
    let refreshed = directory.path().join("worker-refreshed");
    let restarted = directory.path().join("worker-restarted");
    let bytes = format!(
        "current worker binary{}",
        mj_core::worker_build::worker_build_stamp()
    );
    std::fs::write(&source, &bytes).unwrap();
    let current_digest = lower_hex(sha2::Sha256::digest(bytes.as_bytes()));
    let recovery = |installed_digest: &str, require_refresh: bool| {
        let mut restart = if require_refresh {
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "test -f \"$MJ_TEST_REFRESHED\" && touch -- \"$MJ_TEST_RESTARTED\"",
                ],
            )
        } else {
            CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
        }
        .purpose("restart test worker");
        restart.env.insert(
            "MJ_TEST_REFRESHED".into(),
            refreshed.to_string_lossy().into_owned(),
        );
        restart.env.insert(
            "MJ_TEST_RESTARTED".into(),
            restarted.to_string_lossy().into_owned(),
        );
        WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("printf", ["dead\n"])
                .purpose("probe test worker liveness")
                .into(),
            binary_refresh: Some(WorkerBinaryRefresh::Prepared(WorkerBinaryRefreshPlan {
                source: source.clone(),
                installed_digest: CommandSpec::new(
                    "printf",
                    [format!("{installed_digest}  /worker/hel\n")],
                )
                .purpose("identify test worker binary"),
                replace: CommandPlan {
                    description: "refresh test worker".into(),
                    commands: vec![
                        CommandSpec::new("touch", [refreshed.to_string_lossy().into_owned()])
                            .purpose("refresh test worker"),
                    ],
                },
            })),
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart test worker".into(),
                commands: vec![restart],
            }
            .into(),
        }
    };

    assert_eq!(
        recover_worker(recovery(&current_digest, false), false)
            .await
            .unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert!(!refreshed.exists(), "a current binary must not be copied");
    assert!(restarted.exists());

    std::fs::remove_file(&restarted).unwrap();
    assert_eq!(
        recover_worker(recovery(&"0".repeat(64), true), false)
            .await
            .unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert!(refreshed.exists(), "a stale binary must be refreshed");
    assert!(restarted.exists(), "refresh must finish before restart");
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_refreshes_a_stale_launch_config_before_restart() {
    let directory = tempfile::tempdir().unwrap();
    let refreshed = directory.path().join("launch-refreshed");
    let restarted = directory.path().join("worker-restarted");
    let mut restart = CommandSpec::new(
        "sh",
        [
            "-c",
            "test -f \"$MJ_TEST_REFRESHED\" && touch -- \"$MJ_TEST_RESTARTED\"",
        ],
    )
    .purpose("restart test worker");
    restart.env.insert(
        "MJ_TEST_REFRESHED".into(),
        refreshed.to_string_lossy().into_owned(),
    );
    restart.env.insert(
        "MJ_TEST_RESTARTED".into(),
        restarted.to_string_lossy().into_owned(),
    );
    let outcome = recover_worker(
        WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("printf", ["dead\n"])
                .purpose("probe test worker liveness")
                .into(),
            binary_refresh: None,
            launch_refresh: Some(WorkerLaunchRefreshPlan {
                expected_sha256: "a".repeat(64),
                installed_digest: CommandSpec::new(
                    "printf",
                    [format!("{}  /worker/launch.json\n", "b".repeat(64))],
                )
                .purpose("identify test launch config"),
                replace: CommandPlan {
                    description: "refresh test launch config".into(),
                    commands: vec![
                        CommandSpec::new("touch", [refreshed.to_string_lossy().into_owned()])
                            .purpose("refresh test launch config"),
                    ],
                },
            }),
            restart: CommandPlan {
                description: "restart test worker".into(),
                commands: vec![restart],
            }
            .into(),
        },
        false,
    )
    .await
    .unwrap();

    assert_eq!(outcome, WorkerRecoveryOutcome::RestartedDead);
    assert!(refreshed.exists());
    assert!(
        restarted.exists(),
        "config refresh must finish before restart"
    );
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_starts_a_stopped_target_before_probing_its_worker() {
    let directory = tempfile::tempdir().unwrap();
    let target_started = directory.path().join("target-started");
    let worker_restarted = directory.path().join("worker-restarted");
    let inspection = |status: &str| {
        serde_json::to_string(&serde_json::json!([{
            "Config": { "Labels": {
                (crate::targets::MANAGED_LABEL): "true",
                (crate::targets::SESSION_LABEL): "session-1",
            }},
            "State": { "Status": status },
        }]))
        .unwrap()
    };
    let mut inspect = CommandSpec::new(
        "sh",
        [
            "-c",
            "if [ -f \"$MJ_TEST_TARGET_STARTED\" ]; then printf '%s\\n' \"$MJ_TEST_RUNNING\"; else printf '%s\\n' \"$MJ_TEST_EXITED\"; fi",
        ],
    )
    .purpose("inspect test target");
    inspect.env.insert(
        "MJ_TEST_TARGET_STARTED".into(),
        target_started.to_string_lossy().into_owned(),
    );
    inspect
        .env
        .insert("MJ_TEST_RUNNING".into(), inspection("running"));
    inspect
        .env
        .insert("MJ_TEST_EXITED".into(), inspection("exited"));
    let mut start = CommandSpec::new("sh", ["-c", "touch -- \"$MJ_TEST_TARGET_STARTED\""])
        .purpose("start test target");
    start.env.insert(
        "MJ_TEST_TARGET_STARTED".into(),
        target_started.to_string_lossy().into_owned(),
    );
    let mut liveness = CommandSpec::new(
        "sh",
        [
            "-c",
            "test -f \"$MJ_TEST_TARGET_STARTED\" && printf 'dead\\n'",
        ],
    )
    .purpose("probe test worker after target start");
    liveness.env.insert(
        "MJ_TEST_TARGET_STARTED".into(),
        target_started.to_string_lossy().into_owned(),
    );

    let outcome = recover_worker(
        WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: Some(TargetRecoveryPlan {
                exists: CommandSpec::new("true", std::iter::empty::<&str>())
                    .purpose("check test target"),
                inspect,
                start,
                session_id: "session-1".into(),
            }),
            workspace: None,
            exit_record: None,
            liveness_probe: liveness.into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart test worker".into(),
                commands: vec![
                    CommandSpec::new("touch", [worker_restarted.to_string_lossy().into_owned()])
                        .purpose("restart test worker"),
                ],
            }
            .into(),
        },
        false,
    )
    .await
    .unwrap();

    assert_eq!(outcome, WorkerRecoveryOutcome::RestartedDead);
    assert!(target_started.exists());
    assert!(worker_restarted.exists());
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1296: process cancellation and relay recovery assume Unix"
)]
async fn recovery_reports_a_missing_target_without_running_worker_commands() {
    let unreachable = CommandSpec::new("false", std::iter::empty::<&str>());
    let outcome = recover_worker(
        WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: Some(TargetRecoveryPlan {
                exists: unreachable,
                inspect: CommandSpec::new("false", std::iter::empty::<&str>()),
                start: CommandSpec::new("false", std::iter::empty::<&str>()),
                session_id: "session-1".into(),
            }),
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("false", std::iter::empty::<&str>()).into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "must not restart".into(),
                commands: vec![CommandSpec::new("false", std::iter::empty::<&str>())],
            }
            .into(),
        },
        true,
    )
    .await
    .unwrap();

    assert_eq!(outcome, WorkerRecoveryOutcome::TargetMissing);
}

fn target(program: &str) -> RelaySessionTarget {
    RelaySessionTarget {
        session_id: "session-1".to_owned(),
        spec: CommandSpec::new(program, std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }
}

/// A connected view carrying a conversation, so republishing it exercises
/// the case a whole-transcript comparison would have to walk.
fn view_at_ordinal(ordinal: u64) -> ManagedSessionView {
    let digest = "a".repeat(64);
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = ordinal;
    materialized.applied_event_digest = digest.clone();
    materialized.transcript = (1..=200)
        .map(|position| {
            Arc::new(mj_core::state::TranscriptItem {
                stable_id: format!("system:{position}"),
                position,
                latest_content_event_ordinal: None,
                created_at_ms: 1,
                last_changed_at_ms: 1,
                body: mj_core::state::TranscriptBody::System {
                    text: format!("event {position}"),
                },
            })
        })
        .collect();
    ManagedSessionView {
        attention: Default::default(),
        snapshot: Some(ManagedSessionSnapshot {
            subagent_requests: Default::default(),
            subagent_completions: Default::default(),
            subagent_queue_error: None,
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: RelayOperationalState {
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
                store_id: None,
                idle_since_ms: None,
                session_id: "session-1".into(),
                execution: mj_core::relay::RelayExecutionState::Idle,
                latest_ordinal: ordinal,
                latest_digest: digest.clone(),
                acknowledged_through: ordinal,
                acknowledged_digest: digest,
                recovery_floor_ordinal: 0,
                recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
                native_session_id: None,
                native_continuity_lost: false,
                replaced_unused_native_session_id: None,
                checkpoint_only: false,
                acp_ready: None,
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
                tools_in_flight: Vec::new(),
                activity: None,
                harness_turn: None,
                last_harness_turn_started_ordinal: None,
                background_commands: Vec::new(),
                background_work_known: None,
            }
            .into(),
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
        connected: true,
        error: None,
    }
}

// Hard-won: #1037: A request-only sub-agent update must reach the drain before the worker socket ceiling.
#[test]
fn a_new_subagent_request_publishes_without_a_transcript_change() {
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();

    // Establish a baseline view and drain its first-publish notification.
    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
    let _ = updates_rx.try_recv().expect("the first view is news");
    let _ = view_rx.borrow_and_update();

    // A second view identical to the baseline except for a queued sub-agent
    // request: the exact shape that arrives from the subagents.json poll with
    // no coincident relay event (e.g. one that survives a daemon restart).
    // It must still reach the drain, or its `serve_one` waits to the ceiling.
    let mut with_request = view_at_ordinal(7);
    with_request
        .snapshot
        .as_mut()
        .expect("snapshot present")
        .subagent_requests
        .push(mj_core::subagent::SubagentToolRequest {
            originating_command_id: None,
            request_id: "req-1".to_owned(),
            created_at_ms: 1,
            action: mj_core::subagent::SubagentToolAction::ListAgents,
        });

    publish_view("session-1", with_request.clone(), &view_tx, &updates_tx);

    assert!(
        view_rx.has_changed().expect("watch stays open"),
        "a newly queued sub-agent request is a real change"
    );
    assert_eq!(
        updates_rx
            .try_recv()
            .expect("the new sub-agent request must reach the drain")
            .view,
        with_request
    );
}

#[test]
fn publishing_relay_state_that_moved_without_the_frontier_notifies_watchers() {
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();
    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
    let _ = updates_rx.try_recv();
    let _ = view_rx.borrow_and_update();

    let mut view = view_at_ordinal(7);
    view.snapshot
        .as_mut()
        .expect("published snapshot")
        .operational
        .execution = mj_core::relay::RelayExecutionState::Running;
    publish_view("session-1", view, &view_tx, &updates_tx);

    assert!(view_rx.has_changed().expect("watch stays open"));
    assert!(updates_rx.try_recv().is_ok());
}

#[test]
fn losing_the_relay_republishes_the_same_snapshot_as_disconnected() {
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();
    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
    let _ = updates_rx.try_recv();
    let _ = view_rx.borrow_and_update();

    let mut view = view_at_ordinal(7);
    view.connected = false;
    view.error = Some(ViewError::Unreachable("relay is unreachable".into()));
    publish_view("session-1", view, &view_tx, &updates_tx);

    assert!(view_rx.has_changed().expect("watch stays open"));
    assert!(updates_rx.try_recv().is_ok());
}

#[test]
fn leased_actor_defers_replacement_and_uses_latest_queued_target() {
    let original = target("relay-v1");
    let intermediate = target("relay-v2");
    let latest = target("relay-v3");
    let mut lifecycle = ActorLifecycle::default();
    lifecycle.activate_lease(
        7,
        crate::database::test_fixtures::relay_admission("session-1"),
    );

    assert_eq!(
        reconcile_action(Some(&original), Some(&intermediate)),
        ReconcileAction::Retire
    );
    lifecycle.set_retirement_requested(true);
    assert!(!lifecycle.accepts_new_work());
    assert!(!lifecycle.should_stop());

    assert_eq!(
        reconcile_action(Some(&original), Some(&latest)),
        ReconcileAction::Retire
    );
    assert!(lifecycle.return_lease(7));
    assert!(lifecycle.should_stop());

    assert_eq!(
        reconcile_action(None, Some(&latest)),
        ReconcileAction::Spawn
    );
}

#[test]
fn leased_actor_defers_removal_until_its_connection_returns() {
    let original = target("relay-v1");
    let mut lifecycle = ActorLifecycle::default();
    lifecycle.activate_lease(
        11,
        crate::database::test_fixtures::relay_admission("session-1"),
    );

    assert_eq!(
        reconcile_action(Some(&original), None),
        ReconcileAction::Retire
    );
    lifecycle.set_retirement_requested(true);
    assert!(!lifecycle.should_stop());
    assert!(!lifecycle.return_lease(10));
    assert!(!lifecycle.should_stop());
    assert!(lifecycle.return_lease(11));
    assert!(lifecycle.should_stop());
    assert_eq!(reconcile_action(None, None), ReconcileAction::Idle);
}

#[test]
fn queued_change_back_to_current_target_cancels_retirement() {
    let original = target("relay-v1");
    let replacement = target("relay-v2");
    let mut lifecycle = ActorLifecycle::default();
    lifecycle.activate_lease(
        3,
        crate::database::test_fixtures::relay_admission("session-1"),
    );

    assert_eq!(
        reconcile_action(Some(&original), Some(&replacement)),
        ReconcileAction::Retire
    );
    lifecycle.set_retirement_requested(true);
    assert_eq!(
        reconcile_action(Some(&original), Some(&original)),
        ReconcileAction::Keep
    );
    lifecycle.set_retirement_requested(false);

    assert!(lifecycle.return_lease(3));
    assert!(!lifecycle.should_stop());
    assert!(lifecycle.accepts_new_work());
}

#[tokio::test]
async fn stopped_actor_is_replaced_without_late_completion_removing_replacement() {
    let desired = target("sh");
    let desired_targets = target_map(std::slice::from_ref(&desired));
    let mut actors = BTreeMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    let (commands, commands_rx) = mpsc::channel(1);
    drop(commands_rx);
    let (releases, _releases_rx) = mpsc::unbounded_channel();
    let (retirement, _retirement_rx) = watch::channel(false);
    let (_view_tx, view) = watch::channel(ManagedSessionView::default());
    let old_abort = tasks.spawn(async { "session-1".to_owned() });
    let old_task_id = old_abort.id();
    actors.insert(
        "session-1".to_owned(),
        ActorRegistration {
            target: desired.clone(),
            commands,
            releases,
            retirement,
            view,
            abort: old_abort,
        },
    );
    let (updates, _updates_rx) = coalesced_update_channel();

    reconcile_actors(&desired_targets, &mut actors, &mut tasks, &updates);

    let replacement_task_id = actors["session-1"].abort.id();
    assert_ne!(replacement_task_id, old_task_id);
    assert!(!actors["session-1"].commands.is_closed());
    assert_eq!(remove_actor_task(&mut actors, old_task_id), None);
    assert_eq!(actors["session-1"].abort.id(), replacement_task_id);
    tasks.abort_all();
}

#[cfg(unix)]
const UNREACHABLE_VIEW_TEST_CHILD: &str = "MJ_TEST_UNREACHABLE_RELAY_CHILD";

#[cfg(unix)]
// Hard-won: e8138769: An unreachable relay must publish its error instead of self-deadlocking the dashboard.
#[tokio::test(start_paused = true)]
async fn unreachable_relay_publishes_error_view() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in
    // an exact child test instead of racing unrelated tests in this
    // process.
    if std::env::var_os(UNREACHABLE_VIEW_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(exact_test_name("unreachable_relay_publishes_error_view"))
            .env(UNREACHABLE_VIEW_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .run();
        return;
    }

    // A regression in the publish path deadlocks the actor instead of
    // returning an error, so convert a hang into a hard failure.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("unreachable relay error view was never published");
        std::process::exit(101);
    });

    let (_commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();
    mj_core::runtime::spawn(run_session_actor(
        target("hel-relay-program-that-does-not-exist"),
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    loop {
        view_rx.changed().await.unwrap();
        let view = view_rx.borrow_and_update().clone();
        if !view.connected {
            let error = view
                .error
                .expect("unreachable view carries the connect error");
            assert!(
                error.detail().contains("session relay proxy"),
                "unexpected error: {error:?}"
            );
            break;
        }
    }
    let update = updates_rx
        .recv()
        .await
        .expect("dashboard feed received the error view");
    assert_eq!(update.session_id, "session-1");
    assert!(!update.view.connected);
}

#[cfg(unix)]
const UNREADABLE_PROJECTION_TEST_CHILD: &str = "MJ_TEST_UNREADABLE_PROJECTION_CHILD";

#[cfg(unix)]
// Hard-won: 91163d55: A failed reconnect must not load a large projection before the cheap connection check.
#[tokio::test]
async fn connecting_to_an_absent_worker_never_reads_the_projection() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in
    // an exact child test instead of racing unrelated tests in this
    // process.
    if std::env::var_os(UNREADABLE_PROJECTION_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        // A directory where the database file belongs makes every
        // projection read fail, so a read that happens at all shows up in
        // the reported error.
        std::fs::create_dir(directory.path().join("mj.sqlite3")).unwrap();
        let output = IsolatedTest::new(exact_test_name(
            "connecting_to_an_absent_worker_never_reads_the_projection",
        ))
        .env(UNREADABLE_PROJECTION_TEST_CHILD, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .output();
        assert!(
            output.status.success(),
            "isolated projection ordering test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    assert!(
        crate::database::load_materialized_session("session-1").is_err(),
        "this store must fail every projection read for the test to mean anything"
    );
    let connected =
        StandaloneSession::connect(&target("hel-relay-program-that-does-not-exist")).await;
    let error = match connected {
        Ok(_) => panic!("a relay program that does not exist cannot connect"),
        Err(error) => error,
    };
    let detail = format!("{error:#}");
    assert!(
        detail.contains("session relay proxy"),
        "unexpected error: {detail}"
    );
    assert!(
        !detail.contains("Mjolnir database"),
        "connect read the projection before it reached the relay: {detail}"
    );
}

#[cfg(unix)]
const AUTO_RESTART_TEST_CHILD: &str = "MJ_TEST_AUTO_RESTART_CHILD";
#[cfg(unix)]
const AUTO_RESTART_MARKER: &str = "MJ_TEST_AUTO_RESTART_MARKER";
#[cfg(unix)]
const DEFERRED_SUBMIT_TEST_CHILD: &str = "MJ_TEST_DEFERRED_SUBMIT_CHILD";
#[cfg(unix)]
const RETIRED_SUBMIT_TEST_CHILD: &str = "MJ_TEST_RETIRED_SUBMIT_CHILD";
#[cfg(unix)]
const RETURNED_LEASE_VIEW_TEST_CHILD: &str = "MJ_TEST_RETURNED_LEASE_VIEW_CHILD";
#[cfg(unix)]
const EXPLICIT_MEMORY_SYNC_TEST_CHILD: &str = "MJ_TEST_EXPLICIT_MEMORY_SYNC_CHILD";
#[cfg(unix)]
const SUBMIT_WITHOUT_SYNC_TEST_CHILD: &str = "MJ_TEST_SUBMIT_WITHOUT_SYNC_CHILD";
#[cfg(unix)]
const MANAGER_SHUTDOWN_TEST_CHILD: &str = "MJ_TEST_MANAGER_SHUTDOWN_CHILD";
#[cfg(unix)]
const SYNC_CADENCE_TEST_CHILD: &str = "MJ_TEST_SYNC_CADENCE_CHILD";
#[cfg(unix)]
const FULL_DISK_RECOVERY_TEST_CHILD: &str = "MJ_TEST_FULL_DISK_RECOVERY_CHILD";

#[cfg(unix)]
#[tokio::test]
async fn worker_history_poll_executes_real_index_queries_without_delegation() {
    const CHILD: &str = "MJ_TEST_HISTORY_POLL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "worker_history_poll_executes_real_index_queries_without_delegation",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let (_directory, connection) = {
        let _held = crate::sessionwiki::tags::testing::lock();
        crate::sessionwiki::tags::testing::isolated_index()
    };
    crate::sessionwiki::tags::testing::index_row(&connection, "history-session", "mjolnir");
    let text = "é🙂".repeat(30_000);
    connection
        .execute(
            "INSERT INTO messages(session_id, role, text) VALUES ('history-session', 'user', ?1)",
            [&text],
        )
        .unwrap();
    let relay_root = tempfile::tempdir().unwrap();
    std::fs::write(
        relay_root.path().join("history-request.json"),
        serde_json::to_vec(&mj_core::history::HistoryRequest {
            request_id: "read-history".into(),
            blame: None,
            query: mj_core::history::HistoryQuery::ReadSession {
                session_id: "history-session".into(),
                start: 0,
                offset: 0,
                role: None,
                limit: 20,
                max_chars: 64000,
            },
        })
        .unwrap(),
    )
    .unwrap();
    let mut worker = StandaloneSession::connect(&leased_relay_target(relay_root.path()))
        .await
        .unwrap();
    let result_path = relay_root.path().join("history-result.json");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !result_path.exists() {
            worker.sync().await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let result: mj_core::history::HistoryResult =
        serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    assert!(!result.is_error, "{:?}", result.value);
    assert_eq!(result.value["data"]["messages"][0]["text"], text);
    assert!(worker.subagent_requests.is_empty());
    worker.detach().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn guardian_actor_cascades_requests_added_after_attachment() {
    use mj_core::guardian::{
        GUARDIAN_CONFIG_INVALID_TOML, GUARDIAN_CONFIG_NEEDS_UPGRADE, GUARDIAN_CONFIG_UNREADABLE,
        GuardianReviewOutcome, GuardianReviewRequest, GuardianReviewResult,
        WOLF_STAGE1_ALLOW_THRESHOLD,
    };

    struct FakeStage1;
    impl GuardianStage1 for FakeStage1 {
        fn classify<'a>(
            &'a self,
            request: &'a GuardianReviewRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = GuardianStage1Verdict> + Send + 'a>>
        {
            let verdict = match request.request_id.as_str() {
                "low" => GuardianStage1Verdict::Scored {
                    unsafe_prob: WOLF_STAGE1_ALLOW_THRESHOLD,
                    tokens: 15,
                    elapsed_ms: 1,
                },
                "high" => GuardianStage1Verdict::Scored {
                    unsafe_prob: 0.9,
                    tokens: 15,
                    elapsed_ms: 1,
                },
                "overflow" => GuardianStage1Verdict::Overflow { tokens: 6145 },
                "long" => GuardianStage1Verdict::EscalatedActionTooLong,
                "unavailable" => GuardianStage1Verdict::Unavailable {
                    reason: "loading".into(),
                },
                id => panic!("unexpected test request {id}"),
            };
            Box::pin(std::future::ready(verdict))
        }
    }

    const CHILD: &str = "MJ_TEST_GUARDIAN_POLL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "guardian_actor_cascades_requests_added_after_attachment",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    mj_core::config::Config::default().save().unwrap();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let SessionManagerChannels {
        targets,
        control,
        shutdown,
        ..
    } = spawn_session_manager().expect("spawn the session manager");
    targets.send_replace(vec![leased_relay_target(relay_root.path())]);
    let session = control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(2))
        .await
        .unwrap();
    session.sync_now().await.unwrap();
    assert!(session.view().connected);

    let queue = relay_root.path().join("guardian-request.json");
    let response = relay_root.path().join("guardian-result.json");
    let config_path = mj_core::config::config_path();
    let valid_config = std::fs::read(&config_path).unwrap();
    let mut local_off_config = mj_core::config::Config::default();
    local_off_config.guardian.local_classifier = false;
    let mut guardian_off_config = mj_core::config::Config::default();
    guardian_off_config.guardian.enabled = false;
    for (id, expected) in [
        (
            "no-classifier",
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
        ),
        (
            "low",
            GuardianReviewOutcome::Allow {
                stage: "stage1_wolf".into(),
                stage1_score: Some(WOLF_STAGE1_ALLOW_THRESHOLD),
            },
        ),
        (
            "high",
            GuardianReviewOutcome::UseAcpFallback {
                stage1_score: Some(0.9),
            },
        ),
        (
            "overflow",
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
        ),
        (
            "long",
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
        ),
        (
            "unavailable",
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
        ),
        (
            "local-disabled",
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
        ),
        (
            "guardian-disabled",
            GuardianReviewOutcome::AskPerson {
                reason: "Mjolnir guardian is disabled in settings".into(),
            },
        ),
        (
            "invalid-config",
            GuardianReviewOutcome::AskPerson {
                reason: GUARDIAN_CONFIG_INVALID_TOML.into(),
            },
        ),
        (
            "newer-config",
            GuardianReviewOutcome::AskPerson {
                reason: GUARDIAN_CONFIG_NEEDS_UPGRADE.into(),
            },
        ),
        (
            "unreadable-config",
            GuardianReviewOutcome::AskPerson {
                reason: GUARDIAN_CONFIG_UNREADABLE.into(),
            },
        ),
    ] {
        match id {
            "low" => control.install_guardian_stage1(Arc::new(FakeStage1)),
            "local-disabled" => local_off_config.save().unwrap(),
            "guardian-disabled" => guardian_off_config.save().unwrap(),
            "invalid-config" => {
                mj_core::config::atomic_write(&config_path, b"invalid SECRET_DATA").unwrap();
            }
            "newer-config" => {
                let config = format!("version = {}\n", mj_core::config::CONFIG_VERSION + 1);
                mj_core::config::atomic_write(&config_path, config.as_bytes()).unwrap();
            }
            "unreadable-config" => {
                std::fs::remove_file(&config_path).unwrap();
                std::fs::create_dir(&config_path).unwrap();
            }
            _ => {
                mj_core::config::atomic_write(&config_path, &valid_config).unwrap();
            }
        }
        mj_core::config::atomic_write(
            &queue,
            &serde_json::to_vec(&GuardianReviewRequest {
                request_id: id.into(),
                harness: mj_core::config::HarnessKind::Codex,
                session_id: LEASED_RELAY_SESSION.into(),
                tool_call_id: "call".into(),
                action: serde_json::json!({"input": {"command": "cargo test"}}),
                user_instructions: vec!["Run the tests.".into()],
                user_instructions_omitted: false,
                project_instructions: None,
                transcript: String::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !queue.exists() {
                    let result: GuardianReviewResult =
                        serde_json::from_slice(&std::fs::read(&response).unwrap()).unwrap();
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out awaiting {id}"));
        assert_eq!(result.request_id, id);
        assert_eq!(result.decision, expected, "{id}");
    }
    shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
fn exact_test_name(test: &str) -> String {
    test_name(module_path!(), test)
}

/// MJ_DATA_DIR is process-global, so every test that reaches the
/// controller database runs in an exact child with its own data directory.
#[cfg(unix)]
fn run_in_isolated_child(marker: &str, test: &str) {
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(exact_test_name(test))
        .env(marker, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .run();
}

#[cfg(unix)]
#[tokio::test]
async fn session_manager_shutdown_joins_a_live_relay_actor() {
    if std::env::var_os(MANAGER_SHUTDOWN_TEST_CHILD).is_none() {
        run_in_isolated_child(
            MANAGER_SHUTDOWN_TEST_CHILD,
            "session_manager_shutdown_joins_a_live_relay_actor",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let SessionManagerChannels {
        session_cpu: _,
        targets,
        control,
        updates: _updates,
        shutdown,
    } = spawn_session_manager().expect("spawn the session manager");
    targets.send_replace(vec![leased_relay_target(relay_root.path())]);
    let session = control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(2))
        .await
        .expect("manager registered the relay actor");
    session
        .sync_now()
        .await
        .expect("relay actor established a live connection");
    assert!(session.view().connected);

    tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
        .await
        .expect("manager shutdown stayed within its deadline")
        .expect("manager shutdown task completed cleanly");
}

/// A remote connect or catch-up can outlast the session manager's shutdown
/// grace by minutes. Retirement has to end it, so the actor stops in time and
/// answers the submissions it was holding; an abort after the grace would
/// drop their replies, which a caller must read as possibly delivered.
#[cfg(unix)]
// Hard-won: 8a895681: Daemon retirement must end an unanswered relay connect before shutdown aborts queued replies.
#[tokio::test]
async fn retirement_ends_a_connect_that_never_answers() {
    const CHILD: &str = "MJ_TEST_HUNG_CONNECT_RETIREMENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(CHILD, "retirement_ends_a_connect_that_never_answers");
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("a retired actor stayed in a connect that never answers");
    let (commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, _view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    // A proxy that starts and never says hello, as an SSH relay to a busy host
    // can for the whole handshake timeout.
    let mut hung = target("sleep");
    hung.spec = CommandSpec::new("sleep", ["600"]);
    let actor = mj_core::runtime::spawn(run_session_actor(
        hung,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));
    // The first sync starts at once and is now waiting on the proxy.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (reply, response) = oneshot::channel();
    commands_tx
        .send(ActorCommand::Submit {
            queued_at: Instant::now(),
            command_id: new_command_id("prompt").unwrap(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
            reply,
        })
        .await
        .unwrap();

    retirement_tx.send(true).unwrap();
    tokio::time::timeout(SESSION_MANAGER_SHUTDOWN_GRACE, actor)
        .await
        .expect("the retired actor stopped within the shutdown grace")
        .expect("the actor task finished cleanly");
    let error = response
        .await
        .expect("the actor answered the queued submission")
        .expect_err("a retired actor must not deliver the prompt");
    assert!(
        error.message.contains("session manager stopped"),
        "unexpected rejection: {error:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn relay_attach_does_not_probe_or_install_project_memory() {
    if std::env::var_os(EXPLICIT_MEMORY_SYNC_TEST_CHILD).is_none() {
        for protocol in [58, 59] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(exact_test_name(
                "relay_attach_does_not_probe_or_install_project_memory",
            ))
            .env(EXPLICIT_MEMORY_SYNC_TEST_CHILD, protocol.to_string())
            .env("MJ_DATA_DIR", directory.path())
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .run();
        }
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let protocol: u32 = std::env::var(EXPLICIT_MEMORY_SYNC_TEST_CHILD)
        .unwrap()
        .parse()
        .unwrap();
    std::fs::write(
        relay_root.path().join("memory-sync-protocol"),
        protocol.to_string(),
    )
    .unwrap();
    let canonical = tempfile::tempdir().unwrap();
    let mut target = leased_relay_target(relay_root.path());
    target.project_memory = Some(ProjectMemorySyncTarget {
        canonical_root: canonical.path().to_path_buf(),
    });

    let mut connection = StandaloneSession::connect(&target)
        .await
        .expect("relay attach must not depend on its memory endpoint");
    assert!(
        connection.project_memory.is_some(),
        "attach must leave memory pending for an explicit checkpoint sync"
    );

    connection
        .sync()
        .await
        .expect("ordinary polling must not probe memory");
    let requests = std::fs::read_to_string(relay_root.path().join(RELAY_REQUEST_LOG)).unwrap();
    assert!(
        requests
            .lines()
            .all(|method| !method.contains("project_memory")),
        "{requests}"
    );

    let result = connection
        .sync_project_memory(
            &target.spec,
            &mj_core::config::Config::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    if protocol == 58 {
        result.expect("an explicit sync may detect a legacy memory endpoint");
        assert!(
            connection.project_memory.is_none(),
            "legacy absent endpoint was disabled"
        );
    } else {
        let error = result.expect_err("new transfer failures must remain visible");
        assert_eq!(
            error
                .downcast_ref::<crate::worker_client::RelayRejected>()
                .unwrap()
                .0
                .code,
            mj_core::relay::RelayErrorCode::InvalidState
        );
        assert!(
            connection.project_memory.is_some(),
            "new transfer error must not disable memory"
        );
        let requests = std::fs::read_to_string(relay_root.path().join(RELAY_REQUEST_LOG)).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(|method| method.contains("project_memory"))
                .collect::<Vec<_>>(),
            ["project_memory_transfer"]
        );

        // Hard-won: #1580: closing an abandoned memory connection panicked the checkpoint
        std::fs::write(relay_root.path().join("drop-project-memory"), b"").unwrap();
        connection
            .sync_project_memory(
                &target.spec,
                &mj_core::config::Config::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect_err("a memory connection lost mid-transfer is reported");
        assert!(
            connection.project_memory.is_some(),
            "a lost memory connection must not disable memory"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn installed_prompt_handoff_is_journaled_and_persisted_without_a_user_turn() {
    const CHILD: &str = "MJ_TEST_PROMPT_HANDOFF_PERSISTENCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "installed_prompt_handoff_is_journaled_and_persisted_without_a_user_turn",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let mut connection = StandaloneSession::connect(&leased_relay_target(relay_root.path()))
        .await
        .unwrap();
    let source =
        mj_transcript::projection::canonical_session_from_materialized(&connection.materialized)
            .unwrap();
    let text = format!(
        "{}verified model handoff\n\n<retained_recent_context>\nrecent work</retained_recent_context>",
        mj_transcript::handoff::MODEL_HANDOFF_OPENING
    );
    let marked = mj_transcript::handoff::encode_handoff(&source, &text).unwrap();

    connection.install_prompt_context(marked).await.unwrap();

    let canonical =
        mj_transcript::projection::canonical_session_from_materialized(&connection.materialized)
            .unwrap();
    let reusable = mj_transcript::handoff::reusable_handoff(&canonical).unwrap();
    assert_eq!(
        reusable.summary,
        "verified model handoff\n\n<retained_recent_context>\nrecent work</retained_recent_context>"
    );
    assert!(canonical.transcript.is_empty());
    let requests = std::fs::read_to_string(relay_root.path().join(RELAY_REQUEST_LOG)).unwrap();
    assert!(
        requests.lines().any(|request| request == "submit"),
        "{requests}"
    );
    assert!(
        !requests
            .lines()
            .any(|request| request == "install_prompt_context"),
        "{requests}"
    );
    connection.detach().await.unwrap();
}

/// Catching the local projection up to an accepted command is the
/// expensive half of a submit, and a caller waiting to hear that the relay
/// took the command should not wait for it. The two are separate calls, so
/// the cheap one can answer first.
#[cfg(unix)]
// Hard-won: 0db1a867: Relay acceptance must not wait on catch-up and invite retrying an accepted command.
// Hard-won: 1d746fab4: every empty catch-up deeply copied unchanged operational JSON.
// Hard-won: daemon heap profile: publishing snapshots undid the relay's shared-state cache.
#[tokio::test]
async fn submitting_does_not_catch_the_projection_up_until_asked() {
    if std::env::var_os(SUBMIT_WITHOUT_SYNC_TEST_CHILD).is_none() {
        run_in_isolated_child(
            SUBMIT_WITHOUT_SYNC_TEST_CHILD,
            "submitting_does_not_catch_the_projection_up_until_asked",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let mut connection = StandaloneSession::connect(&leased_relay_target(relay_root.path()))
        .await
        .expect("connect to the live test relay");
    let before = connection.materialized.applied_event_ordinal;

    let ordinal = connection
        .submit_accepted(
            new_command_id("prompt").unwrap(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
        )
        .await
        .expect("the relay accepted the command");
    assert!(ordinal > before, "the relay reported where it accepted it");
    assert_eq!(
        connection.materialized.applied_event_ordinal, before,
        "the caller was answered without paying for the catch-up"
    );

    connection.sync().await.expect("catch the projection up");
    assert!(
        connection.materialized.applied_event_ordinal > before,
        "the catch-up is what advances the projection"
    );
    let held = connection.operational.clone();
    let published = connection.snapshot();
    assert!(held.ptr_eq(&published.operational));
    assert!(
        connection
            .subagent_requests
            .ptr_eq(&published.subagent_requests)
    );
    assert!(
        connection
            .subagent_completions
            .ptr_eq(&published.subagent_completions)
    );
    let encoded = serde_json::to_value(&*held).unwrap();
    assert_eq!(
        serde_json::to_value(&published.operational).unwrap(),
        encoded
    );
    let attachment: mj_client::session::RelayAttachment = serde_json::from_value(serde_json::json!({"state": encoded, "events": [], "through_ordinal": 0, "through_digest": ""})).unwrap();
    assert_eq!(*attachment.state, *held);
    let mut changed = published.clone();
    changed.operational.native_session_id = Some("changed-copy".into());
    assert_eq!(
        serde_json::to_value(&published.operational).unwrap(),
        encoded
    );
    assert!(!changed.operational.ptr_eq(&held));
    for _ in 0..3 {
        connection.sync_in_place().await.unwrap();
        assert!(
            held.ptr_eq(&connection.operational),
            "an empty catch-up must retain its operational publication"
        );
    }
    // Model the actor committing and ACKing while a standalone connection
    // still holds an older projection. Both views share the durable store.
    let stale = connection.materialized.clone();
    connection
        .submit(
            new_command_id("actor-notice").unwrap(),
            RelayCommand::RecordNotice {
                text: "another connection advanced the durable projection".into(),
            },
        )
        .await
        .unwrap();
    let durable = connection.materialized.clone();
    connection.materialized = stale;
    connection.window = mj_core::state::ProjectionWindow::of(&connection.materialized);
    // Hard-won: 363ef03: a concurrent ACK sent standalone repair below the retained cursor.
    connection.sync_in_place().await.unwrap();
    assert_eq!(
        connection.materialized.applied_event_ordinal,
        durable.applied_event_ordinal
    );
    assert_eq!(
        connection.materialized.applied_event_digest,
        durable.applied_event_digest
    );
    connection.detach().await.unwrap();
}

#[cfg(unix)]
#[test]
fn stale_recovery_checks_durable_state_under_target_ownership() {
    const CHILD: &str = "MJ_STALE_RECOVERY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "stale_recovery_checks_durable_state_under_target_ownership",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut record = crate::database::load_state().unwrap().sessions[LEASED_RELAY_SESSION].clone();
    record.target = Some(Box::new(recovery_source_target()));
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart".into(),
            commands: vec![CommandSpec::new("restart", std::iter::empty::<&str>())],
        }
        .into(),
    };
    #[derive(Default)]
    struct RecordingExecutor(Mutex<Vec<String>>);
    impl CommandExecutor for RecordingExecutor {
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

        fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
            self.0.lock().unwrap().push(command.program.clone());
            Ok(crate::targets::CommandOutput {
                status: 0,
                stdout: b"dead\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }
    let executor = RecordingExecutor::default();
    use mj_core::state::SessionState;
    for state in [
        SessionState::Destroying,
        SessionState::Stopped,
        // A parked sub-agent's worker was stopped on purpose; recovery must
        // not start it again.
        SessionState::Parked,
        SessionState::Lost,
        SessionState::Error,
        SessionState::Provisioning,
        SessionState::DestroyedWithDataLoss,
    ] {
        record.state = state;
        record.last_error = Some("cleanup is safely retryable".into());
        crate::database::save_session(&record).unwrap();
        assert_eq!(
            recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
            WorkerRecoveryOutcome::Suppressed
        );
    }
    record.state = SessionState::Running;
    for target in [
        None,
        Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/replacement-worker").join(LEASED_RELAY_SESSION),
        }),
    ] {
        record.target = target.map(Box::new);
        crate::database::save_session(&record).unwrap();
        assert_eq!(
            recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
            WorkerRecoveryOutcome::Suppressed
        );
    }
    assert_eq!(
        recover_worker_controlled(plan.clone(), true, Some("removed-session"), &executor).unwrap(),
        WorkerRecoveryOutcome::Suppressed
    );
    assert!(executor.0.lock().unwrap().is_empty());

    record.target = Some(Box::new(recovery_source_target()));
    for state in [
        SessionState::Running,
        SessionState::Disconnected,
        SessionState::Checkpointing,
        SessionState::Closing,
    ] {
        record.state = state;
        crate::database::save_session(&record).unwrap();
        assert_eq!(
            recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
            WorkerRecoveryOutcome::RestartedDead
        );
    }
    executor.0.lock().unwrap().clear();

    // A plan queued while Closing must read Destroying only after cleanup
    // releases ownership, rather than use the actor's old observation.
    let guard = crate::database::test_fixtures::worker_test_permit(&record.id, "test destruction")
        .unwrap()
        .unwrap();
    assert_eq!(
        recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
        WorkerRecoveryOutcome::Suppressed
    );
    let mut destroying = record.clone();
    destroying.state = SessionState::Destroying;
    crate::database::save_session(&destroying).unwrap();
    drop(guard);
    assert_eq!(
        recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
        WorkerRecoveryOutcome::Suppressed
    );
    assert!(executor.0.lock().unwrap().is_empty());

    // A storage failure also refuses recovery before touching the target.
    let connection = rusqlite::Connection::open(crate::database::database_path()).unwrap();
    connection.execute("DROP TABLE sessions", []).unwrap();
    let error = recover_worker_controlled(plan, true, Some(&record.id), &executor).unwrap_err();
    assert!(format!("{error:#}").contains("read durable session before worker recovery"));
    assert!(executor.0.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn unresponsive_live_relay_worker_is_restarted_and_reconnected() {
    if std::env::var_os(AUTO_RESTART_TEST_CHILD).is_none() {
        run_in_isolated_child(
            AUTO_RESTART_TEST_CHILD,
            "unresponsive_live_relay_worker_is_restarted_and_reconnected",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("unresponsive live relay worker was never restarted");
    register_leased_relay_session();
    let mut record = crate::database::load_state().unwrap().sessions[LEASED_RELAY_SESSION].clone();
    record.target = Some(Box::new(recovery_source_target()));
    crate::database::save_session(&record).unwrap();
    let relay_root = tempfile::tempdir().unwrap();
    let restarted = relay_root.path().join("worker-restarted");
    let script = format!(
        "if [ ! -f \"${AUTO_RESTART_MARKER}\" ]; then IFS= read -r _; exit 0; fi; \
         \"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
        CHILD_TEST.to_owned()
    );
    let mut spec = CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            script,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
    )
    .purpose("test restartable relay");
    spec.env.insert(
        LEASED_RELAY_ROOT.to_owned(),
        relay_root.path().to_string_lossy().into_owned(),
    );
    spec.env.insert(
        AUTO_RESTART_MARKER.to_owned(),
        restarted.to_string_lossy().into_owned(),
    );
    let worker_recovery = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["alive\n"])
            .purpose("probe test relay worker")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test relay worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test relay worker"),
            ],
        }
        .into(),
    };
    let target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: Some(worker_recovery),
        project_memory: None,
    };
    let (_commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    let gate = updates_tx.session_gate.clone();
    let foreground = gate
        .enter_foreground(
            LEASED_RELAY_SESSION,
            "test reconnect lifecycle",
            crate::recovery_gate::LifecycleOrigin::Person,
            &std::sync::atomic::AtomicBool::new(false),
            &crate::targets::ProcessExecutor,
        )
        .await
        .unwrap();
    mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            view_rx.changed().await.unwrap();
            let view = view_rx.borrow_and_update().clone();
            if matches!(view.error, Some(ViewError::Unreachable(ref detail)) if detail.contains("waiting for test reconnect lifecycle before worker recovery")) { break; }
        }
    }).await.expect("actor recovery defers to its lifecycle owner without blocking the mailbox");
    assert!(
        !restarted.exists(),
        "no worker restart while foreground owns the session"
    );
    drop(foreground);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            view_rx.changed().await.unwrap();
            let view = view_rx.borrow_and_update().clone();
            if view.connected {
                assert!(restarted.exists(), "the restart plan did not run");
                assert!(view.error.is_none());
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("relay stayed disconnected: {:?}", view_rx.borrow().error));
}

/// The relay actor on a full disk: it shows "disk full" instead of a bare
/// "unreachable", does not restart (or upload into) the target while the
/// storage owner says full, and resumes as soon as a measurement finds room,
/// without waiting out a backoff.
#[cfg(unix)]
// Hard-won: 540c9202: The actor must wait for measured space before resuming worker recovery.
#[tokio::test]
async fn relay_actor_waits_for_disk_space_then_recovers_its_worker() {
    if std::env::var_os(FULL_DISK_RECOVERY_TEST_CHILD).is_none() {
        run_in_isolated_child(
            FULL_DISK_RECOVERY_TEST_CHILD,
            "relay_actor_waits_for_disk_space_then_recovers_its_worker",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("full-disk relay actor never recovered");
    let host = "precision-3260";
    let source_target = mj_core::state::TargetLocator::SshBare {
        host: host.into(),
        workspace: format!("work/{LEASED_RELAY_SESSION}").into(),
        worker_id: None,
    };
    register_leased_relay_session();
    let mut record = crate::database::load_state().unwrap().sessions[LEASED_RELAY_SESSION].clone();
    record.target = Some(Box::new(source_target.clone()));
    crate::database::save_session(&record).unwrap();
    let relay_root = tempfile::tempdir().unwrap();
    let restarted = relay_root.path().join("worker-restarted");
    let script = format!(
        "if [ ! -f \"${AUTO_RESTART_MARKER}\" ]; then IFS= read -r _; exit 0; fi; \
         \"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
        CHILD_TEST.to_owned()
    );
    let mut spec = CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            script,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
    )
    .purpose("test restartable relay");
    spec.env.insert(
        LEASED_RELAY_ROOT.to_owned(),
        relay_root.path().to_string_lossy().into_owned(),
    );
    spec.env.insert(
        AUTO_RESTART_MARKER.to_owned(),
        restarted.to_string_lossy().into_owned(),
    );
    let stopped_at = chrono::Utc::now() - chrono::Duration::seconds(60);
    let exit_record = format!(
        r#"{{"reason":"relay coordinator failed: No space left on device (os error 28)","at":"{}"}}"#,
        stopped_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    let worker_recovery = WorkerRecoveryPlan {
        source_target,
        target: None,
        workspace: None,
        exit_record: Some(
            CommandSpec::new("printf", ["%s", exit_record.as_str()])
                .purpose("read test worker exit record"),
        ),
        liveness_probe: CommandSpec::new("printf", ["dead\n"])
            .purpose("probe test relay worker")
            .into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test relay worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test relay worker"),
            ],
        }
        .into(),
    };
    let target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: Some(worker_recovery),
        project_memory: None,
    };
    let (_commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    let detail = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            view_rx.changed().await.unwrap();
            let view = view_rx.borrow_and_update().clone();
            if let Some(ViewError::Unreachable(detail)) = view.error
                && detail.starts_with("disk full: ")
            {
                return detail;
            }
        }
    })
    .await
    .expect("the session never said its disk was full");
    assert!(detail.contains(host), "{detail}");
    assert!(detail.contains("No space left on device"), "{detail}");
    assert!(
        detail.contains("will not be restarted until space is freed"),
        "{detail}"
    );

    // Reconnects go on, but nothing is restarted on the full disk.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !restarted.exists(),
        "the worker was restarted on a full disk"
    );
    assert!(!view_rx.borrow().connected);

    // Space is freed: the storage owner's next measurement wakes the actor.
    let now = chrono::Utc::now().timestamp() as u64;
    crate::target_storage::record_samples(
        &[crate::target_storage::test_storage_sample(
            host,
            40 << 30,
            40 << 30,
        )],
        now,
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            view_rx.changed().await.unwrap();
            let view = view_rx.borrow_and_update().clone();
            if view.connected {
                assert!(restarted.exists(), "the restart plan did not run");
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("relay stayed disconnected: {:?}", view_rx.borrow().error));
}

/// A deferred submission that is never answered would hang the suite
/// instead of failing it, so turn a stall into a hard error.
#[cfg(unix)]
fn fail_if_the_actor_stalls(reason: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("{reason}");
        std::process::exit(101);
    });
}

#[cfg(unix)]
// Hard-won: 2b74434f9: stale delivery batches and reconnects could restore pre-clear mailbox messages.
#[tokio::test]
async fn mailbox_delivery_serializes_clear_completion_and_rechecks_stale_batches() {
    const CHILD: &str = "MJ_TEST_CLEAR_MAILBOX_ACTOR";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "mailbox_delivery_serializes_clear_completion_and_rechecks_stale_batches",
        );
        return;
    }
    use crate::mailbox_outbox::MailboxEventDelivery;
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let root = tempfile::tempdir().unwrap();
    let mut target = leased_relay_target(root.path());
    target
        .spec
        .env
        .insert(RELAY_SESSION_CONFIGURED.into(), "1".into());
    target
        .spec
        .env
        .insert(CLEAR_MAILBOX_CONTROL.into(), "1".into());
    let (commands, commands_rx) = mpsc::channel(8);
    let (releases, releases_rx) = mpsc::unbounded_channel();
    let (_retirement, retirement_rx) = watch::channel(false);
    let (view_tx, view) = watch::channel(ManagedSessionView::default());
    let (updates, _updates_rx) = coalesced_update_channel();
    let actor = mj_core::runtime::spawn(run_session_actor(
        target.clone(),
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates,
    ));
    let handle = ManagedSessionHandle {
        session_id: LEASED_RELAY_SESSION.into(),
        commands,
        releases,
        view,
    };
    handle.sync_now().await.unwrap();
    let enqueue = |key: &str| {
        let event = mj_core::mailbox::MailboxEvent {
            key: key.into(),
            source: "test".into(),
            wake: false,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::PlainText { text: key.into() },
        };
        crate::database::enqueue_mailbox_event(
            key,
            LEASED_RELAY_SESSION,
            &serde_json::to_string(&event).unwrap(),
            false,
            false,
        )
        .unwrap();
        event
    };
    let before = enqueue("actor-before-clear");
    handle
        .submit("clear-request".into(), RelayCommand::ClearContext)
        .await
        .unwrap();
    let during = enqueue("actor-during-clear");
    assert!(matches!(
        handle.deliver_mailbox_event(during.clone()).await.unwrap(),
        MailboxEventDelivery::Pending(_)
    ));
    std::fs::write(root.path().join("complete-clear"), b"complete").unwrap();
    handle.sync_now().await.unwrap();
    for event in [before.clone(), during] {
        assert_eq!(
            handle.deliver_mailbox_event(event).await.unwrap(),
            MailboxEventDelivery::Discarded
        );
    }
    let after = enqueue("actor-after-clear");
    let MailboxEventDelivery::Accepted(ordinal) =
        handle.deliver_mailbox_event(after.clone()).await.unwrap()
    else {
        panic!("fresh event was not delivered")
    };
    crate::database::mark_mailbox_event_accepted(
        &after.key,
        &crate::mailbox_outbox::mailbox_command_id(&after.key),
        ordinal,
    )
    .unwrap();
    drop(handle);
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
    // A fresh controller connection replays the same worker journal and must
    // still skip the old batch and return the original accepted receipt.
    let mut connection = None;
    assert!(
        submit_actor_command(
            &crate::database::test_fixtures::worker_test_gate(),
            &target,
            &mut connection,
            &crate::mailbox_outbox::mailbox_command_id(&before.key),
            &RelayCommand::DeliverMailboxEvent { event: before }
        )
        .await
        .is_err()
    );
    assert_eq!(
        submit_actor_command(
            &crate::database::test_fixtures::worker_test_gate(),
            &target,
            &mut connection,
            &crate::mailbox_outbox::mailbox_command_id(&after.key),
            &RelayCommand::DeliverMailboxEvent { event: after }
        )
        .await
        .unwrap(),
        ordinal
    );
    connection.unwrap().detach().await.unwrap();
    let mut relay =
        mj_worker::relay::DurableRelay::open(root.path(), LEASED_RELAY_SESSION, "test").unwrap();
    let (text, count) = match relay.drain_mailbox("PostToolBatch", None).unwrap() {
        mj_core::relay::RelayResponsePayload::MailboxDrained { text, count, .. } => (text, count),
        other => panic!("unexpected drain: {other:?}"),
    };
    assert_eq!(count, 1);
    assert!(text.unwrap().contains("actor-after-clear"));
}

// Hard-won: 82e4c8a44: an oversized delegation reply also prevented idle worker replacement.
#[cfg(unix)]
#[tokio::test]
async fn broken_delegation_transport_preserves_worker_control_and_idle_reservation() {
    const CHILD: &str = "MJ_TEST_DELEGATION_CONTROL_ISOLATION";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "broken_delegation_transport_preserves_worker_control_and_idle_reservation",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("disconnect-subagent-queue"),
        b"disconnect every queue poll",
    )
    .unwrap();
    let mut target = leased_relay_target(root.path());
    target
        .spec
        .env
        .insert(RELAY_SESSION_CONFIGURED.into(), "1".into());
    let mut connection = StandaloneSession::connect(&target).await.unwrap();
    connection.start_observing(&target).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = connection.sync().await.unwrap();
            if snapshot
                .subagent_queue_error
                .as_deref()
                .is_some_and(|e| e.contains("synchronization failed"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("queue failure was not reported");
    assert!(
        connection
            .reserve_idle("idle-despite-delegation-failure".into())
            .await
            .unwrap(),
        "delegation failures must not block the worker's atomic idle reservation"
    );
    connection
        .sync()
        .await
        .expect("control sync must remain usable");
    connection.detach().await.unwrap();
}

#[cfg(unix)]
// Hard-won: AFK review #1/#2: equal availability hid missed transitions on reconnect.
#[tokio::test]
async fn reconnect_retries_subagent_admission_after_lost_open_reply() {
    if std::env::var_os("MJ_TEST_SUBAGENT_ADMISSION_RECONNECT_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(exact_test_name(
            "reconnect_retries_subagent_admission_after_lost_open_reply",
        ))
        .env("MJ_TEST_SUBAGENT_ADMISSION_RECONNECT_CHILD", "1")
        .env(DROP_ADMISSION_OPEN_REPLY, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut session = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    session.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_session(&session).unwrap();

    let relay_root = tempfile::tempdir().unwrap();
    let target = leased_relay_target(relay_root.path());
    crate::database::set_operator_away(true, std::time::Duration::ZERO).unwrap();
    let mut connection = None;
    let first = sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await;
    assert!(first.is_err(), "the first worker lost the open reply");
    assert!(
        connection.is_none(),
        "the failed connection must be abandoned"
    );
    assert!(
        relay_root
            .path()
            .join("drop-admission-open-reply-once")
            .exists()
    );

    // The worker misses off/on. Reconnect must deliver revision3 although
    // the last availability value it received was already true.
    crate::database::set_operator_away(false, std::time::Duration::ZERO).unwrap();
    crate::database::set_operator_away(true, std::time::Duration::ZERO).unwrap();
    sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await
    .expect("the next relay connection repairs the gate");
    // A still-connected actor must also send a new revision with the same bool.
    crate::database::set_operator_away(false, std::time::Duration::ZERO).unwrap();
    crate::database::set_operator_away(true, std::time::Duration::ZERO).unwrap();
    sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(relay_root.path().join("operator-presence.log")).unwrap(),
        "1 true\n3 true\n5 true\n"
    );
    assert!(connection.is_some());
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(relay_root.path().join("subagent-admission.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["open"], true, "{state}");
    assert_eq!(state["calls"], 2, "{state}");
}

#[cfg(unix)]
#[tokio::test]
async fn reconnect_retries_message_only_admission_after_lost_open_reply() {
    if std::env::var_os("MJ_TEST_MESSAGE_ONLY_ADMISSION_RECONNECT_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(exact_test_name(
            "reconnect_retries_message_only_admission_after_lost_open_reply",
        ))
        .env("MJ_TEST_MESSAGE_ONLY_ADMISSION_RECONNECT_CHILD", "1")
        .env(DROP_ADMISSION_OPEN_REPLY, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (_config, ()) = mj_core::config::Config::update(|config| {
        config.github.watch.enabled = true;
        config.jev.enabled = true;
        Ok(())
    })
    .unwrap();
    register_leased_relay_session();
    let mut session = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    session.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    crate::database::save_session(&session).unwrap();

    let relay_root = tempfile::tempdir().unwrap();
    let target = leased_relay_target(relay_root.path());
    let mut connection = None;
    let first = sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await;
    assert!(first.is_err(), "the first worker lost the open reply");
    assert!(
        connection.is_none(),
        "the failed connection must be abandoned"
    );
    assert!(
        relay_root
            .path()
            .join("drop-admission-open-reply-once")
            .exists()
    );

    sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await
    .expect("the next relay connection repairs the message-only gate");
    assert!(connection.is_some());
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(relay_root.path().join("subagent-admission.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["open"], true, "{state}");
    assert_eq!(state["calls"], 2, "{state}");
}

#[cfg(unix)]
struct LeasedActor {
    commands: mpsc::Sender<ActorCommand>,
    releases: mpsc::UnboundedSender<ReturnedConnection>,
    retirement: watch::Sender<bool>,
    _views: watch::Receiver<ManagedSessionView>,
    _updates: SessionManagerUpdates,
    _relay_root: tempfile::TempDir,
}

/// Start an actor against a live relay and take its connection under lease.
#[cfg(unix)]
async fn lease_a_live_actor() -> (LeasedActor, u64, StandaloneSession) {
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let (commands_tx, commands_rx) = mpsc::channel(4);
    let (releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, updates_rx) = coalesced_update_channel();
    mj_core::runtime::spawn(run_session_actor(
        leased_relay_target(relay_root.path()),
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    let (reply, response) = oneshot::channel();
    commands_tx
        .send(ActorCommand::Lease {
            returns: releases_tx.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            reply,
            idle_harness: None,
        })
        .await
        .unwrap();
    let mut granted = response
        .await
        .expect("actor answered the lease request")
        .expect("actor leased its relay connection");
    let lease_id = granted.lease_id.take().unwrap();
    let connection = granted.connection.take().unwrap();
    (
        LeasedActor {
            commands: commands_tx,
            releases: releases_tx,
            retirement: retirement_tx,
            _views: view_rx,
            _updates: updates_rx,
            _relay_root: relay_root,
        },
        lease_id,
        connection,
    )
}

#[cfg(unix)]
#[tokio::test]
async fn released_connection_can_be_leased_again_immediately() {
    if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            DEFERRED_SUBMIT_TEST_CHILD,
            "released_connection_can_be_leased_again_immediately",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("actor did not process the returned connection");
    let (actor, mut lease_id, mut connection) = lease_a_live_actor().await;
    let (reply, response) = oneshot::channel();
    actor
        .commands
        .try_send(ActorCommand::Lease {
            returns: actor.releases.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            reply,
            idle_harness: None,
        })
        .unwrap();
    assert!(
        response.await.unwrap().is_err(),
        "an active lease stays exclusive"
    );
    for _ in 0..32 {
        let (reply, response) = oneshot::channel();
        // No await between these sends: both channels are ready when the
        // current-thread runtime next polls the actor.
        actor
            .releases
            .send(ReturnedConnection {
                lease_id,
                connection: Some(Box::new(connection)),
            })
            .unwrap();
        actor
            .commands
            .try_send(ActorCommand::Lease {
                returns: actor.releases.clone(),
                admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
                reply,
                idle_harness: None,
            })
            .unwrap();
        let mut granted = response
            .await
            .unwrap()
            .expect("a returned lease must not reject the next lifecycle operation");
        lease_id = granted.lease_id.take().unwrap();
        connection = granted.connection.take().unwrap();
    }
    actor.retirement.send_replace(true);
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();
}

#[cfg(unix)]
// Hard-won: ec2-m8id16-b move stall: abandoned lease requests must not add relay syncs.
#[tokio::test]
async fn cancelled_lease_request_is_skipped_before_sync() {
    if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            DEFERRED_SUBMIT_TEST_CHILD,
            "cancelled_lease_request_is_skipped_before_sync",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("cancelled lease blocked the actor");
    let (actor, lease_id, connection) = lease_a_live_actor().await;
    let log = actor._relay_root.path().join(RELAY_REQUEST_LOG);
    let syncs = || {
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| *line == "attach")
            .count()
    };
    let before = syncs();
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();
    let (reply, response) = oneshot::channel();
    drop(response);
    actor
        .commands
        .try_send(ActorCommand::Lease {
            returns: actor.releases.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            reply,
            idle_harness: None,
        })
        .unwrap();
    let (reply, response) = oneshot::channel();
    actor
        .commands
        .try_send(ActorCommand::Sync { reply })
        .unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(
        syncs(),
        before + 1,
        "only the explicit Sync may contact the worker"
    );
    // Also cancel after the actor has sent a successful grant but before the
    // receiver consumes it. The queued reply must return its own lease.
    let (reply, granted) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Lease {
            returns: actor.releases.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            idle_harness: None,
            reply,
        })
        .await
        .unwrap();
    let (reply, synced) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Sync { reply })
        .await
        .unwrap();
    assert!(
        synced.await.unwrap().is_err(),
        "the grant must be active before its receiver is dropped"
    );
    drop(granted);
    let (reply, synced) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Sync { reply })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), synced)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    actor.retirement.send_replace(true);
}

#[cfg(unix)]
#[tokio::test]
async fn full_leased_queue_refuses_excess_and_still_processes_release() {
    if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            DEFERRED_SUBMIT_TEST_CHILD,
            "full_leased_queue_refuses_excess_and_still_processes_release",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (actor, lease_id, connection) = lease_a_live_actor().await;
    let mut pending = Vec::new();
    for _ in 0..33 {
        let (reply, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Submit {
                queued_at: Instant::now(),
                command_id: new_command_id("bounded-queue").unwrap(),
                command: RelayCommand::Cancel,
                reply,
            })
            .await
            .unwrap();
        pending.push(response);
    }
    let refused = tokio::time::timeout(Duration::from_secs(5), pending.pop().unwrap())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(!refused.unconfirmed);
    assert!(refused.message.contains("queue is full"));
    // Retirement and lease return still pass the saturated command queue.
    actor.retirement.send_replace(true);
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();
    for response in pending {
        let refused = tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(!refused.unconfirmed);
    }
}

#[cfg(unix)]
async fn submit_a_deferred_prompt(
    actor: &LeasedActor,
) -> oneshot::Receiver<std::result::Result<u64, SubmitFailure>> {
    let (reply, mut response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Submit {
            queued_at: Instant::now(),
            command_id: new_command_id("prompt").unwrap(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
            reply,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut response)
            .await
            .is_err(),
        "a leased actor must hold the prompt instead of answering it"
    );
    response
}

#[cfg(unix)]
#[tokio::test]
async fn prompt_submitted_during_lease_is_delivered_after_release() {
    if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            DEFERRED_SUBMIT_TEST_CHILD,
            "prompt_submitted_during_lease_is_delivered_after_release",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("prompt deferred during a lease was never delivered");

    let (actor, lease_id, connection) = lease_a_live_actor().await;
    let response = submit_a_deferred_prompt(&actor).await;

    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();

    let ordinal = response
        .await
        .expect("actor answered the deferred prompt")
        .expect("deferred prompt reached the relay");
    assert!(
        ordinal > 0,
        "relay accepted the prompt at ordinal {ordinal}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn returned_lease_publishes_what_it_learned_while_it_held_the_connection() {
    if std::env::var_os(RETURNED_LEASE_VIEW_TEST_CHILD).is_none() {
        run_in_isolated_child(
            RETURNED_LEASE_VIEW_TEST_CHILD,
            "returned_lease_publishes_what_it_learned_while_it_held_the_connection",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("a returned lease never republished its session");

    let (actor, lease_id, mut connection) = lease_a_live_actor().await;
    let mut views = actor._views.clone();
    // The lease applies these events itself, so the actor's own next sync
    // has nothing left to catch up on.
    let ordinal = connection
        .submit(
            new_command_id("prompt").unwrap(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
        )
        .await
        .unwrap();
    assert!(views.borrow_and_update().snapshot.is_none());

    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();

    views.changed().await.unwrap();
    let snapshot = views
        .borrow_and_update()
        .snapshot
        .clone()
        .expect("the returned connection republished its session");
    assert!(
        snapshot.materialized.applied_event_ordinal >= ordinal,
        "published frontier {} is behind the leased submission at {ordinal}",
        snapshot.materialized.applied_event_ordinal
    );
}

#[cfg(unix)]
fn relay_requests_logged(relay_root: &std::path::Path) -> usize {
    std::fs::read_to_string(relay_root.join(RELAY_REQUEST_LOG))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

/// T-5: a quiet session must not cost its worker a sync round trip every
/// 150 ms. Fifty idle sessions on remote hosts paid that over SSH all day.
/// Once work is in flight, the actor follows it at the fast cadence again.
#[cfg(unix)]
// Hard-won: 159fbf19: Quiet sessions must not issue four relay round trips every 150 milliseconds.
#[tokio::test]
async fn a_quiet_session_stops_polling_its_worker_until_work_is_in_flight() {
    if std::env::var_os(SYNC_CADENCE_TEST_CHILD).is_none() {
        run_in_isolated_child(
            SYNC_CADENCE_TEST_CHILD,
            "a_quiet_session_stops_polling_its_worker_until_work_is_in_flight",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("the sync cadence test stalled");
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let (commands, commands_rx) = mpsc::channel(4);
    let (_releases, releases_rx) = mpsc::unbounded_channel();
    let (_retirement, retirement_rx) = watch::channel(false);
    let (view_tx, mut views) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates) = coalesced_update_channel();
    let mut target = leased_relay_target(relay_root.path());
    target
        .spec
        .env
        .insert(RELAY_SESSION_CONFIGURED.to_owned(), "1".to_owned());
    mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));
    views
        .wait_for(|view| view.connected && view.snapshot.is_some())
        .await
        .unwrap();
    let quiet = views
        .borrow()
        .snapshot
        .as_ref()
        .unwrap()
        .operational
        .quiet();
    assert!(
        !quiet.work_in_flight(),
        "the fixture relay must start quiet: {}",
        quiet.reason()
    );

    // The delegation connection reads the queue and acknowledges that read
    // as it connects; count what the quiet session costs after that.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let before = relay_requests_logged(relay_root.path());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let idle = relay_requests_logged(relay_root.path()) - before;
    assert!(
        idle <= 6,
        "a quiet session made {idle} relay requests in 3 s; it should make at most one sync"
    );

    let (reply, response) = oneshot::channel();
    commands
        .send(ActorCommand::Submit {
            queued_at: Instant::now(),
            command_id: new_command_id("prompt").unwrap(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
            reply,
        })
        .await
        .unwrap();
    response
        .await
        .unwrap()
        .expect("the relay accepted the prompt");
    // The submit answers before it catches the projection up.
    tokio::time::timeout(
        Duration::from_secs(5),
        views.wait_for(|view| {
            view.snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.has_work_in_flight())
        }),
    )
    .await
    .expect("an accepted prompt with no harness to run it is work in flight")
    .unwrap();
    let before = relay_requests_logged(relay_root.path());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let working = relay_requests_logged(relay_root.path()) - before;
    assert!(
        working >= 12,
        "a session with work in flight made only {working} relay requests in 1 s"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn retirement_rejects_prompts_deferred_during_lease() {
    if std::env::var_os(RETIRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            RETIRED_SUBMIT_TEST_CHILD,
            "retirement_rejects_prompts_deferred_during_lease",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("prompt deferred during a lease was never answered");

    let (actor, lease_id, connection) = lease_a_live_actor().await;
    let response = submit_a_deferred_prompt(&actor).await;

    actor.retirement.send(true).unwrap();
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();

    let error = response
        .await
        .expect("actor answered the deferred prompt")
        .expect_err("a retiring actor must not deliver the prompt");
    assert!(
        error.message.contains("session target is changing"),
        "unexpected rejection: {error:?}"
    );
}

// Hard-won: 111f6dcd: A projection-integrity failure must not be mistaken for an unreachable relay.
#[test]
fn projection_integrity_failure_is_detected_only_for_integrity_errors() {
    let integrity = anyhow::Error::from(ProjectionIntegrityError(
        "transcript item \"tool:call-1\" changed immutable identity fields".into(),
    ))
    .context("apply projection event");
    assert!(projection_integrity_failure(&integrity));

    let concurrent = anyhow::Error::from(ProjectionAdvancedError { event_ordinal: 7 });
    assert!(!projection_integrity_failure(&concurrent));

    let unreachable = anyhow::anyhow!("connection refused").context("connect relay proxy");
    assert!(!projection_integrity_failure(&unreachable));
}

#[test]
fn dashboard_updates_keep_only_the_latest_view_per_session() {
    let (sender, mut receiver) = coalesced_update_channel();
    for revision in 0..1_000 {
        sender.send(SessionManagerUpdate {
            raw_observation: (revision == 0).then(|| {
                mj_core::relay::RelaySnapshot::new("session-1".into())
                    .operational_state()
                    .into()
            }),
            session_id: "session-1".into(),
            view: ManagedSessionView {
                attention: Default::default(),
                error: Some(ViewError::Unreachable(format!("revision-{revision}"))),
                ..ManagedSessionView::default()
            },
        });
    }
    sender.send(SessionManagerUpdate {
        raw_observation: None,
        session_id: "session-2".into(),
        view: ManagedSessionView {
            attention: Default::default(),
            error: Some(ViewError::Unreachable("other".into())),
            ..ManagedSessionView::default()
        },
    });

    assert_eq!(
        sender
            .mailbox
            .lock()
            .expect("session update coalescer poisoned")
            .pending
            .len(),
        2
    );
    let updates = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()]
        .into_iter()
        .map(|update| {
            assert_eq!(
                update.raw_observation.is_some(),
                update.session_id == "session-1",
                "coalescing must retain a pending raw observation"
            );
            (update.session_id, update.view.error.unwrap())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(updates["session-1"].detail(), "revision-999");
    assert_eq!(updates["session-2"].detail(), "other");
    assert!(receiver.try_recv().is_err());
}

// Hard-won: 130c551f: A hot low-ID session must not overtake sessions already waiting in the dashboard queue.
#[tokio::test]
async fn dashboard_updates_deliver_sessions_in_first_pending_order_with_the_latest_view() {
    let (sender, mut receiver) = coalesced_update_channel();
    for (session_id, ordinal) in [
        ("z-first", 1),
        ("a-second", 2),
        ("z-first", 3),
        ("m-third", 4),
    ] {
        sender.send(SessionManagerUpdate {
            raw_observation: None,
            session_id: session_id.into(),
            view: view_at_ordinal(ordinal),
        });
    }
    // All publications share one wake notification, and pending views must
    // still drain after the last sender closes.
    drop(sender);
    for (session_id, ordinal) in [("z-first", 3), ("a-second", 2), ("m-third", 4)] {
        let update = receiver.recv().await.unwrap();
        assert_eq!(update.session_id, session_id);
        assert_eq!(update.view, view_at_ordinal(ordinal));
    }
    assert!(receiver.recv().await.is_none());
}

// Hard-won: 130c551f: A hot producer must not starve other sessions by republishing before their turn.
#[test]
fn dashboard_updates_deliver_waiting_sessions_before_a_republishing_hot_session() {
    let (sender, mut receiver) = coalesced_update_channel();
    for session_id in ["a-hot", "z-waiting", "m-waiting"] {
        sender.send(SessionManagerUpdate {
            raw_observation: None,
            session_id: session_id.into(),
            view: view_at_ordinal(1),
        });
    }
    assert_eq!(receiver.try_recv().unwrap().session_id, "a-hot");
    for expected in ["z-waiting", "m-waiting"] {
        for ordinal in 2..200 {
            sender.send(SessionManagerUpdate {
                raw_observation: None,
                session_id: "a-hot".into(),
                view: view_at_ordinal(ordinal),
            });
        }
        assert_eq!(receiver.try_recv().unwrap().session_id, expected);
    }
    drop(sender);
    let hot = receiver.try_recv().unwrap();
    assert_eq!(hot.session_id, "a-hot");
    assert_eq!(hot.view, view_at_ordinal(199));
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

// Hard-won: 130c551f: Replacing a queued actor must not let its new snapshot jump ahead of waiting sessions.
#[test]
fn dashboard_updates_put_a_replacement_actor_after_sessions_already_waiting() {
    let (sender, mut receiver) = coalesced_update_channel();
    let old = sender.for_actor("a-replaced");
    old.send(SessionManagerUpdate {
        raw_observation: None,
        session_id: "a-replaced".into(),
        view: view_at_ordinal(1),
    });
    for session_id in ["z-waiting", "m-waiting"] {
        sender.send(SessionManagerUpdate {
            raw_observation: None,
            session_id: session_id.into(),
            view: view_at_ordinal(2),
        });
    }
    let current = sender.for_actor("a-replaced");
    current.send(SessionManagerUpdate {
        raw_observation: None,
        session_id: "a-replaced".into(),
        view: view_at_ordinal(3),
    });
    old.send(SessionManagerUpdate {
        raw_observation: None,
        session_id: "a-replaced".into(),
        view: view_at_ordinal(4),
    });
    drop(old);
    drop(current);
    drop(sender);
    for (session_id, ordinal) in [("z-waiting", 2), ("m-waiting", 2), ("a-replaced", 3)] {
        let update = receiver.try_recv().unwrap();
        assert_eq!(update.session_id, session_id);
        assert_eq!(update.view, view_at_ordinal(ordinal));
    }
    assert!(receiver.try_recv().is_err());
}

/// A relay actor for a session that has ended must stop, not keep reconnecting
/// to a socket that will never exist again. Without this the daemon logs a
/// failure for a dead worker every backoff period for as long as it runs,
/// which is what #1078 reported alongside #1065.
#[cfg(unix)]
// Hard-won: #1078: An ended session must stop retrying its dead worker and logging every backoff.
#[tokio::test]
async fn a_terminal_session_retires_its_relay_actor() {
    const TERMINAL_ACTOR_CHILD: &str = "MJ_TEST_TERMINAL_ACTOR_CHILD";
    if std::env::var_os(TERMINAL_ACTOR_CHILD).is_none() {
        run_in_isolated_child(
            TERMINAL_ACTOR_CHILD,
            "a_terminal_session_retires_its_relay_actor",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut record = crate::database::load_state().unwrap().sessions[LEASED_RELAY_SESSION].clone();
    record.state = mj_core::state::SessionState::Error;
    record.last_error = Some("sub-agent startup failed: the worker process is gone".into());
    crate::database::save_session(&record).unwrap();

    let target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        // A worker that is never coming back: every connection attempt fails.
        spec: CommandSpec::new("sh", ["-c", "exit 1"]).purpose("unreachable test relay worker"),
        worker_recovery: None,
        project_memory: None,
    };
    let (_commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    let actor = mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    tokio::time::timeout(Duration::from_secs(30), actor)
        .await
        .expect("the actor of a terminal session must stop reconnecting")
        .expect("the actor must not panic");

    let view = view_rx.borrow().clone();
    let reported = format!("{:?}", view.error);
    assert!(
        reported.contains("ended as Error") && reported.contains("the worker process is gone"),
        "the last view must say why the session ended: {reported}"
    );
}

/// The controller discards the record of a session whose managed target is
/// gone. Its relay actor must read the missing record as the end of the
/// session, not as an unanswered question, or it reconnects forever to a
/// socket that no longer exists.
#[cfg(unix)]
#[tokio::test]
async fn a_discarded_session_retires_its_relay_actor() {
    const DISCARDED_ACTOR_CHILD: &str = "MJ_TEST_DISCARDED_ACTOR_CHILD";
    if std::env::var_os(DISCARDED_ACTOR_CHILD).is_none() {
        run_in_isolated_child(
            DISCARDED_ACTOR_CHILD,
            "a_discarded_session_retires_its_relay_actor",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    crate::database::delete_session(LEASED_RELAY_SESSION).unwrap();
    assert!(
        !crate::database::load_state()
            .unwrap()
            .sessions
            .contains_key(LEASED_RELAY_SESSION),
        "the test needs a session the store no longer holds"
    );

    let target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        // A worker that is never coming back: every connection attempt fails.
        spec: CommandSpec::new("sh", ["-c", "exit 1"]).purpose("unreachable test relay worker"),
        worker_recovery: None,
        project_memory: None,
    };
    let (_commands_tx, commands_rx) = mpsc::channel(4);
    let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
    let (_retirement_tx, retirement_rx) = watch::channel(false);
    let (view_tx, _view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, _updates_rx) = coalesced_update_channel();
    let actor = mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    tokio::time::timeout(Duration::from_secs(30), actor)
        .await
        .expect("the actor of a discarded session must stop reconnecting")
        .expect("the actor must not panic");
}

#[tokio::test]
async fn delegation_bypasses_an_unconsumed_dashboard_and_observes_request_only_changes() {
    let (feed, mut observations) = delegation_channel();
    let (mut updates, _unconsumed_dashboard) = coalesced_update_channel();
    updates.observer = Some(feed.register("parent"));
    let mut view = view_at_ordinal(1);
    let (watch, _) = watch::channel(ManagedSessionView::default());
    publish_view("parent", view.clone(), &watch, &updates);
    assert!(
        observations
            .recv()
            .await
            .unwrap()
            .1
            .unwrap()
            .requests
            .is_empty()
    );
    view.snapshot.as_mut().unwrap().subagent_requests.push(
        mj_core::subagent::SubagentToolRequest {
            originating_command_id: None,
            request_id: "spawn".into(),
            created_at_ms: 1,
            action: mj_core::subagent::SubagentToolAction::ListAgents,
        },
    );
    publish_view("parent", view, &watch, &updates);
    let (_, observation) = tokio::time::timeout(Duration::from_secs(1), observations.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.unwrap().requests[0].request_id, "spawn");
}

#[tokio::test]
async fn delegation_coalesces_fairly_and_ignores_replaced_actor_publications() {
    let (feed, mut observations) = delegation_channel();
    let early = feed.register("a");
    let old = feed.register("z");
    early.publish(&view_at_ordinal(1));
    old.publish(&view_at_ordinal(1));
    assert_eq!(observations.recv().await.unwrap().0, "a");
    for n in 2..200 {
        let mut view = view_at_ordinal(n);
        view.snapshot.as_mut().unwrap().subagent_requests.push(
            mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: n.to_string(),
                created_at_ms: 1,
                action: mj_core::subagent::SubagentToolAction::ListAgents,
            },
        );
        early.publish(&view);
    }
    assert_eq!(observations.recv().await.unwrap().0, "z");
    assert_eq!(
        observations.recv().await.unwrap().1.unwrap().requests[0].request_id,
        "199"
    );
    let replacement = feed.register("z");
    let mut view = view_at_ordinal(2);
    view.snapshot.as_mut().unwrap().subagent_requests.push(
        mj_core::subagent::SubagentToolRequest {
            originating_command_id: None,
            request_id: "new-actor".into(),
            created_at_ms: 1,
            action: mj_core::subagent::SubagentToolAction::ListAgents,
        },
    );
    replacement.publish(&view);
    old.publish(&view_at_ordinal(3));
    drop(old);
    assert_eq!(
        observations.recv().await.unwrap().1.unwrap().requests[0].request_id,
        "new-actor"
    );
    drop(replacement);
    assert!(observations.recv().await.unwrap().1.is_none());
}

#[test]
fn replaced_actors_cannot_publish_or_leave_a_queued_observation() {
    let (sender, mut receiver) = coalesced_update_channel();
    let old = sender.for_actor("session-1");
    let update = |detail: &str| SessionManagerUpdate {
        raw_observation: None,
        session_id: "session-1".into(),
        view: ManagedSessionView {
            attention: Default::default(),
            error: Some(ViewError::Unreachable(detail.into())),
            ..Default::default()
        },
    };
    old.send(update("queued old view"));
    let current = sender.for_actor("session-1");
    assert!(receiver.try_recv().is_err());
    old.send(update("late old view"));
    assert!(receiver.try_recv().is_err());
    drop(old);
    current.send(update("current view"));
    assert_eq!(
        receiver.try_recv().unwrap().view.error.unwrap().detail(),
        "current view"
    );
    // A final observation survives normal actor completion until consumed.
    current.send(update("completed view"));
    drop(current);
    assert_eq!(
        receiver.try_recv().unwrap().view.error.unwrap().detail(),
        "completed view"
    );
}

#[tokio::test]
async fn replacing_a_delegation_actor_revokes_its_queued_observation() {
    let (feed, mut receiver) = delegation_channel();
    let old = feed.register("parent");
    old.publish(&view_at_ordinal(1));
    let current = feed.register("parent");
    let (id, observation) = receiver.recv().await.unwrap();
    assert_eq!(id, "parent");
    assert!(observation.is_none());
    old.publish(&view_at_ordinal(2));
    current.publish(&view_at_ordinal(3));
    assert!(receiver.recv().await.unwrap().1.is_some());
}

#[cfg(unix)]
#[test]
fn durable_worker_restart_never_kills_a_live_replacement_and_fences_old_completion() {
    const CHILD: &str = "MJ_DURABLE_RESTART_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "durable_worker_restart_never_kills_a_live_replacement_and_fences_old_completion",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut record = crate::database::load_state().unwrap().sessions[LEASED_RELAY_SESSION].clone();
    record.target = Some(Box::new(recovery_source_target()));
    record.state = mj_core::state::SessionState::Running;
    crate::database::save_session(&record).unwrap();
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart".into(),
            commands: vec![CommandSpec::new("restart", std::iter::empty::<&str>())],
        }
        .into(),
    };
    struct ProbeExecutor {
        live: bool,
        commands: Mutex<Vec<String>>,
    }
    impl CommandExecutor for ProbeExecutor {
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

        fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
            self.commands.lock().unwrap().push(command.program.clone());
            Ok(crate::targets::CommandOutput {
                status: 0,
                stdout: if self.live {
                    b"alive\n".to_vec()
                } else {
                    b"dead\n".to_vec()
                },
                stderr: Vec::new(),
            })
        }
    }
    let intent = crate::database::WorkerRestartIntent {
        operation_id: "replacement".into(),
        target: recovery_source_target(),
        desired_build: "new-build".into(),
    };
    crate::database::test_fixtures::worker_test_run_blocking(
        &record.id,
        "test intent",
        &crate::targets::ProcessExecutor,
        || crate::database::begin_worker_restart(&record.id, &intent),
    )
    .unwrap();
    for phase in [
        None,
        Some(crate::database::WorkerRestartPhase::Swapping),
        Some(crate::database::WorkerRestartPhase::AwaitingReadiness),
    ] {
        if let Some(phase) = phase {
            crate::database::test_fixtures::worker_test_run_blocking(
                &record.id,
                "test intent",
                &crate::targets::ProcessExecutor,
                || crate::database::advance_worker_restart(&record.id, &intent.operation_id, phase),
            )
            .unwrap();
        }
        let executor = ProbeExecutor {
            live: true,
            commands: Mutex::new(Vec::new()),
        };
        assert_eq!(
            recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor).unwrap(),
            WorkerRecoveryOutcome::Starting
        );
        assert_eq!(*executor.commands.lock().unwrap(), vec!["probe"]);
    }
    // An old attempt cannot erase a newer replacement's ownership.
    crate::database::test_fixtures::worker_test_run_blocking(
        &record.id,
        "test intent",
        &crate::targets::ProcessExecutor,
        || crate::database::finish_worker_restart(&record.id, "previous-attempt"),
    )
    .unwrap();
    assert_eq!(
        crate::database::load_worker_restart(&record.id).unwrap(),
        Some(intent.clone())
    );
    let executor = ProbeExecutor {
        live: false,
        commands: Mutex::new(Vec::new()),
    };
    assert_eq!(
        recover_worker_controlled(plan, true, Some(&record.id), &executor).unwrap(),
        WorkerRecoveryOutcome::RestartedDead
    );
    assert_eq!(*executor.commands.lock().unwrap(), vec!["probe", "restart"]);
    let mut stale = intent.clone();
    stale.target = mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/another-worker"),
    };
    assert!(
        crate::database::test_fixtures::worker_test_run_blocking(
            &record.id,
            "test intent",
            &crate::targets::ProcessExecutor,
            || { crate::database::begin_worker_restart(&record.id, &stale) }
        )
        .is_err()
    );
    crate::database::test_fixtures::worker_test_run_blocking(
        &record.id,
        "test intent",
        &crate::targets::ProcessExecutor,
        || crate::database::finish_worker_restart(&record.id, &intent.operation_id),
    )
    .unwrap();
    let replacement = crate::database::load_worker_restart(&record.id)
        .unwrap()
        .unwrap();
    assert_ne!(replacement.operation_id, intent.operation_id);
    assert_eq!(
        crate::database::worker_restart_phase(&record.id).unwrap(),
        Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
    );
    // Conditional claim cannot overwrite this boot, even on the same target.
    assert!(
        crate::database::test_fixtures::worker_test_run_blocking(
            &record.id,
            "test intent",
            &crate::targets::ProcessExecutor,
            || { crate::database::begin_worker_restart(&record.id, &intent) }
        )
        .is_err()
    );
    assert_eq!(
        crate::database::load_worker_restart(&record.id).unwrap(),
        Some(replacement)
    );
    // A first hello without native readiness cannot complete the handoff.
    let mut snapshot = view_at_ordinal(0).snapshot.unwrap();
    let observer = crate::database::test_fixtures::worker_test_observation(&record.id)
        .unwrap()
        .unwrap();
    crate::worker_lifecycle::observe_worker_restart_outcome(
        &observer,
        &record.target.clone().unwrap(),
        &snapshot,
    )
    .unwrap();
    assert!(
        crate::database::load_worker_restart(&record.id)
            .unwrap()
            .is_some()
    );
    drop(observer);
    snapshot.operational.checkpoint_only = true;
    let checkpoint_owner =
        crate::database::test_fixtures::worker_test_permit(&record.id, "checkpoint")
            .unwrap()
            .unwrap();
    // Even a nested observation cannot acquire an active owner's swap.
    checkpoint_owner.scope_blocking(|| {
        assert!(
            crate::database::test_fixtures::worker_test_observation(&record.id)
                .unwrap()
                .is_none()
        )
    });
    assert!(
        crate::database::load_worker_restart(&record.id)
            .unwrap()
            .is_some()
    );
    drop(checkpoint_owner);
    let observer = crate::database::test_fixtures::worker_test_observation(&record.id)
        .unwrap()
        .unwrap();
    crate::worker_lifecycle::observe_worker_restart_outcome(
        &observer,
        &record.target.clone().unwrap(),
        &snapshot,
    )
    .unwrap();
    assert!(
        crate::database::load_worker_restart(&record.id)
            .unwrap()
            .is_none()
    );
}

/// #1186: a park can hand back its lease after stopping the worker, before
/// the actor learns the target is gone. A deferred submit that then cannot
/// even connect was never sent, so it must reach `send_message` as a definite
/// refusal it can retry, not as a delivery that may have happened.
// Hard-won: #1186: A command that never connected to the parked worker must be retryable as definitely unsent.
#[test]
fn a_submit_that_never_reached_the_worker_is_a_definite_failure() {
    let never_sent = anyhow::anyhow!("connect worker socket: No such file or directory")
        .context(super::actor::NotSent);
    assert!(!super::actor::submit_failure(&never_sent).unconfirmed);
    let lost_reply = anyhow::anyhow!("relay connection closed while awaiting a reply");
    assert!(super::actor::submit_failure(&lost_reply).unconfirmed);
}

#[cfg(unix)]
#[tokio::test]
async fn actor_publishes_worker_cpu_and_removes_it_on_retirement() {
    const CHILD: &str = "MJ_TEST_CPU_ACTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "actor_publishes_worker_cpu_and_removes_it_on_retirement",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let root = tempfile::tempdir().unwrap();
    let usage = mj_core::cpu_usage::SessionCpuUsage {
        scope: Default::default(),
        recent_permille: 230,
        hourly_permille: 100,
        hourly_covered_secs: 20,
        online_cpus: 4,
    };
    std::fs::write(
        root.path().join("cpu-usage.json"),
        serde_json::to_vec(&usage).unwrap(),
    )
    .unwrap();
    let manager = spawn_session_manager().unwrap();
    manager
        .targets
        .send_replace(vec![leased_relay_target(root.path())]);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .session_cpu
                .borrow()
                .contains_key(LEASED_RELAY_SESSION)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager.session_cpu.borrow().get(LEASED_RELAY_SESSION),
        Some(&mj_client::runtime_feed::SessionCpuView::Measured { usage })
    );
    manager.targets.send_replace(Vec::new());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager.session_cpu.borrow().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    manager.shutdown.shutdown().await.unwrap();
}

#[test]
fn retired_cpu_producers_cannot_erase_or_overwrite_their_replacement() {
    let (updates, _receiver) = coalesced_update_channel();
    let old = updates.for_actor("session");
    old.publish_cpu(
        "session",
        Some(mj_client::runtime_feed::SessionCpuView::Unavailable {
            reason: "old".into(),
        }),
    );
    let current = updates.for_actor("session");
    let expected = mj_client::runtime_feed::SessionCpuView::Unavailable {
        reason: "current".into(),
    };
    current.publish_cpu("session", Some(expected.clone()));
    old.publish_cpu("session", None);
    drop(old);
    assert_eq!(updates.cpu.borrow().get("session"), Some(&expected));
    drop(current);
    assert!(updates.cpu.borrow().is_empty());
}

#[cfg(unix)]
// Hard-won: 94d7 container removal: relay hello retried ~10/s with no backoff after its container vanished
#[tokio::test]
async fn suppressed_recovery_preserves_hello_backoff() {
    const CHILD: &str = "MJ_TEST_SUPPRESSED_HELLO_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(CHILD, "suppressed_recovery_preserves_hello_backoff");
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    // The durable target differs from this actor's stale recovery plan.
    let root = tempfile::tempdir().unwrap();
    let attempts = root.path().join("attempts");
    let probed = root.path().join("probed");
    let mut probe = CommandSpec::new("sh", ["-c", "touch \"$PROBED\"; printf starting"])
        .purpose("record recovery probe");
    probe
        .env
        .insert("PROBED".into(), probed.to_string_lossy().into_owned());
    let mut spec = CommandSpec::new("sh", ["-c", "echo attempt >> \"$ATTEMPTS\"; exit 1"])
        .purpose("failed relay hello");
    spec.env
        .insert("ATTEMPTS".into(), attempts.to_string_lossy().into_owned());
    let target = RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.into(),
        spec,
        worker_recovery: Some(WorkerRecoveryPlan {
            source_target: recovery_source_target(),
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: probe.into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "unused restart".into(),
                commands: vec![],
            }
            .into(),
        }),
        project_memory: None,
    };
    let (_commands, commands_rx) = mpsc::channel(4);
    let (_releases, releases_rx) = mpsc::unbounded_channel();
    let (retirement, retirement_rx) = watch::channel(false);
    let (view, mut views) = watch::channel(ManagedSessionView::default());
    let (updates, _updates_rx) = coalesced_update_channel();
    let actor = mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view,
        updates,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            views.changed().await.unwrap();
            if views.borrow_and_update().error.is_some() {
                break;
            }
        }
    })
    .await
    .unwrap();
    // Recovery suppression must retain the actor and its two-second delay.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !actor.is_finished(),
        "suppression must not respawn a fresh reconnect owner"
    );
    assert_eq!(
        std::fs::read_to_string(&attempts).unwrap().lines().count(),
        2
    );
    // Once eligible, recovery must not wait out a cooldown for a probe
    // that suppression never performed.
    let mut record = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    record.target = Some(Box::new(recovery_source_target()));
    crate::database::save_session(&record).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !probed.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("suppression must not consume the recovery cooldown");
    assert_eq!(
        std::fs::read_to_string(&attempts).unwrap().lines().count(),
        3
    );
    retirement.send(true).unwrap();
    actor.await.unwrap();
}

#[cfg(unix)]
// Hard-won: 07d2 handoff: a recent unrelated record write armed the readiness timer and a fresh daemon failed a live session
#[tokio::test]
async fn a_fresh_actor_cannot_fail_a_worker_before_its_first_sync() {
    const CHILD: &str = "MJ_TEST_UNSYNCED_READINESS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "a_fresh_actor_cannot_fail_a_worker_before_its_first_sync",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let mut record = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    record.updated_at = chrono::Utc::now().to_rfc3339();
    crate::database::save_session(&record).unwrap();
    let root = tempfile::tempdir().unwrap();
    let (commands, commands_rx) = mpsc::channel(4);
    let (_releases, releases_rx) = mpsc::unbounded_channel();
    let (retirement, retirement_rx) = watch::channel(false);
    let (view_tx, _views) = watch::channel(ManagedSessionView::default());
    let (updates, _updates_rx) = coalesced_update_channel();
    let mut target = leased_relay_target(root.path());
    // A live worker whose first exchange is still in flight across handoff.
    // No protocol bytes arrive until the actor is retired.
    let started = root.path().join("started");
    target.spec = CommandSpec::new("sh", ["-c", "touch \"$STARTED\"; exec sleep 1000"]);
    target
        .spec
        .env
        .insert("STARTED".into(), started.to_string_lossy().into_owned());
    let actor = mj_core::runtime::spawn(run_session_actor(
        target,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the live worker must be waiting on its first sync");
    tokio::time::pause();
    tokio::time::advance(mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT * 2).await;
    tokio::time::resume();
    assert_eq!(
        crate::database::load_session_record(LEASED_RELAY_SESSION)
            .unwrap()
            .unwrap()
            .state,
        mj_core::state::SessionState::Running
    );
    retirement.send(true).unwrap();
    drop(commands);
    actor.await.unwrap();
}

/// The owner must drain delivery before giving a person's Move the relay.
#[cfg(unix)]
#[tokio::test]
async fn foreground_move_preempts_delegation_delivery_and_obtains_its_relay_lease() {
    if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
        run_in_isolated_child(
            DEFERRED_SUBMIT_TEST_CHILD,
            "foreground_move_preempts_delegation_delivery_and_obtains_its_relay_lease",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let root = tempfile::tempdir().unwrap();
    let channels = spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![leased_relay_target(root.path())])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let delivery = gate
        .try_start(
            LEASED_RELAY_SESSION,
            crate::recovery_gate::BackgroundWork::DelegationDelivery,
        )
        .unwrap();
    let lease = handle.lease_connection(&delivery).await.unwrap();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let mut moving = Box::pin(gate.enter_foreground(
        LEASED_RELAY_SESSION,
        "Move",
        crate::recovery_gate::LifecycleOrigin::Person,
        &cancelled,
        &crate::targets::ProcessExecutor,
    ));
    assert!(futures::poll!(&mut moving).is_pending());
    assert!(delivery.load(std::sync::atomic::Ordering::Acquire));
    drop(delivery);
    assert!(
        futures::poll!(&mut moving).is_pending(),
        "admission must survive until the actor applies the return"
    );
    lease.release();
    let moving = tokio::time::timeout(Duration::from_secs(5), moving)
        .await
        .unwrap()
        .unwrap();
    let moved = handle
        .lease_connection(&moving)
        .await
        .expect("Move must never meet a duplicate actor lease");
    moved.release();
    drop(moving);
    channels.shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
// Hard-won: 583e6ea48: delegation replication redialed workers deliberately stopped under a lifecycle lease.
#[tokio::test]
async fn lifecycle_lease_stops_delegation_dials_until_control_returns() {
    const CHILD: &str = "MJ_TEST_LEASE_STOPS_DELEGATION";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "lifecycle_lease_stops_delegation_dials_until_control_returns",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (actor, lease_id, mut connection) = lease_a_live_actor().await;
    let root = actor._relay_root.path();
    let requests = || std::fs::read_to_string(root.join(RELAY_REQUEST_LOG)).unwrap();
    let before = requests();
    // Model the stopped worker while its lifecycle owner still holds control.
    std::fs::write(root.join("worker-unreachable"), b"stopped for park or swap").unwrap();
    connection.sync().await.unwrap_err();
    let mut record = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    record.state = mj_core::state::SessionState::Parked;
    crate::database::save_session(&record).unwrap();
    let after_stop = requests();
    assert!(after_stop.len() > before.len());
    tokio::time::sleep(RECONNECT_INTERVAL * 3).await;
    assert_eq!(
        requests(),
        after_stop,
        "the leased session must have no background dialer"
    );

    // The same lease can replace a worker. Returning it resumes replication,
    // whereas a park retires the actor while still holding its lease.
    std::fs::remove_file(root.join("worker-unreachable")).unwrap();
    record.state = mj_core::state::SessionState::Running;
    crate::database::save_session(&record).unwrap();
    connection = StandaloneSession::connect(&leased_relay_target(root))
        .await
        .unwrap();
    assert!(
        !connection.waits_for_changes(),
        "lifecycle connections do not start observers"
    );
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: Some(Box::new(connection)),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !requests()[after_stop.len()..].contains("poll_subagent_queue") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("returning control must resume queue observation");

    let (reply, response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Lease {
            returns: actor.releases.clone(),
            admission: crate::database::test_fixtures::relay_admission(LEASED_RELAY_SESSION),
            idle_harness: None,
            reply,
        })
        .await
        .unwrap();
    let mut lease = response.await.unwrap().unwrap();
    // Hard-won: 583e6ea48: close returned its sealed connection before target teardown, restarting background polls.
    let mut closed = (*lease.connection_mut().operational).clone();
    closed.execution = mj_core::relay::RelayExecutionState::Closed;
    lease.connection_mut().operational = closed.into();
    lease.release();
    tokio::time::timeout(Duration::from_secs(5), async {
        while actor
            ._views
            .borrow()
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| {
                snapshot.operational.execution != mj_core::relay::RelayExecutionState::Closed
            })
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let after_close = requests();
    tokio::time::sleep(RECONNECT_INTERVAL * 3).await;
    assert_eq!(
        requests(),
        after_close,
        "a sealed relay must not be followed into target teardown"
    );
    actor.retirement.send_replace(true);
}

#[cfg(unix)]
// Hard-won: 583e6ea48: every transport retry and proxy exit repeated the same worker-loss warning.
#[tokio::test]
async fn worker_loss_warns_once_until_reachable_and_warns_on_the_next_loss() {
    const CHILD: &str = "MJ_TEST_WORKER_LOSS_TRANSITIONS";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "worker_loss_warns_once_until_reachable_and_warns_on_the_next_loss",
        );
        return;
    }
    let log = crate::test_log::CapturedLog::default();
    tracing::subscriber::set_global_default(log.clone()).unwrap();
    let _writer = crate::database::install_isolated_test_writer();
    let (actor, lease_id, connection) = lease_a_live_actor().await;
    let root = actor._relay_root.path();
    let marker = root.join("worker-unreachable");
    std::fs::write(&marker, b"unexpected worker loss").unwrap();
    drop(connection);
    actor
        .releases
        .send(ReturnedConnection {
            lease_id,
            connection: None,
        })
        .unwrap();
    let warnings = || {
        log.at(tracing::Level::WARN)
            .into_iter()
            .filter(|event| event.contains("session worker is "))
            .collect::<Vec<_>>()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while warnings().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(RECONNECT_INTERVAL * 4).await;
    assert_eq!(warnings().len(), 1, "{:?}", warnings());
    assert!(warnings()[0].contains("unreachable"));
    std::fs::remove_file(&marker).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while warnings().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(warnings()[1].contains("reachable again"));
    std::fs::write(marker, b"second worker loss").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while warnings().len() < 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(warnings().len(), 3, "{:?}", warnings());
    assert!(warnings()[2].contains("unreachable"));
    assert!(
        log.at(tracing::Level::WARN)
            .iter()
            .all(|event| !event.contains("relay request failed")
                && !event.contains("dropped relay proxy")
                && !event.contains("delegation is unavailable")),
        "{:?}",
        log.events()
    );
    actor.retirement.send_replace(true);
}

// Hard-won: #1629: waiting for a replacement actor must use the manager's readiness signal instead of a guessed 5-second timeout, and must fail promptly on removal or shutdown.
#[tokio::test]
async fn wait_for_session_ready_wakes_on_registration_and_fails_on_removal_or_shutdown() {
    let channels = spawn_session_manager().unwrap();
    let control = channels.control.clone();

    // 1. Wait before the target is published; it resolves as soon as the actor is registered.
    let mut waiting = Box::pin(control.wait_for_session_ready("session-1"));
    assert!(futures::poll!(&mut waiting).is_pending());

    let mut first_target = target("true");
    first_target.session_id = "session-1".into();
    channels.targets.send(vec![first_target.clone()]).unwrap();

    let handle = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("wait_for_session_ready must wake on actor registration")
        .expect("registered actor handle");
    assert_eq!(handle.session_id(), "session-1");

    // 2. Calling again while the actor matches the current target resolves immediately.
    let immediate = tokio::time::timeout(
        Duration::from_secs(5),
        control.wait_for_session_ready("session-1"),
    )
    .await
    .expect("active actor must resolve immediately")
    .expect("active actor handle");
    assert_eq!(immediate.session_id(), "session-1");

    // 3. Removing a session from desired targets or shutting down fails parked waiters promptly.
    let mut shutdown_wait = Box::pin(control.wait_for_session_ready("session-2"));
    assert!(futures::poll!(&mut shutdown_wait).is_pending());
    channels.shutdown.shutdown().await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), shutdown_wait)
        .await
        .expect("shutdown must fail parked waiters promptly")
        .expect_err("shutdown must return an error");
    assert!(
        err.to_string().contains("session manager stopped"),
        "unexpected shutdown error: {err:#}"
    );
}
