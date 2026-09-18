use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::controller::Controller;
use crate::controller::provisioning::install_attached_resources;
use mj_core::config::{
    AwsAddressSource, Config, ContainerTemplate as ConfigContainer, ProjectBundle,
    ProjectRepository, SshConnection, TargetTemplate,
};
use mj_core::state::{SessionRecord, SessionState, State};

use crate::targets::{
    self, AdditionalMount, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate,
    ImageHost, RefreshWhen, SshTarget,
};

use super::*;

/// A fake executor that fails the SSH probe a fixed number of times.
struct SshProbeExecutor {
    failures_remaining: RefCell<u32>,
    attempts: RefCell<u32>,
    cancel_after: Option<u32>,
}
impl SshProbeExecutor {
    fn new(failures: u32) -> Self {
        Self {
            failures_remaining: RefCell::new(failures),
            attempts: RefCell::new(0),
            cancel_after: None,
        }
    }
}
impl CommandExecutor for SshProbeExecutor {
    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        *self.attempts.borrow_mut() += 1;
        let mut remaining = self.failures_remaining.borrow_mut();
        if *remaining == 0 {
            return Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        *remaining -= 1;
        Ok(CommandOutput {
            status: 255,
            stdout: Vec::new(),
            stderr: b"ssh: connect to host 10.0.0.1 port 22: Connection refused\n".to_vec(),
        })
    }

