use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Barrier, Mutex};

use anyhow::Result;

use crate::controller::test_support::{
    FIXTURE_FETCH_URL, FixtureRemoteExecutor, checkout_with_network_remote,
    checkpoint_test_session, committed_repository, managed_worktree_session, network_remote_for,
    raw_session_on, resume_compatibility_config, write_checkpoint_archive_with_native_state,
    write_checkpoint_gate_archive,
};
use crate::controller::{Controller, SessionResumeOptions};
use mj_checkpoint::archive::{GitCommandRunner, verify_archive_streaming};
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, HarnessProfile, ProjectBundle, ProjectRepository,
    TargetTemplate,
};
use mj_core::state::{SessionRecord, SessionState, State, TargetLocator};
use mj_transcript::projection::materialized_session_from_canonical;

use crate::targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};

use super::*;

/// A person choosing a container for a local session has to see what the
/// move does before it happens, and a person resuming the same session in
/// place must not be asked anything.
#[test]
fn a_local_checkout_resuming_into_a_container_preflights_its_conversion() {
    let (checkout, _remote_parent, remote) = checkout_with_network_remote();
    std::fs::write(checkout.path().join("untracked.txt"), "u".repeat(2048)).unwrap();
    let mut session = raw_session_on("local-bare", &checkout.path().to_string_lossy());
    session.checkpoint = Some(mj_core::state::CheckpointMetadata {
        archive_path: checkout.path().join("unused.hel.zip"),
        sha256: "a".repeat(64),
        created_at: "2026-09-14T00:00:00Z".into(),
        event_frontier: 3,
    });
    let session_id = session.id.clone();
    let controller = Controller {
        config: resume_compatibility_config(),
        state: State {
            sessions: BTreeMap::from([(session_id.clone(), session)]),
            ..State::default()
        },
    };
    let executor = FixtureRemoteExecutor { remote };

    let converting = controller
        .preflight_resume_repository_sources(&session_id, "podman", &executor)
        .unwrap();
    let ResumeRepositorySourcePreflight::ConvertingRawCheckout { receipt, preview } = converting
    else {
        panic!("a container destination converts the checkout, got {converting:?}");
    };
    assert_eq!(receipt.session_id, session_id);
    assert_eq!(preview.fetch_url, FIXTURE_FETCH_URL);
    assert_eq!(preview.untracked_files, 1);
    assert!(preview.host_checkout_retained);

    assert!(
        matches!(
            controller
                .preflight_resume_repository_sources(&session_id, "local-bare", &executor)
                .unwrap(),
            ResumeRepositorySourcePreflight::Ready(_)
        ),
        "resuming in place asks nothing"
    );
}

const RESUME_ROLLBACK_TEST_CHILD: &str = "MJ_RESUME_ROLLBACK_TEST_CHILD";
const RETIRED_WORKTREE_RESUME_TEST_CHILD: &str = "MJ_RETIRED_WORKTREE_RESUME_TEST_CHILD";
const WORKER_PREFLIGHT_TEST_CHILD: &str = "MJ_WORKER_PREFLIGHT_TEST_CHILD";

#[test]
fn muse_resume_allows_workspace_relocation_before_provisioning() {
    let mut config = resume_compatibility_config();
    config
        .targets
        .insert("other-container".into(), config.targets["podman"].clone());
    let controller = Controller {
        config,
        state: State::default(),
    };
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.harness_kind = HarnessKind::Muse;
    assert!(
        controller
            .validate_muse_resume_destination(&session, HarnessKind::Muse, "podman")
            .is_ok()
    );
    assert!(
        controller
            .validate_muse_resume_destination(&session, HarnessKind::Muse, "other-container")
            .is_ok()
    );
    controller
        .validate_muse_resume_destination(&session, HarnessKind::Muse, "ssh-bare")
        .unwrap();
    assert!(
        controller
            .validate_muse_resume_destination(&session, HarnessKind::Codex, "ssh-bare")
            .is_ok()
    );
    assert_eq!(session.state, SessionState::Running);
}

