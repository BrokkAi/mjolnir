use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::anyhow;

use super::*;
use crate::targets::CommandOutput;

struct FakeExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    responses: RefCell<Vec<Result<CommandOutput>>>,
}

impl FakeExecutor {
    fn new(responses: impl IntoIterator<Item = Result<CommandOutput>>) -> Self {
        Self {
            commands: RefCell::new(vec![]),
            responses: RefCell::new(responses.into_iter().collect()),
        }
    }
}

impl CommandExecutor for FakeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        self.responses.borrow_mut().remove(0)
    }
}

/// Answers every command with a plain failure, for a whole-run test whose
/// subject is the reporting rather than any external tool.
struct AlwaysFailingExecutor;

impl CommandExecutor for AlwaysFailingExecutor {
    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        Ok(CommandOutput {
            status: 1,
            stdout: vec![],
            stderr: vec![],
        })
    }
}

fn output(stdout: impl AsRef<[u8]>) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.as_ref().to_vec(),
        stderr: vec![],
    }
}

fn failed(stderr: impl AsRef<[u8]>) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: stderr.as_ref().to_vec(),
    }
}

/// Prefix canned responses with a successful SSH connectivity probe, which
/// every SSH-backed check runs first.
fn reachable_then(
    responses: impl IntoIterator<Item = Result<CommandOutput>>,
) -> Vec<Result<CommandOutput>> {
    let mut all = vec![Ok(output(b""))];
    all.extend(responses);
    all
}

/// The remote probes arrive in one batched SSH command, framed the way the
/// remote script prints them.
fn ssh_podman_probes(linger: (i32, &str, &str)) -> Vec<Result<CommandOutput>> {
    vec![Ok(output(crate::targets::ssh_podman_probe_fixture(&[
        ("version", 0, "podman version 5.4.2\n", ""),
        (
            "uid_map",
            0,
            "         0       1000          1\n         1     100000      65536\n",
            "",
        ),
        ("linger", linger.0, linger.1, linger.2),
    ])))]
}

fn passing_ssh_podman_probes() -> Vec<Result<CommandOutput>> {
    ssh_podman_probes((0, "yes\n", ""))
}

fn passing_podman_probes() -> Vec<Result<CommandOutput>> {
    vec![
        Ok(output(b"podman version 5.4.2\n")),
        Ok(output(
            b"         0       1000          1\n         1     100000      65536\n",
        )),
    ]
}

fn container(image: &str) -> ContainerTemplate {
    ContainerTemplate {
        build_cache: None,
        image: image.to_owned(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: std::collections::BTreeMap::new(),
        workspace_storage: Default::default(),
    }
}

fn ssh_connection() -> mj_core::config::SshConnection {
    mj_core::config::SshConnection {
        host: "example.test".into(),
        user: Some("dev".into()),
        identity_file: None,
        extra_args: vec![],
    }
}

#[test]
fn doctor_tells_the_user_to_update_rather_than_replace_a_newer_builds_config() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = {}\n\n[targets.localhost]\nkind = \"local-bare\"\n",
            mj_core::config::CONFIG_VERSION + 1
        ),
    )
    .unwrap();

    let (config, checks) = configuration_checks(&path);

    assert_eq!(
        config.err(),
        Some(ConfigGap::NewerVersion(mj_core::config::CONFIG_VERSION + 1))
    );
    let check = checks.iter().find(|check| check.id == "config").unwrap();
    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(check.detail.contains("newer Mjolnir"), "{}", check.detail);
    let remediation = check.remediation.as_deref().unwrap_or_default();
    assert!(remediation.contains("Update Mjolnir"), "{remediation}");
    assert!(!remediation.contains("mj setup"), "{remediation}");
}

/// Sessions start from a plain project directory without any bundle, so a
/// configuration with an enabled profile and no bundle is complete.
#[test]
fn a_config_without_a_bundle_is_ready_for_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: directory.path().join("codex-home"),
        environment: std::collections::BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    Config {
        profiles: [("work".to_owned(), profile)].into_iter().collect(),
        ..Config::default()
    }
    .save_to(&path)
    .unwrap();

    let (_, checks) = configuration_checks(&path);

    let check = checks
        .iter()
        .find(|check| check.id == "config.session-prerequisites")
        .unwrap();
    assert_eq!(check.status, CheckStatus::Ready, "{check:?}");
    assert!(all_ready(&checks));
}

#[test]
fn a_config_without_an_enabled_profile_cannot_start_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    Config::default().save_to(&path).unwrap();

    let (_, checks) = configuration_checks(&path);

    let check = checks
        .iter()
        .find(|check| check.id == "config.session-prerequisites")
        .unwrap();
    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(check.detail.contains("profile"), "{}", check.detail);
    assert!(!check.detail.contains("bundle"), "{}", check.detail);
}

/// A newer build's configuration is the one case doctor cannot read and the
/// user cannot repair in the file, so no check in the run may send them to fix
/// TOML; the checks that depend on a configuration skip and say why.
#[test]
fn a_newer_builds_config_never_asks_the_user_to_fix_config_toml() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let newer = mj_core::config::CONFIG_VERSION + 1;
    std::fs::write(
        &path,
        format!("version = {newer}\n\n[targets.localhost]\nkind = \"local-bare\"\n"),
    )
    .unwrap();

    let checks = run_with_config_path(
        &path,
        &AlwaysFailingExecutor,
        ApplePlatform::Linux,
        DoctorOptions { smoke: false },
    );

    for check in &checks {
        let text = format!(
            "{} {}",
            check.detail,
            check.remediation.as_deref().unwrap_or_default()
        );
        assert!(
            !text.contains("config.toml is valid") && !text.contains("Fix config.toml"),
            "{} advises fixing a config that is not broken: {text}",
            check.id
        );
    }
    for id in [
        "harness.profiles",
        "runtime.podman",
        "runtime.docker",
        "worker.containers",
    ] {
        let check = checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("{id} is reported"));
        assert_eq!(check.status, CheckStatus::Unsupported, "{id}");
        assert_eq!(check.remediation, None, "{id}");
        assert!(
            check
                .detail
                .contains(&format!("newer Mjolnir (config version {newer}")),
            "{id}: {}",
            check.detail
        );
    }
}

fn config_with(targets: impl IntoIterator<Item = (&'static str, TargetTemplate)>) -> Config {
    Config {
        targets: targets
            .into_iter()
            .map(|(id, target)| (id.to_owned(), target))
            .collect(),
        ..Config::default()
    }
}

#[test]
fn doctor_warns_once_when_shared_container_host_mbx_is_too_old() {
    let config = config_with([
        (
            "podman",
            TargetTemplate::LocalPodman {
                container: container("ubuntu:24.04"),
            },
        ),
        (
            "docker",
            TargetTemplate::LocalDocker {
                container: container("ubuntu:24.04"),
            },
        ),
    ]);
    let executor = FakeExecutor::new([Ok(output("mbx\nmbx 1.15.0"))]);

    let checks = build_cache_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    let check = &checks[0];
    assert_eq!(check.id, "build-cache.local");
    assert_eq!(check.status, CheckStatus::Warning);
    assert!(check.detail.contains("1.15.0"));
    assert!(check.detail.contains(crate::controller::MBX_VERSION));
    assert!(check.detail.contains("run without the shared build cache"));
    assert!(check.detail.contains("docker, podman"));
    assert!(
        check
            .remediation
            .as_deref()
            .unwrap()
            .contains("Upgrade mbx")
    );
    assert_eq!(executor.commands.borrow().len(), 1);
    assert!(
        all_ready(&checks),
        "an optional cache warning preserves doctor's exit status"
    );

    let json = serde_json::to_value(check).unwrap();
    assert_eq!(json["status"], "warning");
    assert!(
        json["remediation"]
            .as_str()
            .unwrap()
            .contains(crate::controller::MBX_VERSION)
    );
    let mut human = Vec::new();
    render_human(&checks, &mut human).unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("warning Build cache on local"));
    assert!(human.contains("remediation: Upgrade mbx"));
}

