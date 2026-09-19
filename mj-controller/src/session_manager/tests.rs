use super::*;
#[cfg(unix)]
use crate::controller::test_support::{IsolatedTest, test_name};
use mj_core::hex::lower_hex;

fn recovery_source_target() -> mj_core::state::TargetLocator {
    mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/test-worker").join(LEASED_RELAY_SESSION),
    }
}

#[tokio::test]
async fn client_adapter_preserves_actor_replacement_and_submit_completion() {
    let mut fixture = replacement_session_test_fixture("client-session", 73);
    let stopped = fixture.stopped.client();
    assert!(stopped.is_stopped());

    let control = fixture.control.client();
    let replacement = control
        .wait_for_session("client-session", Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!replacement.is_stopped());
    let pending = replacement
        .enqueue_submit("client-command".into(), RelayCommand::Cancel)
        .await
        .unwrap();
    assert!(matches!(
        fixture.submitted.recv().await,
        Some(RelayCommand::Cancel)
    ));
    assert_eq!(pending.wait().await.unwrap(), 73);
}

#[tokio::test]
async fn session_adoption_deadline_also_bounds_an_unanswered_manager_request() {
    let (commands, mut requests) = mpsc::channel(1);
    let control = SessionManagerControl { commands };
    let request = tokio::spawn(async move {
        control
            .wait_for_session("muse", Duration::from_millis(20))
            .await
    });
    let ManagerCommand::Session { mut reply, .. } = requests.recv().await.unwrap();
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
use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
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
        admission: None,
        reply,
    }
}

/// `/effort` followed by a prompt has to reach the relay that way round,
/// or the prompt runs under the old setting. A bridge that spawns every
/// request concurrently loses that, so the order is pinned here: the
/// first request is held up, and the second must not overtake it.
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
async fn slow_reviewer_does_not_delay_primary_or_another_role_at_the_bridge() {
    let mut order = SessionRequestOrder::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let (done, mut received) = mpsc::unbounded_channel();
    let reviewer = |role: &str| RemoteSessionRequest::Reviewer {
        session_id: "one-session".to_owned(),
        role: Some(role.to_owned()),
        action: ReviewerAction::Pause,
        reply: oneshot::channel().0,
    };
    let held = release.clone();
    order.dispatch(
        reviewer("slow"),
        move |_| async move { held.notified().await },
    );
    let done_role = done.clone();
    order.dispatch(reviewer("other"), move |_| async move {
        done_role.send("other").unwrap();
    });
    order.dispatch(
        ordering_request("one-session", "prompt"),
        move |_| async move {
            done.send("primary").unwrap();
        },
    );
    let first = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    release.notify_one();
}

/// A session that has gone quiet must not leave a handle behind for ever:
/// a long-lived daemon serves many sessions.
#[tokio::test]
async fn finished_sessions_are_forgotten() {
    let mut order = SessionRequestOrder::new();
    for index in 0..8 {
        order.dispatch(
            ordering_request(&format!("session-{index}"), "only"),
            |_| async {},
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while order.latest.values().any(|handle| !handle.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request finished");
    }
    // The next dispatch prunes what has finished, so the map tracks live
    // work rather than every session ever seen.
    order.dispatch(ordering_request("session-last", "only"), |_| async {});
    assert_eq!(order.latest.len(), 1);
}

/// A reviewer action reaches a remote controller daemon as JSON, so both
/// halves of the exchange have to survive that round trip intact.
#[test]
fn reviewer_actions_and_outcomes_survive_the_daemon_wire() {
    let config = ReviewerLaunchConfig {
        profile_id: "claude".into(),
        harness: mj_core::config::HarnessKind::Claude,
        bridge_command: "npx".into(),
        bridge_args: vec!["claude-code-acp".into()],
        environment: BTreeMap::from([("EXTRA".into(), "1".into())]),
        execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
        model: Some("sonnet".into()),
        effort: Some("high".into()),
        fast_mode: None,
        generation: 2,
        mcp_servers: Vec::new(),
    };
    let actions = [
        ReviewerAction::Start {
            config: Box::new(config),
        },
        ReviewerAction::Submit {
            command_id: "review-1".into(),
            command: RelayCommand::Cancel,
        },
        ReviewerAction::Attach {
            after_ordinal: 4,
            after_digest: "digest".into(),
        },
        ReviewerAction::Acknowledge {
            through_ordinal: 4,
            through_digest: "digest".into(),
        },
        ReviewerAction::Status,
        ReviewerAction::Pause,
        ReviewerAction::CaptureDelta {
            baselines: BTreeMap::from([(std::path::PathBuf::from("/w/app"), "tree".into())]),
        },
        ReviewerAction::AdvanceBaseline {
            trees: BTreeMap::from([(std::path::PathBuf::from("/w/app"), "tree".into())]),
        },
        ReviewerAction::AnalyzeDelta {
            repositories: vec![mj_core::relay::AnalyzeDeltaRepository {
                root: std::path::PathBuf::from("/w/app"),
                baseline_tree: Some("base".into()),
                current_tree: "target".into(),
            }],
        },
    ];
    for action in actions {
        let encoded = serde_json::to_string(&action).unwrap();
        let decoded: ReviewerAction = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, action);
    }

    let outcome = ReviewerOutcome::Accepted { ordinal: 9 };
    let encoded = serde_json::to_string(&outcome).unwrap();
    let decoded: ReviewerOutcome = serde_json::from_str(&encoded).unwrap();
    assert!(matches!(decoded, ReviewerOutcome::Accepted { ordinal: 9 }));

    let paused = serde_json::to_string(&ReviewerOutcome::Paused).unwrap();
    assert!(matches!(
        serde_json::from_str::<ReviewerOutcome>(&paused).unwrap(),
        ReviewerOutcome::Paused
    ));

    let delta = ReviewerOutcome::Delta {
        repositories: vec![mj_core::relay::RepoDelta {
            root: std::path::PathBuf::from("/w/app"),
            baseline_tree: None,
            current_tree: "target".into(),
            patch: "diff --git a/a b/a\n".into(),
            diffstat: "1 file changed".into(),
            changed_lines: 1,
        }],
    };
    let encoded = serde_json::to_string(&delta).unwrap();
    let ReviewerOutcome::Delta { repositories } =
        serde_json::from_str::<ReviewerOutcome>(&encoded).unwrap()
    else {
        panic!("a captured delta must survive the daemon wire");
    };
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].current_tree, "target");
}

