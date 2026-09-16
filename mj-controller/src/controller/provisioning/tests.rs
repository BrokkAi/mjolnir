use std::collections::BTreeMap;

use crate::controller::test_support::{IsolatedTest, RefusingExecutor, test_name};
use std::sync::Mutex;

use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, HarnessKind, HarnessProfile, ProjectBundle,
    ProjectRepository, SshConnection,
};
use mj_core::state::{SessionRecord, SessionState, State, TargetLocator};

use crate::targets::{self, AdditionalMount, ContainerTemplate, ProjectBundleSpec, SshTarget};

use crate::controller::SessionLaunchOptions;

use super::*;

/// Answers the filesystem probe, and records every notice provisioning
/// reported while it ran.
struct ProbeExecutor {
    answer: std::result::Result<&'static str, &'static str>,
    notices: Mutex<Vec<String>>,
}

impl ProbeExecutor {
    fn answering(answer: &'static str) -> Self {
        Self {
            answer: Ok(answer),
            notices: Mutex::new(Vec::new()),
        }
    }

    fn failing(stderr: &'static str) -> Self {
        Self {
            answer: Err(stderr),
            notices: Mutex::new(Vec::new()),
        }
    }
}

impl CommandExecutor for ProbeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        assert_eq!(command.program, "stat", "only the probe may run here");
        Ok(match self.answer {
            Ok(filesystem) => CommandOutput {
                status: 0,
                stdout: format!("{filesystem}\n").into_bytes(),
                stderr: Vec::new(),
            },
            Err(stderr) => CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            },
        })
    }

    fn notify_notice(&self, notice: &str) {
        self.notices.lock().unwrap().push(notice.to_owned());
    }
}

fn podman_target() -> targets::TargetTemplate {
    targets::TargetTemplate::LocalPodman(ContainerTemplate {
        image: "ubuntu:24.04".into(),
        pull_policy: Default::default(),
        extra_run_args: Vec::new(),
        workspace_storage: Default::default(),
    })
}

fn probe_bundle() -> ProjectBundleSpec {
    ProjectBundleSpec {
        primary: "app".into(),
        repositories: vec![crate::targets::RepositorySpec {
            url: Some("https://github.com/example/app.git".into()),
            push_urls: Vec::new(),
            destination: "app".into(),
            git_ref: None,
            reference: None,
        }],
    }
}

