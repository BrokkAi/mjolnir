use std::cell::RefCell;

use anyhow::Result;

use crate::controller::Controller;
use crate::controller::test_support::{
    IsolatedTest, checkpoint_test_session, committed_repository, managed_worktree_session,
    test_git, write_checkpoint_gate_archive,
};
use mj_core::config::{Config, ContainerTemplate as ConfigContainer, TargetTemplate};
use mj_core::state::{SessionState, State, TargetLocator};

use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};

use super::*;

// Hard-won: b9c3a052: failed worker restart left close without the target needed for destroy
#[test]
fn a_close_whose_restart_left_no_worker_records_error_and_keeps_the_target() {
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.state = SessionState::Running;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: "/tmp/mjolnir-close-test".into(),
    });
    let previous = session.clone();
    let error =
        anyhow::anyhow!("connect to Mjolnir worker").context(super::WorkerRestartLeftNoWorker);

    apply_close_checkpoint_failure(
        &mut session,
        &previous,
        &error,
        "2026-08-14T12:00:00Z".into(),
    );

    assert_eq!(session.state, SessionState::Error);
    assert!(session.last_error.is_some(), "{:?}", session.last_error);
    assert!(session.last_checkpoint_error.is_some());
    assert!(
        session.target.is_some(),
        "a forced destroy still needs the target"
    );
    assert_eq!(session.updated_at, "2026-08-14T12:00:00Z");
}

#[test]
fn a_close_whose_checkpoint_failed_with_a_live_worker_returns_to_its_previous_state() {
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.state = SessionState::Running;
    session.last_error = None;
    let previous = session.clone();
    session.state = SessionState::Closing;
    let error = anyhow::anyhow!("the session was busy");

    apply_close_checkpoint_failure(
        &mut session,
        &previous,
        &error,
        "2026-08-14T12:00:00Z".into(),
    );

    assert_eq!(session.state, SessionState::Running);
    assert!(session.last_error.is_none());
    assert_eq!(
        session.last_checkpoint_error.as_deref(),
        Some("the session was busy")
    );
}

struct DeferredCleanupExecutor {
    statuses: RefCell<Vec<i32>>,
}

impl CommandExecutor for DeferredCleanupExecutor {
    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        let status = self.statuses.borrow_mut().pop().unwrap_or(0);
        Ok(CommandOutput {
            status,
            stdout: Vec::new(),
            stderr: if status != 0 {
                b"cleanup failed".to_vec()
            } else {
                Vec::new()
            },
        })
    }
}

fn stopped_podman_cleanup_controller(session_id: &str) -> Controller {
    let container_id = targets::resource_name(session_id).unwrap();
    let volume = format!("{container_id}-workspace");
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "podman".into();
    session.state = SessionState::Stopped;
    session.target = Some(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume { name: volume },
    });
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    }
}

#[test]
fn deferred_cleanup_failure_is_visible_and_successful_retry_clears_it() {
    if !in_isolated_store("deferred_cleanup_failure_is_visible_and_successful_retry_clears_it") {
        return;
    }
    crate::database::load_state().unwrap();

    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let persisted = RefCell::new(Vec::new());
    let failure = controller
        .cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                // The cleanup command fails, then the exact-absence probe
                // confirms that the owned target is still present.
                statuses: RefCell::new(vec![1, 1]),
            },
            |record| {
                persisted
                    .borrow_mut()
                    .push((record.target.is_some(), record.last_error.clone()));
                Ok(())
            },
        )
        .unwrap_err();
    assert!(format!("{failure:#}").contains("cleanup failed"));
    assert_eq!(persisted.borrow().len(), 1);
    assert!(persisted.borrow()[0].0);
    assert!(
        persisted.borrow()[0]
            .1
            .as_deref()
            .is_some_and(|error| error.contains("deferred target cleanup failed"))
    );
    assert!(controller.state.sessions[session_id].target.is_some());
    assert!(controller.state.sessions[session_id].last_error.is_some());

    let retry_persisted = RefCell::new(Vec::new());
    controller
        .cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                statuses: RefCell::new(Vec::new()),
            },
            |record| {
                retry_persisted
                    .borrow_mut()
                    .push((record.target.is_some(), record.last_error.clone()));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(retry_persisted.borrow().as_slice(), &[(false, None)]);
    assert!(controller.state.sessions[session_id].target.is_none());
    assert!(controller.state.sessions[session_id].last_error.is_none());
}

#[test]
fn deferred_cleanup_persistence_failure_restores_the_stopped_record() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let previous = controller.state.sessions[session_id].clone();
    let failure = controller
        .cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                statuses: RefCell::new(vec![1, 1]),
            },
            |_| Err(anyhow::anyhow!("database unavailable")),
        )
        .unwrap_err();
    let detail = format!("{failure:#}");
    assert!(detail.contains("cleanup failed"), "{detail}");
    assert!(detail.contains("database unavailable"), "{detail}");
    assert_eq!(controller.state.sessions[session_id], previous);
}