/// Every reviewer action names itself for the actor's logs and for the
/// rejection path, so a stalled review can be traced to the step it stalled
/// on.
#[test]
fn every_reviewer_action_names_its_operation() {
    let names = [
        ReviewerAction::Submit {
            command_id: String::new(),
            command: RelayCommand::Cancel,
        }
        .operation_name(),
        ReviewerAction::Attach {
            after_ordinal: 0,
            after_digest: String::new(),
        }
        .operation_name(),
        ReviewerAction::Acknowledge {
            through_ordinal: 0,
            through_digest: String::new(),
        }
        .operation_name(),
        ReviewerAction::Status.operation_name(),
        ReviewerAction::Pause.operation_name(),
    ];
    assert_eq!(
        names,
        [
            "reviewer_submit",
            "reviewer_attach",
            "reviewer_acknowledge",
            "reviewer_status",
            "reviewer_pause",
        ]
    );
    assert!(names.iter().all(|name| name.starts_with("reviewer_")));
}

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
    tokio::spawn(run_session_actor(
        dead,
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    let (reply, response) = oneshot::channel();
    commands_tx
        .send(ActorCommand::Lease { reply })
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
        liveness_probe: CommandSpec::new("printf", [format!("{liveness}\n")])
            .purpose("probe test worker liveness"),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test worker"),
            ],
        },
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
        liveness_probe: CommandSpec::new("printf", ["dead\n"])
            .purpose("probe test worker liveness"),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "must not restart missing workspace worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test worker"),
            ],
        },
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

#[tokio::test]
async fn recovery_replaces_only_a_stale_worker_binary_before_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("current-worker");
    let refreshed = directory.path().join("worker-refreshed");
    let restarted = directory.path().join("worker-restarted");
    std::fs::write(&source, b"current worker binary").unwrap();
    let current_digest = lower_hex(sha2::Sha256::digest(b"current worker binary"));
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
            liveness_probe: CommandSpec::new("printf", ["dead\n"])
                .purpose("probe test worker liveness"),
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
            },
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
            liveness_probe: CommandSpec::new("printf", ["dead\n"])
                .purpose("probe test worker liveness"),
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
            },
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
            liveness_probe: liveness,
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart test worker".into(),
                commands: vec![
                    CommandSpec::new("touch", [worker_restarted.to_string_lossy().into_owned()])
                        .purpose("restart test worker"),
                ],
            },
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
            liveness_probe: CommandSpec::new("false", std::iter::empty::<&str>()),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "must not restart".into(),
                commands: vec![CommandSpec::new("false", std::iter::empty::<&str>())],
            },
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
        snapshot: Some(ManagedSessionSnapshot {
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            window: mj_core::state::ProjectionWindow::of(&materialized),
            materialized,
            operational: RelayOperationalState {
                native_agent_count: 0,
                expected_continuation: None,
                goal: Default::default(),
                capacity_retry: None,
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
                checkpoint_only: false,
                acp_ready: None,
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
                tools_in_flight: Vec::new(),
                activity: None,
                harness_turn: None,
                last_harness_turn_started_ordinal: None,
                background_commands: Vec::new(),
                background_work_known: None,
            },
            latest_credential_sync_signal: None,
            worker_build: None,
        }),
        connected: true,
        error: None,
    }
}

