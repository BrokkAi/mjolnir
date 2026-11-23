pub(crate) use super::test_fixtures::*;
use std::cell::RefCell;

use anyhow::Result;

use crate::controller::Controller;
#[cfg(unix)]
use crate::controller::test_support::IsolatedTest;
use crate::controller::test_support::{
    checkpoint_test_session, committed_repository, managed_worktree_session, test_git,
    write_checkpoint_gate_archive,
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: "/tmp/mjolnir-close-test".into(),
    }));
    let previous = session.clone();
    let error =
        anyhow::anyhow!("connect to Mjolnir worker").context(super::WorkerRestartLeftNoWorker);

    apply_close_checkpoint_failure(
        &mut session,
        &previous,
        &error,
        "2026-08-14T12:00:00Z".into(),
        None,
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
        None,
    );

    assert_eq!(session.state, SessionState::Running);
    assert!(session.last_error.is_none());
    assert_eq!(
        session.last_checkpoint_error.as_deref(),
        Some("the session was busy")
    );
}

#[test]
fn deferred_cleanup_failure_is_visible_and_successful_retry_clears_it() {
    if !in_isolated_store("deferred_cleanup_failure_is_visible_and_successful_retry_clears_it") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let persisted = RefCell::new(Vec::new());
    persist_controller_fixture(&mut controller);
    let failure = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                // The cleanup command fails, then the exact-absence probe
                // confirms that the owned target is still present.
                statuses: std::sync::Mutex::new(vec![1, 1]),
            },
            |record| {
                persisted
                    .borrow_mut()
                    .push((record.target.is_some(), record.last_error.clone()));
                Ok(())
            },
        )
    })
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
    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                statuses: std::sync::Mutex::new(Vec::new()),
            },
            |record| {
                retry_persisted
                    .borrow_mut()
                    .push((record.target.is_some(), record.last_error.clone()));
                Ok(())
            },
        )
    })
    .unwrap();
    assert_eq!(retry_persisted.borrow().as_slice(), &[(false, None)]);
    assert!(controller.state.sessions[session_id].target.is_none());
    assert!(controller.state.sessions[session_id].last_error.is_none());
}

#[test]
fn deferred_cleanup_persistence_failure_restores_the_stopped_record() {
    if !in_isolated_store("deferred_cleanup_persistence_failure_restores_the_stopped_record") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_podman_cleanup_controller(session_id);
    let previous = controller.state.sessions[session_id].clone();
    persist_controller_fixture(&mut controller);
    let failure = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.cleanup_stopped_target_with(
            session_id,
            &DeferredCleanupExecutor {
                statuses: std::sync::Mutex::new(vec![1, 1]),
            },
            |_| Err(anyhow::anyhow!("database unavailable")),
        )
    })
    .unwrap_err();
    let detail = format!("{failure:#}");
    assert!(detail.contains("cleanup failed"), "{detail}");
    assert!(detail.contains("database unavailable"), "{detail}");
    assert_eq!(controller.state.sessions[session_id], previous);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn target_cleanup_persists_destroying_and_rechecks_the_installed_archive() {
    if !in_isolated_store("target_cleanup_persists_destroying_and_rechecks_the_installed_archive") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    struct RecordingExecutor {
        commands: std::sync::Mutex<Vec<CommandSpec>>,
    }

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

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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
        commands: std::sync::Mutex::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &executor,
            |record| {
                persisted.borrow_mut().push(record.state);
                Ok(())
            },
        )
    })
    .unwrap();

    assert_eq!(
        persisted.into_inner(),
        vec![SessionState::Destroying, SessionState::Stopped]
    );
    let commands = executor.commands.lock().unwrap().clone();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].purpose, "kill retired Mjolnir worker daemon");
    assert!(commands[1].args.iter().any(|arg| arg.contains("rm -rf")));
    let stopped = &controller.state.sessions[session_id];
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.target.is_none());
}

#[test]
fn podman_close_persists_stopped_before_deferred_storage_cleanup() {
    if !in_isolated_store("podman_close_persists_stopped_before_deferred_storage_cleanup") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    struct RecordingExecutor {
        commands: std::sync::Mutex<Vec<CommandSpec>>,
    }

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

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
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
    session.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: volume.clone(),
        },
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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
        commands: std::sync::Mutex::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    persist_controller_fixture(&mut controller);
    let deferred = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &executor,
            |record| {
                persisted
                    .borrow_mut()
                    .push((record.state, record.target.is_some()));
                Ok(())
            },
        )
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
    let commands = executor.commands.lock().unwrap().clone();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].args[1].contains("podman stop --time 0"));
    assert!(!commands[0].args[1].contains("podman rm"));
    drop(commands);

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.cleanup_stopped_target_with(session_id, &executor, |record| {
            assert_eq!(record.state, SessionState::Stopped);
            assert!(record.target.is_none());
            Ok(())
        })
    })
    .unwrap();

    let commands = executor.commands.lock().unwrap().clone();
    assert_eq!(
        commands[1].stage,
        Some(targets::ProvisionStage::StoppingTarget)
    );
    assert_eq!(
        commands[2].stage,
        Some(targets::ProvisionStage::RemovingContainer)
    );
    assert_eq!(
        commands[3].stage,
        Some(targets::ProvisionStage::RemovingStorage)
    );
    assert_eq!(
        commands.last().unwrap().stage,
        Some(targets::ProvisionStage::CleaningCache)
    );
    assert!(commands[3].args.contains(&volume));
    assert!(controller.state.sessions[session_id].target.is_none());
}
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn verified_close_retires_managed_checkout_but_keeps_archive_and_branch() {
    if !in_isolated_store("verified_close_retires_managed_checkout_but_keeps_archive_and_branch") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    let archive_directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(archive_directory.path(), session_id, 7);
    let mut session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();
    std::fs::write(worktree.worktree_root.join("dirty.txt"), "worktree state\n").unwrap();
    session.state = SessionState::Closing;
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: archive_directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &ProcessExecutor,
            |_| Ok(()),
        )
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
    // Hard-won: reclaimed ownership skipped a later explicit branch deletion.
    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |_| Ok(()),
        )
    })
    .unwrap();
    assert!(!branch_exists(repository.path(), &worktree.branch));
}
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn force_stop_reuses_verified_archive_and_leaves_session_resumable() {
    if !in_isolated_store("force_stop_reuses_verified_archive_and_leaves_session_resumable") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    let archive_directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(archive_directory.path(), session_id, 7);
    let mut session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();
    session.state = SessionState::Running;
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: archive_directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, true, &ProcessExecutor, |_| Ok(()))
    })
    .unwrap();

    let stopped = &controller.state.sessions[session_id];
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.target.is_none());
    assert_eq!(stopped.checkpoint.as_deref(), Some(&checkpoint));
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
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn closing_a_wedged_provisioning_session_tears_down_its_target_and_settles() {
    if !in_isolated_store("closing_a_wedged_provisioning_session_tears_down_its_target_and_settles")
    {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.checkpoint = None;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
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

    persist_controller_fixture(&mut controller);
    let outcome = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, true, &ProcessExecutor, |_| Ok(()))
    })
    .unwrap();

    assert!(
        !outcome.deferred,
        "a bare target needs no storage cleanup pass"
    );
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
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn closing_without_a_checkpoint_refuses_a_session_that_has_one_to_take() {
    if !in_isolated_store("closing_without_a_checkpoint_refuses_a_session_that_has_one_to_take") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
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

    persist_controller_fixture(&mut controller);
    let outcome = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, true, &FailingExecutor, |_| Ok(()))
    })
    .unwrap();

    assert!(!outcome.deferred);
    assert!(
        !outcome.warnings.is_empty(),
        "best-effort cleanup failure must surface a warning"
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
    assert!(controller.state.sessions[session_id].checkpoint.is_none());
    assert!(
        controller.state.sessions[session_id].target.is_some(),
        "target locator must be preserved when best-effort cleanup fails"
    );
}

/// A sub-agent whose first prompt was refused for good was recorded as
/// failed on a bare target, with its worker stopped.
fn failed_subagent_relation(session_id: &str) -> mj_core::subagent::SubagentRecord {
    mj_core::subagent::SubagentRecord {
        resume_with_parent: false,
        kind: mj_core::subagent::SubagentKind::Worker,
        reviewer_launch: None,
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
        reported_finish: None,
        handback_tool: true,
        supervisor: false,
    }
}

fn failed_subagent_controller(worker_root: &std::path::Path, session_id: &str) -> Controller {
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Error;
    session.last_error = Some("this agent does not offer high as a effort".into());
    session.checkpoint = None;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.to_owned(),
    }));
    let relation = failed_subagent_relation(session_id);
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    Controller {
        config,
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session)].into_iter().collect(),
            subagents: [(session_id.into(), relation.into())].into_iter().collect(),
            ..State::default()
        },
    }
}

fn seed_child_reports(
    controller: &mut Controller,
    child_id: &str,
    directory: &std::path::Path,
) -> PathBuf {
    let parent_id = "abcdefabcdefabcdefabcdefabcdefab";
    let mut parent = checkpoint_test_session(parent_id);
    parent.state = SessionState::Running;
    parent.target_template_id = "local".into();
    parent.project_directory = Some(directory.join("project"));
    parent.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.join(parent_id),
    }));
    crate::database::save_session(&parent).unwrap();
    controller.state.sessions.insert(parent_id.into(), parent);
    let mut relation = failed_subagent_relation(child_id);
    relation.parent_session_id = parent_id.into();
    controller
        .state
        .subagents
        .insert(child_id.into(), (&relation).into());
    crate::database::save_subagent_session(&controller.state.sessions[child_id], &relation)
        .unwrap();
    let root = directory.join("project/.mj/agents");
    std::fs::create_dir_all(root.join(child_id)).unwrap();
    std::fs::create_dir_all(root.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")).unwrap();
    std::fs::write(root.join(child_id).join("report.log"), "durable report").unwrap();
    crate::database::record_subagent_report_dir(child_id, root.join(child_id).to_str().unwrap())
        .unwrap();
    root
}

/// I1-2: closing a failed sub-agent that still names its target takes no
/// checkpoint (its worker is stopped and a child keeps no archive); it tears
/// the target down and settles.
// Hard-won: ba6c3427: failed startup child stayed live and could not close without a checkpoint
#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn closing_a_failed_subagent_tears_down_its_target_without_a_checkpoint() {
    if !in_isolated_store("closing_a_failed_subagent_tears_down_its_target_without_a_checkpoint") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller = failed_subagent_controller(&worker_root, session_id);

    let reports = seed_child_reports(&mut controller, session_id, directory.path());
    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, true, &ProcessExecutor, |_| Ok(()))
    })
    .unwrap();

    let closed = &controller.state.sessions[session_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
    assert!(
        worker_root.exists(),
        "the environment retains private files until all shared writers stop"
    );
    // #1333: close keeps reports parents were told to cite.
    assert!(
        reports.join(session_id).is_dir(),
        "close keeps the child's report directory"
    );
    assert!(reports.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_dir());
}

/// Records the purpose of every command. The worker-root probe either runs
/// for real or fails as if SSH or Podman were unreachable; every other command
/// fails, so a close that tries to restart a worker is visible. A close first
/// tries the relay, which is not an executor command: a root without a worker
/// binary, or whose binary exits at once, is a dead relay transport.
struct RootProbeExecutor {
    probe_fails: bool,
    /// What the worker liveness probe answers.
    worker: &'static str,
    purposes: std::sync::Mutex<Vec<String>>,
}

impl CommandExecutor for RootProbeExecutor {
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
        self.purposes.lock().unwrap().push(command.purpose.clone());
        match command.purpose.as_str() {
            "probe Mjolnir worker root" if self.probe_fails => {
                anyhow::bail!("ssh: connect to host morannon: connection timed out")
            }
            "probe Mjolnir worker daemon liveness" => Ok(CommandOutput {
                status: 0,
                stdout: format!("{}\n", self.worker).into_bytes(),
                stderr: Vec::new(),
            }),
            "probe Mjolnir worker root" | "stop Mjolnir worker daemon" => {
                ProcessExecutor.execute(command)
            }
            other => anyhow::bail!("unexpected command: {other}"),
        }
    }
}