#[test]
fn target_cleanup_persists_destroying_and_rechecks_the_installed_archive() {
    if !in_isolated_store("target_cleanup_persists_destroying_and_rechecks_the_installed_archive") {
        return;
    }
    crate::database::load_state().unwrap();

    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "local".into();
    session.state = SessionState::Closing;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    controller
        .destroy_after_verified_checkpoint_with(session_id, &checkpoint, &executor, |record| {
            persisted.borrow_mut().push(record.state);
            Ok(())
        })
        .unwrap();

    assert_eq!(
        persisted.into_inner(),
        vec![SessionState::Destroying, SessionState::Stopped]
    );
    assert_eq!(executor.commands.borrow().len(), 1);
    let stopped = &controller.state.sessions[session_id];
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.target.is_none());
}

// Hard-won: ce3218ad: destroy raced recovery and failed cleanup could restart the target
#[test]
fn destruction_waits_for_recovery_and_failed_cleanup_never_restarts_the_target() {
    use crate::session_manager::{
        WorkerRecoveryOutcome, WorkerRecoveryPlan, recover_worker_controlled,
    };
    use std::sync::{Mutex, mpsc};
    const CHILD: &str = "MJ_DESTRUCTION_RECOVERY_TEST_CHILD";
    const TEST: &str = "controller::lifecycle::tests::destruction_waits_for_recovery_and_failed_cleanup_never_restarts_the_target";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(TEST)
            .env(CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let record = controller.state.sessions.get_mut(session_id).unwrap();
    record.state = SessionState::Closing;
    record.checkpoint = Some(checkpoint.clone());
    crate::database::save_session(record).unwrap();
    let plan = WorkerRecoveryPlan {
        source_target: record.target.clone().unwrap(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()),
        binary_refresh: None,
        launch_refresh: None,
        restart: targets::CommandPlan {
            description: "restart worker".into(),
            commands: vec![CommandSpec::new("restart", std::iter::empty::<&str>())],
        },
    };
    struct PausedRecovery {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        restarted: Mutex<bool>,
    }
    impl CommandExecutor for PausedRecovery {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.program == "probe" {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            } else {
                assert_eq!(command.program, "restart");
                assert_eq!(crate::database::load_state().unwrap().sessions["0123456789abcdef0123456789abcdef"].state,
                    SessionState::Closing, "recovery must finish before destruction is persisted");
                *self.restarted.lock().unwrap() = true;
            }
            Ok(CommandOutput {
                status: 0,
                stdout: b"dead\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let recovery = PausedRecovery {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        restarted: Mutex::new(false),
    };
    let (persisted_tx, persisted_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let recovering = scope
            .spawn(|| recover_worker_controlled(plan.clone(), false, Some(session_id), &recovery));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let closing = scope.spawn(|| {
            controller.destroy_after_verified_checkpoint_with(
                session_id,
                &checkpoint,
                &DeferredCleanupExecutor {
                    statuses: RefCell::new(vec![125]),
                },
                |record| {
                    assert!(*recovery.restarted.lock().unwrap());
                    crate::database::save_lifecycle_session(record)?;
                    persisted_tx.send(record.state).unwrap();
                    Ok(())
                },
            )
        });
        assert!(
            persisted_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            recovering.join().unwrap().unwrap(),
            WorkerRecoveryOutcome::RestartedDead
        );
        assert!(closing.join().unwrap().is_err());
    });
    assert_eq!(
        persisted_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        SessionState::Destroying
    );
    assert_eq!(
        crate::database::load_state().unwrap().sessions[session_id].state,
        SessionState::Destroying
    );
    assert_eq!(
        controller.state.sessions[session_id].checkpoint.as_ref(),
        Some(&checkpoint)
    );
    // An empty executor panics if a stale retry attempts any target command.
    for _ in 0..3 {
        assert!(crate::pollers::dashboard_worker_targets(&controller).is_empty());
        assert_eq!(
            recover_worker_controlled(
                plan.clone(),
                true,
                Some(session_id),
                &DeferredCleanupExecutor {
                    statuses: RefCell::new(Vec::new())
                }
            )
            .unwrap(),
            WorkerRecoveryOutcome::Suppressed
        );
    }
    assert!(
        controller
            .destroy_after_verified_checkpoint_with(
                session_id,
                &checkpoint,
                &DeferredCleanupExecutor {
                    statuses: RefCell::new(vec![0])
                },
                crate::database::save_lifecycle_session
            )
            .unwrap()
    );
    assert_eq!(
        crate::database::load_state().unwrap().sessions[session_id].state,
        SessionState::Stopped
    );
    assert_eq!(
        mj_checkpoint::checkpoint::checkpoint_sha256(&checkpoint.archive_path).unwrap(),
        checkpoint.sha256
    );
}

#[test]
fn podman_close_persists_stopped_before_deferred_storage_cleanup() {
    if !in_isolated_store("podman_close_persists_stopped_before_deferred_storage_cleanup") {
        return;
    }
    crate::database::load_state().unwrap();

    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let container_id = targets::resource_name(session_id).unwrap();
    let volume = format!("{container_id}-workspace");
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "podman".into();
    session.state = SessionState::Closing;
    session.target = Some(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: volume.clone(),
        },
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    let deferred = controller
        .destroy_after_verified_checkpoint_with(session_id, &checkpoint, &executor, |record| {
            persisted
                .borrow_mut()
                .push((record.state, record.target.is_some()));
            Ok(())
        })
        .unwrap();

    assert!(deferred);
    assert_eq!(
        persisted.borrow().as_slice(),
        &[
            (SessionState::Destroying, true),
            (SessionState::Stopped, true)
        ]
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].args[1].contains("podman stop --time 0"));
    assert!(!commands[0].args[1].contains("podman rm"));
    drop(commands);

    controller
        .cleanup_stopped_target_with(session_id, &executor, |record| {
            assert_eq!(record.state, SessionState::Stopped);
            assert!(record.target.is_none());
            Ok(())
        })
        .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(
        commands[1].stage,
        Some(targets::ProvisionStage::RemovingContainer)
    );
    assert_eq!(
        commands[2].stage,
        Some(targets::ProvisionStage::RemovingStorage)
    );
    assert_eq!(
        commands.last().unwrap().stage,
        Some(targets::ProvisionStage::CleaningCache)
    );
    assert!(commands[2].args.contains(&volume));
    assert!(controller.state.sessions[session_id].target.is_none());
}
#[test]
fn verified_close_retires_managed_checkout_but_keeps_archive_and_branch() {
    if !in_isolated_store("verified_close_retires_managed_checkout_but_keeps_archive_and_branch") {
        return;
    }
    crate::database::load_state().unwrap();

    let archive_directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(archive_directory.path(), session_id, 7);
    let mut session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();
    std::fs::write(worktree.worktree_root.join("dirty.txt"), "worktree state\n").unwrap();
    session.state = SessionState::Closing;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: archive_directory.path().join(session_id),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    controller
        .destroy_after_verified_checkpoint_with(session_id, &checkpoint, &ProcessExecutor, |_| {
            Ok(())
        })
        .unwrap();

    assert!(!worktree.worktree_root.exists());
    assert!(checkpoint.archive_path.is_file());
    assert_eq!(
        test_git(
            repository.path(),
            &[
                "show-ref",
                "--hash",
                &format!("refs/heads/{}", worktree.branch),
            ],
        )
        .len(),
        40
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
}
#[test]
fn force_stop_reuses_verified_archive_and_leaves_session_resumable() {
    if !in_isolated_store("force_stop_reuses_verified_archive_and_leaves_session_resumable") {
        return;
    }
    crate::database::load_state().unwrap();

    let archive_directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(archive_directory.path(), session_id, 7);
    let mut session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();
    session.state = SessionState::Running;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: archive_directory.path().join(session_id),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    controller
        .force_stop_with(session_id, &ProcessExecutor, |_| Ok(()))
        .unwrap();

    let stopped = &controller.state.sessions[session_id];
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.target.is_none());
    assert_eq!(stopped.checkpoint.as_ref(), Some(&checkpoint));
    assert!(checkpoint.archive_path.is_file());
    assert!(!worktree.worktree_root.exists());
    assert!(
        !test_git(
            repository.path(),
            &[
                "show-ref",
                "--hash",
                &format!("refs/heads/{}", worktree.branch),
            ],
        )
        .is_empty()
    );
}
/// A close of a session wedged in provisioning has no relay to latch and no
/// harness state to archive, so it tears the target down and settles instead
/// of waiting forever (#1059).
// Hard-won: 7aea6c5c: wedged provisioning close left its target and lifecycle unsettled
#[test]
fn closing_a_wedged_provisioning_session_tears_down_its_target_and_settles() {
    if !in_isolated_store("closing_a_wedged_provisioning_session_tears_down_its_target_and_settles")
    {
        return;
    }
    crate::database::load_state().unwrap();

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.checkpoint = None;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    });
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let deferred = controller
        .suspend_session_without_checkpoint_with(session_id, &ProcessExecutor, |_| Ok(()))
        .unwrap();

    assert!(!deferred, "a bare target needs no storage cleanup pass");
    let closed = &controller.state.sessions[session_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
    assert!(
        !worker_root.exists(),
        "the interrupted provision's worker root is removed"
    );
}

