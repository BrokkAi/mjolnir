use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::controller::Controller;
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, ProjectBundle, ProjectRepository, SshConnection,
    TargetTemplate,
};
use mj_core::state::State;

use crate::targets::{
    self, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate, ImageHost, RefreshWhen,
};

use super::*;

struct AwsCliFixtureExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    responses: RefCell<std::collections::VecDeque<CommandOutput>>,
}

impl AwsCliFixtureExecutor {
    fn new(responses: impl IntoIterator<Item = &'static [u8]>) -> Self {
        Self {
            commands: RefCell::new(Vec::new()),
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|stdout| CommandOutput {
                        status: 0,
                        stdout: stdout.to_vec(),
                        stderr: Vec::new(),
                    })
                    .collect(),
            ),
        }
    }
}

impl CommandExecutor for AwsCliFixtureExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        Ok(self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("one AWS CLI response per command"))
    }
}

fn aws_options_controller(
    launch_template: &str,
    profile: Option<&str>,
    version: Option<&str>,
) -> Controller {
    Controller {
        config: Config {
            targets: BTreeMap::from([(
                "ec2".to_owned(),
                TargetTemplate::AwsEc2 {
                    aws_profile: profile.map(str::to_owned),
                    region: "us-west-2".to_owned(),
                    launch_template: launch_template.to_owned(),
                    launch_template_version: version.map(str::to_owned),
                    ssh_user: "ec2-user".to_owned(),
                    address_source: Default::default(),
                    identity_file: None,
                    ssh_args: Vec::new(),
                },
            )]),
            ..Config::default()
        },
        state: State::default(),
    }
}