/// A reviewer child whose Create was cancelled before the install step has a
/// target and no worker root. Its close must finish in one pass instead of
/// reconnecting and restarting a worker that never existed; when the probe
/// cannot tell, the old recovery path (and its retry) stays.
// Hard-won: a8c169cd3: closing a child with no worker root retried a worker restart forever
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
async fn closing_a_child_with_no_worker_root_skips_worker_recovery() {
    if !in_isolated_store("closing_a_child_with_no_worker_root_skips_worker_recovery") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    let mut controller = failed_subagent_controller(&worker_root, session_id);
    controller.state.sessions.get_mut(session_id).unwrap().state = SessionState::Closing;
    seed_child_reports(&mut controller, session_id, directory.path());
    assert!(!worker_root.exists());

    let unreachable = RootProbeExecutor {
        probe_fails: true,
        worker: "dead",
        purposes: Default::default(),
    };
    persist_controller_fixture(&mut controller);
    controller
        .suspend_subagent_controlled(
            &crate::controller::test_support::session_admission(session_id),
            session_id,
            &unreachable,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        unreachable.purposes.lock().unwrap()[0],
        "probe Mjolnir worker daemon liveness",
        "a missing worker binary is a dead relay, so the close judges the worker next"
    );
    assert!(
        !unreachable
            .purposes
            .lock()
            .unwrap()
            .iter()
            .any(|purpose| purpose.starts_with("stop a borrowed-target")),
        "an unanswered probe must not be read as an absent root"
    );
    assert_ne!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
    assert!(controller.state.sessions[session_id].target.is_some());

    let answered = RootProbeExecutor {
        probe_fails: false,
        worker: "dead",
        purposes: Default::default(),
    };
    persist_controller_fixture(&mut controller);
    controller
        .suspend_subagent_controlled(
            &crate::controller::test_support::session_admission(session_id),
            session_id,
            &answered,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        *answered.purposes.lock().unwrap(),
        [
            "probe Mjolnir worker daemon liveness",
            "probe Mjolnir worker root",
            "probe Mjolnir worker daemon liveness",
            "stop Mjolnir worker daemon"
        ]
    );
    let closed = &controller.state.sessions[session_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
}

/// Close a Closing child whose root holds the binary and an exit record, plus
/// relay state when `has_state`. The child carries the restart intent a failed
/// checkpoint-only restart left behind. Returns the commands run, the result,
/// the final state, whether the relay state survived and whether the intent did.
async fn close_child_with_partial_root(
    name: &str,
    has_state: bool,
    worker: &'static str,
) -> Option<(Vec<String>, bool, SessionState, bool, bool)> {
    if !in_isolated_store(name) {
        return None;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    // A worker binary that exits at once: its relay proxy disconnects during
    // hello, which is how a dead worker looks from the controller. Stopping the
    // dead worker is a no-op that succeeds, as the real `hel worker stop` is.
    let hel = worker_root.join("hel");
    std::fs::write(&hel, b"#!/bin/sh\n[ \"$2\" = stop ] && exit 0\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hel, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Control commands run a control worker staged in the root; this stand-in
    // answers them.
    #[cfg(unix)]
    {
        let control =
            std::path::PathBuf::from(targets::control_worker_path(&worker_root.to_string_lossy()));
        mj_core::test_hooks::install_fake_command(
            control.parent().unwrap(),
            control.file_name().unwrap().to_str().unwrap(),
            "",
        );
    }
    std::fs::write(worker_root.join("worker-exit.json"), b"{}").unwrap();
    if has_state {
        std::fs::write(worker_root.join(mj_core::relay::RELAY_STATE_FILE), b"{}").unwrap();
    }
    let mut controller = failed_subagent_controller(&worker_root, session_id);
    controller.state.sessions.get_mut(session_id).unwrap().state = SessionState::Closing;
    seed_child_reports(&mut controller, session_id, directory.path());
    let intent = crate::database::WorkerRestartIntent {
        operation_id: "failed-checkpoint-only-restart".into(),
        target: *controller.state.sessions[session_id]
            .target
            .clone()
            .unwrap(),
        desired_build: String::new(),
    };
    crate::controller::test_support::worker_test_run_blocking(
        session_id,
        "seed intent",
        &ProcessExecutor,
        || {
            crate::database::begin_worker_restart(session_id, &intent)?;
            crate::database::advance_worker_restart(
                session_id,
                &intent.operation_id,
                crate::database::WorkerRestartPhase::Swapping,
            )
        },
    )
    .unwrap();
    let executor = RootProbeExecutor {
        probe_fails: false,
        worker,
        purposes: Default::default(),
    };
    persist_controller_fixture(&mut controller);
    let result = controller
        .suspend_subagent_controlled(
            &crate::controller::test_support::session_admission(session_id),
            session_id,
            &executor,
            None,
            None,
        )
        .await;
    let purposes = executor.purposes.lock().unwrap().clone();
    Some((
        purposes,
        result.is_ok(),
        controller.state.sessions[session_id].state,
        worker_root.join(mj_core::relay::RELAY_STATE_FILE).exists(),
        crate::database::load_worker_restart(session_id)
            .unwrap()
            .is_some(),
    ))
}

/// A Create cancelled after install but before the worker reached its durable
/// relay leaves a root with the binary and an exit record but no relay state.
/// A checkpoint-only worker refuses it, so its close settles like an absent root.
// Hard-won: #1438: a partial worker root without relay state kept its close retrying a recovery the worker refuses
// Hard-won: #1438: a child's close left the restart intent of its failed checkpoint-only restart behind
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
async fn closing_a_child_whose_root_has_no_relay_state_skips_worker_recovery() {
    let Some((purposes, ok, state, _, intent_left)) = close_child_with_partial_root(
        "closing_a_child_whose_root_has_no_relay_state_skips_worker_recovery",
        false,
        "dead",
    )
    .await
    else {
        return;
    };
    assert!(ok);
    assert_eq!(
        purposes,
        [
            "probe Mjolnir worker daemon liveness",
            "probe Mjolnir worker root",
            "probe Mjolnir worker daemon liveness",
            "stop Mjolnir worker daemon"
        ]
    );
    assert_eq!(state, SessionState::Stopped);
    assert!(!intent_left, "teardown proved the worker gone");
}

/// A root with relay state keeps the recovery path and is not cleaned up.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
async fn closing_a_child_whose_root_has_relay_state_keeps_worker_recovery() {
    let Some((purposes, _, _, state_kept, _)) = close_child_with_partial_root(
        "closing_a_child_whose_root_has_relay_state_keeps_worker_recovery",
        true,
        "dead",
    )
    .await
    else {
        return;
    };
    assert_eq!(
        purposes[..2],
        [
            "probe Mjolnir worker daemon liveness",
            "probe Mjolnir worker root"
        ]
    );
    assert!(
        !purposes
            .iter()
            .any(|purpose| purpose.starts_with("stop a borrowed-target")),
        "{purposes:?}"
    );
    assert!(state_kept);
}

/// A worker that is still starting has written no relay state yet. Its empty
/// root is not "nothing to recover": the close keeps the recovery path so the
/// conversation the worker is about to start is synchronized, not deleted.
// Hard-won: #1438: an empty root of a worker that was still starting read as nothing to recover
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
async fn closing_a_child_whose_worker_is_still_starting_keeps_worker_recovery() {
    let Some((purposes, _, _, _, _)) = close_child_with_partial_root(
        "closing_a_child_whose_worker_is_still_starting_keeps_worker_recovery",
        false,
        "starting",
    )
    .await
    else {
        return;
    };
    // The worker is not confirmed dead, so the close stops at the liveness
    // probe: it neither reads the root nor cleans it up.
    assert_eq!(purposes, ["probe Mjolnir worker daemon liveness"]);
    assert!(
        !purposes
            .iter()
            .any(|purpose| purpose.starts_with("stop a borrowed-target")),
        "{purposes:?}"
    );
}

/// A child closed by an older daemon can retain a checkpoint; reports
/// survive there too, not only on the no-checkpoint path.
// Hard-won: #1333: close deleted the child's report_dir that parents were told to cite.
#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn checkpointed_close_keeps_a_child_report_directory() {
    if !in_isolated_store("checkpointed_close_keeps_a_child_report_directory") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller = failed_subagent_controller(&worker_root, session_id);
    let reports = seed_child_reports(&mut controller, session_id, directory.path());
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let session = controller.state.sessions.get_mut(session_id).unwrap();
    session.state = SessionState::Closing;
    session.checkpoint = Some(Box::new(checkpoint.clone()));

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &ProcessExecutor,
            |_| Ok(()),
        )
    })
    .unwrap();

    let closed = &controller.state.sessions[session_id];
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
    assert!(
        reports.join(session_id).is_dir(),
        "checkpointed close keeps the child's report directory"
    );
    assert!(reports.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_dir());
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
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
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
                persist
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
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn force_stop_without_a_recovery_archive_does_not_touch_the_target() {
    if !in_isolated_store("force_stop_without_a_recovery_archive_does_not_touch_the_target") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    struct RecordingExecutor {
        calls: std::sync::Mutex<usize>,
    }

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

        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.lock().unwrap() += 1;
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
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
        calls: std::sync::Mutex::new(0),
    };

    persist_controller_fixture(&mut controller);
    let outcome = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, true, &executor, |_| Ok(()))
    })
    .unwrap();

    assert!(!outcome.deferred);
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
    assert!(controller.state.sessions[session_id].checkpoint.is_none());
    assert!(controller.state.sessions[session_id].target.is_none());
}
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn destroying_retry_blocks_cleanup_when_the_archive_gate_changed() {
    if !in_isolated_store("destroying_retry_blocks_cleanup_when_the_archive_gate_changed") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    struct RecordingExecutor {
        calls: std::sync::Mutex<usize>,
    }

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

        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.lock().unwrap() += 1;
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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
        calls: std::sync::Mutex::new(0),
    };
    let persisted = RefCell::new(Vec::new());
    std::fs::write(&checkpoint.archive_path, b"changed after checkpoint").unwrap();

    persist_controller_fixture(&mut controller);
    let error = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &executor,
            |record| {
                persisted.borrow_mut().push(record.state);
                Ok(())
            },
        )
    })
    .unwrap_err();

    assert!(error.to_string().contains("checkpoint SHA changed"));
    assert_eq!(*executor.calls.lock().unwrap(), 0);
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
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::load_state().unwrap();

    struct AlreadyRemovedExecutor {
        commands: std::sync::Mutex<Vec<CommandSpec>>,
    }

    impl CommandExecutor for AlreadyRemovedExecutor {
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
            self.commands.lock().unwrap().push(command.clone());
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
    session.target = Some(Box::new(TargetLocator::AppleContainer {
        borrowed_from: None,
        container_id: targets::resource_name(session_id).unwrap(),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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
        commands: std::sync::Mutex::new(Vec::new()),
    };
    let persisted = RefCell::new(Vec::new());

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_after_verified_checkpoint_with(
            session_id,
            &checkpoint,
            &executor,
            |record| {
                persisted.borrow_mut().push(record.state);
                Ok(())
            },
        )
    })
    .unwrap();

    let commands = executor.commands.lock().unwrap().clone();
    assert_eq!(commands.len(), 3);
    assert_eq!(
        commands[0].purpose,
        "confirm owned container writers stopped through the engine"
    );
    assert_eq!(commands[1].program, "sh");
    assert!(commands[1].args[1].contains("container rm --force"));
    assert!(commands[1].args[1].contains(".cache/mjolnir/git/sessions"));
    assert_eq!(commands[2].args, ["list", "--all", "--quiet"]);
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

/// Run the named test alone, with a data directory of its own: destroying a
/// session removes its attachment store under the data directory. Returns
/// whether this process is that run.
fn in_isolated_store(test: &str) -> bool {
    in_isolated_store_at(module_path!(), test)
}

#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn force_destroy_from_running_removes_target_worktree_branch_and_archive() {
    if !in_isolated_store("force_destroy_from_running_removes_target_worktree_branch_and_archive") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
    })
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
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn force_destroy_keeps_the_branch_and_removes_the_checkout_by_default() {
    if !in_isolated_store("force_destroy_keeps_the_branch_and_removes_the_checkout_by_default") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();

    let worktree_root = repository.path().join(".mj/worktrees").join(session_id);
    assert!(!worktree_root.exists(), "managed worktree must be removed");
    assert!(
        branch_exists(repository.path(), &format!("mj/{session_id}")),
        "the session's branch must survive its destruction"
    );
    assert!(!controller.state.sessions.contains_key(session_id));
}

// Hard-won: 27c51a34c: an unreachable host made explicit Destroy impossible.
// The managed clone fixture and its process executor require a POSIX host.
#[cfg(unix)]
#[test]
fn destroy_warns_when_cleanup_fails_but_automatic_stopping_keeps_the_record() {
    if !in_isolated_store(
        "destroy_warns_when_cleanup_fails_but_automatic_stopping_keeps_the_record",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let repository = committed_repository();
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller =
        running_managed_clone_controller(repository.path(), &worker_root, session_id);
    controller
        .state
        .sessions
        .get_mut(session_id)
        .unwrap()
        .checkpoint = Some(Box::new(checkpoint.clone()));
    let checkout = repository.path().join(".mj/clones").join(session_id);
    crate::database::save_session(&controller.state.sessions[session_id]).unwrap();
    let deleted = RefCell::new(Vec::new());

    persist_controller_fixture(&mut controller);
    let error = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.destroy_session_with_cleanup(
            session_id,
            &FailingExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
            DestroyCleanup::Required,
        )
    })
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
    assert!(deleted.borrow().is_empty());
    // The record is being destroyed: it must stop being polled, so the daemon
    // does not keep reconnecting to a worker the destroy stopped (I2-9). It
    // keeps its target, so the next destroy finishes the removal.
    let record = &controller.state.sessions[session_id];
    assert_eq!(record.state, SessionState::Error);
    assert!(record.target.is_some());
    assert!(!crate::session_manager::session_target_is_pollable(record));

    persist_controller_fixture(&mut controller);
    let outcome = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &FailingExecutor,
            BranchDisposition::Delete,
            crate::database::delete_session,
        )
    })
    .unwrap();
    assert!(
        worker_root.exists(),
        "an unconfirmed worker keeps its files"
    );
    assert!(
        checkout.exists(),
        "an unconfirmed writer keeps its checkout"
    );
    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("Resources may remain"));
    assert!(!checkpoint.archive_path.exists());
    assert!(!controller.state.sessions.contains_key(session_id));
    assert!(
        !crate::database::load_state()
            .unwrap()
            .sessions
            .contains_key(session_id)
    );
}

#[test]
fn force_destroy_tolerates_a_missing_archive() {
    if !in_isolated_store("force_destroy_tolerates_a_missing_archive") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    std::fs::remove_file(&checkpoint.archive_path).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Error;
    session.checkpoint = Some(Box::new(checkpoint));
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |_| Ok(()),
        )
    })
    .unwrap();

    assert!(!controller.state.sessions.contains_key(session_id));
}

/// A live session whose harness never advertised itself ends in `Error` with a
/// reason a driver can act on, and a record that has moved since the daemon
/// observed it is left alone (#1090).
// Hard-won: 7bcfe409: unusable harness left a live session without an actionable failure reason
#[test]
fn a_session_whose_harness_never_became_usable_is_failed_with_its_reason() {
    if !in_isolated_store("a_session_whose_harness_never_became_usable_is_failed_with_its_reason") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
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
    persist_controller_fixture(&mut controller);
    let cause = "the harness never advertised its configuration within 300s";

    assert!(
        !controller
            .fail_unready_session(session_id, cause, "2026-09-18T15:20:00Z")
            .unwrap(),
        "a record that has moved since the observation belongs to whatever moved it"
    );
    assert_eq!(
        crate::database::read_durable_session_record(session_id)
            .unwrap()
            .unwrap()
            .state,
        SessionState::Running
    );

    assert!(
        controller
            .fail_unready_session(session_id, cause, "2026-09-18T15:28:35Z")
            .unwrap()
    );
    let failed = &controller.state.sessions[session_id];
    assert_eq!(failed.state, SessionState::Error);
    assert_eq!(failed.last_error.as_deref(), Some(cause));
    let durable = crate::database::read_durable_session_record(session_id)
        .unwrap()
        .unwrap();
    assert_eq!(durable.state, failed.state);
    assert_eq!(durable.last_error, failed.last_error);
    assert_ne!(
        failed.updated_at, "2026-09-18T15:28:35Z",
        "the failure is a state change of its own"
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
    releases: std::sync::Mutex<Vec<(CommandSpec, bool)>>,
}

#[cfg(unix)]
impl CommandExecutor for RealTargetsRecordedRelease {
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
        if !is_release(command) {
            return ProcessExecutor.execute(command);
        }
        self.releases
            .lock()
            .unwrap()
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
    commands: std::sync::Mutex<Vec<CommandSpec>>,
}

impl RecordingTargets {
    fn releases(&self) -> Vec<CommandSpec> {
        self.commands
            .lock()
            .unwrap()
            .clone()
            .iter()
            .filter(|command| is_release(command))
            .cloned()
            .collect()
    }

    /// The position of the first command whose arguments mention `needle`.
    fn position(&self, needle: &str) -> Option<usize> {
        self.commands
            .lock()
            .unwrap()
            .clone()
            .iter()
            .position(|command| {
                command
                    .args
                    .iter()
                    .any(|argument| argument.contains(needle))
            })
    }
}

impl CommandExecutor for RecordingTargets {
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
        self.commands.lock().unwrap().push(command.clone());
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.to_owned(),
    }));
    session.checkpoint = None;
    let mut config = Config::default();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
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