/// The same close refuses a session that still has a workspace worth
/// archiving, so it can never become a quiet force-destroy.
#[test]
fn closing_without_a_checkpoint_refuses_a_session_that_has_one_to_take() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    });
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let error = controller
        .suspend_session_without_checkpoint_with(session_id, &FailingExecutor, |_| Ok(()))
        .unwrap_err();

    assert!(
        error.to_string().contains("suspend it instead"),
        "{error:#}"
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Running
    );
}

/// A sub-agent whose first prompt was refused for good was recorded as
/// failed on a bare target, with its worker stopped.
fn failed_subagent_controller(worker_root: &std::path::Path, session_id: &str) -> Controller {
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Error;
    session.last_error = Some("this agent does not offer high as a effort".into());
    session.checkpoint = None;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.to_owned(),
    });
    let relation = mj_core::subagent::SubagentRecord {
        child_session_id: session_id.into(),
        parent_session_id: "parent-1".into(),
        task_name: "count lines".into(),
        profile_id: "claude".into(),
        model: Some("haiku".into()),
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "count lines".into(),
        request_key: "request-1".into(),
        created_at: "2026-09-29T00:00:00Z".into(),
        noticed_turn: None,
        handback_tool: true,
    };
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            subagents: [(session_id.into(), relation)].into_iter().collect(),
            ..State::default()
        },
    }
}