    fn cancellation_requested(&self) -> bool {
        self.cancel_after
            .is_some_and(|limit| *self.attempts.borrow() >= limit)
    }
}
fn ssh_probe_spec() -> CommandSpec {
    CommandSpec::new("ssh", ["host", "true"]).purpose("wait for EC2 SSH availability")
}
/// A virtual clock advanced only by the injected sleep hook.
fn virtual_clock() -> (std::rc::Rc<std::cell::Cell<Instant>>, Instant) {
    let start = Instant::now();
    (std::rc::Rc::new(std::cell::Cell::new(start)), start)
}
#[test]
fn ssh_readiness_wait_succeeds_after_failed_probes() {
    let executor = SshProbeExecutor::new(3);
    let (clock, _) = virtual_clock();
    let sleep_clock = clock.clone();
    wait_for_ssh_ready(
        &executor,
        &ssh_probe_spec(),
        Duration::from_secs(300),
        {
            let clock = clock.clone();
            move || clock.get()
        },
        move |delay| sleep_clock.set(sleep_clock.get() + delay),
    )
    .expect("the wait succeeds once SSH answers");
    assert_eq!(*executor.attempts.borrow(), 4);
}
#[test]
fn ssh_readiness_wait_gives_up_at_the_deadline_and_reports_the_last_error() {
    let executor = SshProbeExecutor::new(u32::MAX);
    let (clock, _) = virtual_clock();
    let sleep_clock = clock.clone();
    let error = wait_for_ssh_ready(
        &executor,
        &ssh_probe_spec(),
        Duration::from_secs(30),
        {
            let clock = clock.clone();
            move || clock.get()
        },
        move |delay| sleep_clock.set(sleep_clock.get() + delay),
    )
    .expect_err("the wait stops at the deadline");
    let message = error.to_string();
    assert!(message.contains("timed out after 30s"), "{message}");
    assert!(message.contains("Connection refused"), "{message}");
}
#[test]
fn ssh_readiness_wait_stops_when_cancellation_is_requested() {
    let mut executor = SshProbeExecutor::new(u32::MAX);
    executor.cancel_after = Some(2);
    let (clock, _) = virtual_clock();
    let sleep_clock = clock.clone();
    let error = wait_for_ssh_ready(
        &executor,
        &ssh_probe_spec(),
        Duration::from_secs(300),
        {
            let clock = clock.clone();
            move || clock.get()
        },
        move |delay| sleep_clock.set(sleep_clock.get() + delay),
    )
    .expect_err("the wait stops when cancelled");
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(*executor.attempts.borrow(), 2);
}
#[test]
fn aws_resources_are_compressed_into_one_streamed_ssh_command() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
        streams: RefCell<Vec<Vec<u8>>>,
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

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let mut stream = Vec::new();
            input.read_to_end(&mut stream)?;
            self.streams.borrow_mut().push(stream);
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("many/files")).unwrap();
    std::fs::write(source.path().join("many/files/one"), b"one").unwrap();
    std::fs::write(source.path().join("many/files/two"), b"two").unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let record = SessionRecord {
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: session_id.into(),
        title: "AWS resources".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "aws".into(),
        resource_allocation: None,
        additional_mounts: vec![AdditionalMount {
            source: source.path().to_path_buf(),
            destination: "/home/ubuntu/mj-resources/data".into(),
            access: crate::targets::MountAccess::Cow,
        }],
        state: SessionState::Disconnected,
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
    let state = State {
        subagents: Default::default(),
        version: mj_core::state::STATE_VERSION,
        sessions: BTreeMap::from([(session_id.into(), record)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let backend = targets::TargetLocator::AwsEc2 {
        profile: "default".into(),
        region: "us-east-1".into(),
        instance_id: "i-1234567890abcdef0".into(),
        ssh: SshTarget {
            destination: "ubuntu@example.test".into(),
            ssh_args: Vec::new(),
        },
        workspace: format!(".local/share/hel/workspaces/{session_id}"),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
        streams: RefCell::new(Vec::new()),
    };

    install_attached_resources(
        &state,
        session_id,
        &backend,
        ".local/share/hel/workers/session",
        &executor,
    )
    .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "ssh");
    assert!(
        commands[0]
            .args
            .iter()
            .any(|argument| argument.contains("install-resource"))
    );
    let streams = executor.streams.borrow();
    assert_eq!(streams.len(), 1);
    assert_eq!(&streams[0][..2], &[0x1f, 0x8b]);
}
#[test]
fn canonical_bundle_maps_github_shorthand_and_primary_destination() {
    let bundle = ProjectBundle {
        primary_repo: "app".into(),
        repositories: vec![ProjectRepository {
            id: "app".into(),
            github: Some("example/app".into()),
            local: None,
            destination: PathBuf::from("services/app"),
            git_ref: None,
        }],
    };
    let backend = backend_bundle(&bundle, &targets::ProcessExecutor).unwrap();
    assert_eq!(backend.primary, "services/app");
    assert_eq!(
        backend.repositories[0].url.as_deref(),
        Some("https://github.com/example/app.git")
    );
}
#[test]
fn container_resources_and_environment_become_argv() {
    let template = TargetTemplate::LocalPodman {
        container: ConfigContainer {
            build_cache: None,
            image: "dev:1".into(),
            pull_policy: mj_core::config::ImagePullPolicy::Never,
            platform: Some("linux/arm64".into()),
            cpus: Some("4".into()),
            memory: Some("8g".into()),
            environment: std::collections::BTreeMap::from([("A".into(), "b c".into())]),
            workspace_storage: Default::default(),
        },
    };
    let targets::TargetTemplate::LocalPodman(container) =
        backend_target(&template, None, ContainerOverrides::default()).unwrap()
    else {
        unreachable!()
    };
    assert!(container.extra_run_args.contains(&"--cpus=4".into()));
    assert!(container.extra_run_args.contains(&"A=b c".into()));
    assert_eq!(
        container.pull_policy,
        mj_core::config::ImagePullPolicy::Never
    );
}
#[test]
fn session_size_overrides_beat_the_target_template_and_its_allocation() {
    let template = TargetTemplate::LocalPodman {
        container: ConfigContainer {
            build_cache: None,
            image: "dev:1".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: Some("4".into()),
            memory: Some("8g".into()),
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    let mut session = crate::controller::test_support::checkpoint_test_session("session-size");
    session.container_cpus = Some("2".into());
    session.container_memory = Some("3g".into());
    session.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 16,
        memory_bytes: 64_000_000_000,
    });
    let targets::TargetTemplate::LocalPodman(container) = backend_target(
        &template,
        session.resource_allocation.as_ref(),
        ContainerOverrides::for_session(&session),
    )
    .unwrap() else {
        unreachable!()
    };
    assert!(container.extra_run_args.contains(&"--cpus=2".into()));
    assert!(container.extra_run_args.contains(&"--memory=3g".into()));
    assert!(!container.extra_run_args.iter().any(|argument| {
        argument.starts_with("--cpus=4")
            || argument.starts_with("--cpus=16")
            || argument.starts_with("--memory=8g")
    }));
}
#[test]
fn github_token_is_inherited_only_by_managed_containers() {
    let mut podman = targets::TargetTemplate::LocalPodman(ContainerTemplate {
        build_cache: None,
        image: "dev:1".into(),
        pull_policy: Default::default(),
        extra_run_args: vec![],
        workspace_storage: Default::default(),
    });
    assert!(configure_github_token_environment(&mut podman));
    let targets::TargetTemplate::LocalPodman(container) = podman else {
        unreachable!()
    };
    assert!(
        container
            .extra_run_args
            .windows(2)
            .any(|arguments| arguments == ["--env", "GH_TOKEN"])
    );
    assert!(
        !container
            .extra_run_args
            .iter()
            .any(|argument| argument.contains("github-token"))
    );

    let mut bare = targets::TargetTemplate::LocalBare;
    assert!(!configure_github_token_environment(&mut bare));
    assert_eq!(bare, targets::TargetTemplate::LocalBare);
    assert_eq!(usable_github_token("  token-value\n"), Some("token-value"));
    assert_eq!(usable_github_token("not a token"), None);

    let mut bundle = targets::ProjectBundleSpec {
        primary: "app".into(),
        repositories: vec![targets::RepositorySpec {
            url: Some("git@github.com:example/app.git".into()),
            push_urls: vec![
                "git@github.com:fork/app.git".into(),
                "ssh://git@example.test/app.git".into(),
            ],
            destination: "app".into(),
            git_ref: None,
            reference: None,
        }],
    };
    use_github_https_urls(&mut bundle);
    assert_eq!(
        bundle.repositories[0].url.as_deref(),
        Some("https://github.com/example/app.git")
    );
    assert_eq!(
        bundle.repositories[0].push_urls,
        [
            "https://github.com/fork/app.git",
            "ssh://git@example.test/app.git"
        ]
    );
}
fn container_target(image: &str, pull_policy: mj_core::config::ImagePullPolicy) -> ConfigContainer {
    ConfigContainer {
        build_cache: None,
        image: image.into(),
        pull_policy,
        platform: None,
        cpus: None,
        memory: None,
        environment: BTreeMap::new(),
        workspace_storage: Default::default(),
    }
}

#[test]
fn the_image_refresh_plan_covers_every_configured_container_image_except_never() {
    use mj_core::config::ImagePullPolicy;

    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto),
        },
    );
    // The same image on the same host, named by a second target.
    config.targets.insert(
        "podman-again".into(),
        TargetTemplate::LocalPodman {
            container: container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto),
        },
    );
    config.targets.insert(
        "ssh".into(),
        TargetTemplate::SshPodman {
            ssh: SshConnection {
                host: "builder.example.test".into(),
                user: Some("dev".into()),
                identity_file: Some(PathBuf::from("/home/dev/.ssh/builder")),
                extra_args: Vec::new(),
            },
            container: ConfigContainer {
                platform: Some("linux/amd64".into()),
                ..container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto)
            },
        },
    );
    config.targets.insert(
        "docker".into(),
        TargetTemplate::LocalDocker {
            container: container_target("ghcr.io/example/dev:1.2.3", ImagePullPolicy::Newer),
        },
    );
    config.targets.insert(
        "pinned".into(),
        TargetTemplate::LocalPodman {
            container: container_target(
                "ghcr.io/example/dev@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ImagePullPolicy::Auto,
            ),
        },
    );
    // Apple's engine joins the startup download like every other engine.
    config.targets.insert(
        "apple".into(),
        TargetTemplate::AppleContainer {
            container: container_target("ghcr.io/example/dev:1.2.3", ImagePullPolicy::Auto),
        },
    );
    // An explicit `never` is the one way to keep an image out of the plan.
    config.targets.insert(
        "never".into(),
        TargetTemplate::LocalDocker {
            container: container_target("ghcr.io/example/offline:latest", ImagePullPolicy::Never),
        },
    );
    // Two targets sharing one image on one host, wanting it at different
    // freshness: one download, on the more eager schedule.
    config.targets.insert(
        "merge-missing".into(),
        TargetTemplate::LocalDocker {
            container: container_target("ghcr.io/example/shared:2", ImagePullPolicy::Missing),
        },
    );
    config.targets.insert(
        "merge-newer".into(),
        TargetTemplate::LocalDocker {
            container: container_target("ghcr.io/example/shared:2", ImagePullPolicy::Newer),
        },
    );

    let plan = image_refresh_plan(&config);
    assert_eq!(
        plan.len(),
        6,
        "expected one refresh per host, image and platform: {plan:?}"
    );

    let entry = |host: &ImageHost, image: &str| {
        plan.iter()
            .find(|refresh| &refresh.host == host && refresh.image == image)
            .unwrap_or_else(|| panic!("no refresh for {image} on {}: {plan:?}", host.label()))
    };

    let local = entry(&ImageHost::LocalPodman, "ghcr.io/example/dev:latest");
    assert_eq!(local.when, RefreshWhen::Always);
    assert_eq!(local.pull.program, "podman");
    assert_eq!(local.pull.args, ["pull", "ghcr.io/example/dev:latest"]);
    assert_eq!(
        local.prune.as_ref().expect("podman prunes").args,
        ["image", "prune", "-f"]
    );
    assert_eq!(
        local.image_id.args,
        [
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            "ghcr.io/example/dev:latest"
        ]
    );

    let docker = entry(&ImageHost::LocalDocker, "ghcr.io/example/dev:1.2.3");
    assert_eq!(docker.when, RefreshWhen::Always);
    assert_eq!(docker.pull.program, "docker");
    assert_eq!(docker.pull.args, ["pull", "ghcr.io/example/dev:1.2.3"]);
    assert_eq!(
        docker.prune.as_ref().expect("docker prunes").args,
        ["image", "prune", "-f"]
    );

    // A versioned tag and a digest pin only need the host to have a copy.
    let apple = entry(&ImageHost::AppleContainer, "ghcr.io/example/dev:1.2.3");
    assert_eq!(apple.when, RefreshWhen::WhenAbsent);
    assert_eq!(apple.pull.program, "container");
    assert_eq!(
        apple.pull.args,
        ["image", "pull", "ghcr.io/example/dev:1.2.3"]
    );
    let pinned = plan
        .iter()
        .find(|refresh| refresh.image.contains("sha256:"))
        .expect("a digest pin is still downloaded once when absent");
    assert_eq!(pinned.when, RefreshWhen::WhenAbsent);

    assert!(
        !plan.iter().any(|refresh| refresh.image.contains("offline")),
        "a never policy was downloaded in the background: {plan:?}"
    );

    let shared = entry(&ImageHost::LocalDocker, "ghcr.io/example/shared:2");
    assert_eq!(
        shared.when,
        RefreshWhen::Always,
        "the more eager of two targets sharing an image wins"
    );

    let ssh = plan
        .iter()
        .find(|refresh| matches!(refresh.host, ImageHost::SshPodman(_)))
        .expect("the SSH host is refreshed over its own connection");
    assert_eq!(ssh.when, RefreshWhen::Always);
    assert_eq!(ssh.pull.program, "ssh");
    // The identity file and destination come from the same builder
    // provisioning uses.
    assert!(ssh.pull.args.contains(&"/home/dev/.ssh/builder".to_owned()));
    assert!(
        ssh.pull
            .args
            .contains(&"dev@builder.example.test".to_owned())
    );
    assert_eq!(
        ssh.pull.args.last().map(String::as_str),
        Some("'podman' 'pull' '--platform=linux/amd64' 'ghcr.io/example/dev:latest'")
    );
    assert_eq!(
        ssh.prune
            .as_ref()
            .expect("podman prunes")
            .args
            .last()
            .map(String::as_str),
        Some("'podman' 'image' 'prune' '-f'")
    );
}

