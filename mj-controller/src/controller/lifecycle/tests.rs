use std::cell::RefCell;
use std::collections::BTreeMap;

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
        let status = self
            .statuses
            .borrow_mut()
            .pop()
            .expect("test cleanup command status");
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
                environment: BTreeMap::new(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    }
}

#[test]
fn deferred_cleanup_failure_is_visible_and_successful_retry_clears_it() {
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
                statuses: RefCell::new(vec![0, 0, 0]),
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
                environment: BTreeMap::new(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
    assert_eq!(commands.len(), 4);
    assert_eq!(
        commands[1].stage,
        Some(targets::ProvisionStage::RemovingContainer)
    );
    assert_eq!(
        commands[2].stage,
        Some(targets::ProvisionStage::RemovingStorage)
    );
    assert_eq!(
        commands[3].stage,
        Some(targets::ProvisionStage::CleaningCache)
    );
    assert!(commands[2].args.contains(&volume));
    assert!(controller.state.sessions[session_id].target.is_none());
}
#[test]
fn verified_close_retires_managed_checkout_but_keeps_archive_and_branch() {
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
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

#[test]
fn force_destroy_from_running_removes_target_worktree_branch_and_archive() {
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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

#[test]
fn force_destroy_keeps_the_branch_and_removes_the_checkout_by_default() {
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
fn force_destroy_without_a_target_or_archive_still_removes_the_record() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.target = None;
    session.checkpoint = None;
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
fn force_destroy_aborts_and_keeps_the_record_when_the_target_survives() {
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
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
}

#[test]
fn force_destroy_tolerates_a_missing_archive() {
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
            sessions: BTreeMap::from([(session_id.into(), session)]),
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