/// Compaction costs minutes and paid model requests; resolving the worker
/// binary is local and costs microseconds. A cross-harness resume that
/// cannot produce a worker must say so before it compacts anything.
#[test]
fn a_resume_preflights_the_worker_binary_before_compacting() {
    // MJ_WORKER_BINARY, MJ_DATA_DIR, and MJ_CONFIG_DIR are process-global,
    // so run the half that sets them in an exact child test.
    if std::env::var_os(WORKER_PREFLIGHT_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_resume_preflights_the_worker_binary_before_compacting",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(WORKER_PREFLIGHT_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            // Names a worker binary that is not there, which is how a
            // machine without an installed worker fails the same lookup.
            .env("MJ_WORKER_BINARY", directory.path().join("absent-worker"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated worker preflight test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let archive_directory = data_directory.join("archives");
    std::fs::create_dir_all(&archive_directory).unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);
    let repository = committed_repository();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.checkpoint = Some(checkpoint);

    let profile_home = data_directory.join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    let mut config = resume_compatibility_config();
    // The archive was written by Codex, so resuming onto Claude is a
    // cross-harness resume and would compact the transcript.
    config.profiles.insert(
        "claude".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    crate::database::save_state(&controller.state).unwrap();

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.resume_session_controlled(
            session_id,
            "claude",
            "local-bare",
            SessionResumeOptions {
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: false,
            },
            &ProcessExecutor,
        ))
        .unwrap_err();

    let detail = format!("{error:#}");
    assert!(
        detail.contains("preflight the worker binary before resuming"),
        "{detail}"
    );
    assert!(detail.contains("absent-worker"), "{detail}");
    assert!(
        !detail.contains("compact the cross-harness handoff transcript"),
        "compaction must not run for a resume that cannot install a worker: {detail}"
    );
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
}

#[test]
fn network_resume_ignores_host_history_but_an_explicit_raw_move_checks_it() {
    let directory = tempfile::tempdir().unwrap();
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.checkpoint = Some(
        super::super::test_support::write_network_checkpoint_archive(
            directory.path(),
            session_id,
            0,
        ),
    );
    let mut config = resume_compatibility_config();
    config.bundles.insert(
        "project".into(),
        super::super::test_support::local_bundle(repository.path()),
    );
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    assert!(matches!(
        controller
            .preflight_resume_repository_sources(session_id, "podman", &ProcessExecutor,)
            .unwrap(),
        ResumeRepositorySourcePreflight::Ready(_)
    ));
    let result = controller
        .preflight_resume_repository_sources(session_id, "local-bare", &ProcessExecutor)
        .unwrap();
    let ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) = result else {
        panic!("moving into a host checkout must detect its missing archive base");
    };
    assert_eq!(mismatch.missing_commit, "a".repeat(40));
    assert!(!repository.path().join(".mj/worktrees").exists());
}

#[test]
fn raw_in_place_preflight_does_not_require_its_synthetic_bundle() {
    struct UnusedExecutor;

    impl CommandExecutor for UnusedExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            panic!("raw in-place preflight ran {}", command.purpose);
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.checkpoint = Some(write_checkpoint_gate_archive(
        directory.path(),
        session_id,
        3,
    ));
    session.bundle_id = "remote-project-a66373eef659f856".into();
    session.target_template_id = "localhost".into();
    session.project_directory = Some("/mnt/optane/bifrost-fird".into());
    let controller = Controller {
        config: Config {
            targets: BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]),
            // The raw checkout is still usable even though its synthetic
            // grouping bundle has disappeared from the config.
            bundles: BTreeMap::new(),
            ..Config::default()
        },
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };

    let preflight = controller
        .preflight_resume_repository_sources(session_id, "localhost", &UnusedExecutor)
        .unwrap();
    let ResumeRepositorySourcePreflight::Ready(receipt) = preflight else {
        panic!("raw in-place resume unexpectedly needs a repository replacement");
    };
    assert!(controller.repository_source_receipt_is_current(session_id, &receipt));
}