// Hard-won: 741163fe: removed cross-filesystem clone state remained in the shared mbx store
#[test]
#[cfg(unix)]
fn destroying_a_managed_clone_releases_its_mbx_build_state_once_the_checkout_is_gone() {
    if !in_isolated_store(
        "destroying_a_managed_clone_releases_its_mbx_build_state_once_the_checkout_is_gone",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
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
        releases: std::sync::Mutex::new(Vec::new()),
    };

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &executor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();

    let releases = executor.releases.into_inner().unwrap();
    assert_eq!(releases.len(), 1, "{releases:?}");
    let (release, checkout_present) = &releases[0];
    assert!(
        !checkout_present,
        "mbx is told only after the checkout is gone"
    );
    let arguments = release_arguments(release);
    assert_eq!(arguments[0], "native", "{release:?}");
    let checkout = checkout.to_string_lossy().into_owned();
    // `$6` is the root newer mbx cleans with `clean --under`; the paths after
    // it are the fallback for older mbx. A clone's root is its checkout.
    assert_eq!(arguments[5], checkout, "the clone is the released root");
    assert_eq!(
        &arguments[6..],
        [checkout],
        "the clone is the only fallback workspace"
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
    let _writer = crate::database::install_isolated_test_writer();
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
        releases: std::sync::Mutex::new(Vec::new()),
    };

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &executor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();

    assert_eq!(executor.releases.lock().unwrap().clone().len(), 1);
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
    let _writer = crate::database::install_isolated_test_writer();
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
    session.target = Some(Box::new(TargetLocator::SshBare {
        host: "build.test".into(),
        workspace: workspace.clone().into(),
        worker_id: None,
    }));
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &executor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();

    let releases = executor.releases();
    assert_eq!(
        releases.len(),
        1,
        "{:?}",
        executor.commands.lock().unwrap().clone()
    );
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
#[cfg_attr(
    windows,
    ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
)]
fn removing_a_cached_container_releases_its_workspaces_from_the_shared_cache() {
    if !in_isolated_store(
        "removing_a_cached_container_releases_its_workspaces_from_the_shared_cache",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let _releases = crate::controller::mbx::release::enable_for_test();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut controller = stopped_cached_podman_controller(session_id, true);
    let executor = RecordingTargets::default();

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.cleanup_stopped_target_with(session_id, &executor, |_| Ok(()))
    })
    .unwrap();

    let releases = executor.releases();
    assert_eq!(
        releases.len(),
        1,
        "{:?}",
        executor.commands.lock().unwrap().clone()
    );
    let arguments = release_arguments(&releases[0]);
    assert_eq!(
        &arguments[..4],
        [
            "shared",
            "/srv/mbx-cache",
            "/srv/mbx-cache/.mjolnir/config",
            "/srv/mbx-cache/.mjolnir/bin/mbx"
        ]
    );
    assert_eq!(
        arguments[4],
        mj_core::config::data_dir()
            .join("mbx")
            .to_string_lossy()
            .as_ref()
    );
    // `$6` is the session's own root for `mbx clean --under`; the
    // repository paths after it are the fallback for older mbx.
    assert_eq!(arguments[5], format!("/workspace/{session_id}"));
    assert_eq!(
        &arguments[6..],
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
    let _writer = crate::database::install_isolated_test_writer();
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
    child.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: Some(parent_id.into()),
        container_id: targets::resource_name(parent_id).unwrap(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::ContainerLayer,
    }));
    child.container_workspace = Some(mj_core::targets::new_container_workspace(parent_id).unwrap());
    child.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "local".into(),
        directory: "/srv/mbx-cache".into(),
        max_size: None,
        target_root: None,
    });
    let mut parent = checkpoint_test_session(parent_id);
    parent.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(parent_id).unwrap(),
        workspace_storage: Default::default(),
    }));
    crate::database::save_session(&parent).unwrap();
    controller.state.sessions.insert(parent_id.into(), parent);
    let mut relation = failed_subagent_relation(session_id);
    relation.parent_session_id = parent_id.into();
    crate::database::save_subagent_session(&controller.state.sessions[session_id], &relation)
        .unwrap();
    controller
        .state
        .subagents
        .insert(session_id.into(), relation.into());
    let executor = RecordingTargets::default();

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &executor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();

    assert!(
        executor
            .commands
            .lock()
            .unwrap()
            .clone()
            .iter()
            .any(|command| command.purpose == "stop Mjolnir worker daemon")
    );
    assert!(executor.position("rm -rf").is_none());
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

// Hard-won: DSH campaign 2026-10-23 (B-5): destroy after a Move left the Move's checkpoint archive behind
#[test]
fn destroy_removes_every_archive_of_the_session_and_only_those() {
    if !in_isolated_store("destroy_removes_every_archive_of_the_session_and_only_those") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let session_id = "0123456789abcdef0123456789abcdef";
    let other_id = "fedcba9876543210fedcba9876543210";
    let sessions = mj_core::config::sessions_dir();
    // The record points at the newest archive; the Move's handoff archive is
    // an older file no record refers to.
    std::fs::create_dir_all(&sessions).unwrap();
    let managed = |id: &str, frontier: u64, nonce: char| {
        let mut archive = write_checkpoint_gate_archive(&sessions, id, frontier);
        let path = sessions.join(format!(
            "{id}-{frontier}-archive-{}.hel.zip",
            nonce.to_string().repeat(32)
        ));
        std::fs::rename(&archive.archive_path, &path).unwrap();
        archive.archive_path = path;
        archive
    };
    let move_handoff = managed(session_id, 317, 'a');
    let current = managed(session_id, 400, 'b');
    let other = managed(other_id, 317, 'c');
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Provisioning;
    session.target = None;
    session.checkpoint = Some(Box::new(current.clone()));
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |_| Ok(()),
        )
    })
    .unwrap();

    assert!(!move_handoff.archive_path.exists(), "Move archive remains");
    assert!(!current.archive_path.exists(), "current archive remains");
    assert!(
        other.archive_path.exists(),
        "another session's archive went"
    );
}

#[test]
fn force_destroy_without_a_target_or_archive_still_removes_the_record() {
    if !in_isolated_store("force_destroy_without_a_target_or_archive_still_removes_the_record") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
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

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Delete,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
    })
    .unwrap();

    assert!(!controller.state.sessions.contains_key(session_id));
    assert_eq!(deleted.into_inner(), vec![session_id.to_owned()]);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn force_destroy_removes_a_failed_subagent() {
    if !in_isolated_store("force_destroy_removes_a_failed_subagent") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut controller = failed_subagent_controller(&worker_root, session_id);
    let reports = seed_child_reports(&mut controller, session_id, directory.path());
    let deleted = RefCell::new(Vec::new());

    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(session_id, || {
        controller.force_destroy_session_with(
            session_id,
            &ProcessExecutor,
            BranchDisposition::Keep,
            |id: &str| {
                deleted.borrow_mut().push(id.to_owned());
                Ok(())
            },
        )
    })
    .unwrap();

    assert!(
        worker_root.exists(),
        "the environment retains private files until all shared writers stop"
    );
    assert!(!controller.state.sessions.contains_key(session_id));
    assert!(!controller.state.subagents.contains_key(session_id));
    assert_eq!(deleted.into_inner(), vec![session_id.to_owned()]);
    assert!(
        reports.join(session_id).exists(),
        "the parent still owns these reports"
    );
    assert!(reports.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_dir());
}

// Hard-won: #1331: stopped children have no target and left reports behind on destroy.
#[cfg(unix)]
#[test]
fn destroying_a_stopped_child_retains_reports_until_its_parent_retires() {
    if !in_isolated_store("destroying_a_stopped_child_retains_reports_until_its_parent_retires") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let child_id = "0123456789abcdef0123456789abcdef";
    let mut controller = failed_subagent_controller(&directory.path().join(child_id), child_id);
    let reports = seed_child_reports(&mut controller, child_id, directory.path());
    let child = controller.state.sessions.get_mut(child_id).unwrap();
    child.state = SessionState::Stopped;
    child.target = None;
    crate::database::save_session(child).unwrap();
    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(child_id, || {
        controller.destroy_session_controlled(child_id, &ProcessExecutor)
    })
    .unwrap();
    assert!(reports.join(child_id).exists());
    crate::controller::test_support::worker_test_scope("abcdefabcdefabcdefabcdefabcdefab", || {
        controller.force_destroy_session(
            "abcdefabcdefabcdefabcdefabcdefab",
            &ProcessExecutor,
            BranchDisposition::Keep,
        )
    })
    .unwrap();
    assert!(!reports.join(child_id).exists());
    assert!(reports.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_dir());
}

// Hard-won: #1331: report removal is best effort and must never prevent child destroy.
#[cfg(unix)]
#[test]
fn report_removal_failure_does_not_fail_child_destroy() {
    if !in_isolated_store("report_removal_failure_does_not_fail_child_destroy") {
        return;
    }
    struct ReportFailureExecutor;
    impl CommandExecutor for ReportFailureExecutor {
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
            if command.purpose == "remove sub-agent report directories" {
                anyhow::bail!("report directory is read-only");
            }
            ProcessExecutor.execute(command)
        }
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let child_id = "0123456789abcdef0123456789abcdef";
    let mut controller = failed_subagent_controller(&directory.path().join(child_id), child_id);
    let reports = seed_child_reports(&mut controller, child_id, directory.path());
    persist_controller_fixture(&mut controller);
    crate::controller::test_support::worker_test_scope(child_id, || {
        controller.force_destroy_session_with(
            child_id,
            &ReportFailureExecutor,
            BranchDisposition::Keep,
            |_| Ok(()),
        )
    })
    .unwrap();
    assert!(
        reports.join(child_id).is_dir(),
        "failure did not secretly delete reports"
    );
}

// Hard-won: #1565: a sub-agent whose turn never settled after cancellation could not be suspended and was left in error with its worker running
#[cfg(unix)]
#[tokio::test]
async fn closing_a_child_recovers_a_turn_that_never_settles_after_cancellation() {
    use crate::controller::checkpoint::tests::LATCH_RELAY_SESSION;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    const NAME: &str = "closing_a_child_recovers_a_turn_that_never_settles_after_cancellation";
    const CHILD: &str = "MJ_TEST_UNSETTLED_CHILD_CLOSE";
    if std::env::var_os(CHILD).is_none() {
        for case in ["reject", "commit"] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(crate::controller::test_support::test_name(
                module_path!(),
                NAME,
            ))
            .env(CHILD, case)
            .env("MJ_INSTANCE", "child-close-unsettled")
            .env("MJ_WORKER_BINARY", {
                let source = directory.path().join("worker-source");
                std::fs::write(
                    &source,
                    mj_core::worker_build::worker_build_stamp().as_bytes(),
                )
                .unwrap();
                source
            })
            .env("MJ_TEST_LATCH_JOURNALS_BEFORE_CLOSE", "notification")
            .isolated_store(directory.path())
            .run();
        }
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    std::fs::create_dir_all(data.join("relay")).unwrap();
    std::fs::write(data.join("relay/unsettled-cancellation"), b"keep running").unwrap();
    let relation = failed_subagent_relation(LATCH_RELAY_SESSION);
    let mut parent = checkpoint_test_session(&relation.parent_session_id);
    parent.project_directory = Some(data.join("project"));
    crate::database::save_session(&parent).unwrap();
    let (mut controller, channels, handle) =
        crate::controller::checkpoint::stand_in::close_cut_controller_for_child(
            SessionState::Running,
            true,
            Some(&relation),
        )
        .await;
    controller
        .config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .checkpoint = None;
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .project_directory = Some(data.join("project"));
    persist_controller_fixture(&mut controller);
    let worker_root = data.join("workers").join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(worker_root.join("private"), b"keep until synced").unwrap();
    // Reconnect uses the existing relay fake in checkpoint-only mode, reading
    // the same durable journal after the old connection is stopped.
    let launcher = format!(
        "#!/bin/sh\nexport MJ_TEST_LATCH_RELAY_ROOT={}\nexport MJ_TEST_LATCH_CHECKPOINT_ONLY=1\n{} --exact {} --nocapture | grep --line-buffered '^{{'\n",
        mj_core::targets::posix_quote(&data.join("relay").to_string_lossy()),
        mj_core::targets::posix_quote(&std::env::current_exe().unwrap().to_string_lossy()),
        mj_core::test_hooks::latch_relay::CHILD_TEST,
    );
    std::fs::write(worker_root.join("hel"), &launcher).unwrap();
    std::fs::set_permissions(
        worker_root.join("hel"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    #[derive(Default)]
    struct ClosingExecutor {
        purposes: std::sync::Mutex<Vec<String>>,
        notices: std::sync::Mutex<Vec<String>>,
    }
    impl CommandExecutor for ClosingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }
        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }
        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }
        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if command.purpose == "detect target platform" {
                return ProcessExecutor.execute(command);
            }
            Ok(CommandOutput {
                status: 0,
                stdout: b"present\n".to_vec(),
                stderr: Vec::new(),
            })
        }
        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            std::io::copy(input, &mut std::io::sink())?;
            self.execute(command)
        }
        fn notify_notice(&self, message: &str) {
            self.notices.lock().unwrap().push(message.into());
        }
    }
    let executor = Arc::new(ClosingExecutor::default());
    let before_close_calls = Arc::new(AtomicUsize::new(0));
    let reject_close = std::env::var(CHILD).unwrap() == "reject";
    let before_close: BeforeClose = Box::pin({
        let executor = executor.clone();
        let calls = before_close_calls.clone();
        let data = data.clone();
        async move {
            assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
            assert!(data.join("relay/cancellation-accepted").exists());
            assert!(
                !executor
                    .purposes
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|purpose| purpose == "stop Mjolnir worker daemon"),
                "close must commit before any worker stop"
            );
            if reject_close {
                anyhow::bail!("close rejected before escalation");
            }
            Ok(())
        }
    });
    let original_target = controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let log = crate::test_log::CapturedLog::default();
    let _subscriber = tracing::subscriber::set_default(log.clone());
    let outcome = controller
        .suspend_subagent_controlled(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            executor.as_ref(),
            Some(&channels.control),
            Some(before_close),
        )
        .await;
    assert_eq!(before_close_calls.load(Ordering::SeqCst), 1);
    assert!(data.join("relay/cancellation-accepted").exists());
    if reject_close {
        assert!(format!("{:#}", outcome.unwrap_err()).contains("close rejected before escalation"));
        let live = crate::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(live.state, SessionState::Running);
        assert_eq!(live.target, original_target);
        crate::controller::checkpoint::tests::wait_until_the_actor_serves_again(&handle).await;
        let snapshot = handle.view().snapshot.unwrap();
        assert!(
            snapshot.operational.checkpoint_barrier.is_none(),
            "aborted close retained its barrier"
        );
        assert!(!snapshot.operational.checkpoint_only);
        assert!(snapshot.operational.native_session_is_ready());
        assert_eq!(
            snapshot.operational.native_session_id.as_deref(),
            Some("native-session")
        );
        assert!(
            executor.purposes.lock().unwrap().is_empty(),
            "rejected close must not stop, reinstall or start its original worker"
        );
        assert_eq!(
            std::fs::read(worker_root.join("hel")).unwrap(),
            launcher.as_bytes()
        );
        assert_eq!(
            std::fs::read(worker_root.join("private")).unwrap(),
            b"keep until synced"
        );
        assert!(executor.notices.lock().unwrap().is_empty());
        assert!(
            !log.at(tracing::Level::WARN)
                .iter()
                .any(|message| message.contains("checkpoint requires a worker restart"))
        );
        drop(handle);
        channels.shutdown.shutdown().await.unwrap();
        return;
    }
    outcome.unwrap();
    let closed = crate::database::load_session_record(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(closed.state, SessionState::Stopped);
    assert!(closed.target.is_none());
    let history = crate::database::load_materialized_session(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(
        history.execution,
        mj_core::state::MaterializedExecutionState::Closed
    );
    {
        let purposes = executor.purposes.lock().unwrap();
        let first_stop = purposes
            .iter()
            .position(|p| p == "stop Mjolnir worker daemon")
            .unwrap();
        let start = purposes
            .iter()
            .position(|p| p == "start detached Mjolnir worker")
            .unwrap();
        let final_stop = purposes
            .iter()
            .rposition(|p| p == "stop Mjolnir worker daemon")
            .unwrap();
        assert!(
            first_stop < start && start < final_stop,
            "stop the wedged worker before recovery, then stop the sealed worker: {purposes:?}"
        );
    }
    assert!(
        executor
            .notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("unresponsive worker"))
    );
    assert!(
        log.at(tracing::Level::WARN)
            .iter()
            .any(|message| message.contains("checkpoint requires a worker restart"))
    );
    drop(handle);
    channels.shutdown.shutdown().await.unwrap();
}