#[test]
fn doctor_distinguishes_compatible_absent_and_uncheckable_host_mbx() {
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);
    for (response, expected_status, expected_text) in [
        (
            Ok(output(format!(
                "mbx\nmbx {}",
                crate::controller::MBX_VERSION
            ))),
            CheckStatus::Ready,
            "compatible",
        ),
        (Ok(failed("")), CheckStatus::Ready, "No native mbx"),
        (
            Err(anyhow!("probe timed out")),
            CheckStatus::Warning,
            "probe timed out",
        ),
    ] {
        let executor = FakeExecutor::new([response]);
        let checks = build_cache_checks(Ok(&config), &executor);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, expected_status);
        assert!(checks[0].detail.contains(expected_text), "{:?}", checks[0]);
    }
}

#[test]
fn doctor_reports_remote_host_mbx_for_ssh_container_targets() {
    let config = config_with([(
        "remote",
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container("ubuntu:24.04"),
        },
    )]);
    let executor = FakeExecutor::new([Ok(output("/home/dev/.cargo/bin/mbx\nmbx 1.15.0"))]);

    let checks = build_cache_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Warning);
    assert!(checks[0].id.contains("example.test"));
    assert!(checks[0].detail.contains("targets remote"));
    assert_eq!(executor.commands.borrow()[0].program, "ssh");
}

#[test]
fn doctor_skips_disabled_or_irrelevant_build_caches_without_probing() {
    let mut disabled = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);
    disabled.build_cache.enabled = false;
    let executor = FakeExecutor::new([]);
    assert!(build_cache_checks(Ok(&disabled), &executor).is_empty());
    disabled.build_cache.enabled = true;
    if let TargetTemplate::LocalPodman { container } = disabled.targets.get_mut("podman").unwrap() {
        container.build_cache = Some(mj_core::config::TargetBuildCache {
            enabled: Some(false),
            ..Default::default()
        });
    }
    assert!(build_cache_checks(Ok(&disabled), &executor).is_empty());
    let bare = config_with([("bare", TargetTemplate::LocalBare)]);
    assert!(build_cache_checks(Ok(&bare), &executor).is_empty());
    assert!(executor.commands.borrow().is_empty());
}

fn runtime_ssh() -> RuntimeSshTarget {
    RuntimeSshTarget::from(&ssh_connection())
}

#[test]
fn podman_check_is_unsupported_without_a_valid_config() {
    let executor = FakeExecutor::new([]);

    let check = podman_check(Err(ConfigGap::Unreadable), &executor);

    assert_eq!(check.status, CheckStatus::Unsupported);
    assert_eq!(
        check.detail,
        "Podman prerequisites cannot be evaluated until config.toml is valid."
    );
    assert!(executor.commands.borrow().is_empty());
}

#[test]
fn podman_check_is_unsupported_without_a_local_podman_target() {
    let executor = FakeExecutor::new([]);
    let config = config_with([(
        "apple",
        TargetTemplate::AppleContainer {
            container: container("ubuntu:24.04"),
        },
    )]);

    let check = podman_check(Ok(&config), &executor);

    assert_eq!(check.status, CheckStatus::Unsupported);
    assert_eq!(check.detail, "No local-podman target is configured.");
    assert!(executor.commands.borrow().is_empty());
}

#[test]
fn podman_check_probes_the_host_when_a_local_podman_target_exists() {
    let executor = FakeExecutor::new(passing_podman_probes());
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);

    let check = podman_check(Ok(&config), &executor);

    assert_eq!(check.status, CheckStatus::Ready);
    assert!(check.detail.contains("Podman 5.4.2"));
    assert_eq!(executor.commands.borrow().len(), 2);
}

#[test]
fn podman_check_is_fixable_with_an_upgrade_remediation_for_an_old_runtime() {
    let executor = FakeExecutor::new(reachable_then([Ok(output(b"podman version 3.4.7\n"))]));
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);

    let check = podman_check(Ok(&config), &executor);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(
        check
            .remediation
            .as_deref()
            .unwrap()
            .contains("Install or upgrade Podman")
    );
}

#[test]
fn podman_image_check_is_ready_when_the_image_is_present() {
    let executor = FakeExecutor::new([Ok(output(b""))]);

    let check = podman_image_check("podman", "localhost/hel/agent-dev:latest", &executor, false);

    assert_eq!(check.id, "runtime.podman.image.podman");
    assert_eq!(check.title, "Podman image for target podman");
    assert_eq!(check.status, CheckStatus::Ready);
    assert_eq!(
        executor.commands.borrow()[0].args,
        vec!["image", "exists", "localhost/hel/agent-dev:latest"]
    );
}

#[test]
fn podman_image_check_is_fixable_with_a_pull_remediation_when_the_image_is_missing() {
    let executor = FakeExecutor::new([Ok(failed(b""))]);

    let check = podman_image_check("podman", "ghcr.io/example/dev:1", &executor, false);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(
        check
            .detail
            .contains("is not present in local Podman storage")
    );
    assert_eq!(
        check.remediation.as_deref(),
        Some(
            "Pull it with `podman pull ghcr.io/example/dev:1`, build it from containers/Containerfile.agent-dev, or run `mj doctor --json --smoke` to verify the full pull-and-run path."
        )
    );
}

#[test]
fn podman_image_check_smoke_runs_a_disposable_container() {
    let executor = FakeExecutor::new([
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"removed\n")),
    ]);

    let check = podman_image_check("podman", "ubuntu:24.04", &executor, true);

    assert_eq!(check.status, CheckStatus::Ready);
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert!(commands.iter().all(|command| command.program == "podman"));
    assert_eq!(commands[0].args[0], "run");
    assert_eq!(commands[1].args[0], "exec");
    assert_eq!(commands[2].args[0], "rm");
}

#[test]
fn image_checks_are_skipped_when_the_host_podman_preflight_fails() {
    let executor = FakeExecutor::new(reachable_then([Ok(output(b"podman version 3.4.7\n"))]));
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = podman_checks(Ok(&config), &executor, false);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "runtime.podman");
}

#[test]
fn image_checks_follow_a_passing_preflight_for_each_local_podman_target() {
    let mut responses = passing_podman_probes();
    responses.push(Ok(output(b"")));
    responses.push(Ok(failed(b"")));
    // The built-in `podman` target the dashboard also lists.
    responses.push(Ok(output(b"")));
    let executor = FakeExecutor::new(responses);
    let config = config_with([
        (
            "alpha",
            TargetTemplate::LocalPodman {
                container: container("ubuntu:24.04"),
            },
        ),
        (
            "beta",
            TargetTemplate::LocalPodman {
                container: container("ghcr.io/example/dev:1"),
            },
        ),
    ]);

    let checks = podman_checks(Ok(&config), &executor, false);

    assert_eq!(
        checks
            .iter()
            .map(|check| check.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "runtime.podman",
            "runtime.podman.image.alpha",
            "runtime.podman.image.beta",
            "runtime.podman.image.podman"
        ]
    );
    assert_eq!(checks[1].status, CheckStatus::Ready);
    assert_eq!(checks[2].status, CheckStatus::Fixable);
    assert_eq!(checks[3].status, CheckStatus::Ready);
}

/// Launch finding R5-3: doctor and `mj setup` gave a missing Docker the raw
/// error chain ("run docker for check Docker daemon: No such file or
/// directory (os error 2)"). They now say it is not installed and how to
/// fix that.
#[test]
fn a_missing_docker_is_reported_as_not_installed() {
    let executor = FakeExecutor::new([Err(anyhow::Error::new(std::io::Error::from(
        std::io::ErrorKind::NotFound,
    ))
    .context("run docker for check Docker daemon"))]);

    let check = local_docker_runtime_check(&executor);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(check.detail, "Docker is not installed on this host.");
    assert!(
        check
            .remediation
            .as_deref()
            .is_some_and(|remediation| remediation.starts_with("Install Docker")),
        "{:?}",
        check.remediation
    );
}

#[test]
fn docker_checks_probe_the_daemon_then_the_configured_image() {
    let executor = FakeExecutor::new([
        Ok(output(b"29.0.1 linux\n")),
        Ok(output(b"image metadata\n")),
    ]);
    let config = config_with([(
        "docker",
        TargetTemplate::LocalDocker {
            container: container("ghcr.io/example/dev:1"),
        },
    )]);

    let checks = docker_checks(Ok(&config), &executor, false);

    assert_eq!(
        checks
            .iter()
            .map(|check| check.id.as_str())
            .collect::<Vec<_>>(),
        vec!["runtime.docker", "runtime.docker.image.docker"]
    );
    assert!(
        checks
            .iter()
            .all(|check| check.status == CheckStatus::Ready)
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands[0].program, "docker");
    assert_eq!(
        commands[0].args,
        ["version", "--format", "{{.Server.Version}} {{.Server.Os}}"]
    );
    assert_eq!(
        commands[1].args,
        ["image", "inspect", "ghcr.io/example/dev:1"]
    );
}

