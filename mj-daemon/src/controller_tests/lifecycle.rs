//! Tests of `mj_controller::controller::lifecycle` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use mj_controller::controller::lifecycle::test_fixtures::*;
use std::time::Duration;

use anyhow::Result;

use mj_controller::controller::test_support::{IsolatedTest, write_checkpoint_gate_archive};
use mj_core::state::SessionState;

use mj_controller::targets::{self, CommandExecutor, CommandOutput, CommandSpec};

// Hard-won: ce3218ad: destroy raced recovery and failed cleanup could restart the target
#[test]
fn destruction_waits_for_recovery_and_failed_cleanup_never_restarts_the_target() {
    if !in_isolated_store(
        "destruction_waits_for_recovery_and_failed_cleanup_never_restarts_the_target",
    ) {
        return;
    }

    use mj_controller::session_manager::{
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
    let _writer = mj_controller::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let record = controller.state.sessions.get_mut(session_id).unwrap();
    record.state = SessionState::Closing;
    record.checkpoint = Some(Box::new(checkpoint.clone()));
    mj_controller::database::save_session(record).unwrap();
    let plan = WorkerRecoveryPlan {
        source_target: *record.target.clone().unwrap(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: targets::CommandPlan {
            description: "restart worker".into(),
            commands: vec![CommandSpec::new("restart", std::iter::empty::<&str>())],
        }
        .into(),
    };
    struct PausedRecovery {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        restarted: Mutex<bool>,
    }
    impl CommandExecutor for PausedRecovery {
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
            if command.program == "probe" {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            } else {
                assert_eq!(command.program, "restart");
                assert_eq!(mj_controller::database::load_state().unwrap().sessions["0123456789abcdef0123456789abcdef"].state,
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
            persist_controller_fixture(&mut controller);
            mj_controller::controller::test_support::worker_test_scope(session_id, || {
                controller.destroy_after_verified_checkpoint_with(
                    session_id,
                    &checkpoint,
                    &DeferredCleanupExecutor {
                        statuses: std::sync::Mutex::new(vec![125]),
                    },
                    |record| {
                        assert!(*recovery.restarted.lock().unwrap());
                        mj_controller::database::save_lifecycle_session(record)?;
                        persisted_tx.send(record.state).unwrap();
                        Ok(())
                    },
                )
            })
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
        mj_controller::database::load_state().unwrap().sessions[session_id].state,
        SessionState::Destroying
    );
    assert_eq!(
        controller.state.sessions[session_id].checkpoint.as_deref(),
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
                    statuses: std::sync::Mutex::new(Vec::new())
                }
            )
            .unwrap(),
            WorkerRecoveryOutcome::Suppressed
        );
    }
    persist_controller_fixture(&mut controller);
    assert!(
        mj_controller::controller::test_support::worker_test_scope(session_id, || controller
            .destroy_after_verified_checkpoint_with(
                session_id,
                &checkpoint,
                &DeferredCleanupExecutor {
                    statuses: std::sync::Mutex::new(vec![0])
                },
                mj_controller::database::save_lifecycle_session
            ))
        .unwrap()
    );
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[session_id].state,
        SessionState::Stopped
    );
    assert_eq!(
        mj_checkpoint::checkpoint::checkpoint_sha256(&checkpoint.archive_path).unwrap(),
        checkpoint.sha256
    );
}

#[test]
fn failed_suspension_replaces_stale_internal_error_and_preserves_recovery_data() {
    const CHILD: &str = "MJ_TEST_SUSPENSION_ERROR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(mj_controller::controller::test_support::test_name(
            module_path!(),
            "failed_suspension_replaces_stale_internal_error_and_preserves_recovery_data",
        ))
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(root.path(), id, 7);
    let mut controller = stopped_podman_cleanup_controller(id);
    let record = controller.state.sessions.get_mut(id).unwrap();
    record.state = SessionState::Running;
    record.last_error = Some("old transport error with secret details".into());
    record.checkpoint = Some(Box::new(checkpoint.clone()));
    mj_controller::database::save_session(record).unwrap();
    let target = record.target.clone();
    let reason = format!(
        "{}; see reference suspension-test",
        mj_core::state::CLOSE_FAILURE_PREFIX
    );
    assert!(controller.record_failed_close(id, &reason, None).unwrap());
    let restored = mj_controller::database::load_state().unwrap();
    let record = &restored.sessions[id];
    assert_eq!(record.public_error(), Some(reason.as_str()));
    assert_eq!(record.state, SessionState::Running);
    assert_eq!(record.target, target);
    assert_eq!(record.checkpoint.as_deref(), Some(&checkpoint));
    assert!(checkpoint.archive_path.exists());
    let snapshot =
        crate::server::ViewerSnapshot::from_config_state(&controller.config, &restored, 1);
    assert_eq!(
        snapshot.sessions[0].launch_error.as_deref(),
        Some(reason.as_str())
    );
}

fn in_isolated_store(test: &str) -> bool {
    in_isolated_store_at(module_path!(), test)
}