/// I1-2: closing a failed sub-agent that still names its target takes no
/// checkpoint (its worker is stopped and a child keeps no archive); it tears
/// the target down and settles.
// Hard-won: ba6c3427: failed startup child stayed live and could not close without a checkpoint
#[test]
fn closing_a_failed_subagent_tears_down_its_target_without_a_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller = failed_subagent_controller(&worker_root, session_id);

    controller
        .suspend_session_without_checkpoint_with(session_id, &ProcessExecutor, |_| Ok(()))
        .unwrap();

    let closed = &controller.state.sessions[session_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
    assert!(!worker_root.exists(), "the child's worker root is removed");
}

/// An in-flight state with nobody to finish it becomes a failure the user can
/// read, and a state that still has an owner is left alone (#1070).
// Hard-won: 7aea6c5c: interrupted lifecycle recovery left no readable failure cause
#[test]
fn reconciling_an_orphaned_in_flight_state_records_a_readable_cause() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.target = None;
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let persisted = RefCell::new(Vec::new());
    let persist = |record: &mj_core::state::SessionRecord| {
        persisted
            .borrow_mut()
            .push((record.state, record.last_error.clone()));
        Ok(())
    };

    assert!(
        controller
            .fail_interrupted_lifecycle_with(
                session_id,
                "the daemon stopped while provisioning",
                persist,
            )
            .unwrap()
    );
    assert_eq!(
        persisted.borrow().as_slice(),
        &[(
            SessionState::Error,
            Some("the daemon stopped while provisioning".to_owned())
        )],
        "the cause reaches the store, not just memory"
    );
    let failed = &controller.state.sessions[session_id];
    assert_eq!(failed.state, SessionState::Error);
    assert_eq!(
        failed.last_error.as_deref(),
        Some("the daemon stopped while provisioning")
    );

    // A record that has already settled is not overwritten by a stale
    // reconciliation decision.
    assert!(
        !controller
            .fail_interrupted_lifecycle_with(session_id, "a later restart", persist)
            .unwrap()
    );
    assert_eq!(
        controller.state.sessions[session_id].last_error.as_deref(),
        Some("the daemon stopped while provisioning")
    );
}

#[test]
fn force_stop_without_a_recovery_archive_does_not_touch_the_target() {
    struct RecordingExecutor {
        calls: RefCell<usize>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.borrow_mut() += 1;
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.checkpoint = None;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    });
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = RecordingExecutor {
        calls: RefCell::new(0),
    };

    let error = controller
        .force_stop_with(session_id, &executor, |_| Ok(()))
        .unwrap_err();

    assert!(error.to_string().contains("existing recovery archive"));
    assert_eq!(*executor.calls.borrow(), 0);
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Running
    );
    assert!(controller.state.sessions[session_id].target.is_some());
}
#[test]
fn destroying_retry_blocks_cleanup_when_the_archive_gate_changed() {
    struct RecordingExecutor {
        calls: RefCell<usize>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.borrow_mut() += 1;
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();
    session.target_template_id = "local".into();
    session.state = SessionState::Destroying;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = RecordingExecutor {
        calls: RefCell::new(0),
    };
    let persisted = RefCell::new(Vec::new());
    std::fs::write(&checkpoint.archive_path, b"changed after checkpoint").unwrap();

    let error = controller
        .destroy_after_verified_checkpoint_with(session_id, &checkpoint, &executor, |record| {
            persisted.borrow_mut().push(record.state);
            Ok(())
        })
        .unwrap_err();

    assert!(error.to_string().contains("checkpoint SHA changed"));
    assert_eq!(*executor.calls.borrow(), 0);
    assert!(persisted.into_inner().is_empty());
    assert!(worktree.worktree_root.is_dir());
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Destroying
    );
}
#[test]
fn destroying_retry_finalizes_when_apple_container_is_confirmed_absent() {
    if !in_isolated_store("destroying_retry_finalizes_when_apple_container_is_confirmed_absent") {
        return;
    }
    crate::database::load_state().unwrap();

    struct AlreadyRemovedExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for AlreadyRemovedExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            if command.program == "sh"
                && command
                    .args
                    .get(1)
                    .is_some_and(|script| script.contains("container rm --force"))
            {
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"container not found".to_vec(),
                })
            } else {
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "apple".into();
    session.state = SessionState::Destroying;
    session.target = Some(TargetLocator::AppleContainer {
        borrowed_from: None,
        container_id: targets::resource_name(session_id).unwrap(),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config.targets.insert(
        "apple".into(),
        TargetTemplate::AppleContainer {
            container: ConfigContainer {
                build_cache: None,
                image: "test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = AlreadyRemovedExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    controller
        .destroy_after_verified_checkpoint_with(session_id, &checkpoint, &executor, |record| {
            persisted.borrow_mut().push(record.state);
            Ok(())
        })
        .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].program, "sh");
    assert!(commands[0].args[1].contains("container rm --force"));
    assert!(commands[0].args[1].contains(".cache/mjolnir/git/sessions"));
    assert_eq!(commands[1].args, ["list", "--all", "--quiet"]);
    assert_eq!(persisted.into_inner(), vec![SessionState::Stopped]);
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
}

#[test]
fn interrupted_close_error_preserves_destroying_phase() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Destroying;

    apply_interrupted_close_error(
        &mut session,
        &anyhow::anyhow!("podman unavailable"),
        "2026-08-14T12:00:00Z",
    );

    assert_eq!(session.state, SessionState::Destroying);
    assert_eq!(session.updated_at, "2026-08-14T12:00:00Z");
    assert!(
        session
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("cleanup is safely retryable"))
    );
}

struct FailingExecutor;

impl CommandExecutor for FailingExecutor {
    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        Ok(CommandOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: b"teardown unavailable".to_vec(),
        })
    }
}