#[test]
fn repository_preflight_distinguishes_the_original_source_from_a_reused_name() {
    fn git(repository: &Path, arguments: &[&str]) {
        let output = SystemGit
            .run(
                repository,
                &mj_checkpoint::archive::GitCommand {
                    arguments: arguments.iter().map(std::ffi::OsString::from).collect(),
                    stdin: Vec::new(),
                    env: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("original");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["config", "user.name", "Hel Test"]);
    git(&origin, &["config", "user.email", "hel@example.test"]);
    git(&origin, &["commit", "--allow-empty", "-qm", "base"]);
    let source = directory.path().join("source");
    git(
        directory.path(),
        &["clone", "-q", origin.to_str().unwrap(), "source"],
    );
    git(&source, &["config", "user.name", "Hel Test"]);
    git(&source, &["config", "user.email", "hel@example.test"]);
    git(&source, &["commit", "--allow-empty", "-qm", "session"]);
    let snapshot = mj_checkpoint::archive::collect_git_snapshot(
        &SystemGit,
        &source,
        &mj_checkpoint::archive::GitCollectionSpec {
            id: "project".into(),
            relative_destination: "project".into(),
            history: mj_checkpoint::archive::GitHistoryMode::SessionDelta,
            origin_override: None,
        },
    )
    .unwrap();
    let configured = ProjectRepository {
        id: "project".into(),
        github: None,
        local: Some(origin.clone()),
        destination: "project".into(),
        git_ref: None,
    };
    assert_eq!(
        checkpoint_source_missing_commit(
            &configured,
            &CheckpointRepositoryBundle {
                metadata: snapshot.metadata.clone(),
                committed_bundle: snapshot.committed_bundle.clone(),
            },
            &ProcessExecutor,
            None,
        )
        .unwrap(),
        None
    );

    let replacement = directory.path().join("replacement");
    std::fs::create_dir(&replacement).unwrap();
    git(&replacement, &["init", "-q", "-b", "main"]);
    git(&replacement, &["config", "user.name", "Hel Test"]);
    git(&replacement, &["config", "user.email", "hel@example.test"]);
    git(
        &replacement,
        &["commit", "--allow-empty", "-qm", "different history"],
    );
    let configured = ProjectRepository {
        local: Some(replacement),
        ..configured
    };
    assert!(
        checkpoint_source_missing_commit(
            &configured,
            &CheckpointRepositoryBundle {
                metadata: snapshot.metadata,
                committed_bundle: snapshot.committed_bundle,
            },
            &ProcessExecutor,
            None,
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn repository_preflight_checks_independent_sources_concurrently_and_receipts_are_scoped() {
    struct ConcurrentSourceExecutor {
        source_checks: Barrier,
    }

    impl CommandExecutor for ConcurrentSourceExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.purpose == "check checkpoint base commit" {
                self.source_checks.wait();
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 3);
    let repositories = ["one", "two"]
        .map(|id| ProjectRepository {
            id: id.into(),
            github: None,
            local: Some(PathBuf::from(format!("/origin/{id}"))),
            destination: id.into(),
            git_ref: None,
        })
        .to_vec();
    let mut session = checkpoint_test_session(session_id);
    session.checkpoint = Some(checkpoint.clone());
    let mut controller = Controller {
        config: Config {
            bundles: BTreeMap::from([(
                session.bundle_id.clone(),
                ProjectBundle {
                    primary_repo: "one".into(),
                    repositories: repositories.clone(),
                },
            )]),
            ..Config::default()
        },
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    let verified = ResumeRepositoryBundles {
        checkpoint_sha256: checkpoint.sha256,
        repositories: repositories
            .iter()
            .map(|repository| CheckpointRepositoryBundle {
                metadata: mj_checkpoint::archive::RepositoryMetadata {
                    push_urls: Vec::new(),
                    remote_workspace: false,
                    id: repository.id.clone(),
                    relative_destination: repository.destination.clone(),
                    origin: repository.source_label(),
                    base_commit: String::new(),
                    head_commit: if repository.id == "one" {
                        "a".repeat(40)
                    } else {
                        "b".repeat(40)
                    },
                    branch: Some("main".into()),
                },
                committed_bundle: Vec::new(),
            })
            .collect(),
    };
    let executor = ConcurrentSourceExecutor {
        source_checks: Barrier::new(2),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let preflight = pool
        .install(|| {
            controller
                .preflight_verified_repository_sources(session_id, verified, None, false, &executor)
        })
        .unwrap();
    let ResumeRepositorySourcePreflight::Ready(receipt) = preflight else {
        panic!("expected repository source receipt");
    };
    assert!(controller.repository_source_receipt_is_current(session_id, &receipt));

    controller
        .config
        .bundles
        .values_mut()
        .next()
        .unwrap()
        .repositories[0]
        .local = Some(PathBuf::from("/different-origin"));
    assert!(!controller.repository_source_receipt_is_current(session_id, &receipt));
}

#[test]
fn repository_preflight_checks_declared_boundary_without_importing_delta_bundle() {
    struct RecordingExecutor {
        commands: Mutex<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let prerequisite = "a".repeat(40);
    let head = "b".repeat(40);
    let archived = CheckpointRepositoryBundle {
        metadata: mj_checkpoint::archive::RepositoryMetadata {
            push_urls: Vec::new(),
            remote_workspace: false,
            id: "project".into(),
            relative_destination: "project".into(),
            origin: "https://github.com/archived/should-not-be-contacted.git".into(),
            base_commit: prerequisite.clone(),
            head_commit: head.clone(),
            branch: Some("main".into()),
        },
        committed_bundle: format!(
            "# v2 git bundle\n-{prerequisite} base\n{head} HEAD\n\nPACKnot-read"
        )
        .into_bytes(),
    };
    let configured = ProjectRepository {
        id: "project".into(),
        github: Some("configured/project".into()),
        local: None,
        destination: "project".into(),
        git_ref: None,
    };
    let executor = RecordingExecutor {
        commands: Mutex::new(Vec::new()),
    };

    assert_eq!(
        checkpoint_source_missing_commit(&configured, &archived, &executor, Some("secret-token"))
            .unwrap(),
        None
    );

    let commands = executor.commands.into_inner().unwrap();
    assert_eq!(commands.len(), 2, "commands: {commands:?}");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.purpose.as_str())
            .collect::<Vec<_>>(),
        [
            "initialize repository source preflight",
            "check checkpoint base commit"
        ]
    );
    let source_check = &commands[1];
    assert!(
        source_check
            .args
            .iter()
            .any(|argument| argument == "credential.helper=")
    );
    assert_eq!(
        source_check
            .env
            .get("GIT_NO_LAZY_FETCH")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        source_check
            .env
            .get("GIT_TERMINAL_PROMPT")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(
        source_check.args.last().map(String::as_str),
        Some(prerequisite.as_str())
    );
    assert!(
        !source_check
            .args
            .iter()
            .any(|argument| argument.contains("archived"))
    );
}

#[test]
fn self_contained_bundle_validation_cannot_lazy_fetch_or_prompt() {
    let command = checkpoint_bundle_import_command(
        Path::new("/tmp/repository.git"),
        Path::new("/tmp/checkpoint.bundle"),
    );
    assert_eq!(
        command.env.get("GIT_NO_LAZY_FETCH").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        command.env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
        Some("0")
    );
}

#[test]
fn lost_bundle_sessions_reach_resume_compatibility_before_the_record_changes() {
    struct UnusedExecutor;

    impl CommandExecutor for UnusedExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            panic!("resume ran {} before rejecting the target", command.program);
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 3);
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Lost;
    session.checkpoint = Some(checkpoint);
    let previous = session.clone();
    let profile_home = directory.path().join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.resume_session_controlled(
            session_id,
            "codex",
            "localhost",
            SessionResumeOptions {
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: false,
            },
            &UnusedExecutor,
        ))
        .unwrap_err();

    let detail = format!("{error:#}");
    assert!(detail.contains("created from a project bundle"), "{detail}");
    assert!(
        detail.contains("resume it on a container, SSH, or EC2 target"),
        "{detail}"
    );
    assert_eq!(controller.state.sessions[session_id], previous);
}
/// Records what it ran and blocks every command on a barrier sized to
/// both lanes, so a run only finishes if the second lane started before
/// the first one's command returned.
struct BarrierExecutor {
    seen: Mutex<Vec<String>>,
    barrier: Barrier,
}

impl CommandExecutor for BarrierExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.seen.lock().unwrap().push(command.purpose.clone());
        self.barrier.wait();
        Ok(CommandOutput {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

fn lane_command(purpose: &str) -> CommandSpec {
    CommandSpec::new("hel", ["worker"]).purpose(purpose)
}

/// Launch progress must not claim "Start" while the target is still
/// receiving the worker binary, the checkpoint archive and the restore.
/// Everything before the daemon launch reports as Sync; the launch itself
/// names its own stage, so a Sync-labelled executor cannot relabel it.
#[test]
fn start_begins_at_the_worker_launch_not_at_the_transfers_before_it() {
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

    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = format!("/var/lib/hel/workers/{session_id}");
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let syncing = StagedExecutor::new(&executor, ProvisionStage::Syncing);
    let backend = targets::TargetLocator::LocalPodman {
        container_id: "abcdef0123456789".into(),
        workspace_storage: Default::default(),
    };

    upload_checkpoint_spec(
        &syncing,
        &backend,
        session_id,
        Path::new("/archives/session.hel.zip"),
        &format!("{worker_root}/restore.hel.zip"),
    )
    .unwrap();
    execute_checked(
        &syncing,
        restore_command(
            &backend,
            session_id,
            &format!("{worker_root}/restore-spec.json"),
        )
        .unwrap(),
    )
    .unwrap();
    // Deliberately run the launch through the Sync-labelled executor: it
    // must still report Start.
    start_worker(&syncing, &backend, &worker_root).unwrap();

    let stages = executor
        .commands
        .borrow()
        .iter()
        .map(|command| (command.purpose.clone(), command.stage))
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        vec![
            (
                "upload checkpoint specification".to_owned(),
                Some(ProvisionStage::Syncing)
            ),
            (
                "restore target checkpoint".to_owned(),
                Some(ProvisionStage::Syncing)
            ),
            (
                "start detached Mjolnir worker".to_owned(),
                Some(ProvisionStage::Starting)
            ),
        ]
    );
}
#[test]
fn independent_target_lanes_run_at_the_same_time() {
    let executor = BarrierExecutor {
        seen: Mutex::new(Vec::new()),
        barrier: Barrier::new(2),
    };

    execute_concurrent_lanes(
        || execute_checked(&executor, lane_command("install the worker")).map(|_| ()),
        || execute_checked(&executor, lane_command("upload the checkpoint")).map(|_| ()),
    )
    .unwrap();

    let mut seen = executor.seen.into_inner().unwrap();
    seen.sort();
    assert_eq!(seen, ["install the worker", "upload the checkpoint"]);
}
#[test]
fn a_lane_failure_is_reported_in_lane_order_and_never_abandons_the_other_lane() {
    let reached = Mutex::new(Vec::new());

    // The first lane fails slowly and the second immediately, so a
    // completion-order report could only pick the second.
    let error = execute_concurrent_lanes(
        || -> Result<()> {
            std::thread::sleep(Duration::from_millis(50));
            bail!("worker install failed")
        },
        || -> Result<()> {
            reached.lock().unwrap().push("second");
            bail!("checkpoint upload failed")
        },
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "worker install failed");
    assert_eq!(
        *reached.lock().unwrap(),
        ["second"],
        "a failing first lane must not cut the second one short"
    );

    let error = execute_concurrent_lanes(
        || Ok(()),
        || -> Result<()> { bail!("checkpoint upload failed") },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "checkpoint upload failed");
}

#[test]
fn cross_harness_lanes_prove_overlap_with_handshake_channels() {
    let (provision_started_tx, provision_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (handoff_started_tx, handoff_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (provision_seen_handoff_tx, provision_seen_handoff_rx) =
        std::sync::mpsc::sync_channel::<()>(1);
    let (handoff_seen_provision_tx, handoff_seen_provision_rx) =
        std::sync::mpsc::sync_channel::<()>(1);

    execute_joined_cross_harness_work(
        "provision",
        move |_cancellation| -> Result<()> {
            provision_started_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal provisioning start: {error}"))?;
            handoff_started_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| anyhow::anyhow!("wait for handoff start: {error}"))?;
            provision_seen_handoff_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal provisioning overlap: {error}"))?;
            Ok(())
        },
        "handoff",
        move |_cancellation| -> Result<()> {
            handoff_started_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal handoff start: {error}"))?;
            provision_started_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| anyhow::anyhow!("wait for provisioning start: {error}"))?;
            handoff_seen_provision_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal handoff overlap: {error}"))?;
            Ok(())
        },
    )
    .unwrap();

    assert!(provision_seen_handoff_rx.recv().is_ok());
    assert!(handoff_seen_provision_rx.recv().is_ok());
}

#[test]
fn cross_harness_lane_failure_cancels_and_joins_the_peer() {
    let (handoff_started_tx, handoff_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (handoff_joined_tx, handoff_joined_rx) = std::sync::mpsc::sync_channel::<()>(1);

    let error = execute_joined_cross_harness_work(
        "provision",
        move |_cancellation| -> Result<()> {
            handoff_started_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| anyhow::anyhow!("wait for handoff start: {error}"))?;
            bail!("provisioning failed after handoff started");
        },
        "handoff",
        move |cancellation| -> Result<()> {
            handoff_started_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal handoff start: {error}"))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow::anyhow!("create cancellation test runtime: {error}"))?;
            runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled()).await
                })
                .map_err(|error| anyhow::anyhow!("peer was not cancelled: {error}"))?;
            handoff_joined_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("signal handoff join: {error}"))?;
            Ok(())
        },
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "provisioning failed after handoff started"
    );
    assert!(handoff_joined_rx.recv().is_ok());
}

#[test]
fn a_projection_standing_at_the_archived_frontier_is_reused() {
    let digest = "a".repeat(64);
    let other = "b".repeat(64);

    assert!(!projection_rebuild_required(
        Some((82_000, &digest)),
        82_000,
        &digest
    ));

    for stored in [
        // Same ordinal, different event chain.
        Some((82_000, other.as_str())),
        // Behind the archive, and ahead of it.
        Some((81_999, digest.as_str())),
        Some((82_001, digest.as_str())),
        // No projection stored, or none that could be read.
        None,
    ] {
        assert!(
            projection_rebuild_required(stored, 82_000, &digest),
            "{stored:?} must not be mistaken for the archived projection"
        );
    }
}

#[test]
fn local_bare_restore_reuses_verified_absolute_archive_without_upload() {
    let archive = Path::new("/var/lib/hel/archives/session.hel.zip");
    let remote = Path::new("/var/lib/hel/workers/session/restore.hel.zip");
    let local = targets::TargetLocator::LocalBare {
        worker_root: "/var/lib/hel/workers/session".into(),
    };
    let container = targets::TargetLocator::LocalPodman {
        container_id: "container".into(),
        workspace_storage: Default::default(),
    };

    assert_eq!(restore_archive_path(&local, archive, remote), archive);
    assert!(!should_upload_restore_archive(&local));
    assert_eq!(restore_archive_path(&container, archive, remote), remote);
    assert!(should_upload_restore_archive(&container));
}

#[test]
fn cross_harness_provision_cancellation_stops_the_next_command() {
    struct RecordingExecutor {
        commands: Mutex<Vec<String>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.purpose.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let inner = RecordingExecutor {
        commands: Mutex::new(Vec::new()),
    };
    let cancellation = CancellationToken::new();
    let provision = CrossHarnessProvisionExecutor {
        inner: &inner,
        cancellation: cancellation.clone(),
    };
    let command = CommandSpec::new("hel", ["worker"]).purpose("provision target");
    provision.execute(&command).unwrap();
    cancellation.cancel();

    let error = provision.execute(&command).unwrap_err();
    assert!(error.to_string().contains("cancelled while provisioning"));
    assert_eq!(
        inner.commands.lock().unwrap().as_slice(),
        ["provision target"]
    );
}

#[test]
fn failed_resume_rolls_back_only_after_target_cleanup() {
    let previous = SessionRecord {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: "0123456789abcdef0123456789abcdef".into(),
        title: "imported session".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex-old".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman-old".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Stopped,
        target: None,
        native_session_id: Some("native-session".into()),
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    };
    let partial_target = TargetLocator::LocalPodman {
        container_id: "partial-container".into(),
        workspace_storage: Default::default(),
    };
    let mut cleaned = previous.clone();
    cleaned.state = SessionState::Error;
    cleaned.last_profile = "codex-new".into();
    cleaned.target = Some(partial_target.clone());

    let failure =
        apply_failed_resume_rollback(&mut cleaned, &previous, "worker upload failed", None);

    assert_eq!(cleaned.state, SessionState::Stopped);
    assert_eq!(cleaned.last_profile, "codex-old");
    assert_eq!(cleaned.target, None);
    assert_eq!(failure.to_string(), "worker upload failed");
    assert_eq!(
        cleaned.last_error.as_deref(),
        Some("resume failed: worker upload failed")
    );

    let mut cleanup_failed = previous.clone();
    cleanup_failed.state = SessionState::Error;
    cleanup_failed.last_profile = "codex-new".into();
    cleanup_failed.target = Some(partial_target.clone());
    let partial_checkout = crate::controller::test_support::managed_raw_session(
        mj_core::state::ManagedWorktreeTarget::Local,
    );
    cleanup_failed.project_directory = partial_checkout.project_directory.clone();
    cleanup_failed.managed_worktree = partial_checkout.managed_worktree.clone();

    let failure = apply_failed_resume_rollback(
        &mut cleanup_failed,
        &previous,
        "worker upload failed",
        Some("podman rm failed".into()),
    );

    assert_eq!(cleanup_failed.state, SessionState::Error);
    assert_eq!(cleanup_failed.last_profile, "codex-new");
    assert_eq!(cleanup_failed.target, Some(partial_target));
    assert_eq!(
        cleanup_failed.project_directory,
        partial_checkout.project_directory
    );
    assert_eq!(
        cleanup_failed.managed_worktree,
        partial_checkout.managed_worktree
    );
    assert!(failure.to_string().contains("cleanup"));
}
#[test]
fn failed_worktree_cleanup_notice_names_mjolnir_and_the_recovery_command() {
    let notice = worktree_cleanup_notice(
        Path::new("/workspace/project"),
        &anyhow::anyhow!("permission denied"),
    );

    assert!(
        notice.starts_with(
            "Mjolnir could not remove the worktree at /workspace/project: permission denied."
        ),
        "{notice}"
    );
    assert!(
        notice.contains("`git worktree remove --force /workspace/project`"),
        "{notice}"
    );
    assert!(!notice.contains("Hel"), "{notice}");
}
#[test]
fn failed_resume_provisioning_preserves_checkpoint_and_projection_lineage() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(RESUME_ROLLBACK_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::failed_resume_provisioning_preserves_checkpoint_and_projection_lineage",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(RESUME_ROLLBACK_TEST_CHILD, "1")
            // A remote target needs a portable worker; any existing file
            // satisfies the preflight so the test reaches provisioning.
            .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
            .env("MJ_DATA_DIR", directory.path())
            .env("GH_TOKEN", "test-token")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated resume rollback test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    /// Provisioning runs after the resumed record is persisted, so the
    /// durable mounts read here are the ones resume just committed.
    #[derive(Default)]
    struct FailingPreflightExecutor {
        mounts_during_provisioning: Mutex<Option<Vec<AdditionalMount>>>,
    }

    impl CommandExecutor for FailingPreflightExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            // Provisioning probes the mount source's filesystem before it
            // builds the run arguments; a local disk keeps the overlay.
            if command.program == "stat" {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"ext4\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            assert_eq!(command.program, "podman");
            let mut observed = self.mounts_during_provisioning.lock().unwrap();
            if observed.is_none() {
                let durable = crate::database::load_state().unwrap();
                *observed = Some(
                    durable.sessions["0123456789abcdef0123456789abcdef"]
                        .additional_mounts
                        .clone(),
                );
            }
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"podman is temporarily unavailable".to_vec(),
            })
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let archive_directory = data_directory.join("archives");
    std::fs::create_dir_all(&archive_directory).unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = super::super::test_support::write_network_checkpoint_archive(
        &archive_directory,
        session_id,
        7,
    );
    let archive = verify_archive_streaming(&checkpoint.archive_path).unwrap();
    let expected_projection =
        materialized_session_from_canonical(session_id, &archive.canonical_session).unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.checkpoint = Some(checkpoint.clone());
    session.additional_mounts = vec![AdditionalMount {
        source: PathBuf::from("/host/old"),
        destination: PathBuf::from("/mnt/old"),
        read_only: false,
    }];
    let previous = session.clone();
    let resumed_mounts = vec![AdditionalMount {
        source: PathBuf::from("/host/new"),
        destination: PathBuf::from("/mnt/new"),
        read_only: false,
    }];
    let profile_home = data_directory.join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: None,
                local: Some(data_directory.join("host-clone-that-no-longer-exists")),
                destination: "project".into(),
                git_ref: None,
            }],
        },
    );
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "example.invalid/hel-test:latest".into(),
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
    crate::database::save_state(&controller.state).unwrap();
    crate::database::save_materialized_session(&expected_projection).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = FailingPreflightExecutor::default();
    let error = runtime
        .block_on(controller.resume_session_controlled(
            session_id,
            "codex",
            "podman",
            SessionResumeOptions {
                additional_mounts: Some(resumed_mounts.clone()),
                resource_allocation: None,
                discard_queue: false,
            },
            &executor,
        ))
        .unwrap_err();
    let detail = format!("{error:#}");
    assert!(
        detail.contains("podman is temporarily unavailable"),
        "{detail}"
    );
    assert!(!detail.contains("returned to stopped"), "{detail}");
    assert!(!detail.contains("unknown session"), "{detail}");
    assert_eq!(
        executor.mounts_during_provisioning.into_inner().unwrap(),
        Some(resumed_mounts)
    );

    let retained = controller.state.sessions.get(session_id).unwrap();
    assert_eq!(retained.state, SessionState::Stopped);
    assert_eq!(retained.checkpoint, previous.checkpoint);
    assert_eq!(retained.managed_worktree, previous.managed_worktree);
    assert!(checkpoint.archive_path.is_file());

    let durable = crate::database::load_state().unwrap();
    let durable_session = durable.sessions.get(session_id).unwrap();
    assert_eq!(durable_session.state, SessionState::Stopped);
    assert_eq!(durable_session.checkpoint, previous.checkpoint);
    assert_eq!(
        durable_session.additional_mounts,
        previous.additional_mounts
    );
    assert_eq!(
        crate::database::load_materialized_session(session_id).unwrap(),
        Some(expected_projection)
    );
}
#[test]
fn failed_resume_retires_a_checkout_it_recreated() {
    if std::env::var_os(RETIRED_WORKTREE_RESUME_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::failed_resume_retires_a_checkout_it_recreated",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(RETIRED_WORKTREE_RESUME_TEST_CHILD, "1")
            // A remote target needs a portable worker; any existing file
            // satisfies the preflight so the test reaches provisioning.
            .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated retired-worktree resume test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    struct FailAfterWorktreeRestore;

    impl CommandExecutor for FailAfterWorktreeRestore {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if matches!(command.program.as_str(), "git" | "mkdir") {
                return ProcessExecutor.execute(command);
            }
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"stop after recreating the checkout".to_vec(),
            })
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let archive_directory = data_directory.join("archives");
    std::fs::create_dir_all(&archive_directory).unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);
    let repository = committed_repository();
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.checkpoint = Some(checkpoint);
    let worktree = session.managed_worktree.clone().unwrap();
    retire_managed_worktree(&ProcessExecutor, &worktree).unwrap();
    assert!(!worktree.worktree_root.exists());

    let profile_home = data_directory.join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    let mut config = resume_compatibility_config();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    crate::database::save_state(&controller.state).unwrap();

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.resume_session_controlled(
            session_id,
            "codex",
            "local-bare",
            SessionResumeOptions {
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: false,
            },
            &FailAfterWorktreeRestore,
        ))
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("stop after recreating the checkout"),
        "{error:#}"
    );
    assert!(!worktree.worktree_root.exists());
    assert_eq!(
        controller.state.sessions[session_id].state,
        SessionState::Stopped
    );
    let branch = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args([
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", worktree.branch),
        ])
        .status()
        .unwrap();
    assert!(branch.success(), "resume rollback must retain the branch");
}
#[test]
fn a_conversion_archive_carries_the_checkouts_remote_and_the_conversation() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let previous = write_checkpoint_archive_with_native_state(directory.path(), session_id, 7);
    let (checkout, _remote_parent, _remote) = checkout_with_network_remote();
    let source =
        mj_core::remote_git::resolve_local_repository(checkout.path(), &ProcessExecutor).unwrap();
    let dirname = PathBuf::from(checkout.path().file_name().unwrap());
    let snapshot = raw_checkout_snapshot(checkout.path(), &source, &dirname, &SystemGit).unwrap();

    let output = directory.path().join("converted.hel.zip");
    let converted = conversion_checkpoint(&previous.archive_path, snapshot, &output).unwrap();

    // The archive provisioning will read: a real network clone of the
    // checkout's own remote, landing where the archive says.
    let verified = mj_checkpoint::archive::read_archive_verified(&output).unwrap();
    assert_eq!(converted.archive_path, output);
    assert_eq!(converted.sha256, verified.archive_sha256);
    assert_eq!(converted.event_frontier, previous.event_frontier);
    let bundle = crate::controller::network_git::bundle_from_manifest(&verified.manifest).unwrap();
    assert_eq!(bundle.primary, dirname.to_string_lossy());
    assert_eq!(bundle.repositories.len(), 1);
    assert_eq!(
        bundle.repositories[0].url.as_deref(),
        Some(FIXTURE_FETCH_URL)
    );
    assert_eq!(bundle.repositories[0].push_urls, [FIXTURE_FETCH_URL]);
    assert_eq!(
        bundle.repositories[0].destination,
        dirname.to_string_lossy()
    );

    // Everything the conversation is made of comes across untouched.
    let original = mj_checkpoint::archive::read_archive_verified(&previous.archive_path).unwrap();
    assert_eq!(
        verified.canonical_session().unwrap(),
        original.canonical_session().unwrap()
    );
    assert_eq!(verified.manifest.session, original.manifest.session);
    assert_eq!(native_state(&original), native_state(&verified));
    assert!(
        !native_state(&verified).is_empty(),
        "the fixture has native state"
    );
}