#[test]
fn raw_git_session_uses_origin_delta_raw_policy_and_removes_launch_record_on_destroy() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(&["init", "--initial-branch=main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "initial"]);

    mj_checkpoint::snapshot::record_launch_if_absent(
        &mj_checkpoint::archive::SystemGit,
        &repo,
        "raw-session",
    )
    .unwrap();
    mj_checkpoint::snapshot::record_launch_if_absent(
        &mj_checkpoint::archive::SystemGit,
        &repo,
        "sibling-session",
    )
    .unwrap();

    let mut config = Config::default();
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);

    let mut raw_session = checkpoint_test_session("raw-session");
    raw_session.target_template_id = "local-bare".into();
    raw_session.project_directory = Some(repo.clone());
    raw_session.state = SessionState::Running;

    let mut plain_session = checkpoint_test_session("plain-session");
    plain_session.target_template_id = "local-bare".into();
    plain_session.project_directory = Some(temp.path().to_path_buf());
    plain_session.project = Some(std::sync::Arc::new(
        serde_json::from_value(serde_json::json!({
            "bundle": {
                "primary_repo": "plain",
                "repositories": [{
                    "id": "plain",
                    "local": temp.path(),
                    "destination": "plain"
                }]
            },
            "identities": {
                "plain": { "local": temp.path() }
            },
            "plain_directories": ["plain"],
            "network_sources": {}
        }))
        .unwrap(),
    ));
    plain_session.state = SessionState::Running;

    let controller = Controller {
        config,
        state: State {
            environment_owners: [
                ("raw-session".into(), "raw-session".into()),
                ("plain-session".into(), "plain-session".into()),
            ]
            .into_iter()
            .collect(),
            sessions: [
                ("raw-session".into(), raw_session.clone()),
                ("plain-session".into(), plain_session),
            ]
            .into_iter()
            .collect(),
            ..State::default()
        },
    };

    assert_eq!(
        controller
            .explicit_suspend_export_policy("raw-session")
            .unwrap(),
        CheckpointExportPolicy::OriginDeltaRaw
    );
    assert_eq!(
        controller
            .explicit_suspend_export_policy("plain-session")
            .unwrap(),
        CheckpointExportPolicy::ReuseUnchangedArchive
    );

    let mut cutoff_session = raw_session.clone();
    cutoff_session.id = "cutoff-session".into();
    cutoff_session.project = None;
    cutoff_session.project_directory = None;
    cutoff_session.checkout = Some(Box::new(mj_core::remote_git::ExactCheckout {
        repository_id: "repo".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        branch: None,
        no_later_history: true,
    }));
    let cutoff_controller = Controller {
        config: controller.config.clone(),
        state: State {
            environment_owners: [("cutoff-session".into(), "cutoff-session".into())]
                .into_iter()
                .collect(),
            sessions: [("cutoff-session".into(), cutoff_session)]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };
    assert_eq!(
        cutoff_controller
            .explicit_suspend_export_policy("cutoff-session")
            .unwrap(),
        CheckpointExportPolicy::OriginDeltaCutoff
    );

    controller
        .remove_raw_launch_record(&raw_session, &mj_core::targets::ProcessExecutor)
        .unwrap();
    assert!(
        mj_checkpoint::snapshot::LaunchRecord::read(
            &mj_checkpoint::archive::SystemGit,
            &repo,
            "raw-session",
        )
        .unwrap()
        .is_none()
    );
    assert!(
        mj_checkpoint::snapshot::LaunchRecord::read(
            &mj_checkpoint::archive::SystemGit,
            &repo,
            "sibling-session",
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn suspend_without_checkpoint_respects_cancellation_without_settling_stopped() {
    if !in_isolated_store(
        "suspend_without_checkpoint_respects_cancellation_without_settling_stopped",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    struct CancellingFailingExecutor;
    impl CommandExecutor for CancellingFailingExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            anyhow::bail!("cancelled cleanup")
        }
        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }
        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute(command)
        }
        fn pause_deadline(&self) -> mj_targets::targets::DeadlinePause {
            mj_targets::targets::DeadlinePause::none()
        }
        fn cancellation_requested(&self) -> bool {
            true
        }
        fn cancellation_requested_without_deadline(&self) -> bool {
            true
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "cancelled-without-checkpoint";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
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
    persist_controller_fixture(&mut controller);
    let err = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(
            session_id,
            true,
            &CancellingFailingExecutor,
            |_| Ok(()),
        )
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("cancelled cleanup"));
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Running
    );
    assert!(controller.state.sessions[session_id].target.is_some());
}

#[test]
fn suspend_preflight_inventories_origin_delta_cutoff_and_keep_dir() {
    if !in_isolated_store("suspend_preflight_inventories_origin_delta_cutoff_and_keep_dir") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "cutoff-preflight-session";
    let repo = directory.path().join(session_id);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("tracked.txt"), "hello\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial"]);
    let head = git(&["rev-parse", "HEAD"]);

    // Add untracked file, session keep-dir file, and an in-progress merge marker.
    std::fs::write(repo.join(".git/info/exclude"), "/.mj/keep/\n").unwrap();
    std::fs::write(repo.join("scratch.txt"), "untracked\n").unwrap();
    let keep_dir = repo.join(".mj/keep").join(session_id);
    std::fs::create_dir_all(&keep_dir).unwrap();
    std::fs::write(keep_dir.join("note.txt"), "keep-me\n").unwrap();
    std::fs::write(repo.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.project = None;
    session.project_directory = None;
    session.managed_worktree = None;
    session.checkout = Some(Box::new(mj_core::remote_git::ExactCheckout {
        repository_id: "workspace".into(),
        commit: head.clone(),
        branch: None,
        no_later_history: true,
    }));
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.launch_base = Some(head.clone());
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
    persist_controller_fixture(&mut controller);

    let preflight = controller
        .suspend_preflight(session_id, true, false, &ProcessExecutor)
        .unwrap();
    assert_eq!(
        preflight.untracked.len(),
        1,
        "keep_dir files must not be duplicated into untracked: {:?}",
        preflight.untracked
    );
    assert_eq!(
        preflight.untracked[0].path,
        std::path::Path::new("scratch.txt")
    );
    assert_eq!(preflight.keep_dir_entries.len(), 1);
    assert_eq!(
        preflight.keep_dir_entries[0].path,
        std::path::PathBuf::from(session_id).join("note.txt")
    );
    assert_eq!(preflight.untracked_count(), 2);
    assert!(preflight.keep_dir_bytes > 0);
    assert_eq!(preflight.operations.len(), 1);
    assert_eq!(
        preflight.operations[0].kind,
        mj_core::suspend_consent::GitOperationKind::Merge
    );
    let dry_run = preflight.dry_run.expect("dry_run estimate");
    assert_eq!(dry_run.session_state_bytes, None);
    assert!(
        dry_run
            .git_delta_unavailable_reason
            .as_deref()
            .is_some_and(|r| r.contains("merge"))
    );
}

#[test]
fn suspend_preflight_managed_worktree_shown_consent_matches_capture_repository_id() {
    if !in_isolated_store(
        "suspend_preflight_managed_worktree_shown_consent_matches_capture_repository_id",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin.git");
    let source = directory.path().join("source");
    let session_id = "preflight-shown-consent-repo-id";
    let clone_root = source.join(".mj/clones").join(session_id);

    let run_git = |cwd: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "--bare", "-b", "main"]);
    std::fs::create_dir_all(&source).unwrap();
    run_git(&source, &["init", "-b", "main"]);
    run_git(&source, &["config", "user.name", "Test"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("tracked.txt"), "base\n").unwrap();
    run_git(&source, &["add", "tracked.txt"]);
    run_git(&source, &["commit", "-m", "initial"]);
    run_git(
        &source,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    run_git(&source, &["push", "-u", "origin", "main"]);

    std::fs::create_dir_all(clone_root.parent().unwrap()).unwrap();
    run_git(
        directory.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            clone_root.to_str().unwrap(),
        ],
    );
    run_git(&clone_root, &["config", "user.name", "Test"]);
    run_git(&clone_root, &["config", "user.email", "test@example.com"]);
    let head = run_git(&clone_root, &["rev-parse", "HEAD"]);
    std::fs::write(clone_root.join(".git/info/exclude"), "/.mj/keep/\n").unwrap();
    std::fs::write(clone_root.join("conflict-note.txt"), "untracked\n").unwrap();
    let keep_dir = clone_root.join(".mj/keep").join(session_id);
    std::fs::create_dir_all(&keep_dir).unwrap();
    std::fs::write(keep_dir.join("note.txt"), "keep-me\n").unwrap();
    std::fs::write(clone_root.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.project_directory = Some(clone_root.clone());
    session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        source_project_directory: source.clone(),
        source_repository: source.clone(),
        worktree_root: clone_root.clone(),
        branch: "main".into(),
        target: mj_core::state::ManagedWorktreeTarget::Local,
        base_commit: Some(head),
    }));
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join("workers").join(session_id),
    }));
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
    persist_controller_fixture(&mut controller);

    let preflight = controller
        .suspend_preflight(session_id, false, false, &ProcessExecutor)
        .unwrap();
    let layout = controller
        .session_export_layout(session_id, &ProcessExecutor)
        .unwrap();
    let expected_repo_id = layout.repositories[0].id.as_str();
    assert_eq!(
        expected_repo_id,
        crate::controller::checkpoint::HOST_CHECKOUT_REPOSITORY
    );
    assert_eq!(preflight.untracked.len(), 1);
    assert_eq!(
        preflight.untracked[0].checkout.repository_id,
        expected_repo_id
    );
    assert_eq!(preflight.keep_dir_entries.len(), 1);
    assert_eq!(
        preflight.keep_dir_entries[0].checkout.repository_id,
        expected_repo_id
    );
    assert_eq!(preflight.operations.len(), 1);
    assert_eq!(
        preflight.operations[0].checkout.repository_id,
        expected_repo_id
    );

    // Passing `preflight.shown_consent()` directly into capture using the
    // layout's repository id must succeed without refusing mismatched ids.
    let shown = preflight.shown_consent();
    let input = mj_checkpoint::checkpoint::managed_clone_snapshot_input(
        expected_repo_id,
        &clone_root,
        Some(session_id),
        directory.path().join("snapshot.pack"),
        None,
    )
    .unwrap();
    let capture_consent = mj_checkpoint::snapshot::SessionCaptureConsent {
        repositories: [(
            expected_repo_id.to_owned(),
            mj_checkpoint::snapshot::CaptureConsent {
                move_keep: None,
                untracked: shown.discard_untracked,
                operations: shown.abort_operations,
            },
        )]
        .into_iter()
        .collect(),
    };
    mj_checkpoint::snapshot::capture_session_snapshot(
        &mj_checkpoint::archive::SystemGit,
        &[input],
        &capture_consent,
    )
    .expect("preflight.shown_consent() must match capture repository id");
}

#[test]
fn suspend_without_checkpoint_refuses_corrupt_archive_without_confirm_no_checkpoint() {
    if !in_isolated_store(
        "suspend_without_checkpoint_refuses_corrupt_archive_without_confirm_no_checkpoint",
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "corrupt-archive-refusal";
    let archive_path = directory.path().join("corrupt.zip");
    std::fs::write(&archive_path, b"corrupted bytes").unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path,
        sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        created_at: "2026-11-21T00:00:00Z".into(),
        event_frontier: 1,
    }));
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
    persist_controller_fixture(&mut controller);

    let err = crate::controller::test_support::worker_test_scope(session_id, || {
        controller.suspend_without_checkpoint_with(session_id, false, &FailingExecutor, |_| Ok(()))
    })
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("No verified checkpoint exists"),
        "expected refusal, got: {err:#}"
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Running
    );
    assert!(controller.state.sessions[session_id].target.is_some());
}

#[tokio::test]
async fn suspend_without_checkpoint_runs_before_close_before_parent_teardown() {
    if !in_isolated_store("suspend_without_checkpoint_runs_before_close_before_parent_teardown") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "before-close-order";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
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
    persist_controller_fixture(&mut controller);

    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let before_close_order = order.clone();
    let before_close: BeforeClose = Box::pin(async move {
        before_close_order.lock().unwrap().push("before_close");
        Ok(())
    });
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let admission = crate::controller::test_support::session_admission(session_id);
    let consent = mj_core::suspend_consent::SuspendConsent {
        without_checkpoint: true,
        confirm_no_checkpoint: true,
        ..Default::default()
    };
    controller
        .suspend_session_managed_controlled(
            &admission,
            session_id,
            &ProcessExecutor,
            &channels.control,
            &consent,
            Some(before_close),
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();
    assert_eq!(order.lock().unwrap().as_slice(), &["before_close"]);
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
}

#[test]
fn suspend_preflight_without_checkpoint_skips_unreachable_target() {
    if !in_isolated_store("suspend_preflight_without_checkpoint_skips_unreachable_target") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "preflight-offline-target";
    let archive_path = directory.path().join("corrupt.zip");
    std::fs::write(&archive_path, b"corrupted bytes").unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "remote".into();
    session.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: "mj-offline".into(),
        borrowed_from: None,
        workspace_storage: Default::default(),
    }));
    session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path,
        sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        created_at: "2026-11-21T00:00:00Z".into(),
        event_frontier: 1,
    }));
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    persist_controller_fixture(&mut controller);

    let preflight = controller
        .suspend_preflight(session_id, false, true, &FailingExecutor)
        .expect("without_checkpoint preflight must not contact unreachable target");
    let existing = preflight
        .existing_checkpoint
        .expect("existing_checkpoint summary");
    assert!(
        !existing.verified,
        "corrupt archive must report verified=false"
    );
}