fn branch_exists(repository: &std::path::Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

const ISOLATED_STORE_CHILD: &str = "MJ_LIFECYCLE_ISOLATED_STORE_CHILD";

/// Run the named test alone, with a data directory of its own: destroying a
/// session removes its attachment store under the data directory. Returns
/// whether this process is that run.
fn in_isolated_store(test: &str) -> bool {
    if std::env::var_os(ISOLATED_STORE_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(crate::controller::test_support::test_name(
        module_path!(),
        test,
    ))
    .env(ISOLATED_STORE_CHILD, "1")
    .isolated_store(directory.path())
    .run();
    false
}

#[test]
fn force_destroy_from_running_removes_target_worktree_branch_and_archive() {
    if !in_isolated_store("force_destroy_from_running_removes_target_worktree_branch_and_archive") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let deleted = RefCell::new(Vec::new());

    controller
        .force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
        .unwrap();

    assert!(!worker_root.exists(), "local target must be removed");
    let worktree_root = repository.path().join(".mj/worktrees").join(session_id);
    assert!(!worktree_root.exists(), "managed worktree must be removed");
    assert!(
        !branch_exists(repository.path(), &format!("mj/{session_id}")),
        "generated branch must be removed"
    );
    assert!(!checkpoint.archive_path.exists(), "archive must be removed");
    assert!(!controller.state.sessions.contains_key(session_id));
    assert_eq!(deleted.into_inner(), vec![session_id.to_owned()]);
}

// Hard-won: 472aff0b: default force destroy removed the generated branch with its checkout
#[test]
fn force_destroy_keeps_the_branch_and_removes_the_checkout_by_default() {
    if !in_isolated_store("force_destroy_keeps_the_branch_and_removes_the_checkout_by_default") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    });
    session.checkpoint = None;
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    controller
        .force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
        .unwrap();

    let worktree_root = repository.path().join(".mj/worktrees").join(session_id);
    assert!(!worktree_root.exists(), "managed worktree must be removed");
    assert!(
        branch_exists(repository.path(), &format!("mj/{session_id}")),
        "the session's branch must survive its destruction"
    );
    assert!(!controller.state.sessions.contains_key(session_id));
}

#[test]
fn force_destroy_aborts_and_keeps_the_record_when_the_target_survives() {
    if !in_isolated_store("force_destroy_aborts_and_keeps_the_record_when_the_target_survives") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "local".into();
    session.state = SessionState::Running;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    });
    session.checkpoint = Some(checkpoint.clone());
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    crate::database::save_session(&controller.state.sessions[session_id]).unwrap();
    let deleted = RefCell::new(Vec::new());

    let error = controller
        .force_destroy_session_with(
            session_id,
            &FailingExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
        .unwrap_err();

    assert!(
        error.to_string().contains("teardown unavailable"),
        "{error:#}"
    );
    assert!(
        controller.state.sessions.contains_key(session_id),
        "a surviving target must keep the record for a retry"
    );
    assert!(
        checkpoint.archive_path.exists(),
        "a surviving target must keep the recovery archive"
    );
    assert!(deleted.into_inner().is_empty());
    // The record is being destroyed: it must stop being polled, so the daemon
    // does not keep reconnecting to a worker the destroy stopped (I2-9). It
    // keeps its target, so the next destroy finishes the removal.
    let record = &controller.state.sessions[session_id];
    assert_eq!(record.state, SessionState::Error);
    assert!(record.target.is_some());
    assert!(!crate::pollers::session_target_is_pollable(record));
}

#[test]
fn force_destroy_tolerates_a_missing_archive() {
    if !in_isolated_store("force_destroy_tolerates_a_missing_archive") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    std::fs::remove_file(&checkpoint.archive_path).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Error;
    session.checkpoint = Some(checkpoint);
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    controller
        .force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |_| Ok(()),
        )
        .unwrap();

    assert!(!controller.state.sessions.contains_key(session_id));
}