fn ssh_docker_registration_config() -> Config {
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: PathBuf::from("/home/dev/.codex"),
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
                github: Some("owner/project".into()),
                local: None,
                destination: PathBuf::from("project"),
                git_ref: None,
            }],
        },
    );
    config.targets.insert(
        "docker".into(),
        TargetTemplate::SshDocker {
            ssh: SshConnection {
                host: "builder".into(),
                user: Some("agent".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            container: ConfigContainer {
                image: "failimage:never".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    config
}

#[test]
fn a_source_that_cannot_overlay_is_mounted_read_only_and_reported() {
    let executor = ProbeExecutor::answering("nfs");
    let mut mounts = vec![AdditionalMount {
        source: PathBuf::from("/nfs/share"),
        destination: PathBuf::from("/mnt/share"),
        read_only: false,
    }];

    let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

    assert!(mounts[0].read_only);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains(
            "Mounted /nfs/share read-only: the overlay is unreliable on nfs (network filesystem)"
        ),
        "{notices:?}"
    );
    let plan = targets::provision_plan(
        &podman_target(),
        "0123456789abcdef0123456789abcdef",
        &probe_bundle(),
        &mounts,
    )
    .unwrap();
    assert!(
        plan.commands[0]
            .args
            .windows(2)
            .any(|args| args == ["--volume", "/nfs/share:/mnt/share:ro"]),
        "{:?}",
        plan.commands[0].args
    );
}

#[test]
fn a_probe_that_cannot_answer_keeps_the_overlay_and_says_so() {
    let executor = ProbeExecutor::failing("stat: cannot read file system information");
    let mut mounts = vec![AdditionalMount {
        source: PathBuf::from("/host/cache"),
        destination: PathBuf::from("/mnt/cache"),
        read_only: false,
    }];

    let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

    assert!(!mounts[0].read_only);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("keep the copy-on-write overlay")
            && notices[0].contains("cannot read file system information"),
        "{notices:?}"
    );
    let plan = targets::provision_plan(
        &podman_target(),
        "0123456789abcdef0123456789abcdef",
        &probe_bundle(),
        &mounts,
    )
    .unwrap();
    assert!(
        plan.commands[0]
            .args
            .windows(2)
            .any(|args| args == ["--volume", "/host/cache:/mnt/cache:O"]),
        "{:?}",
        plan.commands[0].args
    );
}

#[test]
fn engines_without_an_overlay_to_lose_are_never_probed() {
    let mut mounts = vec![AdditionalMount {
        source: PathBuf::from("/host/cache"),
        destination: PathBuf::from("/mnt/cache"),
        read_only: false,
    }];
    for target in [
        targets::TargetTemplate::AppleContainer(ContainerTemplate {
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        }),
        targets::TargetTemplate::AwsEc2(targets::AwsTemplate {
            profile: "default".into(),
            region: "us-east-1".into(),
            launch_template: "lt-0123456789abcdef0".into(),
            launch_template_version: None,
            instance_type: None,
            ssh: SshTarget {
                destination: "ubuntu@example.test".into(),
                ssh_args: Vec::new(),
            },
        }),
    ] {
        assert!(
            enforce_overlay_capable_mounts(
                &target,
                &mut mounts,
                &RefusingExecutor("a target that must not probe"),
            )
            .is_empty()
        );
        assert!(!mounts[0].read_only);
    }
}

/// A mount the user already marked read-only has no overlay to protect, so
/// the probe never has to reach a host that may not answer.
#[test]
fn mounts_already_read_only_are_not_probed() {
    let mut mounts = vec![AdditionalMount {
        source: PathBuf::from("/host/cache"),
        destination: PathBuf::from("/mnt/cache"),
        read_only: true,
    }];

    assert!(
        enforce_overlay_capable_mounts(
            &podman_target(),
            &mut mounts,
            &RefusingExecutor("a read-only mount"),
        )
        .is_empty()
    );
}

#[test]
fn failed_new_session_provisioning_retains_error_record() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let record = SessionRecord {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: session_id.into(),
        title: "new session".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Provisioning,
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
    };
    let mut state = State::default();
    state.sessions.insert(session_id.into(), record);

    let result = apply_new_session_provisioning_result(
        &mut state,
        session_id,
        Err(anyhow::anyhow!("container creation failed")),
    );

    assert!(result.is_err());
    let retained = &state.sessions[session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());
    assert!(
        retained
            .last_error
            .as_deref()
            .unwrap()
            .contains("container creation failed")
    );
}

const SSH_DOCKER_FAILURE_CHILD: &str = "MJ_TEST_SSH_DOCKER_FAILURE_CHILD";

#[test]
fn failed_ssh_docker_preflight_retains_durable_error_record() {
    if std::env::var_os(SSH_DOCKER_FAILURE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test = "failed_ssh_docker_preflight_retains_durable_error_record";
        IsolatedTest::new(test_name(module_path!(), test))
            .env(SSH_DOCKER_FAILURE_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .env("MJ_CONFIG_DIR", directory.path())
            .run();
        return;
    }

    let _writer = crate::database::install_isolated_test_writer();
    let config = ssh_docker_registration_config();
    config.save().unwrap();
    let mut controller = Controller {
        config,
        state: State::default(),
    };
    let session_id = controller
        .register_session_with_resources(
            "codex",
            "project",
            "docker",
            "failed image",
            SessionLaunchOptions {
                mjolnir_subagents: None,
                create_managed_worktree: None,
                initial_prompt: None,
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                additional_mounts: Vec::new(),
                resource_allocation: None,
                project_directory: None,
                session_title_override: None,
            },
        )
        .unwrap();
    assert!(
        crate::database::load_state()
            .unwrap()
            .sessions
            .contains_key(&session_id)
    );

    let executor = RecordingExecutor::failing("check Docker daemon");
    let error = futures::executor::block_on(controller.provision_session_with_failure_disposition(
        &session_id,
        &executor,
        None,
        ProvisioningFailureDisposition::Discard,
    ))
    .unwrap_err();
    let reported = format!("{error:#}");
    assert!(
        reported.contains("remote Docker preflight failed"),
        "{reported}"
    );
    assert!(
        executor.commands().iter().any(|argv| {
            let command = argv.join(" ");
            command.contains("'docker' 'version'")
        }),
        "the fake preflight did not run: {:?}",
        executor.commands()
    );
    let retained = &controller.state.sessions[&session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());

    let reloaded = Controller::load().unwrap();
    let retained = &reloaded.state.sessions[&session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());
    assert!(retained.last_error.is_some());
}

#[test]
fn failed_node_preflight_retains_error_before_provisioning() {
    if std::env::var_os(SSH_DOCKER_FAILURE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test = "failed_node_preflight_retains_error_before_provisioning";
        IsolatedTest::new(test_name(module_path!(), test))
            .env(SSH_DOCKER_FAILURE_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .env("MJ_CONFIG_DIR", directory.path())
            .run();
        return;
    }

    let _writer = crate::database::install_isolated_test_writer();
    let mut config = ssh_docker_registration_config();
    config.targets.insert(
        "docker".into(),
        TargetTemplate::SshBare {
            ssh: SshConnection {
                host: "builder".into(),
                user: Some("agent".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Guardian,
            workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
        },
    );
    config.save().unwrap();
    let mut controller = Controller {
        config,
        state: State::default(),
    };
    let session_id = controller
        .register_session_with_resources(
            "codex",
            "project",
            "docker",
            "missing Node",
            SessionLaunchOptions {
                mjolnir_subagents: None,
                create_managed_worktree: None,
                initial_prompt: None,
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                additional_mounts: Vec::new(),
                resource_allocation: None,
                project_directory: Some("/srv/project".into()),
                session_title_override: None,
            },
        )
        .unwrap();
    assert!(
        crate::database::load_state()
            .unwrap()
            .sessions
            .contains_key(&session_id)
    );

    let executor = RecordingExecutor::failing("preflight managed harness Node.js and npm");
    let error = futures::executor::block_on(controller.provision_session_with_failure_disposition(
        &session_id,
        &executor,
        None,
        ProvisioningFailureDisposition::Discard,
    ))
    .unwrap_err();
    let reported = format!("{error:#}");
    assert!(reported.contains("Node.js 22+ and npm"), "{reported}");
    assert_eq!(
        executor.commands().len(),
        1,
        "preflight must fail before provisioning"
    );
    let retained = &controller.state.sessions[&session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());

    let reloaded = Controller::load().unwrap();
    let retained = &reloaded.state.sessions[&session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());
    assert!(retained.last_error.is_some());
}

#[test]
fn failed_new_worker_start_retains_session_only_after_target_cleanup() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = SessionRecord {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: session_id.into(),
        title: "new session".into(),
        harness_kind: mj_core::config::HarnessKind::Kimi,
        last_profile: "kimi".into(),
        bundle_id: "raw-project".into(),
        project_directory: Some("/srv/project".into()),
        managed_worktree: None,
        target_template_id: "remote".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Disconnected,
        target: Some(TargetLocator::SshBare {
            host: "builder".into(),
            workspace: format!(".local/share/hel/workspaces/{session_id}").into(),
            worker_id: None,
        }),
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
    };
    let mut cleaned = State::default();
    cleaned.sessions.insert(session_id.into(), session.clone());

    let failure =
        apply_failed_new_session_rollback(&mut cleaned, session_id, "ACP startup failed", None);

    let retained = &cleaned.sessions[session_id];
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_none());
    assert!(failure.to_string().contains("failed session retained"));

    session.state = SessionState::Disconnected;
    let mut cleanup_failed = State::default();
    cleanup_failed.sessions.insert(session_id.into(), session);
    let failure = apply_failed_new_session_rollback(
        &mut cleanup_failed,
        session_id,
        "ACP startup failed",
        Some("ssh unavailable".into()),
    );
    let retained = cleanup_failed.sessions.get(session_id).unwrap();
    assert_eq!(retained.state, SessionState::Error);
    assert!(retained.target.is_some());
    assert!(failure.to_string().contains("cleanup"));
}
#[test]
fn launch_failure_is_persisted_separately_from_session_state() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let detail = format!(
        "specific startup cause\n{}\nstderr tail survives",
        "x".repeat(MAX_LAUNCH_DIAGNOSTIC_BYTES)
    );

    let path = persist_launch_failure_to(directory.path(), session_id, &detail).unwrap();
    let saved = std::fs::read_to_string(path).unwrap();

    assert!(saved.contains("specific startup cause"));
    assert!(saved.contains("launch diagnostic truncated"));
    assert!(saved.contains("stderr tail survives"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn noting_a_launch_failure_writes_the_diagnostic_and_returns_the_reason() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let error = anyhow::anyhow!("connect worker").context("Connection closed by 10.0.0.1 port 22");

    let detail = note_new_session_launch_failure_in(directory.path(), session_id, &error);

    assert!(
        detail.contains("Connection closed by 10.0.0.1 port 22"),
        "the returned reason keeps the underlying error text"
    );
    assert!(
        detail.contains("full diagnostic saved to"),
        "the reason points at the saved diagnostic"
    );
    let saved = std::fs::read_to_string(
        directory
            .path()
            .join(format!("{session_id}-launch-error.txt")),
    )
    .unwrap();
    assert!(saved.contains("Connection closed by 10.0.0.1 port 22"));
}
#[test]
fn inherited_git_settings_allow_only_portable_non_executable_values() {
    let settings = parse_inherited_git_settings(
            b"user.name\nAgent User\0USER.EMAIL\nagent@example.test\0pull.rebase\ntrue\0alias.deploy\n!ship\0credential.helper\nstore\0core.editor\nvim\0include.path\n/host/config\0user.name\nFinal User\0",
        )
        .unwrap();

    assert_eq!(
        settings,
        BTreeMap::from([
            ("pull.rebase".into(), "true".into()),
            ("user.email".into(), "agent@example.test".into()),
            ("user.name".into(), "Final User".into()),
        ])
    );
}
#[test]
fn inherited_git_settings_reject_malformed_or_non_utf8_output() {
    assert!(parse_inherited_git_settings(b"user.name\0").is_err());
    assert!(parse_inherited_git_settings(b"user.name\n\xff\0").is_err());
}
#[test]
fn inherited_git_settings_target_only_isolated_workers() {
    let ssh = SshTarget {
        destination: "worker@example.test".into(),
        ssh_args: vec!["-p".into(), "2222".into()],
    };
    let ephemeral = [
        targets::TargetLocator::LocalPodman {
            container_id: "abcdef012345".into(),
            workspace_storage: Default::default(),
        },
        targets::TargetLocator::AppleContainer {
            container_id: "abcdef012346".into(),
        },
        targets::TargetLocator::AwsEc2 {
            profile: "default".into(),
            region: "us-east-1".into(),
            instance_id: "i-1234567890abcdef0".into(),
            ssh: ssh.clone(),
            workspace: ".local/share/hel/workspaces/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
        },
        targets::TargetLocator::SshPodman {
            ssh: ssh.clone(),
            container_id: "abcdef012347".into(),
            workspace_storage: Default::default(),
        },
    ];
    for locator in &ephemeral {
        assert!(inherits_controller_git_settings(locator));
        let commands = inherited_git_setting_commands(
            locator,
            "018f9dd2-a3b4-7c8d-9000-123456789abc",
            BTreeMap::from([("user.name".into(), "- Agent O'Brien 日本語".into())]),
        )
        .unwrap();
        assert_eq!(commands.len(), 1);
        assert!(
            commands[0]
                .args
                .iter()
                .any(|argument| argument.contains("user.name"))
        );
        assert!(
            commands[0]
                .args
                .iter()
                .any(|argument| argument.contains("- Agent O'"))
        );
    }

    let persistent = targets::TargetLocator::SshBare {
        worker_id: None,
        ssh,
        workspace: "/srv/hel/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
    };
    let local = targets::TargetLocator::LocalBare {
        worker_root: "/var/lib/hel/workers/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
    };
    assert!(!inherits_controller_git_settings(&persistent));
    assert!(!inherits_controller_git_settings(&local));
    assert!(
        inherited_git_setting_commands(
            &persistent,
            "018f9dd2-a3b4-7c8d-9000-123456789abc",
            BTreeMap::from([("user.name".into(), "Agent".into())]),
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn raw_ssh_targets_select_permissions_and_ssh_podman_is_unconstrained() {
    let ssh = mj_core::config::SshConnection {
        host: "builder".into(),
        user: None,
        identity_file: None,
        extra_args: Vec::new(),
    };
    let guardian = TargetTemplate::SshBare {
        ssh: ssh.clone(),
        permissions: mj_core::config::PermissionMode::Guardian,
        workspace_prefix: ".local/share/hel/workspaces".into(),
    };
    let podman = TargetTemplate::SshPodman {
        ssh: ssh.clone(),
        container: mj_core::config::ContainerTemplate {
            image: "example.invalid/agent:latest".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    let yolo = TargetTemplate::SshBare {
        ssh,
        permissions: mj_core::config::PermissionMode::Yolo,
        workspace_prefix: ".local/share/hel/workspaces".into(),
    };

    assert_eq!(
        TargetTemplate::LocalBare.execution_policy(),
        mj_core::config::ExecutionPolicy::ConfiguredApprovals
    );
    assert_eq!(
        guardian.execution_policy(),
        mj_core::config::ExecutionPolicy::ConfiguredApprovals
    );
    assert_eq!(
        podman.execution_policy(),
        mj_core::config::ExecutionPolicy::Unconstrained
    );
    assert_eq!(
        yolo.execution_policy(),
        mj_core::config::ExecutionPolicy::Unconstrained
    );
}
const PROVISIONED_SESSION: &str = "0123456789abcdef0123456789abcdef";

/// Records every command a plan runs, and fails the one whose purpose it
/// was told to fail.
struct RecordingExecutor {
    failing_purpose: String,
    commands: Mutex<Vec<Vec<String>>>,
}

impl RecordingExecutor {
    fn failing(purpose: impl Into<String>) -> Self {
        Self {
            failing_purpose: purpose.into(),
            commands: Mutex::new(Vec::new()),
        }
    }

    fn succeeding() -> Self {
        Self::failing(String::new())
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().unwrap().clone()
    }
}

impl CommandExecutor for RecordingExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let mut argv = vec![command.program.clone()];
        argv.extend(command.args.clone());
        self.commands.lock().unwrap().push(argv);
        Ok(CommandOutput {
            status: i32::from(command.purpose == self.failing_purpose),
            stdout: Vec::new(),
            stderr: b"the step failed".to_vec(),
        })
    }
}

fn container_targets() -> Vec<targets::TargetTemplate> {
    let container = ContainerTemplate {
        image: "ubuntu:24.04".into(),
        pull_policy: Default::default(),
        extra_run_args: Vec::new(),
        workspace_storage: Default::default(),
    };
    vec![
        targets::TargetTemplate::LocalPodman(container.clone()),
        targets::TargetTemplate::AppleContainer(container.clone()),
        targets::TargetTemplate::SshPodman {
            ssh: SshTarget {
                destination: "dev@example.test".into(),
                ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
            },
            container,
        },
    ]
}

#[test]
fn a_failure_after_the_container_exists_removes_it_and_keeps_the_original_error() {
    let name = targets::resource_name(PROVISIONED_SESSION).unwrap();
    for target in container_targets() {
        let plan =
            targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[]).unwrap();
        let executor = RecordingExecutor::failing("clone app");

        let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            unreachable!("locator discovery must not run after a failed plan")
        })
        .unwrap_err();

        let reported = format!("{error:#}");
        assert!(reported.contains("clone app failed"), "{reported}");
        assert!(reported.contains("cleanup succeeded"), "{reported}");
        // Remote commands reach the target posix-quoted.
        let removal = executor
            .commands()
            .into_iter()
            .map(|arguments| arguments.join(" ").replace('\'', ""))
            .find(|command| command.contains("rm --force") && command.contains(&name))
            .expect("cleanup removes the exact provisioned container");
        assert!(removal.contains("rm --force"), "{removal}");
        assert!(removal.contains(&name), "{removal}");
    }
}

#[test]
fn target_creation_returns_repository_setup_without_running_it() {
    let target = podman_target();
    let plan = targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[]).unwrap();
    let executor = RecordingExecutor::succeeding();

    let (_, repositories) =
        provision_target_creation(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            Ok(TargetLocator::LocalPodman {
                container_id: targets::resource_name(PROVISIONED_SESSION)?,
                workspace_storage: Default::default(),
            })
        })
        .unwrap();

    assert_eq!(executor.commands().len(), 1, "only podman run may execute");
    assert!(
        repositories
            .commands
            .iter()
            .any(|command| command.purpose == "clone app")
    );
}

#[test]
fn a_target_whose_creation_failed_is_never_torn_down() {
    for target in container_targets() {
        let plan =
            targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[]).unwrap();
        let creation = plan.split_at_target_creation().unwrap().0;
        let executor =
            RecordingExecutor::failing(creation.commands.last().unwrap().purpose.clone());

        let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            unreachable!("locator discovery must not run after a failed plan")
        })
        .unwrap_err();

        let reported = format!("{error:#}");
        assert!(!reported.contains("cleanup"), "{reported}");
        assert!(
            !executor
                .commands()
                .iter()
                .any(|argv| argv.join(" ").contains("rm --force")),
            "{:?}",
            executor.commands()
        );
    }
}