#[tokio::test]
async fn suspend_without_checkpoint_refusal_does_not_invoke_before_close() {
    if !in_isolated_store("suspend_without_checkpoint_refusal_does_not_invoke_before_close") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "before-close-refusal";
    let archive_path = directory.path().join("corrupt.zip");
    std::fs::write(&archive_path, b"corrupted bytes").unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path,
        sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        created_at: "2026-11-21T00:00:00Z".into(),
        event_frontier: 1,
    }));
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
    persist_controller_fixture(&mut controller);

    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called_clone = called.clone();
    let before_close: BeforeClose = Box::pin(async move {
        called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let admission = crate::controller::test_support::session_admission(session_id);
    let consent = mj_core::suspend_consent::SuspendConsent {
        without_checkpoint: true,
        confirm_no_checkpoint: false,
        ..Default::default()
    };
    let err = controller
        .suspend_session_managed_controlled(
            &admission,
            session_id,
            &FailingExecutor,
            &channels.control,
            &consent,
            Some(before_close),
        )
        .await
        .unwrap_err();
    channels.shutdown.shutdown().await.unwrap();
    assert!(
        format!("{err:#}").contains("No verified checkpoint exists"),
        "expected refusal, got: {err:#}"
    );
    assert!(
        !called.load(std::sync::atomic::Ordering::SeqCst),
        "before_close must not be invoked when without_checkpoint is refused"
    );
}

#[test]
fn suspend_preflight_and_capture_align_session_keep_dir_shown_consent() {
    if !in_isolated_store("suspend_preflight_and_capture_align_session_keep_dir_shown_consent") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "keep-dir-roundtrip";
    let repo = directory.path().join(session_id);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "init"]);
    let head = String::from_utf8(
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    std::fs::write(repo.join(".git/info/exclude"), "/.mj/keep/\n").unwrap();
    let keep_dir = mj_checkpoint::checkpoint::session_keep_dir(&repo, session_id).unwrap();
    std::fs::create_dir_all(keep_dir.join("sub")).unwrap();
    std::fs::write(keep_dir.join("sub").join("notes.txt"), "keep note\n").unwrap();
    std::fs::write(keep_dir.join("ignored.txt"), "direct keep file\n").unwrap();
    // Legacy direct file under `.mj/keep/` and another subdirectory must also
    // be surfaced and require consent before deleting a managed checkout.
    let checkout_keep = mj_checkpoint::checkpoint::checkout_keep_dir(&repo);
    std::fs::write(checkout_keep.join("legacy.txt"), "legacy keep file\n").unwrap();
    std::fs::create_dir_all(checkout_keep.join("other-session")).unwrap();
    std::fs::write(
        checkout_keep.join("other-session").join("sibling.txt"),
        "sibling keep file\n",
    )
    .unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local".into();
    session.project = None;
    session.project_directory = None;
    session.managed_worktree = None;
    session.checkout = Some(Box::new(mj_core::remote_git::ExactCheckout {
        repository_id: "workspace".into(),
        commit: head.clone(),
        branch: None,
        no_later_history: true,
    }));
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(session_id),
    }));
    session.launch_base = Some(head.clone());
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
    persist_controller_fixture(&mut controller);

    let preflight = controller
        .suspend_preflight(session_id, false, false, &ProcessExecutor)
        .unwrap();
    let keep_paths: Vec<&std::path::Path> = preflight
        .keep_dir_entries
        .iter()
        .map(|entry| entry.path.as_path())
        .collect();
    assert_eq!(
        keep_paths.len(),
        4,
        "expected legacy, sibling, and session keep files: {keep_paths:?}"
    );
    assert!(keep_paths.contains(&std::path::Path::new("legacy.txt")));
    assert!(keep_paths.contains(&std::path::Path::new("other-session/sibling.txt")));
    assert!(
        keep_paths.contains(
            &std::path::PathBuf::from(session_id)
                .join("ignored.txt")
                .as_path()
        )
    );
    assert!(
        keep_paths.contains(
            &std::path::PathBuf::from(session_id)
                .join("sub/notes.txt")
                .as_path()
        )
    );
    assert!(
        preflight.untracked.is_empty(),
        "keep files must not be duplicated in untracked: {:?}",
        preflight.untracked
    );
    let preflight_refused = controller
        .preflight_origin_delta_suspension(
            session_id,
            &mj_core::suspend_consent::SuspendConsent::default(),
            &ProcessExecutor,
        )
        .expect_err("preflight_origin_delta_suspension must refuse unconsented keep files");
    let preflight_refused_text = format!("{preflight_refused:#}");
    assert!(
        preflight_refused_text.contains(".mj/keep/legacy.txt")
            && preflight_refused_text.contains(&format!(".mj/keep/{session_id}/ignored.txt"))
            && !preflight_refused_text.contains(&format!(".mj/keep/{session_id}/{session_id}/")),
        "refusal must show actual keep paths without doubling session_id: {preflight_refused_text}"
    );

    let consent = preflight.shown_consent().capture_consent();
    let input = mj_checkpoint::checkpoint::cutoff_snapshot_input(
        "workspace",
        &repo,
        Some(session_id),
        vec![head],
        directory.path().join("workspace.pack"),
    )
    .unwrap();
    let checkout = mj_checkpoint::snapshot::CheckoutKey {
        repository_id: "workspace".into(),
        checkout_path: std::path::PathBuf::new(),
    };
    mj_checkpoint::snapshot::check_keep_dir_consent(
        &checkout,
        input.keep_dir.as_deref().unwrap(),
        &consent.untracked,
        None,
    )
    .expect("capture check_keep_dir_consent must accept Shown consent built from preflight");
    let refused = mj_checkpoint::snapshot::capture_session_snapshot(
        &mj_checkpoint::archive::SystemGit,
        std::slice::from_ref(&input),
        &mj_checkpoint::snapshot::SessionCaptureConsent::default(),
    )
    .expect_err("capture_session_snapshot must refuse unconsented keep_dir contents");
    assert!(
        format!("{refused:#}").contains("legacy.txt"),
        "expected legacy.txt in refusal: {refused:#}"
    );
    let session_consent = mj_checkpoint::snapshot::SessionCaptureConsent {
        repositories: [("workspace".into(), consent)].into_iter().collect(),
    };
    mj_checkpoint::snapshot::capture_session_snapshot(
        &mj_checkpoint::archive::SystemGit,
        &[input],
        &session_consent,
    )
    .expect("capture_session_snapshot must succeed with Shown consent built from preflight");
}

// Hard-won: #1660: Move asked for discard consent for files its keep stream carries.
#[cfg(unix)]
#[test]
fn move_keep_preflight_and_capture_share_source_selection() {
    let name = "move_keep_preflight_and_capture_share_source_selection";
    if !in_isolated_store(name) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let (origin, repo) = init_managed_clone_with_origin(directory.path(), name);
    let source_root = mj_checkpoint::checkpoint::session_keep_dir(&repo, name).unwrap();
    let keep_root = mj_checkpoint::checkpoint::checkout_keep_dir(&repo);
    let outside = directory.path().join("outside");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {:?}", output.stderr);
    };
    git(&["worktree", "add", "--detach", outside.to_str().unwrap()]);
    std::fs::write(repo.join(".git/info/exclude"), "/.mj/keep/\n").unwrap();
    std::fs::create_dir_all(source_root.join("excluded")).unwrap();
    std::fs::write(source_root.join("evidence.txt"), "carry this").unwrap();
    std::fs::write(source_root.join("excluded/cache.dat"), "cache data").unwrap();
    std::fs::write(keep_root.join("legacy.txt"), "older scratch").unwrap();
    std::fs::create_dir(keep_root.join("sibling")).unwrap();
    std::fs::write(keep_root.join("sibling/note.txt"), "sibling scratch").unwrap();
    std::fs::write(repo.join("untracked.txt"), "untracked data").unwrap();

    let mut session = checkpoint_test_session(name);
    session.state = SessionState::Running;
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: directory.path().join(name),
    }));
    attach_local_managed_clone(&mut session, repo.clone(), origin);
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(name.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    persist_controller_fixture(&mut controller);

    use mj_core::suspend_consent::{CaptureConsent, SuspendConsent};
    let source = &controller.state.sessions[name];
    let (backend, _) = controller.worker_placement(name).unwrap();
    let keep = mj_core::session_keep::MoveKeep::new(
        controller
            .move_source_keep_dir(source, &backend, &ProcessExecutor)
            .unwrap(),
        &[PathBuf::from("./excluded")],
    )
    .unwrap();
    let capture_input = mj_checkpoint::checkpoint::managed_clone_snapshot_input(
        super::super::checkpoint::HOST_CHECKOUT_REPOSITORY,
        &repo,
        Some(name),
        directory.path().join("workspace.pack"),
        None,
    )
    .unwrap();
    let capture = |consent| {
        mj_checkpoint::snapshot::capture_session_snapshot(
            &mj_checkpoint::archive::SystemGit,
            std::slice::from_ref(&capture_input),
            &mj_checkpoint::snapshot::SessionCaptureConsent {
                repositories: [(capture_input.id.clone(), consent)].into_iter().collect(),
            },
        )
    };
    let (blocked, _) = controller
        .checkpoint_preflight(name, true, false, Some(&keep), &ProcessExecutor)
        .unwrap();
    let blocker = &blocked.blockers[0].remedy;
    assert!(blocker.contains("unrepresented worktree"), "{blocker}");
    assert!(blocker.contains("or move it inside the clone"), "{blocker}");
    let refusal = controller
        .check_origin_delta_preflight(
            name,
            &SuspendConsent::default(),
            Some(&keep),
            &ProcessExecutor,
        )
        .unwrap_err();
    assert!(format!("{refusal:#}").contains(blocker));
    assert!(format!("{:#}", capture(CaptureConsent::default()).unwrap_err()).contains(blocker));
    git(&["worktree", "remove", outside.to_str().unwrap()]);

    let (preflight, skips) = controller
        .checkpoint_preflight(name, true, false, Some(&keep), &ProcessExecutor)
        .unwrap();
    assert_eq!(skips, [(PathBuf::from("excluded"), 10)]);
    let lost = preflight.untracked_display_paths();
    assert_eq!(
        lost,
        [
            "untracked.txt",
            ".mj/keep/legacy.txt",
            &format!(".mj/keep/{name}/excluded/cache.dat"),
            ".mj/keep/sibling/note.txt",
        ]
    );
    let refusal = controller
        .check_origin_delta_preflight(
            name,
            &SuspendConsent::default(),
            Some(&keep),
            &ProcessExecutor,
        )
        .unwrap_err();
    let refusal = format!("{refusal:#}");
    assert!(lost.iter().all(|path| refusal.contains(path)), "{refusal}");
    assert!(!refusal.contains("evidence.txt"), "{refusal}");

    let shown = preflight.shown_consent();
    let mut current_worker = shown.capture_consent();
    current_worker.move_keep = Some(keep.clone());
    // The older worker's nested CaptureConsent accepts unknown fields. Its
    // capture still sees carried scratch as uncovered and refuses safely.
    #[derive(serde::Deserialize)]
    struct LegacyCaptureConsent {
        untracked: mj_core::suspend_consent::UntrackedConsent,
        operations: mj_core::suspend_consent::OperationConsent,
    }
    let legacy: LegacyCaptureConsent =
        serde_json::from_value(serde_json::to_value(&current_worker).unwrap()).unwrap();
    let refusal = capture(CaptureConsent {
        untracked: legacy.untracked,
        operations: legacy.operations,
        move_keep: None,
    })
    .unwrap_err();
    assert!(format!("{refusal:#}").contains("evidence.txt"));

    // Files created after preview still travel even though the user's consent
    // only covers the exact lost files they saw.
    std::fs::write(source_root.join("after-preview.txt"), "also carried").unwrap();
    controller
        .check_origin_delta_preflight(name, &shown, Some(&keep), &ProcessExecutor)
        .unwrap();
    capture(current_worker).unwrap();
}

#[cfg(unix)]
fn init_managed_clone_with_origin(temp: &std::path::Path, session_id: &str) -> (PathBuf, PathBuf) {
    let origin = temp.join("origin.git");
    let seed = temp.join("seed");
    let clone = origin.join(".mj/clones").join(session_id);
    if let Some(parent) = clone.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let _ = std::fs::remove_dir_all(&clone);
    let run_git = |dir: Option<&std::path::Path>, args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        if let Some(dir) = dir {
            cmd.arg("-C").arg(dir);
        }
        let output = cmd
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(
        None,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            origin.to_str().unwrap(),
        ],
    );
    run_git(
        None,
        &["init", "--initial-branch=main", seed.to_str().unwrap()],
    );
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    run_git(Some(&seed), &["add", "README.md"]);
    run_git(Some(&seed), &["commit", "-m", "initial"]);
    run_git(
        Some(&seed),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    run_git(Some(&seed), &["push", "origin", "main"]);
    run_git(
        None,
        &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
    );
    (origin, clone)
}

#[cfg(unix)]
fn attach_local_managed_clone(session: &mut SessionRecord, clone: PathBuf, origin: PathBuf) {
    session.project_directory = Some(clone.clone());
    session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        source_repository: origin.clone(),
        source_project_directory: origin,
        worktree_root: clone,
        branch: "main".into(),
        base_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        target: mj_core::state::ManagedWorktreeTarget::Local,
    }));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_move_keep_lookup_recovers_the_accepted_suspend() {
    use crate::controller::move_session::test_fixtures::source_recovery_operation;
    use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest, WorkerIdentity};

    let name = "failed_move_keep_lookup_recovers_the_accepted_suspend";
    if !in_isolated_store(name) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let (origin, clone) = init_managed_clone_with_origin(directory.path(), name);
    let worker_root = directory.path().join("workers").join(name);
    std::fs::create_dir_all(&worker_root).unwrap();
    let operation = "origin-delta-suspend-test".to_owned();
    wire::write(&worker_root, wire::ACCEPTED, &operation).unwrap();
    wire::write(
        &worker_root,
        wire::REQUEST,
        &SuspendRequest {
            operation: operation.clone(),
            worker: WorkerIdentity {
                pid: u32::MAX,
                start: 0,
            },
            checkout: clone.clone(),
            child_sessions: Vec::new(),
        },
    )
    .unwrap();
    wire::write(
        &worker_root,
        wire::RECEIPT,
        &SuspendReceipt {
            operation: operation.clone(),
            error: None,
        },
    )
    .unwrap();

    let mut session = checkpoint_test_session(name);
    session.state = SessionState::Closing;
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    attach_local_managed_clone(&mut session, clone.clone(), origin);
    let mut move_operation = source_recovery_operation(&session);
    let mut controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(name.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    persist_controller_fixture(&mut controller);
    // The worker already stopped. The retained checkout can disappear before
    // the resumed Move resolves its source keep path.
    std::fs::rename(&clone, directory.path().join("unavailable-checkout")).unwrap();
    let admission = crate::controller::test_support::session_admission(name);
    let error = controller
        .suspend_session_controlled_with_manager(
            &admission,
            name,
            &ProcessExecutor,
            None,
            Some((&mut move_operation, None)),
            SourceTargetDisposition::RetainForInPlaceSwap,
            CheckpointExportPolicy::OriginDelta,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            None,
            LatchExclusivity::HoldThroughClose,
            None,
        )
        .await
        .unwrap_err();
    let error = format!("{error:#}");
    let lookup_error = format!("resolve checkout {}", clone.display());
    assert!(error.contains(&lookup_error), "{error}");
    let restart = wire::read::<SuspendReceipt>(&worker_root, wire::RESTART)
        .unwrap()
        .expect("the accepted suspend must durably request a restart");
    assert_eq!(restart.operation, operation);
    assert!(restart.error.unwrap().contains(&lookup_error));
    // This fixture has no installed worker to restart. Its Closing state must
    // survive so the durable restart can be retried, instead of claiming Running.
    let stored = crate::database::load_session_record(name).unwrap().unwrap();
    assert_eq!(stored.state, SessionState::Closing);
    assert_eq!(controller.state.sessions[name].state, SessionState::Closing);
}

#[cfg(unix)]
struct NoExportExecutor;

#[cfg(unix)]
impl CommandExecutor for NoExportExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }
    fn cancellation_requested_without_deadline(&self) -> bool {
        false
    }
    fn execute_without_deadline(
        &self,
        command: &CommandSpec,
    ) -> Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }
    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }
    fn execute(&self, command: &CommandSpec) -> Result<mj_core::targets::CommandOutput> {
        assert_ne!(
            command.purpose, "export target checkpoint",
            "sealed legacy close must not run a new export"
        );
        if command.program == "git"
            || command.program == "uname"
            || command.args.iter().any(|arg| arg.contains("uname"))
        {
            return ProcessExecutor.execute(command);
        }
        let stdout = if command.purpose.contains("liveness") {
            b"dead\n".to_vec()
        } else {
            Vec::new()
        };
        Ok(mj_core::targets::CommandOutput {
            status: 0,
            stdout,
            stderr: Vec::new(),
        })
    }
    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        _input: &mut (dyn std::io::Read + Send),
    ) -> Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }
}

