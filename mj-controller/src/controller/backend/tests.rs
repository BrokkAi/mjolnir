use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::controller::Controller;
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, ProjectBundle, ProjectRepository, SshConnection,
    TargetTemplate,
};
use mj_core::state::State;

use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate};

use super::*;

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