fn render_aws_allocations(allocations: &[SessionResourceAllocation]) -> String {
    allocations
        .iter()
        .map(|allocation| match allocation {
            SessionResourceAllocation::AwsEc2 {
                instance_type,
                vcpus,
                memory_bytes,
            } => format!("{instance_type}: {vcpus} vCPUs, {memory_bytes} bytes"),
            SessionResourceAllocation::Container { .. } => {
                panic!("AWS discovery returned a container allocation")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn golden_aws_resource_options_read_documented_cli_responses() {
    // These fixtures follow the AWS CLI v2 response shapes:
    // https://docs.aws.amazon.com/cli/latest/reference/ec2/describe-launch-template-versions.html
    // https://docs.aws.amazon.com/cli/latest/reference/ec2/describe-instance-types.html
    const RESPONSES: [&[u8]; 4] = [
        br#"{"LaunchTemplateVersions":[{"LaunchTemplateId":"lt-0123456789abcdef0","VersionNumber":7,"LaunchTemplateData":{"ImageId":"ami-0123456789abcdef0","InstanceType":"m7i.large"}}]}"#,
        br#"{"InstanceTypes":[{"InstanceType":"m7i.4xlarge","VCpuInfo":{"DefaultVCpus":16},"MemoryInfo":{"SizeInMiB":65536}},{"InstanceType":"m7i.2xlarge","VCpuInfo":{"DefaultVCpus":8},"MemoryInfo":{"SizeInMiB":32768}},{"InstanceType":"m7i.large","VCpuInfo":{"DefaultVCpus":2},"MemoryInfo":{"SizeInMiB":8192}},{"InstanceType":"m7i.xlarge","VCpuInfo":{"DefaultVCpus":4},"MemoryInfo":{"SizeInMiB":16384}},{"InstanceType":"m7i.malformed","VCpuInfo":{"DefaultVCpus":4}},{"InstanceType":"m7i.overflow","VCpuInfo":{"DefaultVCpus":32},"MemoryInfo":{"SizeInMiB":18446744073709551615}}]}"#,
        br#"{"LaunchTemplateVersions":[{"LaunchTemplateName":"legacy","VersionNumber":7,"LaunchTemplateData":{"ImageId":"ami-0123456789abcdef0","InstanceType":"c6i.large"}}]}"#,
        br#"{"InstanceTypes":[{"InstanceType":"c6i.2xlarge","VCpuInfo":{"DefaultVCpus":8},"MemoryInfo":{"SizeInMiB":16384}},{"InstanceType":"c6i.large","VCpuInfo":{"DefaultVCpus":2},"MemoryInfo":{"SizeInMiB":4096}}]}"#,
    ];
    let executor = AwsCliFixtureExecutor::new(RESPONSES);
    let by_id = aws_options_controller("lt-0123456789abcdef0", Some("build"), Some("7"))
        .resolve_aws_resource_options("ec2", &executor)
        .expect("resolve ID-addressed launch template");
    let by_name = aws_options_controller("legacy", None, None)
        .resolve_aws_resource_options("ec2", &executor)
        .expect("resolve name-addressed launch template");

    let mut rendered = String::new();
    rendered.push_str("=== launch-template id ===\n");
    rendered.push_str(&render_aws_allocations(&by_id));
    rendered.push_str("\n\n=== launch-template name ===\n");
    rendered.push_str(&render_aws_allocations(&by_name));
    rendered.push_str("\n\n=== AWS CLI commands ===\n");
    for command in executor.commands.borrow().iter() {
        rendered.push_str(&format!("{} {}\n", command.program, command.args.join(" ")));
    }
    rendered.pop();
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "aws-resource-options",
        &rendered,
    );
}

#[test]
fn provisioning_uses_accepted_fetch_and_push_settings_after_the_catalog_changes() {
    struct NoProbe;
    impl CommandExecutor for NoProbe {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            panic!(
                "accepted source must not be recomputed: {}",
                command.purpose
            );
        }
    }
    let mut session = crate::database::test_session("accepted-project", "saved");
    session.project = Some(mj_core::repository::ProjectBundleSnapshot {
        bundle: ProjectBundle {
            primary_repo: "original-repository-id".into(),
            repositories: vec![ProjectRepository {
                id: "original-repository-id".into(),
                github: None,
                local: Some("/removed/checkout".into()),
                destination: "original-layout".into(),
                git_ref: None,
            }],
        },
        identities: BTreeMap::from([(
            "original-repository-id".into(),
            mj_core::repository::RepositoryIdentity::Github("acme".into(), "app".into()),
        )]),
        network_sources: BTreeMap::from([(
            "original-repository-id".into(),
            mj_core::remote_git::NetworkGitSource {
                fetch_url: "https://github.com/acme/app.git".into(),
                push_urls: vec![
                    "git@github.com:developer/app.git".into(),
                    "ssh://mirror.example/app.git".into(),
                ],
            },
        )]),
    });
    let spec = backend_session_bundle(&session, &Config::default(), &NoProbe).unwrap();
    assert_eq!(spec.primary, "original-layout");
    assert_eq!(
        spec.repositories[0].url.as_deref(),
        Some("https://github.com/acme/app.git")
    );
    assert_eq!(
        spec.repositories[0].push_urls,
        vec![
            "git@github.com:developer/app.git",
            "ssh://mirror.example/app.git"
        ]
    );
}

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
        environment: Default::default(),
        workspace_storage: Default::default(),
    }
}

// Hard-won: 8c5355ea19ec: auto-policy images were skipped, leaving first Create to download them
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

// Hard-won: d7afd671: the old preflight rejected Podman 4.0 despite root-default sessions no longer needing keep-id.
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
            environment: Default::default(),
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

    let error = preflight_target(&template, &executor, TargetCheck::Launch)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Retry launch"));
    assert!(error.contains("Podman 4.0.0"));
}

// Hard-won: d7afd671: the old preflight rejected Podman 4.0 despite root-default sessions no longer needing keep-id.
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
            environment: Default::default(),
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

    let error = verify_target(&template, &executor, TargetCheck::Launch)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Retry launch"));
    assert!(error.contains("dev@example.test"));
    assert!(error.contains("Podman 4.0.0"));
}