/// The dashboard lists the built-in `docker` target (and downloads its image)
/// without any configured Docker target. Doctor checks the same target set:
/// it probes Docker and the image for the built-in target, and when Docker
/// is missing it reports the built-in target as unavailable, not as a fault.
#[test]
fn docker_checks_cover_the_built_in_docker_target_the_dashboard_lists() {
    let executor = FakeExecutor::new([
        Ok(output(b"29.0.1 linux\n")),
        Ok(output(b"image metadata\n")),
    ]);
    let checks = docker_checks(Ok(&Config::default()), &executor, false);
    assert_eq!(
        checks
            .iter()
            .map(|check| (check.id.as_str(), check.status))
            .collect::<Vec<_>>(),
        vec![
            ("runtime.docker", CheckStatus::Ready),
            ("runtime.docker.image.docker", CheckStatus::Ready)
        ]
    );

    let missing_image = FakeExecutor::new([Ok(output(b"29.0.1 linux\n")), Ok(failed(b""))]);
    let checks = docker_checks(Ok(&Config::default()), &missing_image, false);
    assert_eq!(
        checks[1].status,
        CheckStatus::Warning,
        "the dashboard downloads a built-in target's image itself: {}",
        checks[1].detail
    );

    let checks = docker_checks(Ok(&Config::default()), &AlwaysFailingExecutor, false);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Unsupported);
    assert!(
        checks[0].detail.contains("built-in `docker` target"),
        "{}",
        checks[0].detail
    );

    let configured = config_with([(
        "docker",
        TargetTemplate::LocalDocker {
            container: container("ghcr.io/example/dev:1"),
        },
    )]);
    let checks = docker_checks(Ok(&configured), &AlwaysFailingExecutor, false);
    assert_eq!(
        checks[0].status,
        CheckStatus::Fixable,
        "a target the user configured is still a fault to fix"
    );
}

/// Launch finding R3-3: a Setup save once wrote the built-in `[targets.docker]`
/// and `[targets.podman]` blocks into config.toml, and doctor then reported a
/// missing engine as a fault to fix. A block identical to the built-in target
/// is still the built-in target.
#[test]
fn a_target_block_identical_to_a_built_in_is_still_treated_as_built_in() {
    let written = config_with([
        (
            "docker",
            TargetTemplate::LocalDocker {
                container: container(mj_core::config::DEFAULT_CONTAINER_IMAGE),
            },
        ),
        (
            "podman",
            TargetTemplate::LocalPodman {
                container: container(mj_core::config::DEFAULT_CONTAINER_IMAGE),
            },
        ),
    ]);
    let docker = docker_checks(Ok(&written), &AlwaysFailingExecutor, false);
    assert_eq!(docker.len(), 1);
    assert_eq!(
        docker[0].status,
        CheckStatus::Unsupported,
        "{}",
        docker[0].detail
    );
    assert!(docker[0].detail.contains("built-in `docker` target"));
    let podman = podman_checks(Ok(&written), &AlwaysFailingExecutor, false);
    assert_eq!(podman.len(), 1);
    assert_eq!(
        podman[0].status,
        CheckStatus::Unsupported,
        "{}",
        podman[0].detail
    );

    let missing_image = FakeExecutor::new([Ok(output(b"29.0.1 linux\n")), Ok(failed(b""))]);
    let checks = docker_checks(Ok(&written), &missing_image, false);
    assert_eq!(
        checks[1].status,
        CheckStatus::Warning,
        "{}",
        checks[1].detail
    );
}

#[test]
fn podman_checks_cover_the_built_in_podman_target() {
    let checks = podman_checks(Ok(&Config::default()), &AlwaysFailingExecutor, false);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Unsupported);
    assert!(
        checks[0].detail.contains("built-in `podman` target"),
        "{}",
        checks[0].detail
    );
}

#[test]
fn docker_image_smoke_uses_managed_overlay_run_exec_and_cleanup() {
    let executor = FakeExecutor::new([
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"removed\n")),
    ]);

    let check = docker_image_check("docker", "ubuntu:24.04", &executor, true);

    assert_eq!(check.status, CheckStatus::Ready);
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].program, "sh");
    assert!(commands[0].args[1].contains("docker volume create"));
    assert!(commands[0].args.contains(&"--pull=missing".to_owned()));
    assert_eq!(commands[1].program, "docker");
    assert_eq!(commands[1].args[0], "exec");
    assert_eq!(commands[2].program, "sh");
    assert!(commands[2].args[1].contains("docker rm --force"));
    assert!(commands[2].args[1].contains("docker volume rm --force"));
}

#[test]
fn ssh_podman_check_is_ready_after_ssh_wrapped_probes_without_smoke() {
    let executor = FakeExecutor::new(reachable_then(passing_ssh_podman_probes()));

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

    assert_eq!(check.id, "runtime.ssh-podman.remote");
    assert_eq!(check.title, "Remote Podman for target remote");
    assert_eq!(check.status, CheckStatus::Ready);
    assert!(check.detail.contains("Remote rootless Podman 5.4.2"));
    assert!(check.detail.contains("dev@example.test"));
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args.last().unwrap(), "'true'");
    for command in commands.iter().skip(1) {
        assert_eq!(command.program, "ssh");
        assert!(command.args.contains(&"dev@example.test".to_owned()));
    }
    assert!(
        commands[1]
            .args
            .last()
            .unwrap()
            .contains("loginctl show-user")
    );
}

#[test]
fn ssh_podman_check_warns_when_remote_user_lingering_is_disabled() {
    let executor = FakeExecutor::new(reachable_then(ssh_podman_probes((0, "no\n", ""))));

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

    assert_eq!(check.status, CheckStatus::Warning);
    assert!(all_ready(std::slice::from_ref(&check)));
    assert!(check.detail.contains("Podman 5.4.2 is available"));
    assert!(check.detail.contains("last SSH connection closes"));
    assert!(
        check
            .remediation
            .as_deref()
            .unwrap()
            .contains("sudo loginctl enable-linger")
    );
}

#[test]
fn ssh_podman_check_explains_when_durability_cannot_be_verified() {
    let executor = FakeExecutor::new(reachable_then(ssh_podman_probes((
        127,
        "",
        "sh: loginctl: not found\n",
    ))));

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

    assert_eq!(check.status, CheckStatus::Warning);
    assert!(all_ready(std::slice::from_ref(&check)));
    assert!(check.detail.contains("durability check is unavailable"));
    assert!(check.detail.contains("may not use systemd"));
    assert!(check.detail.contains("cannot verify"));
    let remediation = check.remediation.as_deref().unwrap();
    assert!(remediation.contains("service manager"));
    assert!(!remediation.contains("sudo loginctl enable-linger"));
}

#[test]
fn ssh_podman_check_failure_scopes_the_remediation_to_the_remote_host() {
    let executor = FakeExecutor::new(reachable_then([Ok(output(
        crate::targets::ssh_podman_probe_fixture(&[("version", 0, "podman version 3.4.7\n", "")]),
    ))]));

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(check.detail.contains("dev@example.test"));
    assert!(
        check
            .remediation
            .as_deref()
            .unwrap()
            .starts_with("On dev@example.test: Install or upgrade Podman")
    );
}

#[test]
fn ssh_podman_check_reports_the_shared_ssh_remediation_before_probing_podman() {
    let executor = FakeExecutor::new([Ok(failed(
        b"dev@example.test: Permission denied (publickey).",
    ))]);

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(
        check.remediation.as_deref(),
        Some("Install your public key on the host with `ssh-copy-id dev@example.test`.")
    );
    // The remote Podman probes never ran: the host is not reachable.
    assert_eq!(executor.commands.borrow().len(), 1);
}