#[test]
fn republishing_an_unchanged_view_notifies_nobody() {
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();

    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
    assert!(view_rx.has_changed().expect("watch stays open"));
    assert_eq!(
        updates_rx.try_recv().expect("the first view is news").view,
        view_at_ordinal(7)
    );
    let _ = view_rx.borrow_and_update();

    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);

    assert!(
        !view_rx.has_changed().expect("watch stays open"),
        "a sync tick that moved nothing must not wake the dashboard"
    );
    assert!(updates_rx.try_recv().is_err());
}

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
fn publishing_an_advanced_event_frontier_notifies_watchers() {
    let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
    let (updates_tx, mut updates_rx) = coalesced_update_channel();
    publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
    let _ = updates_rx.try_recv();
    let _ = view_rx.borrow_and_update();

    publish_view("session-1", view_at_ordinal(8), &view_tx, &updates_tx);

    assert!(view_rx.has_changed().expect("watch stays open"));
    let update = updates_rx.try_recv().expect("the advance is news");
    assert_eq!(update.session_id, "session-1");
    assert_eq!(
        update
            .view
            .snapshot
            .expect("published snapshot")
            .materialized
            .applied_event_ordinal,
        8
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
fn command_ids_are_namespaced_and_unique() {
    let first = new_command_id("prompt").unwrap();
    let second = new_command_id("prompt").unwrap();
    assert!(first.starts_with("prompt-"));
    assert_ne!(first, second);
}

#[test]
fn leased_actor_defers_replacement_and_uses_latest_queued_target() {
    let original = target("relay-v1");
    let intermediate = target("relay-v2");
    let latest = target("relay-v3");
    let mut lifecycle = ActorLifecycle::default();
    lifecycle.activate_lease(7);

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
    lifecycle.activate_lease(11);

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
    lifecycle.activate_lease(3);

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
    tokio::spawn(run_session_actor(
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

const LEASED_RELAY_ROOT: &str = "MJ_TEST_LEASED_RELAY_ROOT";
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
const LEASED_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

/// Relay server half of the leased-submission tests. It does nothing unless
/// a parent test points it at a relay journal root.
#[test]
fn leased_relay_child_serves_stdio() {
    let Some(root) = std::env::var_os(LEASED_RELAY_ROOT) else {
        return;
    };
    // With `--nocapture` libtest writes `test <name> ... ` without a
    // trailing newline before the body runs. End that line first so it
    // cannot glue itself onto the first protocol frame.
    println!();
    let mut relay = mj_worker::relay::DurableRelay::open(
        std::path::Path::new(&root),
        LEASED_RELAY_SESSION,
        "1.0.0",
    )
    .expect("open the test relay journal");
    {
        let marker = std::env::var_os("MJ_TEST_BLOCKED_REVIEWER");
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        while let Some(request) = mj_core::relay::read_relay_frame(&mut input).unwrap() {
            use mj_core::relay::{
                RelayRequest, RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload,
            };
            let history = match &request.request {
                RelayRequest::HistoryRequests => {
                    let path = PathBuf::from(&root).join("history-request.json");
                    let requests = if path.exists() {
                        vec![
                            serde_json::from_slice::<mj_core::history::HistoryRequest>(
                                &std::fs::read(path).unwrap(),
                            )
                            .unwrap(),
                        ]
                    } else {
                        Vec::new()
                    };
                    Some(RelayResponsePayload::HistoryRequests { requests })
                }
                RelayRequest::CompleteHistoryRequest { result } => {
                    std::fs::write(
                        PathBuf::from(&root).join("history-result.json"),
                        serde_json::to_vec(result).unwrap(),
                    )
                    .unwrap();
                    std::fs::remove_file(PathBuf::from(&root).join("history-request.json"))
                        .unwrap();
                    Some(RelayResponsePayload::HistoryRequestCompleted)
                }
                _ => None,
            };
            if let Some(payload) = history {
                mj_core::relay::write_relay_frame(
                    &mut output,
                    &RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: RelayResponseBody::Ok { payload },
                    },
                )
                .unwrap();
                continue;
            }
            let response =
                if let mj_core::relay::RelayRequest::Reviewer { role, .. } = &request.request {
                    if let Some(marker) = &marker
                        && role.as_deref() == Some("slow")
                    {
                        std::fs::write(marker, b"started").unwrap();
                        std::io::copy(&mut input, &mut std::io::sink()).unwrap();
                        std::fs::write(marker, b"disconnected").unwrap();
                        return;
                    }
                    mj_core::relay::RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: mj_core::relay::RelayResponseBody::Ok {
                            payload: mj_core::relay::RelayResponsePayload::ReviewerPaused,
                        },
                    }
                } else {
                    relay.handle(request)
                };
            mj_core::relay::write_relay_frame(&mut output, &response).unwrap();
        }
    }
}

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

#[cfg(unix)]
#[tokio::test]
async fn a_blocked_reviewer_keeps_primary_responsive_and_disconnects_on_cancellation() {
    const CHILD: &str = "MJ_TEST_REVIEWER_CANCELLATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "a_blocked_reviewer_keeps_primary_responsive_and_disconnects_on_cancellation",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("reviewer-status");
    let mut target = leased_relay_target(directory.path());
    target.spec.env.insert(
        "MJ_TEST_BLOCKED_REVIEWER".into(),
        marker.to_string_lossy().into_owned(),
    );
    let manager = spawn_session_manager().unwrap();
    manager.targets.send_replace(vec![target]);
    let session = manager
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(5))
        .await
        .unwrap();
    session.sync_now().await.unwrap();
    let slow = session.clone();
    let blocked = tokio::spawn(async move {
        slow.reviewer_as(Some("slow".into()), ReviewerAction::Pause)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slow reviewer started");
    tokio::time::timeout(Duration::from_secs(2), session.sync_now())
        .await
        .expect("primary sync must not wait for reviewer")
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        session.reviewer_as(Some("other".into()), ReviewerAction::Pause),
    )
    .await
    .expect("another role must remain responsive")
    .unwrap();
    blocked.abort();
    assert!(blocked.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read(&marker).unwrap() != b"disconnected" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelling the caller must disconnect its in-flight reviewer proxy");
    tokio::time::timeout(Duration::from_secs(2), manager.shutdown.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn relay_attach_does_not_probe_or_install_project_memory() {
    if std::env::var_os(EXPLICIT_MEMORY_SYNC_TEST_CHILD).is_none() {
        run_in_isolated_child(
            EXPLICIT_MEMORY_SYNC_TEST_CHILD,
            "relay_attach_does_not_probe_or_install_project_memory",
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
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
        .sync_project_memory()
        .await
        .expect("an explicit sync may detect a legacy memory endpoint");
    assert!(
        connection.project_memory.is_none(),
        "the explicit sync reached the relay and disabled its unavailable endpoint"
    );
}

/// Catching the local projection up to an accepted command is the
/// expensive half of a submit, and a caller waiting to hear that the relay
/// took the command should not wait for it. The two are separate calls, so
/// the cheap one can answer first.
#[cfg(unix)]
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
    record.target = Some(recovery_source_target());
    let plan = WorkerRecoveryPlan {
        source_target: recovery_source_target(),
        target: None,
        workspace: None,
        liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart".into(),
            commands: vec![CommandSpec::new("restart", std::iter::empty::<&str>())],
        },
    };
    #[derive(Default)]
    struct RecordingExecutor(Mutex<Vec<String>>);
    impl CommandExecutor for RecordingExecutor {
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
        record.target = target;
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

    record.target = Some(recovery_source_target());
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
    let mutex = crate::recovery_gate::worker_target_mutex(&record.id);
    let guard = mutex.lock().unwrap();
    std::thread::scope(|scope| {
        let pending = scope
            .spawn(|| recover_worker_controlled(plan.clone(), true, Some(&record.id), &executor));
        let mut destroying = record.clone();
        destroying.state = SessionState::Destroying;
        crate::database::save_session(&destroying).unwrap();
        drop(guard);
        assert_eq!(
            pending.join().unwrap().unwrap(),
            WorkerRecoveryOutcome::Suppressed
        );
    });
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
    record.target = Some(recovery_source_target());
    crate::database::save_session(&record).unwrap();
    let relay_root = tempfile::tempdir().unwrap();
    let restarted = relay_root.path().join("worker-restarted");
    let script = format!(
        "if [ ! -f \"${AUTO_RESTART_MARKER}\" ]; then IFS= read -r _; exit 0; fi; \
         \"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
        exact_test_name("leased_relay_child_serves_stdio")
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
        liveness_probe: CommandSpec::new("printf", ["alive\n"]).purpose("probe test relay worker"),
        binary_refresh: None,
        launch_refresh: None,
        restart: CommandPlan {
            description: "restart test relay worker".into(),
            commands: vec![
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                    .purpose("restart test relay worker"),
            ],
        },
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
    tokio::spawn(run_session_actor(
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

/// A relay target served by this test binary over stdio.
#[cfg(unix)]
fn leased_relay_target(relay_root: &std::path::Path) -> RelaySessionTarget {
    // `RelayClient` parses every stdout line as JSON, so libtest's own
    // progress lines are dropped before they reach the protocol reader.
    let script = format!(
        "\"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
        exact_test_name("leased_relay_child_serves_stdio")
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
    .purpose("test leased relay");
    spec.env.insert(
        LEASED_RELAY_ROOT.to_owned(),
        relay_root.to_string_lossy().into_owned(),
    );
    RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: None,
        project_memory: None,
    }
}

/// Register the session the projection writes to. `apply_projection_event`
/// rejects events for sessions the controller database does not know.
#[cfg(unix)]
fn register_leased_relay_session() {
    crate::database::save_session(&mj_core::state::SessionRecord {
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: LEASED_RELAY_SESSION.into(),
        title: "leased relay".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: mj_core::state::SessionState::Running,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    })
    .expect("register the test session");
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
    tokio::spawn(run_session_actor(
        leased_relay_target(relay_root.path()),
        commands_rx,
        releases_rx,
        retirement_rx,
        view_tx,
        updates_tx,
    ));

    let (reply, response) = oneshot::channel();
    commands_tx
        .send(ActorCommand::Lease { reply })
        .await
        .unwrap();
    let (lease_id, connection) = response
        .await
        .expect("actor answered the lease request")
        .expect("actor leased its relay connection");
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
async fn submit_a_deferred_prompt(
    actor: &LeasedActor,
) -> oneshot::Receiver<std::result::Result<u64, mj_client::session::SubmitFailure>> {
    let (reply, mut response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Submit {
            command_id: new_command_id("prompt").unwrap(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
            admission: None,
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
            connection: Some(connection),
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
            connection: Some(connection),
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
            connection: Some(connection),
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
            session_id: "session-1".into(),
            view: ManagedSessionView {
                error: Some(ViewError::Unreachable(format!("revision-{revision}"))),
                ..ManagedSessionView::default()
            },
        });
    }
    sender.send(SessionManagerUpdate {
        session_id: "session-2".into(),
        view: ManagedSessionView {
            error: Some(ViewError::Unreachable("other".into())),
            ..ManagedSessionView::default()
        },
    });

    assert_eq!(
        sender
            .pending
            .lock()
            .expect("session update coalescer poisoned")
            .len(),
        2
    );
    let updates = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()]
        .into_iter()
        .map(|update| (update.session_id, update.view.error.unwrap()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(updates["session-1"].detail(), "revision-999");
    assert_eq!(updates["session-2"].detail(), "other");
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn remote_session_manager_fans_out_views_and_forwards_commands() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![target("unused")]);
    remote
        .publisher
        .publish("session-1".into(), view_at_ordinal(7))
        .await
        .unwrap();

    let session = remote
        .control
        .wait_for_session("session-1", Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        session
            .view()
            .snapshot
            .as_ref()
            .unwrap()
            .materialized
            .applied_event_ordinal,
        7
    );

    let submitted = session
        .enqueue_submit("prompt-1".into(), RelayCommand::Cancel)
        .await
        .unwrap();
    let request = remote.requests.recv().await.unwrap();
    match request {
        RemoteSessionRequest::Submit {
            session_id,
            command_id,
            command: RelayCommand::Cancel,
            admission: None,
            reply,
        } => {
            assert_eq!(session_id, "session-1");
            assert_eq!(command_id, "prompt-1");
            reply.send(Ok(8)).unwrap();
        }
        _ => panic!("unexpected remote session request"),
    }
    assert_eq!(submitted.wait().await.unwrap(), 8);
    remote.shutdown.shutdown().await.unwrap();
}

/// A relay actor for a session that has ended must stop, not keep reconnecting
/// to a socket that will never exist again. Without this the daemon logs a
/// failure for a dead worker every backoff period for as long as it runs,
/// which is what #1078 reported alongside #1065.
#[cfg(unix)]
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
    let actor = tokio::spawn(run_session_actor(
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
    let actor = tokio::spawn(run_session_actor(
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