// Hard-won: d64ca35d9: cross-target Move held the source relay while OriginDelta tried to lease it again.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn origin_delta_move_reuses_the_source_lease_until_quiescence() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    use crate::controller::move_session::MoveSourceRelay;
    use mj_core::worker_suspend::{self as wire, WorkerIdentity};
    use std::os::unix::fs::PermissionsExt;

    const NAME: &str = "origin_delta_move_reuses_the_source_lease_until_quiescence";
    const CHILD: &str = "MJ_TEST_ORIGIN_DELTA_MOVE_LEASE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "origin-delta-move-lease")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let (origin, clone) = init_managed_clone_with_origin(&data, LATCH_RELAY_SESSION);
    let (mut controller, channels, handle) =
        close_cut_controller_with(SessionState::Running, false).await;
    let session = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    let Some(TargetLocator::LocalBare { worker_root }) = session.target.as_deref() else {
        panic!("expected a local worker");
    };
    let worker_root = worker_root.clone();
    attach_local_managed_clone(session, clone, origin);
    persist_controller_fixture(&mut controller);

    std::fs::create_dir_all(&worker_root).unwrap();
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let start = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    wire::write(
        &worker_root,
        wire::CAPABILITY,
        &WorkerIdentity {
            pid: std::process::id(),
            start: start.parse().unwrap(),
        },
    )
    .unwrap();
    let worker = worker_root.join("hel");
    std::fs::write(&worker, "#!/bin/sh\n[ \"$1 $2\" = 'worker quiesce' ] || exit 2\necho 'injected quiescence refusal' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();

    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let mut source_relay =
        MoveSourceRelay::lease(&admission, &channels.control, LATCH_RELAY_SESSION)
            .await
            .unwrap();
    let held_relay = source_relay.take().expect("Move holds the live relay");
    let error = controller
        .suspend_session_controlled_with_manager(
            &admission,
            LATCH_RELAY_SESSION,
            &ProcessExecutor,
            Some(&channels.control),
            None,
            SourceTargetDisposition::RetainForInPlaceSwap,
            CheckpointExportPolicy::OriginDelta,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            Some(held_relay),
            LatchExclusivity::HoldThroughClose,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("injected quiescence refusal"),
        "{error:#}"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    handle.lease_connection(&admission).await.unwrap().release();
    channels.shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn older_worker_suspend_preflight_refuses_before_touching_worker() {
    const NAME: &str = "older_worker_suspend_preflight_refuses_before_touching_worker";
    const CHILD: &str = "MJ_TEST_OLDER_WORKER_PREFLIGHT";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "older-worker-preflight")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let temp = tempfile::tempdir().unwrap();
    let session_id = "older-worker-preflight";
    let (origin, clone) = init_managed_clone_with_origin(temp.path(), session_id);
    std::fs::write(clone.join("untracked.txt"), "dirty\n").unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    attach_local_managed_clone(&mut session, clone.clone(), origin);
    let mut config = Config::default();
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);
    session.target_template_id = "local-bare".into();
    let worker_root = temp.path().join("workers").join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    // Deliberately NO suspend-worker.json (older worker).
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));

    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    persist_controller_fixture(&mut controller);

    assert_eq!(
        controller
            .explicit_suspend_export_policy(session_id)
            .unwrap(),
        CheckpointExportPolicy::OriginDelta,
        "older worker on local managed clone with origin must not fall back to v2 capture"
    );

    let admission = crate::controller::test_support::session_admission(session_id);
    let error = controller
        .suspend_session_controlled(&admission, session_id, &ProcessExecutor)
        .await
        .expect_err("untracked file must fail preflight before touching the older worker");
    assert!(
        format!("{error:#}").contains("untracked.txt"),
        "expected untracked file refusal, got: {error:#}"
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Running
    );
    assert!(
        !worker_root.join(mj_core::worker_suspend::ACCEPTED).exists(),
        "refused preflight must not write suspend-operation.json"
    );
    assert!(clone.join("untracked.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn older_worker_suspend_fails_clearly_when_lease_cannot_be_acquired_without_v2_fallback() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    const NAME: &str =
        "older_worker_suspend_fails_clearly_when_lease_cannot_be_acquired_without_v2_fallback";
    const CHILD: &str = "MJ_TEST_OLDER_WORKER_BUSY_LEASE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let worker_bin = directory.path().join("worker-source");
        std::fs::write(
            &worker_bin,
            format!(
                "#!/bin/sh\n# {}\nexit 0\n",
                mj_core::worker_build::worker_build_stamp()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&worker_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "older-worker-busy-lease")
        .env("MJ_WORKER_BINARY", &worker_bin)
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    std::fs::create_dir_all(data.join("relay")).unwrap();
    std::fs::write(data.join("relay/unsettled-cancellation"), b"keep running").unwrap();
    let (origin, clone) = init_managed_clone_with_origin(&data, LATCH_RELAY_SESSION);
    let (mut controller, channels, _handle) =
        close_cut_controller_with(SessionState::Running, true).await;
    controller
        .config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let worker_root = data.join("workers").join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    // Deliberately NO suspend-worker.json (older worker).
    {
        let session = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        session.target_template_id = "local".into();
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));
        attach_local_managed_clone(session, clone, origin);
    }
    persist_controller_fixture(&mut controller);

    struct SkipHarnessPrepExecutor;
    impl CommandExecutor for SkipHarnessPrepExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            ProcessExecutor.pause_deadline()
        }
        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }
        fn execute_without_deadline(
            &self,
            command: &CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }
        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }
        fn execute(&self, command: &CommandSpec) -> Result<mj_core::targets::CommandOutput> {
            assert_ne!(
                command.purpose, "export target checkpoint",
                "older worker suspend must not fall back to version-2 export"
            );
            if command.purpose.contains("harness")
                || command.purpose.contains("prepare")
                || command.purpose.contains("runtime")
            {
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            ProcessExecutor.execute(command)
        }
        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            assert_ne!(
                command.purpose, "export target checkpoint",
                "older worker suspend must not fall back to version-2 export"
            );
            ProcessExecutor.execute_with_stdin(command, input)
        }
    }

    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let error = controller
        .suspend_session_managed_controlled(
            &admission,
            LATCH_RELAY_SESSION,
            &SkipHarnessPrepExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
        )
        .await
        .expect_err("older worker upgrade must fail when worker is not idle");
    channels.shutdown.shutdown().await.unwrap();
    assert!(
        format!("{error:#}").contains("worker is not idle"),
        "expected clear busy-worker refusal, got: {error:#}"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert!(
        !worker_root.join(mj_core::worker_suspend::ACCEPTED).exists(),
        "failed upgrade must clear suspend-operation.json"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn older_worker_swap_failure_attempts_one_in_place_recovery_before_recording_error() {
    const NAME: &str =
        "older_worker_swap_failure_attempts_one_in_place_recovery_before_recording_error";
    const CHILD: &str = "MJ_TEST_OLDER_WORKER_SWAP_FAILURE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "older-worker-swap-failure")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let temp = tempfile::tempdir().unwrap();
    let session_id = "older-worker-swap-failure";
    let (origin, clone) = init_managed_clone_with_origin(temp.path(), session_id);
    let worker_root = temp.path().join("workers").join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let target = mj_core::state::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    };
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "local-bare".into();
    session.target = Some(Box::new(target.clone()));
    attach_local_managed_clone(&mut session, clone.clone(), origin);
    let mut config = Config::default();
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.into(), session.clone())].into_iter().collect(),
            ..State::default()
        },
    };
    persist_controller_fixture(&mut controller);
    let previous = session;

    // Verify no leftover worker_restart intent remains after retiring on successful in-place recovery.
    let admission = crate::controller::test_support::session_admission(session_id);
    crate::worker_lifecycle::run_admitted(&admission, session_id, async {
        crate::database::begin_worker_restart(
            session_id,
            &crate::database::WorkerRestartIntent {
                operation_id: "swap-recovery-op".into(),
                target: target.clone(),
                desired_build: "test-build".into(),
            },
        )
        .unwrap();
        assert!(
            crate::database::load_worker_restart(session_id)
                .unwrap()
                .is_some()
        );
        retire_worker_restart_intent(session_id).unwrap();
        assert!(
            crate::database::load_worker_restart(session_id)
                .unwrap()
                .is_none(),
            "worker_restart intent must be cleared after successful in-place recovery"
        );
        anyhow::Ok(())
    })
    .await
    .unwrap();

    // Case A: recovery succeeded -> marker stripped -> session stays Running, checkout intact.
    let swap_error = anyhow::anyhow!("start replacement worker failed")
        .context(crate::controller::worker_restart::WorkerRestartLeftNoWorker);
    let recovered_error = anyhow::anyhow!("{swap_error:#}");
    let mut record_a = previous.clone();
    apply_close_checkpoint_failure(&mut record_a, &previous, &recovered_error, now(), None);
    assert_eq!(record_a.state, SessionState::Running);
    assert!(clone.exists());

    // Case B: recovery also failed -> WorkerRestartLeftNoWorker preserved -> Error with --without-checkpoint, checkout intact.
    let unrecovered_error = swap_error
        .context("in-place worker recovery also failed; retry suspend or use --without-checkpoint: spawn failed")
        .context(crate::controller::worker_restart::WorkerRestartLeftNoWorker);
    let mut record_b = previous.clone();
    apply_close_checkpoint_failure(&mut record_b, &previous, &unrecovered_error, now(), None);
    assert_eq!(record_b.state, SessionState::Error);
    assert!(
        record_b.last_error.as_deref().is_some_and(
            |msg| msg.contains("--without-checkpoint") && msg.contains("without a live worker")
        ),
        "expected no-worker error mentioning --without-checkpoint, got: {:?}",
        record_b.last_error
    );
    assert!(clone.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_closing_with_idle_relay_enters_origin_delta_and_refuses_without_consent() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    const NAME: &str =
        "legacy_closing_with_idle_relay_enters_origin_delta_and_refuses_without_consent";
    const CHILD: &str = "MJ_TEST_LEGACY_CLOSING_IDLE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-closing-idle")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let (origin, clone) = init_managed_clone_with_origin(&data, LATCH_RELAY_SESSION);
    std::fs::write(clone.join("untracked.txt"), "pre-upgrade\n").unwrap();
    let (mut controller, channels, _handle) =
        close_cut_controller_with(SessionState::Closing, false).await;
    controller
        .config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let worker_root = data.join("workers").join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    {
        let session = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        session.state = SessionState::Closing;
        session.checkpoint = None;
        session.target_template_id = "local".into();
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));
        attach_local_managed_clone(session, clone.clone(), origin);
    }
    persist_controller_fixture(&mut controller);

    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let error = controller
        .recover_interrupted_close_managed(
            &admission,
            LATCH_RELAY_SESSION,
            &ProcessExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            Some(Box::pin(async { Ok(()) })),
        )
        .await
        .expect_err("legacy Closing with Idle relay and untracked file must refuse");
    channels.shutdown.shutdown().await.unwrap();
    assert!(
        format!("{error:#}").contains("suspend requested before the upgrade needs consent"),
        "expected pre-upgrade consent notice, got: {error:#}"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert!(clone.join("untracked.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_closing_with_running_relay_enters_origin_delta_and_refuses_without_consent() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    const NAME: &str =
        "legacy_closing_with_running_relay_enters_origin_delta_and_refuses_without_consent";
    const CHILD: &str = "MJ_TEST_LEGACY_CLOSING_RUNNING";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-closing-running")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let (origin, clone) = init_managed_clone_with_origin(&data, LATCH_RELAY_SESSION);
    std::fs::write(clone.join("untracked.txt"), "pre-upgrade\n").unwrap();
    let (mut controller, channels, _handle) =
        close_cut_controller_with(SessionState::Closing, true).await;
    controller
        .config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let worker_root = data.join("workers").join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&worker_root).unwrap();
    {
        let session = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        session.state = SessionState::Closing;
        session.checkpoint = None;
        session.target_template_id = "local".into();
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));
        attach_local_managed_clone(session, clone.clone(), origin);
    }
    persist_controller_fixture(&mut controller);

    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let error = controller
        .recover_interrupted_close_managed(
            &admission,
            LATCH_RELAY_SESSION,
            &ProcessExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            Some(Box::pin(async { Ok(()) })),
        )
        .await
        .expect_err("legacy Closing with Running relay and untracked file must refuse");
    channels.shutdown.shutdown().await.unwrap();
    assert!(
        format!("{error:#}").contains("suspend requested before the upgrade needs consent"),
        "expected pre-upgrade consent notice, got: {error:#}"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert!(clone.join("untracked.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_closing_with_closed_relay_finishes_from_installed_checkpoint() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    const NAME: &str = "legacy_closing_with_closed_relay_finishes_from_installed_checkpoint";
    const CHILD: &str = "MJ_TEST_LEGACY_CLOSING_CLOSED";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-closing-closed")
        .env("MJ_TEST_LATCH_JOURNALS_BEFORE_CLOSE", "notification")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let (mut controller, channels, handle) =
        close_cut_controller_with(SessionState::Closing, false).await;
    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    // Seal the relay to Closed before recovery, mimicking a previous-release close after seal.
    {
        let mut lease = handle.lease_connection(&admission).await.unwrap();
        let barrier_id = new_command_id("legacy-seal").unwrap();
        lease
            .connection_mut()
            .submit(
                barrier_id.clone(),
                RelayCommand::BeginCheckpoint { reason: None },
            )
            .await
            .unwrap();
        let expected = loop {
            let snapshot = lease.connection_mut().sync().await.unwrap();
            if let Some(cursor) = snapshot.operational.checkpoint_ready.clone() {
                break cursor;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        lease
            .connection_mut()
            .submit(
                new_command_id("legacy-close").unwrap(),
                RelayCommand::Close {
                    barrier_command_id: barrier_id.clone(),
                    expected,
                },
            )
            .await
            .unwrap();
        lease
            .connection_mut()
            .submit(
                new_command_id("legacy-complete").unwrap(),
                RelayCommand::CompleteCheckpoint {
                    barrier_command_id: barrier_id,
                },
            )
            .await
            .unwrap();
        wait_for_relay_closed(lease.connection_mut()).await.unwrap();
        lease.release();
    }
    let archive_path = data.join("legacy-closed.tar.zst");
    let verified = crate::controller::test_support::write_checkpoint_gate_archive(
        &archive_path,
        LATCH_RELAY_SESSION,
        1,
    );
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .checkpoint = Some(Box::new(verified));
    persist_controller_fixture(&mut controller);

    controller
        .recover_interrupted_close_managed(
            &admission,
            LATCH_RELAY_SESSION,
            &NoExportExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_closing_with_closing_relay_waits_and_finishes_from_installed_checkpoint() {
    use crate::controller::checkpoint::tests::{LATCH_RELAY_SESSION, close_cut_controller_with};
    const NAME: &str =
        "legacy_closing_with_closing_relay_waits_and_finishes_from_installed_checkpoint";
    const CHILD: &str = "MJ_TEST_LEGACY_CLOSING_CLOSING";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-closing-closing")
        .env("MJ_TEST_LATCH_JOURNALS_BEFORE_CLOSE", "notification")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let (mut controller, channels, handle) =
        close_cut_controller_with(SessionState::Closing, false).await;
    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    // Seal the relay (BeginCheckpoint + Close + CompleteCheckpoint) without waiting for Closed first.
    {
        let mut lease = handle.lease_connection(&admission).await.unwrap();
        let barrier_id = new_command_id("legacy-seal").unwrap();
        lease
            .connection_mut()
            .submit(
                barrier_id.clone(),
                RelayCommand::BeginCheckpoint { reason: None },
            )
            .await
            .unwrap();
        let expected = loop {
            let snapshot = lease.connection_mut().sync().await.unwrap();
            if let Some(cursor) = snapshot.operational.checkpoint_ready.clone() {
                break cursor;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        lease
            .connection_mut()
            .submit(
                new_command_id("legacy-close").unwrap(),
                RelayCommand::Close {
                    barrier_command_id: barrier_id.clone(),
                    expected,
                },
            )
            .await
            .unwrap();
        lease
            .connection_mut()
            .submit(
                new_command_id("legacy-complete").unwrap(),
                RelayCommand::CompleteCheckpoint {
                    barrier_command_id: barrier_id,
                },
            )
            .await
            .unwrap();
        lease.release();
    }
    let archive_path = data.join("legacy-closing.tar.zst");
    let verified = crate::controller::test_support::write_checkpoint_gate_archive(
        &archive_path,
        LATCH_RELAY_SESSION,
        1,
    );
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .checkpoint = Some(Box::new(verified));
    persist_controller_fixture(&mut controller);

    controller
        .recover_interrupted_close_managed(
            &admission,
            LATCH_RELAY_SESSION,
            &NoExportExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Stopped
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_destroying_finishes_from_installed_checkpoint_without_relay_or_export() {
    const NAME: &str =
        "legacy_destroying_finishes_from_installed_checkpoint_without_relay_or_export";
    const CHILD: &str = "MJ_TEST_LEGACY_DESTROYING";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-destroying")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let temp = tempfile::tempdir().unwrap();
    let session_id = "legacy-destroying";
    let worker_root = temp.path().join("workers").join(session_id);
    std::fs::create_dir_all(&worker_root).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Destroying;
    session.target_template_id = "local-bare".into();
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root,
    }));
    let archive_path = temp.path().join("legacy-destroying.tar.zst");
    let verified = crate::controller::test_support::write_checkpoint_gate_archive(
        &archive_path,
        session_id,
        1,
    );
    session.checkpoint = Some(Box::new(verified));
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
    persist_controller_fixture(&mut controller);

    // Empty session manager: Destroying must not query any relay.
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let admission = crate::controller::test_support::session_admission(session_id);
    controller
        .recover_interrupted_close_managed(
            &admission,
            session_id,
            &NoExportExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_closing_and_destroying_subagent_routes_through_archive_free_completion() {
    const NAME: &str =
        "legacy_closing_and_destroying_subagent_routes_through_archive_free_completion";
    const CHILD: &str = "MJ_TEST_LEGACY_SUBAGENT_CLOSE";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "legacy-subagent-close")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    for state in [SessionState::Closing, SessionState::Destroying] {
        let temp = tempfile::tempdir().unwrap();
        let session_id = match state {
            SessionState::Closing => "legacy-closing-subagent",
            _ => "legacy-destroying-subagent",
        };
        let worker_root = temp.path().join("workers").join(session_id);
        let mut controller = failed_subagent_controller(&worker_root, session_id);
        controller.state.sessions.get_mut(session_id).unwrap().state = state;
        controller
            .state
            .sessions
            .get_mut(session_id)
            .unwrap()
            .checkpoint = None;
        let mut relation = failed_subagent_relation(session_id);
        relation.request_key = format!("close-{session_id}");
        let mut parent = checkpoint_test_session(&relation.parent_session_id);
        parent.target_template_id = "local".into();
        crate::database::save_session(&parent).unwrap();
        crate::database::save_subagent_session(&controller.state.sessions[session_id], &relation)
            .unwrap();
        persist_controller_fixture(&mut controller);

        let admission = crate::controller::test_support::session_admission(session_id);
        controller
            .suspend_session_controlled(&admission, session_id, &ProcessExecutor)
            .await
            .unwrap();
        assert_eq!(
            controller.state.sessions[session_id].state,
            SessionState::Stopped
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_bare_origin_delta_quiescence_routes_all_wire_and_process_ops_through_target_locator() {
    use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest, WorkerIdentity};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type RecordedCommands = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    #[derive(Clone, Default)]
    struct TargetWireFake {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        commands: RecordedCommands,
        worker_alive: Arc<Mutex<bool>>,
    }

    impl CommandExecutor for TargetWireFake {
        fn execute(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.commands
                .lock()
                .unwrap()
                .push((command.program.clone(), command.args.clone()));
            let joined = command.args.join(" ");
            if joined.contains("remote.origin.url") {
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: b"https://example.com/repo.git\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            if joined.contains("identity-alive") {
                let alive = *self.worker_alive.lock().unwrap();
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: if alive {
                        b"alive\n".to_vec()
                    } else {
                        b"dead\n".to_vec()
                    },
                    stderr: Vec::new(),
                });
            }
            if joined.contains("quiesce") {
                // Extract --operation and --checkout and record request + receipt, then mark dead.
                let op = command
                    .args
                    .last()
                    .and_then(|s| s.split("'--operation' ").nth(1))
                    .and_then(|s| s.split_whitespace().next())
                    .map(|s| s.trim_matches('\''))
                    .unwrap_or("op")
                    .to_owned();
                let checkout = command
                    .args
                    .last()
                    .and_then(|s| s.split("'--checkout' ").nth(1))
                    .and_then(|s| s.split_whitespace().next())
                    .map(|s| s.trim_matches('\''))
                    .unwrap_or("/remote/clone")
                    .to_owned();
                let mut files = self.files.lock().unwrap();
                files.insert(
                    wire::REQUEST.to_owned(),
                    serde_json::to_vec(&SuspendRequest {
                        operation: op.clone(),
                        worker: WorkerIdentity {
                            pid: 4242,
                            start: 99,
                        },
                        checkout: PathBuf::from(checkout),
                        child_sessions: Vec::new(),
                    })
                    .unwrap(),
                );
                files.insert(
                    wire::RECEIPT.to_owned(),
                    serde_json::to_vec(&SuspendReceipt {
                        operation: op,
                        error: None,
                    })
                    .unwrap(),
                );
                *self.worker_alive.lock().unwrap() = false;
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if joined.contains("'rm'") && joined.contains("'-f'") {
                let mut files = self.files.lock().unwrap();
                for name in [
                    wire::ACCEPTED,
                    wire::REQUEST,
                    wire::RECEIPT,
                    wire::PROGRESS,
                    wire::RESTART,
                ] {
                    if joined.contains(name) {
                        files.remove(name);
                    }
                }
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            // Wire read/write via sh -c
            for name in [
                wire::ACCEPTED,
                wire::CAPABILITY,
                wire::REQUEST,
                wire::RECEIPT,
                wire::PROGRESS,
                wire::RESTART,
            ] {
                if joined.contains(name) {
                    if let Some(stdin) = command.sensitive_stdin() {
                        self.files
                            .lock()
                            .unwrap()
                            .insert(name.to_owned(), stdin.to_vec());
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                    }
                    if let Some(bytes) = self.files.lock().unwrap().get(name).cloned() {
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: bytes,
                            stderr: Vec::new(),
                        });
                    }
                    return Ok(mj_core::targets::CommandOutput {
                        status: 44,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
            }
            anyhow::bail!("unexpected command: {} {:?}", command.program, command.args)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    let session_id = "0123456789abcdef0123456789abcde1";
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "ssh-bare".into();
    session.target = Some(Box::new(TargetLocator::SshBare {
        host: "remote.example".into(),
        workspace: PathBuf::from("/remote/clones/ssh-origin-delta"),
        worker_id: Some(session_id.into()),
    }));
    session.project_directory = Some(PathBuf::from("/remote/clones/ssh-origin-delta"));
    session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        source_project_directory: PathBuf::from("/remote/repo"),
        source_repository: PathBuf::from("/remote/repo"),
        worktree_root: PathBuf::from("/remote/clones/ssh-origin-delta"),
        branch: "mj/ssh-origin-delta".into(),
        target: mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: "user@remote.example".into(),
            ssh_args: Vec::new(),
        },
        base_commit: None,
    }));
    let mut config = Config::default();
    config.targets.insert(
        "ssh-bare".into(),
        TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "remote.example".into(),
                user: Some("user".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let fake = TargetWireFake::default();
    *fake.worker_alive.lock().unwrap() = true;
    fake.files.lock().unwrap().insert(
        wire::CAPABILITY.to_owned(),
        serde_json::to_vec(&WorkerIdentity {
            pid: 4242,
            start: 99,
        })
        .unwrap(),
    );

    assert_eq!(
        controller
            .explicit_suspend_export_policy_with_executor(session_id, &fake)
            .unwrap(),
        CheckpointExportPolicy::OriginDelta
    );

    let mut attempt =
        origin_delta::OriginDeltaSuspend::load(&controller, session_id, &fake).unwrap();
    assert!(!attempt.accepted());
    assert!(attempt.worker_capable(&fake).unwrap());

    attempt.accept(&fake, false).unwrap();
    assert!(attempt.accepted());
    attempt.request_stop(&fake).unwrap();
    assert!(attempt.request_recorded());

    let receipt = attempt.wait_for_stop(&fake).await.unwrap();
    assert!(receipt.error.is_none());

    attempt
        .record_restart(&anyhow::anyhow!("cancelled"), &fake)
        .unwrap();
    assert_eq!(
        attempt.restart_reason(&fake).unwrap().as_deref(),
        Some("cancelled")
    );
    attempt.clear_after_restart(&fake).unwrap();

    // Every single command must have been dispatched via `ssh`, never host-local.
    let commands = fake.commands.lock().unwrap();
    assert!(!commands.is_empty());
    assert!(
        commands.iter().all(|(program, _)| program == "ssh"),
        "expected all commands via ssh, got: {commands:?}"
    );
    // CAPABILITY file must be preserved while the 5 suspend wire files are removed.
    let files = fake.files.lock().unwrap();
    assert!(files.contains_key(wire::CAPABILITY));
    assert!(!files.contains_key(wire::ACCEPTED));
    assert!(!files.contains_key(wire::REQUEST));
    assert!(!files.contains_key(wire::RECEIPT));
    assert!(!files.contains_key(wire::RESTART));
}

#[cfg(unix)]
#[tokio::test]
async fn podman_managed_workspace_origin_delta_and_cutoff_quiescence_routes_through_container_exec()
{
    use mj_core::worker_suspend::{self as wire, SuspendRequest, WorkerIdentity};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type RecordedCommands = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    #[derive(Clone, Default)]
    struct ContainerWireFake {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        commands: RecordedCommands,
        worker_alive: Arc<Mutex<bool>>,
        write_receipt_on_quiesce: bool,
    }

    impl CommandExecutor for ContainerWireFake {
        fn execute(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.commands
                .lock()
                .unwrap()
                .push((command.program.clone(), command.args.clone()));
            let joined = command.args.join(" ");
            if joined.contains("config --get remote.origin.url") {
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: b"https://example.com/app.git\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            if joined.contains("worker identity-alive") {
                let alive = *self.worker_alive.lock().unwrap();
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: if alive {
                        b"alive\n".to_vec()
                    } else {
                        b"dead\n".to_vec()
                    },
                    stderr: Vec::new(),
                });
            }
            if joined.contains("worker quiesce") {
                let op_idx = command
                    .args
                    .iter()
                    .position(|a| a == "--operation")
                    .unwrap();
                let op = command.args[op_idx + 1].clone();
                let checkout_idx = command.args.iter().position(|a| a == "--checkout").unwrap();
                let checkout = command.args[checkout_idx + 1].clone();
                assert_eq!(checkout, "/workspace/0123456789abcdef0123456789abcde2");
                let mut files = self.files.lock().unwrap();
                files.insert(
                    wire::REQUEST.to_owned(),
                    serde_json::to_vec(&SuspendRequest {
                        operation: op,
                        worker: WorkerIdentity { pid: 17, start: 55 },
                        checkout: PathBuf::from(checkout),
                        child_sessions: Vec::new(),
                    })
                    .unwrap(),
                );
                *self.worker_alive.lock().unwrap() = false;
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            for name in [
                wire::ACCEPTED,
                wire::CAPABILITY,
                wire::REQUEST,
                wire::RECEIPT,
                wire::PROGRESS,
                wire::RESTART,
            ] {
                if joined.contains(name) {
                    if let Some(stdin) = command.sensitive_stdin() {
                        self.files
                            .lock()
                            .unwrap()
                            .insert(name.to_owned(), stdin.to_vec());
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                    }
                    if let Some(bytes) = self.files.lock().unwrap().get(name).cloned() {
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: bytes,
                            stderr: Vec::new(),
                        });
                    }
                    return Ok(mj_core::targets::CommandOutput {
                        status: 44,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
            }
            let _ = self.write_receipt_on_quiesce;
            anyhow::bail!("unexpected command: {} {:?}", command.program, command.args)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    let session_id = "0123456789abcdef0123456789abcde2";
    let container_id = targets::resource_name(session_id).unwrap();
    let volume = format!("{container_id}-workspace");
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "local-podman".into();
    session.bundle_id = "bundle".into();
    session.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume { name: volume },
    }));
    session.container_workspace = Some(PathBuf::from(format!("/workspace/{session_id}")));
    let mut config = Config::default();
    config.targets.insert(
        "local-podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "agent-dev:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    config.bundles.insert(
        "bundle".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "app".into(),
                github: Some("owner/app".into()),
                destination: PathBuf::from("app"),
                ..Default::default()
            }],
        },
    );
    let controller = Controller {
        config: config.clone(),
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session.clone())].into_iter().collect(),
            ..State::default()
        },
    };

    let fake = ContainerWireFake::default();
    *fake.worker_alive.lock().unwrap() = true;
    fake.files.lock().unwrap().insert(
        wire::CAPABILITY.to_owned(),
        serde_json::to_vec(&WorkerIdentity { pid: 17, start: 55 }).unwrap(),
    );

    assert_eq!(
        controller
            .explicit_suspend_export_policy_with_executor(session_id, &fake)
            .unwrap(),
        CheckpointExportPolicy::OriginDelta
    );

    let mut attempt =
        origin_delta::OriginDeltaSuspend::load(&controller, session_id, &fake).unwrap();
    assert!(attempt.worker_capable(&fake).unwrap());
    attempt.accept(&fake, false).unwrap();
    attempt.request_stop(&fake).unwrap();
    // Worker exited without writing a receipt: wait_for_stop must report an error
    // rather than silently succeeding over potentially live writers.
    let receipt = attempt.wait_for_stop(&fake).await.unwrap();
    assert!(
        receipt
            .error
            .as_deref()
            .is_some_and(|msg| msg.contains("exited without confirming checkout quiescence")),
        "expected missing-receipt error, got: {receipt:?}"
    );

    let commands = fake.commands.lock().unwrap();
    assert!(!commands.is_empty());
    assert!(
        commands.iter().all(|(program, args)| program == "podman"
            && args.first().map(String::as_str) == Some("exec")),
        "expected all commands via podman exec, got: {commands:?}"
    );
    drop(commands);

    // History-cutoff container session selects OriginDeltaCutoff and still loads OriginDeltaSuspend.
    let mut cutoff_session = session;
    cutoff_session.checkout = Some(Box::new(mj_core::remote_git::ExactCheckout {
        repository_id: "app".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        branch: None,
        no_later_history: true,
    }));
    let cutoff_controller = Controller {
        config,
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), cutoff_session)].into_iter().collect(),
            ..State::default()
        },
    };
    assert_eq!(
        cutoff_controller
            .explicit_suspend_export_policy_with_executor(session_id, &fake)
            .unwrap(),
        CheckpointExportPolicy::OriginDeltaCutoff
    );
    let cutoff_attempt =
        origin_delta::OriginDeltaSuspend::load(&cutoff_controller, session_id, &fake).unwrap();
    assert!(cutoff_attempt.accepted());
}

#[cfg(unix)]
#[test]
fn suspend_wait_is_worker_owned_returns_true_without_io_for_unreachable_ssh_and_podman_targets() {
    let ssh_id = "0123456789abcdef0123456789abcde3";
    let mut ssh_session = checkpoint_test_session(ssh_id);
    ssh_session.state = SessionState::Closing;
    ssh_session.target_template_id = "ssh-bare".into();
    ssh_session.target = Some(Box::new(TargetLocator::SshBare {
        host: "192.0.2.1".into(),
        workspace: PathBuf::from("/unreachable/clone"),
        worker_id: Some(ssh_id.into()),
    }));
    ssh_session.project_directory = Some(PathBuf::from("/unreachable/clone"));
    ssh_session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        source_project_directory: PathBuf::from("/unreachable/repo"),
        source_repository: PathBuf::from("/unreachable/repo"),
        worktree_root: PathBuf::from("/unreachable/clone"),
        branch: "mj/test".into(),
        target: mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: "user@192.0.2.1".into(),
            ssh_args: Vec::new(),
        },
        base_commit: None,
    }));

    let podman_id = "0123456789abcdef0123456789abcde4";
    let container_id = targets::resource_name(podman_id).unwrap();
    let mut podman_session = checkpoint_test_session(podman_id);
    podman_session.state = SessionState::Closing;
    podman_session.target_template_id = "local-podman".into();
    podman_session.bundle_id = "bundle".into();
    podman_session.target = Some(Box::new(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: container_id.clone(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: format!("{container_id}-workspace"),
        },
    }));

    let mut config = Config::default();
    config.targets.insert(
        "ssh-bare".into(),
        TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "192.0.2.1".into(),
                user: Some("user".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    config.targets.insert(
        "local-podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "agent-dev:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
            },
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [
                (ssh_id.into(), ssh_id.into()),
                (podman_id.into(), podman_id.into()),
            ]
            .into_iter()
            .collect(),
            sessions: [
                (ssh_id.into(), ssh_session),
                (podman_id.into(), podman_session),
            ]
            .into_iter()
            .collect(),
            ..State::default()
        },
    };

    // Must decide from daemon state alone without touching unreachable SSH/Podman targets.
    assert!(controller.suspend_wait_is_worker_owned(ssh_id));
    assert!(controller.suspend_wait_is_worker_owned(podman_id));
}