#[test]
fn ssh_podman_check_smoke_runs_an_ssh_wrapped_disposable_container() {
    let mut responses = passing_ssh_podman_probes();
    responses.extend([
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"removed\n")),
    ]);
    let executor = FakeExecutor::new(reachable_then(responses));

    let (check, _) = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, true);

    assert_eq!(check.status, CheckStatus::Ready);
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 5);
    for command in commands.iter().skip(2) {
        assert_eq!(command.program, "ssh");
        assert!(command.args.contains(&"dev@example.test".to_owned()));
    }
    assert!(commands[2].args.last().unwrap().contains("'run' '--init'"));
    assert!(commands[3].args.last().unwrap().ends_with("'true'"));
    assert!(commands[4].args.last().unwrap().contains("'rm' '--force'"));
}

/// Everything the limits script can print, as one host would report it.
fn host_limits_output() -> Vec<u8> {
    b"keys.max=200\nkeys.used=101\nkeys.quota=4096\nmaxstartups=100:30:200\n".to_vec()
}

#[test]
fn host_limits_parse_reports_every_field_the_script_printed() {
    let limits = parse_host_limits(&host_limits_output());

    assert_eq!(limits.keys_max, Some(200));
    assert_eq!(limits.keys_used, Some(101));
    assert_eq!(limits.keys_quota, Some(4096));
    assert_eq!(limits.max_startups.as_deref(), Some("100:30:200"));
    assert!(!limits.max_startups_unreadable);
    assert!(!limits.is_empty());
    assert!(!limits.keyring_is_under_pressure());
}

#[test]
fn host_limits_report_pressure_when_keys_reach_the_quota() {
    let limits = parse_host_limits(b"keys.used=3300\nkeys.quota=4096\n");

    assert!(limits.keyring_is_under_pressure());
    assert!(
        limits
            .keyring_sentence("dev@example.test")
            .contains("3300 of its 4096")
    );
}

#[test]
fn host_limits_report_an_explicit_max_startups_directive() {
    let limits = parse_host_limits(b"maxstartups=10:30:60\n");

    assert_eq!(
        limits.max_startups_sentence(),
        "sshd MaxStartups is 10:30:60."
    );
}

#[test]
fn host_limits_say_a_drop_in_may_override_an_unread_max_startups() {
    let limits = parse_host_limits(b"keys.used=10\nkeys.quota=4096\nmaxstartups.unreadable=1\n");

    assert!(limits.max_startups_unreadable);
    let sentence = limits.max_startups_sentence();
    assert!(sentence.contains("unreadable drop-in"), "{sentence}");
    assert!(!sentence.contains("10:30"), "{sentence}");
}

#[test]
fn host_limits_say_the_sshd_default_applies_when_every_file_was_readable() {
    let limits = parse_host_limits(b"keys.used=10\nkeys.quota=4096\n");

    assert_eq!(
        limits.max_startups_sentence(),
        "sshd MaxStartups is not set in sshd_config, so sshd's default applies."
    );
}

#[test]
fn host_limits_are_empty_when_the_script_printed_nothing() {
    assert!(parse_host_limits(b"").is_empty());
    assert!(parse_host_limits(b"unrelated line\n").is_empty());
}

#[test]
fn ssh_podman_checks_report_host_limits_after_the_podman_check() {
    let mut responses = passing_ssh_podman_probes();
    responses.push(Ok(output(host_limits_output())));
    let executor = FakeExecutor::new(reachable_then(responses));
    let config = config_with([(
        "remote",
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = ssh_podman_checks(Ok(&config), &executor, false);

    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].id, "runtime.ssh-podman.remote");
    assert_eq!(checks[1].id, "runtime.ssh-podman.remote.limits");
    assert_eq!(checks[1].title, "Host limits for target remote");
    assert_eq!(checks[1].status, CheckStatus::Ready);
    assert!(
        checks[1].detail.contains("101 of its 4096"),
        "{}",
        checks[1].detail
    );
    assert!(
        checks[1].detail.contains("sshd MaxStartups is 100:30:200"),
        "{}",
        checks[1].detail
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert!(commands[2].args.last().unwrap().contains("key-users"));
}

#[test]
fn ssh_podman_checks_skip_host_limits_when_the_host_is_unreachable() {
    let executor = FakeExecutor::new([Ok(failed(
        b"dev@example.test: Permission denied (publickey).",
    ))]);
    let config = config_with([(
        "remote",
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = ssh_podman_checks(Ok(&config), &executor, false);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "runtime.ssh-podman.remote");
    assert_eq!(checks[0].status, CheckStatus::Fixable);
    assert_eq!(executor.commands.borrow().len(), 1);
}

#[test]
fn ssh_docker_check_probes_connectivity_daemon_and_remote_image() {
    let executor = FakeExecutor::new([
        Ok(output(b"")),
        Ok(output(b"29.0.1 linux\n")),
        Ok(output(b"image metadata\n")),
    ]);
    let check = ssh_docker_check(
        "remote",
        &runtime_ssh(),
        "ghcr.io/example/dev:1",
        &executor,
        false,
    );

    assert_eq!(check.id, "runtime.ssh-docker.remote");
    assert_eq!(check.title, "Remote Docker for target remote");
    assert_eq!(check.status, CheckStatus::Ready);
    assert!(check.detail.contains("Remote Docker 29.0.1"));
    assert!(
        check
            .detail
            .contains("image ghcr.io/example/dev:1 is present")
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert!(commands.iter().all(|command| command.program == "ssh"));
    assert!(commands[0].args.last().unwrap().contains("'true'"));
    assert!(
        commands[1]
            .args
            .last()
            .unwrap()
            .contains("'docker' 'version'")
    );
    assert!(
        commands[2]
            .args
            .last()
            .unwrap()
            .contains("'docker' 'image' 'inspect'")
    );
}

#[test]
fn ssh_docker_check_smoke_runs_overlay_on_the_remote_host() {
    let executor = FakeExecutor::new([
        Ok(output(b"")),
        Ok(output(b"29.0.1 linux\n")),
        Ok(output(b"/tmp/mj-docker-overlay-smoke.fixture\n")),
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"verified\n")),
        Ok(output(b"removed\n")),
        Ok(output(b"removed\n")),
    ]);
    let check = ssh_docker_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, true);

    assert_eq!(check.status, CheckStatus::Ready);
    assert!(check.detail.contains("remote OverlayFS attachment"));
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 8);
    assert!(commands.iter().all(|command| command.program == "ssh"));
    assert!(commands[2].args.last().unwrap().contains("mktemp"));
    assert!(commands[3].args.last().unwrap().contains("'docker' 'run'"));
    assert!(commands[4].args.last().unwrap().contains("'docker' 'exec'"));
    assert!(commands[5].args.last().unwrap().contains("original.txt"));
    assert!(commands[6].args.last().unwrap().contains("docker rm"));
    assert!(commands[7].args.last().unwrap().contains("'rm' '-rf'"));
}

#[test]
fn ssh_podman_checks_are_skipped_without_a_valid_config() {
    let executor = FakeExecutor::new([]);

    assert!(ssh_podman_checks(Err(ConfigGap::Unreadable), &executor, false).is_empty());
    assert!(executor.commands.borrow().is_empty());
}

fn ssh_bare_config() -> Config {
    config_with([(
        "builder",
        TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "example.test".into(),
                user: Some("dev".into()),
                identity_file: Some(PathBuf::from("/home/dev/.ssh/id_ed25519")),
                extra_args: vec![],
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
        },
    )])
}