// Hard-won: 69e2f7a632ab: launch and move repeated slow remote container checks on multiple wizard pages
#[test]
fn launch_preflight_checks_only_reachability_for_ssh_container_targets() {
    struct Recording(RefCell<Vec<CommandSpec>>);
    impl CommandExecutor for Recording {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.0.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    let ssh = SshConnection {
        host: "example.test".into(),
        user: Some("dev".into()),
        identity_file: None,
        extra_args: vec![],
    };
    let container = ConfigContainer {
        build_cache: None,
        image: "ubuntu:24.04".into(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: Default::default(),
        workspace_storage: Default::default(),
    };
    for template in [
        TargetTemplate::SshPodman {
            ssh: ssh.clone(),
            container: container.clone(),
        },
        TargetTemplate::SshDocker { ssh, container },
    ] {
        let executor = Recording(RefCell::default());

        preflight_target(&template, &executor, TargetCheck::Launch).unwrap();

        let commands = executor.0.borrow();
        assert_eq!(commands.len(), 1, "{template:?}");
        assert_eq!(commands[0].purpose, "verify SSH connectivity");
        assert!(
            !commands[0]
                .args
                .iter()
                .any(|arg| arg.contains("podman") || arg.contains("docker")),
            "{:?}",
            commands[0].args
        );
    }
}

/// F-7: an executor that behaves like a host without Docker. Running the
/// program fails the way `std::process::Command` does when it is not on PATH.
struct NoDockerExecutor;

impl CommandExecutor for NoDockerExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if command.program == "docker" {
            return Err(
                anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound)).context(
                    format!("run {} for {}", command.program, "check Docker daemon"),
                ),
            );
        }
        Ok(CommandOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: b"engine stopped".to_vec(),
        })
    }
}

// Hard-won: ba563efac06a: launch options reported local Docker ready when no engine was installed
#[test]
fn local_engine_readiness_tells_a_missing_engine_from_one_that_did_not_answer() {
    assert_eq!(
        local_engine_readiness("local-docker", &NoDockerExecutor),
        Some(LocalEngineReadiness::NotInstalled)
    );
    assert_eq!(
        local_engine_readiness("apple-container", &NoDockerExecutor),
        Some(LocalEngineReadiness::NotReady)
    );
    assert_eq!(
        local_engine_readiness("local-bare", &NoDockerExecutor),
        None
    );
    assert_eq!(
        local_engine_readiness("ssh-docker", &NoDockerExecutor),
        None
    );
}

/// Launch finding R5-3: the session wizard said "local Docker is not ready.
/// Start Docker or fix the proble…" for an engine that is not installed. Its
/// check now leads with the words the launch options use.
///
/// Launch finding R6-3: the wizard's row then ended "…then Retry launch."
/// before anything was launched. The wizard's check now says what to do
/// without it; a launch that fails the same check still says it, next to the
/// failure dialog's Retry launch button.
// Hard-won: 028b83276375: missing Docker errors disagreed across launch surfaces and buried the cause
#[test]
fn the_wizard_says_docker_is_not_installed_in_the_launch_options_words() {
    let template = TargetTemplate::LocalDocker {
        container: mj_core::config::ContainerTemplate {
            build_cache: None,
            image: "example.invalid/dev:latest".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
        },
    };
    let controller = Controller {
        config: Config {
            targets: BTreeMap::from([("docker".to_owned(), template.clone())]),
            ..Config::default()
        },
        state: State::default(),
    };
    let before_launch = controller
        .check_target_readiness("docker", &NoDockerExecutor)
        .unwrap_err()
        .to_string();
    assert_eq!(
        before_launch,
        "Docker is not installed on this host. Install Docker or choose another target."
    );
    let launch = preflight_target(&template, &NoDockerExecutor, TargetCheck::Launch)
        .unwrap_err()
        .to_string();
    assert_eq!(
        launch,
        "Docker is not installed on this host. Install Docker or choose another target, then Retry launch."
    );
    assert_eq!(
        local_engine_readiness("local-docker", &NoDockerExecutor),
        Some(LocalEngineReadiness::NotInstalled)
    );
}
