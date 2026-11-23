//! Fixtures shared by the lifecycle tests here and in mj-daemon, which reaches
//! them through the `test-support` feature.

use anyhow::Result;

use crate::controller::Controller;
use crate::controller::test_support::{IsolatedTest, checkpoint_test_session};
use mj_core::config::{Config, ContainerTemplate as ConfigContainer, TargetTemplate};
use mj_core::state::{SessionState, State, TargetLocator};

use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};

pub struct DeferredCleanupExecutor {
    pub statuses: std::sync::Mutex<Vec<i32>>,
}

impl CommandExecutor for DeferredCleanupExecutor {
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

    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        let status = self.statuses.lock().unwrap().pop().unwrap_or(0);
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

pub fn stopped_podman_cleanup_controller(session_id: &str) -> Controller {
    let container_id = targets::resource_name(session_id).unwrap();
    let volume = format!("{container_id}-workspace");
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "podman".into();
    session.state = SessionState::Stopped;
    session.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume { name: volume },
    }));
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                memory_hard_limit: None,
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
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    }
}

pub const ISOLATED_STORE_CHILD: &str = "MJ_LIFECYCLE_ISOLATED_STORE_CHILD";

/// Resource fixtures publish the same durable ownership as real provisioning.
pub fn persist_controller_fixture(controller: &mut Controller) {
    for session in controller.state.sessions.values() {
        crate::database::save_session(session).unwrap();
    }
    controller.state.environment_owners = crate::database::load_state().unwrap().environment_owners;
}

/// Run `test` alone in a child with its own store; true in that child.
/// `module_path` is the calling test module's `module_path`.
pub fn in_isolated_store_at(module_path: &str, test: &str) -> bool {
    if std::env::var_os(ISOLATED_STORE_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(crate::controller::test_support::test_name(
        module_path,
        test,
    ))
    .env(ISOLATED_STORE_CHILD, "1")
    .isolated_store(directory.path())
    .run();
    false
}