#[test]
fn a_target_whose_locator_cannot_be_discovered_is_removed_again() {
    let target = podman_target();
    let plan = targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[]).unwrap();
    let executor = RecordingExecutor::succeeding();

    let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
        bail!("the container never reported an address")
    })
    .unwrap_err();

    let reported = format!("{error:#}");
    assert!(reported.contains("never reported an address"), "{reported}");
    assert!(reported.contains("cleanup succeeded"), "{reported}");
    let removal = executor
        .commands()
        .into_iter()
        .map(|arguments| arguments.join(" "))
        .find(|command| command.contains("podman rm --force --ignore"))
        .expect("cleanup removes the provisioned Podman container");
    assert!(removal.contains("podman rm --force --ignore"), "{removal}");
}

/// A raw project directory is the user's own: provisioning it creates
/// nothing that a failure could leak.
#[test]
fn a_bare_project_failure_removes_nothing() {
    let target = targets::TargetTemplate::LocalBare;
    let plan =
        targets::provision_bare_project_plan(&target, PROVISIONED_SESSION, "/srv/project").unwrap();
    let executor = RecordingExecutor::succeeding();

    let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
        bail!("the worker root was unreadable")
    })
    .unwrap_err();

    assert!(!format!("{error:#}").contains("cleanup"));
    assert!(executor.commands().is_empty());
}