/// A live session whose harness never advertised itself ends in `Error` with a
/// reason a driver can act on, and a record that has moved since the daemon
/// observed it is left alone (#1090).
// Hard-won: 7bcfe409: unusable harness left a live session without an actionable failure reason
#[test]
fn a_session_whose_harness_never_became_usable_is_failed_with_its_reason() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.updated_at = "2026-09-18T15:28:35Z".into();
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let persisted = RefCell::new(Vec::new());
    let persist = |record: &mj_core::state::SessionRecord| {
        persisted
            .borrow_mut()
            .push((record.state, record.last_error.clone()));
        Ok(())
    };
    let cause = "the harness never advertised its configuration within 300s";

    assert!(
        !controller
            .fail_unready_session_with(session_id, cause, "2026-09-18T15:20:00Z", persist)
            .unwrap(),
        "a record that has moved since the observation belongs to whatever moved it"
    );
    assert!(persisted.borrow().is_empty());

    assert!(
        controller
            .fail_unready_session_with(session_id, cause, "2026-09-18T15:28:35Z", persist)
            .unwrap()
    );
    assert_eq!(
        persisted.borrow().as_slice(),
        &[(SessionState::Error, Some(cause.to_owned()))],
        "the reason reaches the store, not just memory"
    );
    let failed = &controller.state.sessions[session_id];
    assert_eq!(failed.state, SessionState::Error);
    assert_eq!(failed.last_error.as_deref(), Some(cause));
    assert_ne!(
        failed.updated_at, "2026-09-18T15:28:35Z",
        "the failure is a state change of its own"
    );
}

#[test]
fn failed_suspension_replaces_stale_internal_error_and_preserves_recovery_data() {
    const CHILD: &str = "MJ_TEST_SUSPENSION_ERROR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            "failed_suspension_replaces_stale_internal_error_and_preserves_recovery_data",
        ))
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(root.path(), id, 7);
    let mut controller = stopped_podman_cleanup_controller(id);
    let record = controller.state.sessions.get_mut(id).unwrap();
    record.state = SessionState::Running;
    record.last_error = Some("old transport error with secret details".into());
    record.checkpoint = Some(checkpoint.clone());
    crate::database::save_session(record).unwrap();
    let target = record.target.clone();
    let reason = format!(
        "{}; see reference suspension-test",
        mj_core::state::CLOSE_FAILURE_PREFIX
    );
    assert!(controller.record_failed_close(id, &reason).unwrap());
    let restored = crate::database::load_state().unwrap();
    let record = &restored.sessions[id];
    assert_eq!(record.public_error(), Some(reason.as_str()));
    assert_eq!(record.state, SessionState::Running);
    assert_eq!(record.target, target);
    assert_eq!(record.checkpoint.as_ref(), Some(&checkpoint));
    assert!(checkpoint.archive_path.exists());
    let snapshot =
        crate::server::ViewerSnapshot::from_config_state(&controller.config, &restored, 1);
    assert_eq!(
        snapshot.sessions[0].launch_error.as_deref(),
        Some(reason.as_str())
    );
}

// -- releasing mbx build state when a workspace goes -----------------------

/// The `$0` the mbx release script runs under.
const RELEASE_LABEL: &str = "mj-mbx-release";

fn is_release(command: &CommandSpec) -> bool {
    command
        .args
        .iter()
        .any(|argument| argument.contains(RELEASE_LABEL))
}

/// Runs every command for real except the mbx release, which it records and
/// answers with `release_status`. It also notes whether the checkout still
/// existed when the release ran.
#[cfg(unix)]
struct RealTargetsRecordedRelease {
    release_status: i32,
    checkout: std::path::PathBuf,
    releases: RefCell<Vec<(CommandSpec, bool)>>,
}

#[cfg(unix)]
impl CommandExecutor for RealTargetsRecordedRelease {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if !is_release(command) {
            return ProcessExecutor.execute(command);
        }
        self.releases
            .borrow_mut()
            .push((command.clone(), self.checkout.exists()));
        Ok(CommandOutput {
            status: self.release_status,
            stdout: Vec::new(),
            stderr: if self.release_status == 0 {
                Vec::new()
            } else {
                b"mbx: permission denied".to_vec()
            },
        })
    }
}

/// Answers every command with success and records them in order.
#[derive(Default)]
struct RecordingTargets {
    commands: RefCell<Vec<CommandSpec>>,
}

impl RecordingTargets {
    fn releases(&self) -> Vec<CommandSpec> {
        self.commands
            .borrow()
            .iter()
            .filter(|command| is_release(command))
            .cloned()
            .collect()
    }

    /// The position of the first command whose arguments mention `needle`.
    fn position(&self, needle: &str) -> Option<usize> {
        self.commands.borrow().iter().position(|command| {
            command
                .args
                .iter()
                .any(|argument| argument.contains(needle))
        })
    }
}