#[cfg(unix)]
#[tokio::test]
async fn wait_for_stop_uses_uncancelled_executor_after_cancellation_on_non_local_target() {
    use mj_core::worker_suspend::{
        self as wire, SuspendProgress, SuspendReceipt, SuspendRequest, WorkerIdentity,
    };
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct CancelledExecutor;

    impl CommandExecutor for CancelledExecutor {
        fn execute(
            &self,
            _command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            anyhow::bail!("cancelled executor must not run target commands")
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn cancellation_requested(&self) -> bool {
            true
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            true
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    #[derive(Clone, Default)]
    struct UncancelledTargetFake {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        alive_polls: Arc<AtomicUsize>,
    }

    impl CommandExecutor for UncancelledTargetFake {
        fn execute(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> Result<mj_core::targets::CommandOutput> {
            let joined = command.args.join(" ");
            if joined.contains("identity-alive") {
                // Alive on first poll, dead on second poll.
                let poll = self.alive_polls.fetch_add(1, Ordering::SeqCst);
                return Ok(mj_core::targets::CommandOutput {
                    status: 0,
                    stdout: if poll == 0 {
                        b"alive\n".to_vec()
                    } else {
                        b"dead\n".to_vec()
                    },
                    stderr: Vec::new(),
                });
            }
            for name in [
                wire::ACCEPTED,
                wire::REQUEST,
                wire::RECEIPT,
                wire::PROGRESS,
                wire::RESTART,
            ] {
                if joined.contains(name) {
                    if let Some(stdin) = command.sensitive_stdin() {
                        self.files
                            .lock()
                            .unwrap()
                            .insert(name.to_owned(), stdin.to_vec());
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                    }
                    if let Some(bytes) = self.files.lock().unwrap().get(name).cloned() {
                        return Ok(mj_core::targets::CommandOutput {
                            status: 0,
                            stdout: bytes,
                            stderr: Vec::new(),
                        });
                    }
                    return Ok(mj_core::targets::CommandOutput {
                        status: 44,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
            }
            anyhow::bail!("unexpected command: {} {:?}", command.program, command.args)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<mj_core::targets::CommandOutput> {
            self.execute_without_deadline(command)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    let session_id = "0123456789abcdef0123456789abcde5";
    let mut session = checkpoint_test_session(session_id);
    session.target_template_id = "ssh-bare".into();
    session.target = Some(Box::new(TargetLocator::SshBare {
        host: "remote.example".into(),
        workspace: PathBuf::from("/remote/clones/cancel-test"),
        worker_id: Some(session_id.into()),
    }));
    session.project_directory = Some(PathBuf::from("/remote/clones/cancel-test"));
    session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        source_project_directory: PathBuf::from("/remote/repo"),
        source_repository: PathBuf::from("/remote/repo"),
        worktree_root: PathBuf::from("/remote/clones/cancel-test"),
        branch: "mj/cancel-test".into(),
        target: mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: "user@remote.example".into(),
            ssh_args: Vec::new(),
        },
        base_commit: None,
    }));
    let mut config = Config::default();
    config.targets.insert(
        "ssh-bare".into(),
        TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "remote.example".into(),
                user: Some("user".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let op = "origin-delta-suspend-cancel-1".to_owned();
    let target_fake = UncancelledTargetFake::default();
    {
        let mut files = target_fake.files.lock().unwrap();
        files.insert(wire::ACCEPTED.to_owned(), serde_json::to_vec(&op).unwrap());
        files.insert(
            wire::REQUEST.to_owned(),
            serde_json::to_vec(&SuspendRequest {
                operation: op.clone(),
                worker: WorkerIdentity {
                    pid: 9001,
                    start: 77,
                },
                checkout: PathBuf::from("/remote/clones/cancel-test"),
                child_sessions: Vec::new(),
            })
            .unwrap(),
        );
        files.insert(
            wire::PROGRESS.to_owned(),
            serde_json::to_vec(&SuspendProgress {
                operation: op.clone(),
                remaining_pids: vec![9002],
            })
            .unwrap(),
        );
        files.insert(
            wire::RECEIPT.to_owned(),
            serde_json::to_vec(&SuspendReceipt {
                operation: op,
                error: None,
            })
            .unwrap(),
        );
    }

    let attempt =
        origin_delta::OriginDeltaSuspend::load(&controller, session_id, &target_fake).unwrap();
    let receipt = attempt
        .wait_for_stop_with_uncancelled(&CancelledExecutor, &target_fake)
        .await
        .unwrap();
    assert!(receipt.error.is_none());
    assert_eq!(target_fake.alive_polls.load(Ordering::SeqCst), 2);
    assert_eq!(
        attempt.restart_reason(&target_fake).unwrap().as_deref(),
        Some("suspend was cancelled")
    );
}

#[cfg(unix)]
#[test]
fn remote_suspend_preflight_stages_current_worker_when_installed_worker_lacks_subcommand() {
    use std::sync::Mutex;

    const NAME: &str =
        "remote_suspend_preflight_stages_current_worker_when_installed_worker_lacks_subcommand";
    const CHILD: &str = "MJ_TEST_REMOTE_PREFLIGHT_STAGED_WORKER";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let worker_bin = directory.path().join("fake-current-worker");
        std::fs::write(
            &worker_bin,
            format!(
                "#!/bin/sh\n# {}\nexit 0\n",
                mj_core::worker_build::worker_build_stamp()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&worker_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            NAME,
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "remote-preflight-staged")
        .env("MJ_WORKER_BINARY", &worker_bin)
        .isolated_store(directory.path())
        .run();
        return;
    }

    #[derive(Default)]
    struct RemotePreflightStageFake {
        old_preflight_calls: Mutex<usize>,
        staged_preflight_binaries: Mutex<Vec<String>>,
        cleaned_paths: Mutex<Vec<String>>,
    }

    impl CommandExecutor for RemotePreflightStageFake {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let joined = command.args.join(" ");
            if joined.contains("remote.origin.url") {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"https://example.com/org/repo.git\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            if joined.contains("uname") {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"Linux x86_64\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            if command.purpose.starts_with("read suspend record ") {
                return Ok(CommandOutput {
                    status: 44,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if command.purpose == "discard private worker staging" {
                self.cleaned_paths.lock().unwrap().push(joined);
                return Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            let mut stdin_bytes = Vec::new();
            input.read_to_end(&mut stdin_bytes)?;
            let joined = command.args.join(" ");
            if command.purpose == "suspend preflight target" {
                let spec: mj_core::suspend_consent::WorkerSuspendPreflightSpec =
                    serde_json::from_slice(&stdin_bytes).unwrap();
                assert_eq!(spec.session_id, "remote-old-worker");
                if let Some(staged) = command
                    .args
                    .iter()
                    .find(|arg| arg.contains("hel.prepared-"))
                {
                    self.staged_preflight_binaries
                        .lock()
                        .unwrap()
                        .push(staged.clone());
                    let result = mj_core::suspend_consent::WorkerSuspendPreflightResult {
                        untracked: vec![mj_core::suspend_consent::UntrackedPath {
                            checkout: mj_core::suspend_consent::CheckoutKey::default(),
                            path: PathBuf::from("scratch.txt"),
                            bytes: 42,
                            identity: mj_core::suspend_consent::FileIdentity::new(
                                "deadbeef".into(),
                            ),
                        }],
                        ..Default::default()
                    };
                    return Ok(CommandOutput {
                        status: 0,
                        stdout: serde_json::to_vec(&result).unwrap(),
                        stderr: Vec::new(),
                    });
                }
                if joined.contains("/hel") {
                    *self.old_preflight_calls.lock().unwrap() += 1;
                    return Ok(CommandOutput {
                        status: 2,
                        stdout: Vec::new(),
                        stderr: b"error: unrecognized subcommand 'suspend-preflight'\n".to_vec(),
                    });
                }
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }
    }

    let session_id = "remote-old-worker";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Running;
    session.target_template_id = "ssh-dev".into();
    session.target = Some(Box::new(TargetLocator::SshBare {
        host: "example.test".into(),
        workspace: PathBuf::from("/home/dev/repo/.mj/clones/remote-old-worker"),
        worker_id: Some(session_id.into()),
    }));
    session.project_directory = Some(PathBuf::from("/home/dev/repo/.mj/clones/remote-old-worker"));
    session.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
        target: mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: "dev@example.test".into(),
            ssh_args: Vec::new(),
        },
        source_project_directory: PathBuf::from("/home/dev/repo"),
        source_repository: PathBuf::from("/home/dev/repo"),
        worktree_root: PathBuf::from("/home/dev/repo/.mj/clones/remote-old-worker"),
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        branch: "mj/remote-old-worker".into(),
        base_commit: None,
    }));
    let mut config = Config::default();
    config.targets.insert(
        "ssh-dev".into(),
        TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "example.test".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(session_id.into(), session_id.into())]
                .into_iter()
                .collect(),
            sessions: [(session_id.into(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let fake = RemotePreflightStageFake::default();
    let preflight = controller
        .suspend_preflight(session_id, false, false, &fake)
        .expect("preflight must succeed by staging current worker when installed worker lacks subcommand");
    assert_eq!(*fake.old_preflight_calls.lock().unwrap(), 1);
    let staged = fake.staged_preflight_binaries.lock().unwrap();
    assert_eq!(staged.len(), 1, "expected one staged preflight invocation");
    let staged_start = staged[0]
        .find("hel.prepared-")
        .expect("expected hel.prepared- in staged command");
    let staged_name: String = staged[0][staged_start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        .collect();
    assert!(staged_name.starts_with("hel.prepared-"));
    assert!(
        fake.cleaned_paths
            .lock()
            .unwrap()
            .iter()
            .any(|cmd| cmd.contains(&staged_name)),
        "expected drop guard to clean up staged binary {staged_name}"
    );
    assert_eq!(preflight.untracked.len(), 1);
    assert_eq!(preflight.untracked[0].path, PathBuf::from("scratch.txt"));
}

#[cfg(unix)]
#[test]
fn checkpoint_report_roots_follow_the_environment_binding_after_owned_child_registration() {
    const NAME: &str =
        "checkpoint_report_roots_follow_the_environment_binding_after_owned_child_registration";
    if !in_isolated_store(NAME) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let borrowed_id = "0123456789abcdef0123456789abcdef";
    let owned_id = "123456789abcdef0123456789abcdef0";
    let mut controller = failed_subagent_controller(&root.path().join(borrowed_id), borrowed_id);
    let borrowed_root = seed_child_reports(&mut controller, borrowed_id, root.path());
    let parent_id = controller.state.subagents[borrowed_id]
        .parent_session_id
        .clone();
    let mut relation = failed_subagent_relation(owned_id);
    relation.parent_session_id = parent_id.clone();
    relation.request_key = "own-environment-report".into();
    let mut owned = checkpoint_test_session(owned_id);
    let project = root.path().join("owned-project");
    std::fs::create_dir(&project).unwrap();
    owned.target_template_id = "local".into();
    owned.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.path().join(owned_id),
    }));
    owned.project_directory = Some(project);
    crate::database::register_subagent_with_startup(&owned, &relation, vec![], None, None, true)
        .unwrap();
    controller.state = crate::database::load_state().unwrap();
    let (backend, _) = controller.worker_placement(owned_id).unwrap();
    controller
        .prepare_subagent_report_dir(owned_id, &backend, &ProcessExecutor)
        .unwrap();
    let report = crate::database::load_subagent_report(owned_id)
        .unwrap()
        .report_dir
        .unwrap();
    let report = PathBuf::from(report);
    assert!(report.is_dir());
    assert_eq!(
        controller
            .checkpoint_report_directories(&parent_id)
            .unwrap(),
        [borrowed_root.join(borrowed_id)]
    );
    assert_eq!(
        controller.checkpoint_report_directories(owned_id).unwrap(),
        [report]
    );
    // Moving the owning child to a new workspace layout (as Resume does) refreshes
    // its persisted report_dir and launch env before staging worker files.
    let moved_project = root.path().join("resumed-owned-project");
    std::fs::create_dir(&moved_project).unwrap();
    let mut moved = controller.state.sessions[owned_id].clone();
    moved.project_directory = Some(moved_project.clone());
    crate::database::save_session(&moved).unwrap();
    controller.state = crate::database::load_state().unwrap();
    let (moved_backend, _) = controller.worker_placement(owned_id).unwrap();
    controller
        .prepare_subagent_report_dir(owned_id, &moved_backend, &ProcessExecutor)
        .unwrap();
    let moved_report = PathBuf::from(
        crate::database::load_subagent_report(owned_id)
            .unwrap()
            .report_dir
            .unwrap(),
    );
    assert!(moved_report.starts_with(moved_project.canonicalize().unwrap()));
    assert!(moved_report.is_dir());
    controller.config.profiles.insert(
        "codex".into(),
        mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            root.path().join("codex-profile"),
        ),
    );
    let (launch, _, _) = controller
        .session_launch_config(
            owned_id,
            &moved_backend,
            crate::controller::worker_binary::LaunchHistory::Stored,
        )
        .unwrap();
    assert_eq!(
        launch.environment[mj_core::subagent::CHILD_REPORT_DIR_ENV],
        moved_report.to_string_lossy()
    );
    crate::database::delete_session(borrowed_id).unwrap();
    controller.state = crate::database::load_state().unwrap();
    assert_eq!(
        controller
            .checkpoint_report_directories(&parent_id)
            .unwrap(),
        [borrowed_root.join(borrowed_id)],
        "the environment retains the borrower report after the borrower is stopped and removed"
    );
}