#[test]
fn ssh_bare_check_is_ready_when_the_batch_mode_probe_succeeds() {
    let executor = FakeExecutor::new([Ok(output(b""))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "runtime.ssh-bare.builder");
    assert_eq!(checks[0].status, CheckStatus::Ready);
    let command = &executor.commands.borrow()[0];
    assert_eq!(command.program, "ssh");
    assert_eq!(
        command.args[..4],
        ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"]
    );
    assert!(command.args.contains(&"dev@example.test".to_owned()));
    assert_eq!(command.args.last().unwrap(), "'true'");
}

#[test]
fn ssh_bare_check_permission_denied_recommends_ssh_copy_id_with_the_identity() {
    let executor = FakeExecutor::new([Ok(failed(
        b"dev@example.test: Permission denied (publickey).",
    ))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks[0].status, CheckStatus::Fixable);
    assert_eq!(
        checks[0].remediation.as_deref(),
        Some(
            "Install your public key on the host with `ssh-copy-id -i /home/dev/.ssh/id_ed25519.pub dev@example.test`."
        )
    );
}

#[test]
fn ssh_bare_check_host_key_failure_recommends_keyscan_with_a_fingerprint_caution() {
    let executor = FakeExecutor::new([Ok(failed(
        b"Host key verification failed.\nNo ECDSA host key is known for example.test",
    ))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks[0].status, CheckStatus::Fixable);
    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("ssh-keyscan -H example.test >> ~/.ssh/known_hosts"),
        "{remediation}"
    );
    assert!(
        remediation.contains("Verify the fingerprint"),
        "{remediation}"
    );
}

#[test]
fn ssh_bare_check_without_an_ssh_client_recommends_installing_openssh() {
    let executor = FakeExecutor::new([Err(anyhow::Error::new(std::io::Error::from(
        std::io::ErrorKind::NotFound,
    ))
    .context("run ssh for verify SSH connectivity"))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks[0].status, CheckStatus::Fixable);
    assert_eq!(
        checks[0].remediation.as_deref(),
        Some(SSH_MISSING_REMEDIATION)
    );
}

#[test]
fn ssh_bare_check_probe_timeout_recommends_checking_the_host_is_reachable() {
    let executor = FakeExecutor::new([Err(anyhow::Error::new(CommandTimedOut {
        program: "ssh".into(),
        purpose: "verify SSH connectivity".into(),
        timeout: std::time::Duration::from_secs(15),
    }))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks[0].status, CheckStatus::Fixable);
    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("example.test is up and reachable"),
        "{remediation}"
    );
    assert!(!remediation.contains("openssh-client"), "{remediation}");
    assert!(
        checks[0]
            .detail
            .contains("did not answer within 15 seconds"),
        "{}",
        checks[0].detail
    );
}

#[test]
fn ssh_bare_check_connect_timeout_recommends_checking_the_host_is_reachable() {
    let executor = FakeExecutor::new([Ok(failed(
        b"ssh: connect to host example.test port 22: Connection timed out",
    ))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("example.test is up and reachable"),
        "{remediation}"
    );
}

#[test]
fn ssh_bare_check_falls_back_to_quoting_an_unrecognized_ssh_failure() {
    let executor = FakeExecutor::new([Ok(failed(
        b"kex_exchange_identification: read: Connection reset by peer",
    ))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("Connection reset by peer"),
        "{remediation}"
    );
    assert!(
        remediation.contains("Run `ssh dev@example.test true` by hand"),
        "{remediation}"
    );
}

#[test]
fn ssh_bare_check_other_launch_failure_falls_back_to_running_ssh_by_hand() {
    let executor = FakeExecutor::new([Err(anyhow!(
        "operation cancelled while verify SSH connectivity"
    ))]);

    let checks = ssh_bare_checks(Ok(&ssh_bare_config()), &executor);

    assert_eq!(checks[0].status, CheckStatus::Fixable);
    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("Run `ssh dev@example.test true` by hand"),
        "{remediation}"
    );
    assert!(!remediation.contains("openssh-client"), "{remediation}");
}

#[test]
fn ssh_bare_checks_are_skipped_without_a_valid_config() {
    let executor = FakeExecutor::new([]);

    assert!(ssh_bare_checks(Err(ConfigGap::Unreadable), &executor).is_empty());
    assert!(executor.commands.borrow().is_empty());
}

/// With no target blocks in config.toml, the dashboard still offers the
/// built-in `podman` target, and doctor's engine and image checks cover it;
/// the worker-binary checks said "No container target is configured" and the
/// freshness check for Linux targets was missing (launch finding R5-2). A
/// built-in target whose engine is unavailable needs no worker.
#[test]
fn worker_checks_cover_a_built_in_target_whose_engine_is_ready() {
    let engines = [
        DoctorCheck::ready(
            "runtime.podman",
            "Rootless Podman",
            "Podman 5.7.0 has a valid rootless UID map.",
        ),
        DoctorCheck::unsupported(
            "runtime.docker",
            "Docker",
            "Docker is not available, so the built-in `docker` target is marked unavailable.",
        ),
    ];

    let offered = offered_targets(&Config::default(), &engines);

    let ids = worker_binary_checks(Ok(&offered))
        .into_iter()
        .map(|check| check.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["worker.podman"]);
    assert_eq!(
        container_worker_architectures(Ok(&offered)),
        [normalized_worker_architecture(std::env::consts::ARCH)]
    );
}

/// A target the user configured is checked whatever its engine's state, as
/// before.
#[test]
fn worker_checks_keep_a_configured_target_whose_engine_is_unavailable() {
    let config = config_with([(
        "pd",
        TargetTemplate::LocalPodman {
            container: container("example.test/own:latest"),
        },
    )]);
    let engines = [DoctorCheck::unsupported(
        "runtime.podman",
        "Rootless Podman",
        "Podman is not installed.",
    )];

    let offered = offered_targets(&config, &engines);

    let ids = worker_binary_checks(Ok(&offered))
        .into_iter()
        .map(|check| check.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["worker.pd"]);
}

#[test]
fn worker_check_for_an_ssh_podman_target_without_platform_is_unsupported() {
    let config = config_with([(
        "remote",
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = worker_binary_checks(Ok(&config));

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "worker.remote");
    assert_eq!(checks[0].status, CheckStatus::Unsupported);
    assert_eq!(
        checks[0].detail,
        "Set `platform` on this ssh-podman target to check its worker binary; the remote architecture is unknown until provisioning."
    );
}

#[test]
fn worker_check_for_an_ssh_podman_target_with_platform_uses_the_normal_check() {
    let mut remote = container("ubuntu:24.04");
    remote.platform = Some("linux/amd64".into());
    let config = config_with([(
        "remote",
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: remote,
        },
    )]);

    let checks = worker_binary_checks(Ok(&config));

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "worker.remote");
    assert_ne!(checks[0].status, CheckStatus::Unsupported);
    assert!(checks[0].detail.contains("x86_64-unknown-linux-musl"));
}

#[test]
fn worker_check_for_an_ssh_docker_target_hints_at_remote_architecture() {
    let config = config_with([(
        "remote",
        TargetTemplate::SshDocker {
            ssh: ssh_connection(),
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = worker_binary_checks(Ok(&config));

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Unsupported);
    assert_eq!(
        checks[0].detail,
        "Set `platform` on this ssh-docker target to check its worker binary; the remote architecture is unknown until provisioning."
    );
}

#[test]
fn an_unauthenticated_profile_is_fixed_by_hel_login_for_that_profile() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home,
        environment: std::collections::BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let config = Config {
        profiles: [("work".to_owned(), profile.clone())].into_iter().collect(),
        ..Config::default()
    };

    let executor = FakeExecutor::new([Ok(output(br#"{"loggedIn":false}"#))]);
    let checks = harness_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Fixable);
    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("mj login --profile work"),
        "{remediation}"
    );
    // The underlying command is quoted from the one place that verified it,
    // so doctor cannot recommend something `mj login` does not run.
    let (program, arguments) = login_command(&profile).expect("OAuth profile has a login command");
    assert!(
        remediation.contains(&format!("`{program} {}`", arguments.join(" "))),
        "{remediation}"
    );
}

fn claude_config_with_home<const N: usize>(
    home: &std::path::Path,
    targets: [(&str, TargetTemplate); N],
) -> Config {
    Config {
        profiles: [(
            "work".to_owned(),
            HarnessProfile {
                enabled: true,
                kind: HarnessKind::Claude,
                home: home.to_path_buf(),
                environment: std::collections::BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        )]
        .into_iter()
        .collect(),
        targets: targets
            .into_iter()
            .map(|(id, target)| (id.to_owned(), target))
            .collect(),
        ..Config::default()
    }
}

/// A local session runs from a staged copy of whatever home the profile names,
/// on macOS as elsewhere, so a Claude home other than `~/.claude` is used as
/// configured and is not reported.
#[test]
fn doctor_accepts_a_claude_home_other_than_the_default_on_this_machine() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("claude-work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".credentials.json"), b"{}").unwrap();
    let config = claude_config_with_home(&home, [("localhost", TargetTemplate::LocalBare)]);

    let executor = FakeExecutor::new([]);
    let checks = harness_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Ready, "{:?}", checks[0]);
}

#[test]
fn doctor_reports_disabled_profiles_without_probing_them() {
    let profile = HarnessProfile {
        enabled: false,
        kind: HarnessKind::Claude,
        home: PathBuf::from("/missing/disabled-profile"),
        environment: std::collections::BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let config = Config {
        profiles: [("retired".to_owned(), profile)].into_iter().collect(),
        ..Config::default()
    };
    let executor = FakeExecutor::new([]);

    let checks = harness_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Ready);
    assert!(checks[0].detail.contains("disabled"));
    assert!(executor.commands.borrow().is_empty());
}

#[test]
fn harness_discovery_reports_each_authentication_state() {
    let check = harness_discovery_check_from(
        &[
            DiscoveredHome {
                kind: HarnessKind::Codex,
                path: "/agents/codex".into(),
                authenticated: true,
            },
            DiscoveredHome {
                kind: HarnessKind::Kimi,
                path: "/agents/kimi".into(),
                authenticated: false,
            },
        ],
        true,
        "ctrl+b s",
    );

    assert_eq!(check.status, CheckStatus::Ready);
    assert!(
        check
            .detail
            .contains("Codex at /agents/codex (authenticated)")
    );
    assert!(
        check
            .detail
            .contains("Kimi Code at /agents/kimi (not authenticated)")
    );
}

/// Settings opens with `prefix+s`; F7 is not bound, so no fix may send the
/// user there, and every fix names the key the configuration binds.
#[test]
fn settings_fixes_name_the_bound_settings_key() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.toml");
    let (_, checks) = configuration_checks(&missing);
    let empty = Config::default();
    let executor = FakeExecutor::new([]);
    let checks = checks
        .into_iter()
        .chain(harness_checks(Ok(&empty), &executor))
        .collect::<Vec<_>>();
    for check in &checks {
        let remediation = check.remediation.as_deref().unwrap_or_default();
        assert!(!remediation.contains("F7"), "{remediation}");
    }
    assert!(
        checks
            .iter()
            .filter_map(|check| check.remediation.as_deref())
            .all(|fix| !fix.contains("Settings") || fix.contains("ctrl+b s")),
        "{checks:?}"
    );
}

/// An installed user has no repository checkout, so the Podman fix links the
/// published guide, and the human report prints the fix once, not also inside
/// the detail.
#[test]
fn missing_podman_names_its_fix_once_and_links_the_published_guide() {
    for response in [
        Err(anyhow!("No such file or directory (os error 2)")),
        Ok(failed(b"podman: command not found")),
    ] {
        let check = local_podman_runtime_check(&FakeExecutor::new([response]));
        assert_eq!(check.status, CheckStatus::Fixable);
        let remediation = check.remediation.as_deref().unwrap();
        let mut human = Vec::new();
        render_human(std::slice::from_ref(&check), &mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(!human.contains("docs/PODMAN.md"), "{human}");
        assert!(
            remediation.contains("https://mjolnir.brokk.ai/podman/"),
            "{remediation}"
        );
        assert_eq!(human.matches("sudo apt install").count(), 1, "{human}");
    }
}

#[test]
fn missing_harness_homes_are_fixable_without_a_configured_profile() {
    let check = harness_discovery_check_from(&[], false, "ctrl+b s");

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(
        check.remediation.as_deref(),
        Some(
            "Install and sign in to a supported harness, then open Mjolnir, press ctrl+b s for Settings, and choose Agent Profiles."
        )
    );
}

#[test]
fn apple_container_is_unsupported_on_intel_macs() {
    let executor = FakeExecutor::new([]);

    let check = apple_container_check(
        &ApplePlatform::Macos {
            architecture: "x86_64".into(),
            major_version: 26,
        },
        &executor,
        false,
        DEFAULT_CONTAINER_IMAGE.into(),
    );

    assert_eq!(check.status, CheckStatus::Unsupported);
    assert!(check.detail.contains("Intel Macs"));
    assert!(executor.commands.borrow().is_empty());
}

#[test]
fn apple_container_is_unsupported_before_macos_26() {
    let executor = FakeExecutor::new([]);

    let check = apple_container_check(
        &ApplePlatform::Macos {
            architecture: "aarch64".into(),
            major_version: 25,
        },
        &executor,
        false,
        DEFAULT_CONTAINER_IMAGE.into(),
    );

    assert_eq!(check.status, CheckStatus::Unsupported);
    assert!(check.detail.contains("macOS 26"));
}

#[test]
fn apple_container_not_installed_has_official_package_remediation() {
    let executor = FakeExecutor::new([Err(anyhow!("No such file or directory"))]);

    let check = apple_container_check(
        &ApplePlatform::Macos {
            architecture: "aarch64".into(),
            major_version: 26,
        },
        &executor,
        false,
        DEFAULT_CONTAINER_IMAGE.into(),
    );

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(
        check.remediation.as_deref(),
        Some(
            format!("Install the official signed package: {APPLE_CONTAINER_INSTALL_URL}").as_str()
        )
    );
}

#[test]
fn apple_container_stopped_daemon_has_start_remediation() {
    let executor = FakeExecutor::new([
        Ok(output(b"container version 1\n")),
        Ok(CommandOutput {
            status: 1,
            stdout: vec![],
            stderr: b"daemon is not running".to_vec(),
        }),
    ]);

    let check = apple_container_check(
        &ApplePlatform::Macos {
            architecture: "aarch64".into(),
            major_version: 26,
        },
        &executor,
        false,
        DEFAULT_CONTAINER_IMAGE.into(),
    );

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(
        check.remediation.as_deref(),
        Some("Run `container system start`.")
    );
}

#[test]
fn apple_container_is_ready_only_after_the_opt_in_smoke_test() {
    let executor = FakeExecutor::new([
        Ok(output(b"container version 1\n")),
        Ok(output(b"running\n")),
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"removed\n")),
    ]);

    let check = apple_container_check(
        &ApplePlatform::Macos {
            architecture: "aarch64".into(),
            major_version: 26,
        },
        &executor,
        true,
        DEFAULT_CONTAINER_IMAGE.into(),
    );

    assert_eq!(check.status, CheckStatus::Ready);
    assert_eq!(executor.commands.borrow().len(), 5);
    assert_eq!(executor.commands.borrow()[2].args[0], "run");
    assert_eq!(executor.commands.borrow()[3].args[0], "exec");
    assert_eq!(executor.commands.borrow()[4].args[0], "rm");
}

#[test]
fn linux_reports_apple_container_as_macos_only() {
    let executor = FakeExecutor::new([]);
    let check = apple_container_check(
        &ApplePlatform::Linux,
        &executor,
        false,
        DEFAULT_CONTAINER_IMAGE.into(),
    );
    assert_eq!(check.status, CheckStatus::Unsupported);
    assert_eq!(check.detail, "macOS only");
}

fn aws_target(launch_template: &str) -> TargetTemplate {
    TargetTemplate::AwsEc2 {
        aws_profile: Some("hel".into()),
        region: "us-east-1".into(),
        launch_template: launch_template.to_owned(),
        launch_template_version: None,
        ssh_user: "ubuntu".into(),
        address_source: mj_core::config::AwsAddressSource::default(),
        identity_file: None,
        ssh_args: vec![],
    }
}

#[test]
fn aws_check_is_ready_after_the_cli_credential_and_launch_template_probes() {
    let executor = FakeExecutor::new([
        Ok(output(b"aws-cli/2.17.0\n")),
        Ok(output(b"{\"Account\":\"123456789012\"}\n")),
        Ok(output(b"{\"LaunchTemplates\":[{}]}\n")),
    ]);
    let config = config_with([("aws", aws_target("hel-runson"))]);

    let checks = aws_checks(Ok(&config), &executor);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "runtime.aws-ec2.aws");
    assert_eq!(checks[0].status, CheckStatus::Ready);
    let commands = executor.commands.borrow();
    assert!(commands.iter().all(|command| command.program == "aws"));
    // Profile and region are applied exactly as provisioning applies them.
    assert_eq!(
        commands[2].args,
        vec![
            "--profile",
            "hel",
            "--region",
            "us-east-1",
            "ec2",
            "describe-launch-templates",
            "--launch-template-names",
            "hel-runson",
            "--output",
            "json"
        ]
    );
}

#[test]
fn aws_check_is_fixable_with_an_install_remediation_without_the_cli() {
    let executor = FakeExecutor::new([Err(anyhow!("No such file or directory"))]);

    let check = aws_target_check("aws", None, "us-east-1", "hel-runson", &executor);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(
        check
            .remediation
            .as_deref()
            .unwrap()
            .contains(AWS_CLI_INSTALL_URL)
    );
    assert_eq!(executor.commands.borrow().len(), 1);
}

#[test]
fn aws_check_is_fixable_with_a_sign_in_remediation_for_expired_credentials() {
    let executor = FakeExecutor::new([
        Ok(output(b"aws-cli/2.17.0\n")),
        Ok(failed(b"ExpiredToken: the security token has expired")),
    ]);

    let check = aws_target_check("aws", Some("hel"), "us-east-1", "hel-runson", &executor);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(check.detail.contains("ExpiredToken"));
    assert_eq!(
        check.remediation.as_deref(),
        Some(
            "Configure credentials with `aws configure --profile hel`, or sign in with `aws sso login --profile hel`."
        )
    );
}

#[test]
fn aws_check_is_fixable_when_the_launch_template_is_missing() {
    let executor = FakeExecutor::new([
        Ok(output(b"aws-cli/2.17.0\n")),
        Ok(output(b"{\"Account\":\"123456789012\"}\n")),
        Ok(failed(b"InvalidLaunchTemplateName.NotFoundException")),
    ]);

    let check = aws_target_check("aws", Some("hel"), "us-east-1", "lt-0123456789", &executor);

    assert_eq!(check.status, CheckStatus::Fixable);
    assert!(check.detail.contains("was not found in us-east-1"));
    // An `lt-` value is a template id, not a name.
    assert_eq!(
        executor.commands.borrow()[2].args[6],
        "--launch-template-ids"
    );
}

#[test]
fn aws_checks_are_skipped_for_configs_without_an_aws_target() {
    let executor = FakeExecutor::new([]);
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);

    assert!(aws_checks(Ok(&config), &executor).is_empty());
    assert!(aws_checks(Err(ConfigGap::Unreadable), &executor).is_empty());
    assert!(executor.commands.borrow().is_empty());
}

#[test]
fn apple_container_daemon_check_is_ready_once_the_daemon_answers() {
    let executor = FakeExecutor::new([
        Ok(output(b"container version 1\n")),
        Ok(output(b"running\n")),
    ]);

    let check = apple_container_daemon_check(&executor);

    assert_eq!(check.status, CheckStatus::Ready);
    assert_eq!(executor.commands.borrow().len(), 2);
}

#[test]
fn a_worker_rebuilt_after_the_daemon_started_is_reported_as_changed() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("mj-worker");
    std::fs::write(&worker, b"worker").unwrap();
    let started_at = "2026-09-17T23:48:36Z";
    let started: SystemTime = chrono::DateTime::parse_from_rfc3339(started_at)
        .unwrap()
        .into();
    let file = std::fs::File::options().write(true).open(&worker).unwrap();

    file.set_times(std::fs::FileTimes::new().set_modified(started - Duration::from_secs(60)))
        .unwrap();
    assert!(
        !worker_changed_since_daemon_start(&worker, started_at).unwrap(),
        "a worker older than the daemon is the one that daemon pinned"
    );

    file.set_times(std::fs::FileTimes::new().set_modified(started + Duration::from_secs(60)))
        .unwrap();
    assert!(
        worker_changed_since_daemon_start(&worker, started_at).unwrap(),
        "a worker rebuilt after the daemon started is not the one it serves"
    );
}

#[test]
fn container_platforms_name_the_worker_architecture_the_daemon_resolves() {
    assert_eq!(normalized_worker_architecture("amd64"), "x86_64");
    assert_eq!(normalized_worker_architecture("arm64"), "aarch64");
    assert_eq!(normalized_worker_architecture("x86_64"), "x86_64");
}

#[test]
fn every_configured_container_architecture_is_checked_once() {
    let amd64 = |image: &str| {
        let mut template = container(image);
        template.platform = Some("linux/amd64".into());
        template
    };
    let mut arm64 = container("ubuntu:24.04");
    arm64.platform = Some("linux/arm64".into());
    let config = config_with([
        (
            "one",
            TargetTemplate::LocalPodman {
                container: amd64("ubuntu:24.04"),
            },
        ),
        (
            "two",
            TargetTemplate::LocalDocker {
                container: amd64("ghcr.io/example/dev:1"),
            },
        ),
        (
            "three",
            TargetTemplate::SshPodman {
                ssh: ssh_connection(),
                container: arm64,
            },
        ),
        (
            "bare",
            TargetTemplate::SshBare {
                ssh: ssh_connection(),
                permissions: mj_core::config::PermissionMode::Yolo,
                workspace_prefix: PathBuf::from("workspaces"),
            },
        ),
    ]);

    let mut architectures = container_worker_architectures(Ok(&config));
    architectures.sort();
    assert_eq!(
        architectures,
        vec!["aarch64".to_owned(), "x86_64".to_owned()],
        "each architecture is reported once, and a non-container target adds none"
    );
    assert!(container_worker_architectures(Err(ConfigGap::Unreadable)).is_empty());
}

#[test]
fn linux_instructions_embed_podman_postconditions_and_doctor_loop() {
    let instructions = setup_instructions(InstructionsPlatform::Linux);
    assert!(instructions.contains("mj doctor --json"));
    assert!(instructions.contains("mj doctor --json --smoke"));
    assert!(instructions.contains("podman unshare cat /proc/self/uid_map"));
    assert!(instructions.contains("Podman **4.3.0 or newer**"));
    assert!(instructions.contains("kind = \"docker\""));
    assert!(instructions.contains("--opt type=overlay"));
}

#[test]
fn setup_instructions_name_mjolnir_and_the_local_bare_prerequisites() {
    for platform in [InstructionsPlatform::Linux, InstructionsPlatform::Macos] {
        let instructions = setup_instructions(platform);
        assert!(!instructions.contains("Hel"), "{instructions}");
        assert!(instructions.contains("## Local bare runtime"));
        assert!(instructions.contains("Node.js 22 or newer and npm"));
        assert!(instructions.ends_with('\n'));
        assert!(!instructions.contains("](#"), "{instructions}");
        assert!(!instructions.contains("keep-id:uid=,"));
        assert!(!instructions.contains("Disposable EC2"));
    }
    let macos = setup_instructions(InstructionsPlatform::Macos);
    assert!(!macos.contains("local Podman"), "{macos}");
}

/// Releases before this one wrote Mjolnir's own refs into user repositories
/// and could leave a scratch index behind. Doctor tells the user what is there
/// and how to remove it, and changes nothing itself.
#[test]
fn review_leftovers_are_reported_and_left_alone() {
    let repository = tempfile::tempdir().unwrap();
    let git_dir = repository.path().join(".git");
    std::fs::create_dir_all(git_dir.join("refs/hel")).unwrap();
    std::fs::write(git_dir.join("refs/hel/review-capture"), "a".repeat(41)).unwrap();
    std::fs::write(
        git_dir.join("packed-refs"),
        format!("{} refs/hel/review-baseline\n", "b".repeat(40)),
    )
    .unwrap();
    let scratch = git_dir.join("hel-review-index-AbCdEf");
    std::fs::write(&scratch, b"scratch").unwrap();

    let residue = crate::doctor::review_residue(repository.path());

    assert_eq!(
        residue.refs,
        ["refs/hel/review-baseline", "refs/hel/review-capture"]
    );
    assert_eq!(residue.scratch_indexes, std::slice::from_ref(&scratch));
    assert!(
        scratch.is_file() && git_dir.join("refs/hel/review-capture").is_file(),
        "doctor reports; it never removes anything from a user's repository"
    );
}

#[test]
fn a_repository_with_no_leftovers_reports_none() {
    let repository = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository.path().join(".git/refs/heads")).unwrap();

    let residue = crate::doctor::review_residue(repository.path());

    assert!(residue.refs.is_empty());
    assert!(residue.scratch_indexes.is_empty());
}

/// R2-6: every running session's own managed clone holds
/// `refs/hel/review-baseline`, and doctor reported each one as left in a
/// repository Mjolnir does not own. A managed checkout is Mjolnir's working
/// state, and a repository a live session works in has its refs in use.
#[test]
fn review_leftovers_skip_managed_checkouts_and_repositories_in_use() {
    use crate::controller::test_support::checkpoint_test_session;
    use mj_core::state::{
        ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeTarget, SessionState,
    };
    use std::path::PathBuf;

    let project = PathBuf::from("/srv/project");
    let managed = |id: &str, kind: ManagedCheckoutKind, state: SessionState| {
        let directory = match kind {
            ManagedCheckoutKind::Clone => "clones",
            ManagedCheckoutKind::Worktree => "worktrees",
        };
        let root = project.join(".mj").join(directory).join(id);
        let mut session = checkpoint_test_session(id);
        session.state = state;
        session.project_directory = Some(root.clone());
        session.managed_worktree = Some(ManagedWorktree {
            kind,
            source_project_directory: project.clone(),
            source_repository: project.clone(),
            worktree_root: root,
            branch: format!("mj/{id}"),
            target: ManagedWorktreeTarget::Local,
            base_commit: None,
        });
        session
    };
    let running_clone = managed("a1", ManagedCheckoutKind::Clone, SessionState::Running);
    let stopped_clone = managed("b2", ManagedCheckoutKind::Clone, SessionState::Stopped);
    let mut live_bare = checkpoint_test_session("c3");
    live_bare.project_directory = Some(PathBuf::from("/home/me/live"));
    let mut stopped_bare = checkpoint_test_session("d4");
    stopped_bare.state = SessionState::Stopped;
    stopped_bare.project_directory = Some(PathBuf::from("/home/me/stopped"));

    let sessions = [&running_clone, &stopped_clone, &live_bare, &stopped_bare];
    let repositories = crate::doctor::review_residue_repositories(
        [project.clone(), PathBuf::from("/srv/other/.mj/clones/e5/")],
        &sessions,
    );
    assert_eq!(
        repositories,
        [PathBuf::from("/home/me/stopped"), project.clone()],
        "only repositories a person works in by hand, and not while a live session uses them"
    );

    // A linked worktree shares its refs with the repository it came from, so
    // a live worktree session keeps that repository out of the report.
    let running_worktree = managed("f6", ManagedCheckoutKind::Worktree, SessionState::Running);
    let repositories =
        crate::doctor::review_residue_repositories([project.clone()], &[&running_worktree]);
    assert!(repositories.is_empty(), "{repositories:?}");
}

fn doctor_profile(kind: HarnessKind, home: PathBuf) -> HarnessProfile {
    HarnessProfile {
        // Disabled, so the check reports the profile without probing its
        // login: the summary sentence is the subject here.
        enabled: false,
        kind,
        home,
        environment: Default::default(),
        context_window_bytes: None,
        guardian_review_model: None,
    }
}

fn subagent_config(eligible: &[&str], profiles: &[&str]) -> Config {
    Config {
        profiles: profiles
            .iter()
            .map(|id| {
                (
                    (*id).to_owned(),
                    HarnessProfile {
                        enabled: true,
                        ..doctor_profile(HarnessKind::Codex, PathBuf::from("/nonexistent").join(id))
                    },
                )
            })
            .collect(),
        subagents: mj_core::config::SubagentConfig {
            eligible_profiles: eligible.iter().map(|id| ((*id).to_owned(), true)).collect(),
            ..Default::default()
        },
        ..Config::default()
    }
}

#[test]
fn the_subagent_policy_names_how_many_children_and_which_profiles() {
    let config = subagent_config(&["codex2", "deepseek"], &["codex", "codex2", "deepseek"]);
    let checks = subagent_eligibility_checks(Ok(&config));
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].id, "subagents.policy");
    assert_eq!(checks[0].status, CheckStatus::Ready);
    assert_eq!(
        checks[0].detail,
        "Claude and Codex sessions may opt in, up to 6 sub-agents at once per session. A \
         session's sub-agents may use its own profile and: codex2, deepseek."
    );

    let alone = subagent_config(&[], &["codex"]);
    assert!(
        subagent_eligibility_checks(Ok(&alone))[0]
            .detail
            .ends_with("its own profile and: no other profile."),
    );

    // The deprecated global switch no longer changes this check's detail.
    let mut off = subagent_config(&["codex"], &["codex"]);
    off.subagents.enabled = false;
    assert_eq!(
        subagent_eligibility_checks(Ok(&off))[0].detail,
        subagent_eligibility_checks(Ok(&subagent_config(&["codex"], &["codex"])))[0].detail
    );
}