impl CommandExecutor for RecordingTargets {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        Ok(CommandOutput {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// The arguments the release script receives after its `$0`.
fn release_arguments(release: &CommandSpec) -> &[String] {
    let label = release
        .args
        .iter()
        .position(|argument| argument == RELEASE_LABEL)
        .expect("the release script's $0");
    &release.args[label + 1..]
}

/// The repositories of the bundle the release tests' sessions check out.
fn two_repository_bundle() -> mj_core::config::ProjectBundle {
    let repository = |id: &str| mj_core::config::ProjectRepository {
        id: id.into(),
        github: Some(format!("owner/{id}")),
        destination: id.into(),
        ..Default::default()
    };
    mj_core::config::ProjectBundle {
        primary_repo: "app".into(),
        repositories: vec![repository("app"), repository("lib")],
    }
}

/// A running local managed clone session whose worker root is `worker_root`.
#[cfg(unix)]
fn running_managed_clone_controller(
    repository: &std::path::Path,
    worker_root: &std::path::Path,
    session_id: &str,
) -> Controller {
    let mut session =
        crate::controller::test_support::managed_clone_session(repository, session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.to_owned(),
    });
    session.checkpoint = None;
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    }
}

// Hard-won: 741163fe: removed cross-filesystem clone state remained in the shared mbx store
#[test]
#[cfg(unix)]
fn destroying_a_managed_clone_releases_its_mbx_build_state_once_the_checkout_is_gone() {
    if !in_isolated_store(
        "destroying_a_managed_clone_releases_its_mbx_build_state_once_the_checkout_is_gone",
    ) {
        return;
    }
    let _releases = crate::controller::mbx::release::enable_for_test();
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller =
        running_managed_clone_controller(repository.path(), &worker_root, session_id);
    let checkout = repository.path().join(".mj/clones").join(session_id);
    assert!(checkout.exists());
    let executor = RealTargetsRecordedRelease {
        release_status: 0,
        checkout: checkout.clone(),
        releases: RefCell::new(Vec::new()),
    };

    controller
        .force_destroy_session_with(session_id, &executor, BranchDisposition::Keep, |_| Ok(()))
        .unwrap();

    let releases = executor.releases.into_inner();
    assert_eq!(releases.len(), 1, "{releases:?}");
    let (release, checkout_present) = &releases[0];
    assert!(
        !checkout_present,
        "mbx is told only after the checkout is gone"
    );
    let arguments = release_arguments(release);
    assert_eq!(arguments[0], "native", "{release:?}");
    assert_eq!(
        &arguments[5..],
        [checkout.to_string_lossy().into_owned()],
        "the clone is the only workspace released"
    );
    assert!(!controller.state.sessions.contains_key(session_id));
}

#[cfg(unix)]
#[test]
fn a_failed_mbx_release_still_destroys_the_session_and_is_reported_by_doctor() {
    if !in_isolated_store(
        "a_failed_mbx_release_still_destroys_the_session_and_is_reported_by_doctor",
    ) {
        return;
    }
    let _releases = crate::controller::mbx::release::enable_for_test();
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller =
        running_managed_clone_controller(repository.path(), &worker_root, session_id);
    let checkout = repository.path().join(".mj/clones").join(session_id);
    let executor = RealTargetsRecordedRelease {
        release_status: 1,
        checkout: checkout.clone(),
        releases: RefCell::new(Vec::new()),
    };

    controller
        .force_destroy_session_with(session_id, &executor, BranchDisposition::Keep, |_| Ok(()))
        .unwrap();

    assert_eq!(executor.releases.borrow().len(), 1);
    assert!(!checkout.exists());
    assert!(!worker_root.exists());
    assert!(!controller.state.sessions.contains_key(session_id));
    let failures = crate::controller::recent_release_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].host, "local");
    assert!(
        failures[0].error.contains("permission denied"),
        "{failures:?}"
    );
    assert!(
        failures[0]
            .remediation
            .contains(&format!("mbx clean {}", checkout.display())),
        "{failures:?}"
    );
}

#[test]
fn destroying_an_ssh_bare_session_releases_each_repository_on_its_host() {
    if !in_isolated_store("destroying_an_ssh_bare_session_releases_each_repository_on_its_host") {
        return;
    }
    let _releases = crate::controller::mbx::release::enable_for_test();
    let session_id = "0123456789abcdef0123456789abcdef";
    let workspace = format!(".local/share/hel/workspaces/{session_id}");
    let template: TargetTemplate = serde_json::from_value(serde_json::json!({
        "kind": "ssh-bare", "host": "build.test", "user": "ubuntu", "permissions": "guardian",
    }))
    .unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "ssh".into();
    session.target_runtime = Some((&template).into());
    session.checkpoint = None;
    session.target = Some(TargetLocator::SshBare {
        host: "build.test".into(),
        workspace: workspace.clone().into(),
        worker_id: None,
    });
    let mut config = Config::default();
    config.targets.insert("ssh".into(), template);
    config
        .bundles
        .insert("project".into(), two_repository_bundle());
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = RecordingTargets::default();

    controller
        .force_destroy_session_with(session_id, &executor, BranchDisposition::Keep, |_| Ok(()))
        .unwrap();

    let releases = executor.releases();
    assert_eq!(releases.len(), 1, "{:?}", executor.commands.borrow());
    let release = &releases[0];
    assert_eq!(release.program, "ssh");
    let remote = release.args.last().unwrap();
    assert!(remote.contains("native"), "{remote}");
    for repository in ["app", "lib"] {
        assert!(
            remote.contains(&format!("{workspace}/{repository}")),
            "{repository} is released: {remote}"
        );
    }
    let removal = executor
        .position("rm -rf")
        .expect("the workspace removal ran");
    assert!(
        removal < executor.position(RELEASE_LABEL).unwrap(),
        "the workspace and its worker go before mbx is told"
    );
    assert!(!controller.state.sessions.contains_key(session_id));
}

