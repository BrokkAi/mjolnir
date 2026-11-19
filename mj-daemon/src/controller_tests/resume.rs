//! Tests of `mj_controller::controller::resume` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use std::sync::Mutex;

use anyhow::Result;

use mj_controller::controller::test_support::{
    IsolatedTest, checkpoint_test_session, resume_compatibility_config,
};
use mj_controller::controller::{Controller, SessionResumeOptions};
use mj_core::state::{SessionState, State, TargetLocator};

use mj_controller::targets::{CommandExecutor, CommandOutput, CommandSpec};

// Hard-won: 94d7 resume: failed Resume deleted the container its live sub-agents used.
#[test]
fn failed_resume_keeps_present_and_unknown_environments_without_an_archive() {
    const CHILD: &str = "MJ_RESUME_PRESENT_ENVIRONMENT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new("controller::resume::tests::failed_resume_keeps_present_and_unknown_environments_without_an_archive")
            .env(CHILD, "1").isolated_store(directory.path())
            .env("MJ_WORKER_BINARY", directory.path().join("unavailable-worker"))
            .run();
        return;
    }
    struct Engine {
        exists_status: i32,
        commands: Mutex<Vec<String>>,
    }
    impl CommandExecutor for Engine {
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
            self.commands.lock().unwrap().push(command.purpose.clone());
            let (status, stdout) = match command.purpose.as_str() {
                "check for Mjolnir session container" => (self.exists_status, Vec::new()),
                "inspect Mjolnir session container" => (0, serde_json::to_vec(&serde_json::json!([{
                    "Config": {"Labels": {"dev.mj.managed": "true", "dev.mj.session": "0123456789abcdef0123456789abcdef"}},
                    "State": {"Status": "running"}
                }])).unwrap()),
                "probe Mjolnir worker daemon liveness" => (0, b"dead\n".to_vec()),
                "probe retained environment directory" => (0, b"present\n".to_vec()),
                "probe Mjolnir worker root" => (0, b"recoverable\n".to_vec()),
                _ => anyhow::bail!("injected harness preparation failure: {}", command.purpose),
            };
            Ok(CommandOutput {
                status,
                stdout,
                stderr: Vec::new(),
            })
        }
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let mut parent = checkpoint_test_session(id);
    parent.state = SessionState::Error;
    parent.checkpoint = None;
    parent.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: mj_controller::targets::resource_name(id).unwrap(),
        borrowed_from: None,
        workspace_storage: Default::default(),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let original = mj_controller::database::session_environment(id)
        .unwrap()
        .unwrap();
    let mut controller = Controller {
        config: resume_compatibility_config(),
        state: State {
            sessions: [(id.into(), parent.clone())].into_iter().collect(),
            ..State::default()
        },
    };
    let engine = Engine {
        exists_status: 0,
        commands: Mutex::default(),
    };
    controller
        .rollback_failed_resume(
            id,
            &parent,
            Some(&original),
            false,
            anyhow::anyhow!("failure before creating a destination"),
            &engine,
        )
        .unwrap();
    assert!(engine.commands.lock().unwrap().is_empty());
    assert_eq!(
        mj_controller::database::session_environment(id)
            .unwrap()
            .unwrap()
            .placement
            .target,
        original.placement.target
    );
    // Rollback settles its attempt as stopped; probe each condition from Error.
    controller.state.sessions.insert(id.into(), parent.clone());
    mj_controller::database::save_session(&parent).unwrap();
    for status in [0, 125, 1] {
        let engine = Engine {
            exists_status: status,
            commands: Mutex::default(),
        };
        let error = runtime
            .block_on(
                mj_controller::controller::test_support::worker_test_scope_async(
                    id,
                    controller.resume_session_controlled(
                        id,
                        "codex",
                        "podman",
                        SessionResumeOptions {
                            additional_mounts: None,
                            resource_allocation: None,
                            discard_queue: false,
                        },
                        &engine,
                    ),
                ),
            )
            .unwrap_err();
        let error = format!("{error:#}");
        if status == 125 {
            assert!(error.contains("Retry Resume"), "{error}");
        }
        if status == 1 {
            assert!(error.contains("no checkpoint"), "{error}");
        }
        if status == 0 {
            assert!(!error.contains("no checkpoint"), "{error}");
        }
        if status == 1 {
            assert!(controller.state.sessions[id].target.is_none());
            assert_eq!(controller.state.sessions[id].state, SessionState::Lost);
            let view = crate::server::ViewerSnapshot::from_config_state(
                &controller.config,
                &controller.state,
                1,
            );
            assert!(!view.sessions[0].capabilities.resume);
            assert!(view.sessions[0].capabilities.destroy);
        } else {
            assert_eq!(controller.state.sessions[id].target, parent.target);
        }
        let environment = mj_controller::database::session_environment(id)
            .unwrap()
            .unwrap();
        assert_eq!(environment.id, original.id);
        assert_eq!(environment.phase, original.phase);
        assert_eq!(environment.placement.target, original.placement.target);
        let commands = engine.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .all(|purpose| !purpose.contains("remove") && !purpose.contains("stop")),
            "{commands:?}"
        );
    }
    // A new placement stops being disposable when its boot owner is admitted,
    // including a lost launch acknowledgement before Running was published.
    parent.state = SessionState::Provisioning;
    controller.state.sessions.insert(id.into(), parent.clone());
    mj_controller::database::save_session(&parent).unwrap();
    let owner = mj_controller::controller::test_support::worker_test_permit(
        id,
        "resume rollback boot regression",
    )
    .unwrap()
    .unwrap();
    owner.scope_blocking(|| {
        let intent = mj_controller::database::WorkerRestartIntent {
            operation_id: owner.operation_id().into(),
            target: *parent.target.clone().unwrap(),
            desired_build: String::new(),
        };
        mj_controller::database::begin_worker_restart(id, &intent).unwrap();
        let engine = Engine {
            exists_status: 0,
            commands: Mutex::default(),
        };
        controller
            .rollback_failed_resume(
                id,
                &parent,
                None,
                false,
                anyhow::anyhow!("lost startup acknowledgement"),
                &engine,
            )
            .unwrap();
        assert!(engine.commands.lock().unwrap().is_empty());
        assert_eq!(controller.state.sessions[id].state, SessionState::Error);
        assert_eq!(controller.state.sessions[id].target, parent.target);
        mj_controller::database::finish_worker_restart(id, &intent.operation_id).unwrap();
    });
}