#[test]
fn ssh_docker_image_refresh_runs_docker_on_the_configured_host() {
    use mj_core::config::ImagePullPolicy;

    let mut config = Config::default();
    config.targets.insert(
        "docker".into(),
        TargetTemplate::SshDocker {
            ssh: SshConnection {
                host: "builder.example.test".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            container: ConfigContainer {
                build_cache: None,
                image: "ghcr.io/example/dev:latest".into(),
                pull_policy: ImagePullPolicy::Auto,
                platform: Some("linux/amd64".into()),
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );

    let refresh = image_refresh_plan(&config).pop().expect("refresh plan");
    assert_eq!(
        refresh.host,
        ImageHost::SshDocker(SshTarget::from(
            match config.targets.get("docker").unwrap() {
                TargetTemplate::SshDocker { ssh, .. } => ssh,
                _ => unreachable!(),
            }
        ))
    );
    assert_eq!(refresh.pull.program, "ssh");
    assert_eq!(
        refresh.pull.args.last().map(String::as_str),
        Some("'docker' 'pull' '--platform=linux/amd64' 'ghcr.io/example/dev:latest'")
    );
    assert_eq!(
        refresh
            .prune
            .as_ref()
            .expect("docker prunes")
            .args
            .last()
            .map(String::as_str),
        Some("'docker' 'image' 'prune' '-f'")
    );
    assert_eq!(refresh.when, RefreshWhen::Always);
}

#[test]
fn aws_resource_options_follow_the_launch_template_family() {
    let mut config = Config::default();
    config.targets.insert(
        "aws".into(),
        TargetTemplate::AwsEc2 {
            aws_profile: None,
            region: "us-east-1".into(),
            launch_template: "hel-runson".into(),
            launch_template_version: None,
            ssh_user: "ubuntu".into(),
            address_source: AwsAddressSource::PublicIp,
            identity_file: None,
            ssh_args: Vec::new(),
        },
    );
    let executor = PreflightExecutor {
        outputs: RefCell::new(vec![
            CommandOutput {
                status: 0,
                stdout: br#"{"LaunchTemplateVersions":[{"LaunchTemplateData":{"InstanceType":"m8i-flex.large"}}]}"#.to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                status: 0,
                stdout: br#"{"InstanceTypes":[{"InstanceType":"m8i-flex.4xlarge","VCpuInfo":{"DefaultVCpus":16},"MemoryInfo":{"SizeInMiB":65536}},{"InstanceType":"m8i-flex.2xlarge","VCpuInfo":{"DefaultVCpus":8},"MemoryInfo":{"SizeInMiB":32768}}]}"#.to_vec(),
                stderr: Vec::new(),
            },
        ]),
        notices: RefCell::new(vec![]),
    };
    let controller = Controller {
        config,
        state: State::default(),
    };

    let options = controller
        .resolve_aws_resource_options("aws", &executor)
        .unwrap();
    assert_eq!(
        options.iter().map(allocation_cpus).collect::<Vec<_>>(),
        [8, 16]
    );
}
#[test]
fn deployment_capacity_groups_local_and_same_host_targets() {
    let container = || ConfigContainer {
        build_cache: None,
        image: "dev:1".into(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: BTreeMap::new(),
        workspace_storage: Default::default(),
    };
    let ssh = |host: &str| SshConnection {
        host: host.into(),
        user: Some("builder".into()),
        identity_file: None,
        extra_args: Vec::new(),
    };
    let config = Config {
        build_cache: Default::default(),
        subagents: Default::default(),
        version: mj_core::config::CONFIG_VERSION,
        sessions_side: Default::default(),
        advanced: Default::default(),
        show_stopped_sessions: false,
        spinner: Default::default(),
        theme: Default::default(),
        phone: Default::default(),
        review: Default::default(),
        sessionwiki: Default::default(),
        legacy_startup: (),
        machines: Default::default(),
        profiles: BTreeMap::new(),
        bundles: BTreeMap::new(),
        targets: BTreeMap::from([
            (
                "apple".into(),
                TargetTemplate::AppleContainer {
                    container: container(),
                },
            ),
            (
                "local".into(),
                TargetTemplate::LocalPodman {
                    container: container(),
                },
            ),
            (
                "bare".into(),
                TargetTemplate::SshBare {
                    ssh: ssh("builder"),
                    permissions: mj_core::config::PermissionMode::Yolo,
                    workspace_prefix: ".local/share/hel/workspaces".into(),
                },
            ),
            (
                "remote-container".into(),
                TargetTemplate::SshPodman {
                    ssh: ssh("builder"),
                    container: container(),
                },
            ),
            (
                "alias".into(),
                TargetTemplate::SshBare {
                    ssh: ssh("builder-alias"),
                    permissions: mj_core::config::PermissionMode::Yolo,
                    workspace_prefix: ".local/share/hel/workspaces".into(),
                },
            ),
        ]),
    };
    let controller = Controller {
        config,
        state: State::default(),
    };

    let targets = controller.deployment_capacity_targets();

    assert_eq!(targets.len(), 3);
    let local = targets.iter().find(|target| target.id == "local").unwrap();
    assert_eq!(local.target_ids, ["apple", "local"]);
    let builder = targets
        .iter()
        .find(|target| target.id == "ssh:builder")
        .unwrap();
    assert_eq!(builder.target_ids, ["bare", "remote-container"]);
    assert_eq!(builder.probes.len(), 1);
    assert!(
        targets
            .iter()
            .any(|target| target.id == "ssh:builder-alias")
    );
}
struct PreflightExecutor {
    outputs: RefCell<Vec<CommandOutput>>,
    notices: RefCell<Vec<String>>,
}
impl CommandExecutor for PreflightExecutor {
    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        Ok(self.outputs.borrow_mut().remove(0))
    }

    fn notify_notice(&self, notice: &str) {
        self.notices.borrow_mut().push(notice.to_owned());
    }
}
#[test]
fn local_podman_preflight_failures_explain_the_problem_and_offer_retry() {
    let template = TargetTemplate::LocalPodman {
        container: ConfigContainer {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    let executor = PreflightExecutor {
        outputs: RefCell::new(vec![CommandOutput {
            status: 0,
            stdout: b"podman version 3.4.7\n".to_vec(),
            stderr: vec![],
        }]),
        notices: RefCell::new(vec![]),
    };

    let error = preflight_target(&template, &executor)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Retry launch"));
    assert!(error.contains("Podman 4.3.0"));
}
#[test]
fn ssh_podman_preflight_failures_name_the_destination_and_offer_retry() {
    let template = TargetTemplate::SshPodman {
        ssh: SshConnection {
            host: "example.test".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: vec![],
        },
        container: ConfigContainer {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    let executor = PreflightExecutor {
        outputs: RefCell::new(vec![CommandOutput {
            status: 0,
            stdout: crate::targets::ssh_podman_probe_fixture(&[(
                "version",
                0,
                "podman version 3.4.7\n",
                "",
            )]),
            stderr: vec![],
        }]),
        notices: RefCell::new(vec![]),
    };

    let error = preflight_target(&template, &executor)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Retry launch"));
    assert!(error.contains("dev@example.test"));
    assert!(error.contains("Podman 4.3.0"));
}
#[test]
fn ssh_podman_preflight_notifies_when_remote_user_lingering_is_disabled() {
    let template = TargetTemplate::SshPodman {
        ssh: SshConnection {
            host: "example.test".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: vec![],
        },
        container: ConfigContainer {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    let executor = PreflightExecutor {
        outputs: RefCell::new(vec![CommandOutput {
            status: 0,
            stdout: crate::targets::ssh_podman_probe_fixture(&[
                ("version", 0, "podman version 5.4.2\n", ""),
                ("rootless", 0, "true\n", ""),
                ("uid_map", 0, "0 1000 1\n1 100000 65536\n", ""),
                ("linger", 0, "no\n", ""),
            ]),
            stderr: vec![],
        }]),
        notices: RefCell::new(vec![]),
    };

    preflight_target(&template, &executor).unwrap();

    let notices = executor.notices.borrow();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("last SSH connection closes"));
    assert!(notices[0].contains("sudo loginctl enable-linger"));
}
#[test]
fn apple_container_preflight_failures_recommend_doctor() {
    let template = TargetTemplate::AppleContainer {
        container: ConfigContainer {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        },
    };
    for (stdout, stderr) in [
        (
            "apiserver is not running and not registered with launchd",
            "",
        ),
        ("", "daemon is not running"),
        ("apiserver is not running", "service unavailable"),
    ] {
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![CommandOutput {
                status: 1,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }]),
            notices: RefCell::new(vec![]),
        };

        let error = preflight_target(&template, &executor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Retry launch"));
        assert!(error.contains("container system start"));
        assert!(error.contains(stdout));
        assert!(error.contains(stderr));
    }
}