/// A stopped Podman session, with or without the shared build cache.
fn stopped_cached_podman_controller(session_id: &str, cached: bool) -> Controller {
    let mut controller = stopped_podman_cleanup_controller(session_id);
    controller
        .config
        .bundles
        .insert("project".into(), two_repository_bundle());
    let session = controller.state.sessions.get_mut(session_id).unwrap();
    session.container_workspace =
        Some(mj_core::targets::new_container_workspace(session_id).unwrap());
    session.build_cache = cached.then(|| mj_core::state::SessionBuildCache {
        host: "local".into(),
        directory: "/srv/mbx-cache".into(),
        max_size: None,
        target_root: None,
    });
    controller
}

// Hard-won: 741163fe: container workspace state remained cached after the container was removed
#[test]
fn removing_a_cached_container_releases_its_workspaces_from_the_shared_cache() {
    if !in_isolated_store(
        "removing_a_cached_container_releases_its_workspaces_from_the_shared_cache",
    ) {
        return;
    }
    let _releases = crate::controller::mbx::release::enable_for_test();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_cached_podman_controller(session_id, true);
    let executor = RecordingTargets::default();

    controller
        .cleanup_stopped_target_with(session_id, &executor, |_| Ok(()))
        .unwrap();

    let releases = executor.releases();
    assert_eq!(releases.len(), 1, "{:?}", executor.commands.borrow());
    let arguments = release_arguments(&releases[0]);
    assert_eq!(
        &arguments[..4],
        [
            "shared",
            crate::controller::MBX_VERSION,
            "/srv/mbx-cache",
            "/srv/mbx-cache/.mjolnir/config"
        ]
    );
    assert_eq!(
        &arguments[5..],
        [
            format!("/workspace/{session_id}/app"),
            format!("/workspace/{session_id}/lib"),
        ]
    );
    assert!(
        executor.position("podman rm").unwrap() < executor.position(RELEASE_LABEL).unwrap(),
        "the container goes before mbx is told"
    );
    assert!(controller.state.sessions[session_id].target.is_none());
}

#[test]
fn destroying_a_subagent_never_releases_the_workspace_it_borrows() {
    if !in_isolated_store("destroying_a_subagent_never_releases_the_workspace_it_borrows") {
        return;
    }
    let _releases = crate::controller::mbx::release::enable_for_test();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let parent_id = "fedcba9876543210fedcba9876543210";
    let mut controller = failed_subagent_controller(directory.path(), session_id);
    controller
        .config
        .bundles
        .insert("project".into(), two_repository_bundle());
    let podman = stopped_podman_cleanup_controller(session_id).config.targets["podman"].clone();
    controller.config.targets.insert("podman".into(), podman);
    let child = controller.state.sessions.get_mut(session_id).unwrap();
    // A child runs in its parent's container, workspace and build cache.
    child.target_template_id = "podman".into();
    child.target = Some(TargetLocator::LocalPodman {
        borrowed_from: Some(parent_id.into()),
        container_id: targets::resource_name(parent_id).unwrap(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::ContainerLayer,
    });
    child.container_workspace = Some(mj_core::targets::new_container_workspace(parent_id).unwrap());
    child.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "local".into(),
        directory: "/srv/mbx-cache".into(),
        max_size: None,
        target_root: None,
    });
    let executor = RecordingTargets::default();

    controller
        .force_destroy_session_with(session_id, &executor, BranchDisposition::Keep, |_| Ok(()))
        .unwrap();

    assert!(
        executor.position("rm -rf").is_some(),
        "the child's own worker state still goes: {:?}",
        executor.commands.borrow()
    );
    assert!(executor.releases().is_empty());
    assert!(!controller.state.sessions.contains_key(session_id));
}

#[test]
fn starting_close_persists_its_intent_before_checkpointing() {
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.state = SessionState::Running;
    session.last_checkpoint_error = Some("old failure".into());

    apply_close_checkpoint_started(&mut session, "2026-08-14T12:00:00Z".into());

    assert_eq!(session.state, SessionState::Closing);
    assert_eq!(session.updated_at, "2026-08-14T12:00:00Z");
    assert!(session.last_checkpoint_error.is_none());
}

#[test]
fn force_destroy_without_a_target_or_archive_still_removes_the_record() {
    if !in_isolated_store("force_destroy_without_a_target_or_archive_still_removes_the_record") {
        return;
    }
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.target = None;
    session.checkpoint = None;
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let deleted = RefCell::new(Vec::new());

    controller
        .force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
        .unwrap();

    assert!(!controller.state.sessions.contains_key(session_id));
    assert_eq!(deleted.into_inner(), vec![session_id.to_owned()]);
}

#[test]
fn force_destroy_removes_a_failed_subagent() {
    if !in_isolated_store("force_destroy_removes_a_failed_subagent") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller = failed_subagent_controller(&worker_root, session_id);
    let deleted = RefCell::new(Vec::new());

    controller
        .force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Keep,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
        .unwrap();

    assert!(!worker_root.exists(), "the child's worker root is removed");
    assert!(!controller.state.sessions.contains_key(session_id));
    assert!(!controller.state.subagents.contains_key(session_id));
    assert_eq!(deleted.into_inner(), vec![session_id.to_owned()]);
}