/// Every native payload of an archive as (path, mode, bytes).
fn native_state(archive: &mj_checkpoint::archive::VerifiedArchive) -> Vec<(PathBuf, u32, Vec<u8>)> {
    archive
        .manifest
        .payloads
        .iter()
        .filter_map(|descriptor| match &descriptor.role {
            mj_checkpoint::archive::PayloadRole::NativeArtifact { relative_path } => Some((
                relative_path.clone(),
                descriptor.mode,
                archive.payload(descriptor).unwrap().to_vec(),
            )),
            _ => None,
        })
        .collect()
}
const RAW_CONVERSION_TEST_CHILD: &str = "MJ_RAW_CONVERSION_TEST_CHILD";
#[test]
fn a_failed_raw_conversion_keeps_the_checkout_and_its_previous_checkpoint() {
    // MJ_DATA_DIR and MJ_CONFIG_DIR are process-global, so run the half
    // that writes them in an exact child test.
    if std::env::var_os(RAW_CONVERSION_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_failed_raw_conversion_keeps_the_checkout_and_its_previous_checkpoint",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(RAW_CONVERSION_TEST_CHILD, "1")
            // A remote target needs a portable worker; any existing file
            // satisfies the preflight so the test reaches provisioning.
            .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .env("GH_TOKEN", "test-token")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated raw conversion test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    /// Real Git, no container runtime. Provisioning fails at preflight,
    /// after the conversion has already reshaped the record.
    struct GitWithoutPodmanExecutor;

    impl CommandExecutor for GitWithoutPodmanExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.program == "git" {
                return ProcessExecutor.execute(command);
            }
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"podman is temporarily unavailable".to_vec(),
            })
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let archive_directory = data_directory.join("archives");
    std::fs::create_dir_all(&archive_directory).unwrap();
    std::fs::create_dir_all(mj_core::config::config_dir()).unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);

    let repository = committed_repository();
    // An isolated workspace is a clone of a network remote, so the
    // checkout that converts has to have one, with its base pushed.
    let (_remote_parent, _remote) = network_remote_for(repository.path());
    let mut session = managed_worktree_session(repository.path(), session_id);
    session.checkpoint = Some(checkpoint.clone());
    let worktree = session.managed_worktree.clone().unwrap();
    let previous = session.clone();

    let profile_home = data_directory.join("profile");
    std::fs::create_dir_all(&profile_home).unwrap();
    let mut config = resume_compatibility_config();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    // Production controllers read this configuration from disk; bundle
    // updates now deliberately reload it under the transaction lock.
    config.save().unwrap();
    let original_config = config.clone();
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    crate::database::save_state(&controller.state).unwrap();

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.resume_session_controlled(
            session_id,
            "codex",
            "podman",
            SessionResumeOptions {
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: false,
            },
            &GitWithoutPodmanExecutor,
        ))
        .unwrap_err();
    // The conversion ran: it wrote its archive and reshaped the record,
    // and then the destination could not be provisioned.
    assert!(
        format!("{error:#}").contains("podman is temporarily unavailable"),
        "{error:#}"
    );
    assert!(!format!("{error:#}").contains("returned to stopped"));

    // A conversion installs a bundle for the checkout it converts, and
    // reuses it on a retry. Nothing else about the configuration moves.
    let mut expected_config = original_config.clone();
    let (bundle_id, bundle) = mj_core::config::Config::load()
        .unwrap()
        .bundles
        .into_iter()
        .next()
        .expect("the conversion installed a bundle for the checkout");
    expected_config.bundles.insert(bundle_id, bundle);
    assert_eq!(
        controller.config,
        expected_config.clone().with_local_targets()
    );
    assert_eq!(
        mj_core::config::Config::load_from(&mj_core::config::config_path()).unwrap(),
        expected_config
    );

    let retained = controller.state.sessions.get(session_id).unwrap();
    assert_eq!(retained.state, SessionState::Stopped);
    assert_eq!(retained.checkpoint, Some(checkpoint.clone()));
    assert_eq!(retained.project_directory, previous.project_directory);
    assert_eq!(retained.managed_worktree, previous.managed_worktree);
    assert_eq!(retained.bundle_id, previous.bundle_id);
    assert!(worktree.worktree_root.is_dir(), "the checkout stays put");
    assert!(
        checkpoint.archive_path.is_file(),
        "the previous archive is what the rolled-back record names"
    );
    let durable = crate::database::load_state().unwrap();
    assert_eq!(durable.sessions[session_id].checkpoint, Some(checkpoint));
    // The conversion archive is litter once the resume has failed, and the
    // directory it was written in proves the conversion really ran.
    let sessions = mj_core::config::sessions_dir();
    assert!(sessions.is_dir(), "the conversion wrote an archive");
    let leftover: Vec<_> = std::fs::read_dir(&sessions)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().ends_with(".hel.zip"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "{leftover:?} in {}",
        sessions.display()
    );
}

/// Resuming into a different harness cannot reload the archived native
/// session, so the transcript is handed over as the first context instead.
#[test]
fn only_the_same_harness_keeps_native_continuity_on_resume() {
    use mj_core::config::HarnessKind;

    assert!(super::native_continuity_preserved(
        HarnessKind::Codex,
        HarnessKind::Codex
    ));
    assert!(super::native_continuity_preserved(
        HarnessKind::Claude,
        HarnessKind::Claude
    ));
    assert!(!super::native_continuity_preserved(
        HarnessKind::Claude,
        HarnessKind::Codex
    ));
}