/// An eligible id that names no profile is a configuration error, not a
/// warning: the file fails to load, and doctor's configuration check says so.
#[test]
fn an_eligible_id_that_names_no_profile_fails_the_configuration_check() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = {}\n\n[subagents.eligible_profiles]\ncodx = true\n",
            mj_core::config::CONFIG_VERSION
        ),
    )
    .unwrap();

    let checks = run_with_config_path(
        &path,
        &AlwaysFailingExecutor,
        ApplePlatform::Linux,
        DoctorOptions { smoke: false },
    );

    let config = checks
        .iter()
        .find(|check| check.id == "config")
        .expect("the configuration is checked");
    assert_eq!(config.status, CheckStatus::Fixable);
    assert!(
        config.detail.contains("\"codx\" is not defined"),
        "{}",
        config.detail
    );
}

#[test]
fn each_profile_line_says_where_its_quota_comes_from_and_who_may_delegate_to_it() {
    let homes = tempfile::tempdir().unwrap();
    let chatgpt = homes.path().join("codex");
    let deepseek = homes.path().join("deepseek");
    let keyless = homes.path().join("keyless");
    for home in [&chatgpt, &deepseek, &keyless] {
        std::fs::create_dir_all(home).unwrap();
    }
    let provider = "model_provider = \"deepseek\"\n\n[model_providers.deepseek]\n\
                    base_url = \"https://api.deepseek.com/v1\"\nwire_api = \"responses\"\n\
                    env_key = \"DEEPSEEK_API_KEY\"\n";
    std::fs::write(deepseek.join("config.toml"), provider).unwrap();
    std::fs::write(keyless.join("config.toml"), provider).unwrap();
    let mut deepseek_profile = doctor_profile(HarnessKind::Codex, deepseek);
    deepseek_profile
        .environment
        .insert("DEEPSEEK_API_KEY".into(), "sk-test".into());
    let config = Config {
        profiles: [
            ("codex", doctor_profile(HarnessKind::Codex, chatgpt)),
            ("deepseek", deepseek_profile),
            ("keyless", doctor_profile(HarnessKind::Codex, keyless)),
            (
                "claude",
                doctor_profile(HarnessKind::Claude, homes.path().join("claude")),
            ),
        ]
        .into_iter()
        .map(|(id, profile)| (id.to_owned(), profile))
        .collect(),
        subagents: mj_core::config::SubagentConfig {
            eligible_profiles: [("codex".to_owned(), true), ("deepseek".to_owned(), true)]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        ..Config::default()
    };

    let checks = harness_checks(Ok(&config), &AlwaysFailingExecutor);
    let detail = |id: &str| {
        checks
            .iter()
            .find(|check| check.id == format!("harness.{id}"))
            .unwrap_or_else(|| panic!("{id} is reported"))
            .detail
            .clone()
    };
    assert_eq!(
        detail("codex"),
        "Codex; ChatGPT subscription quota; any session's sub-agents may use it. Profile is \
         disabled; home and authentication checks were skipped."
    );
    assert!(
        detail("deepseek").starts_with(
            "Codex; pay-per-use through api.deepseek.com, counted as 100% left when choosing a \
             sub-agent's profile; any session's sub-agents may use it."
        ),
        "{}",
        detail("deepseek")
    );
    assert!(
        detail("keyless").contains("custom provider \"deepseek\" has no API key"),
        "{}",
        detail("keyless")
    );
    assert!(
        detail("claude").starts_with(
            "Claude Code; Claude subscription quota; only its own sessions' sub-agents may use it."
        ),
        "{}",
        detail("claude")
    );
}
