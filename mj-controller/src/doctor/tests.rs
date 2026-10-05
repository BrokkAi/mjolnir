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

fn container(image: &str) -> ContainerTemplate {
    ContainerTemplate {
        build_cache: None,
        image: image.to_owned(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: Default::default(),
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

// Hard-won: 43be5270: doctor treated a newer config as invalid TOML and advised replacing it.
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
// Hard-won: 9a0c1c2b: doctor incorrectly failed valid project-directory sessions when no saved bundle existed.
#[test]
fn a_config_without_a_bundle_is_ready_for_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: directory.path().join("codex-home"),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
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

/// With no config.toml, doctor said "Fix config.toml, then rerun `mj doctor
/// --json`" for two checks, although there was no file to fix and a person
/// reading the report does not need `--json` (launch finding R14-3,
/// reverify-14 cli/020). Those checks now say how to write the first
/// configuration, and a broken file's checks point at plain `mj doctor`.
// Hard-won: 2eb56a91: missing config was called invalid and remediation pointed to a nonexistent file and --json.
#[test]
fn checks_waiting_for_a_config_say_how_to_get_one_without_json() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("config.toml");
    let run = |path: &Path| {
        run_with_config_path(
            path,
            &AlwaysFailingExecutor,
            ApplePlatform::Linux,
            DoctorOptions { smoke: false },
        )
    };
    let find = |checks: &[DoctorCheck], id: &str| {
        checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("{id} is reported"))
            .clone()
    };

    let checks = run(&missing);
    for check in &checks {
        let text = format!(
            "{} {}",
            check.detail,
            check.remediation.as_deref().unwrap_or_default()
        );
        assert!(
            !text.contains("Fix config.toml")
                && !text.contains("config.toml is valid")
                && !text.contains("--json"),
            "{} advises fixing a file that does not exist: {text}",
            check.id
        );
    }
    for id in ["harness.profiles", "worker.containers"] {
        let check = find(&checks, id);
        assert_eq!(check.status, CheckStatus::Fixable, "{id}");
        let remediation = check.remediation.unwrap_or_default();
        assert!(
            remediation.contains("Run `mj`")
                && remediation.contains("`mj setup`")
                && remediation.contains("rerun `mj doctor`"),
            "{id}: {remediation}"
        );
    }

    let invalid = directory.path().join("invalid.toml");
    std::fs::write(&invalid, "version = \n").unwrap();
    let checks = run(&invalid);
    for id in ["harness.profiles", "worker.containers"] {
        assert_eq!(
            find(&checks, id).remediation.as_deref(),
            Some("Fix config.toml, then rerun `mj doctor`."),
            "{id}"
        );
    }
}

/// A newer build's configuration is the one case doctor cannot read and the
/// user cannot repair in the file, so no check in the run may send them to fix
/// TOML; the checks that depend on a configuration skip and say why.
// Hard-won: a8ec1783: a newer config produced contradictory advice to fix the TOML instead of update Mjolnir.
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
        let executor = FakeExecutor::new([Ok(output("Linux x86_64")), response]);
        let checks = build_cache_checks(Ok(&config), &executor);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, expected_status);
        assert!(checks[0].detail.contains(expected_text), "{:?}", checks[0]);
    }
}

/// Launch finding R5-3: doctor and `mj setup` gave a missing Docker the raw
/// error chain ("run docker for check Docker daemon: No such file or
/// directory (os error 2)"). They now say it is not installed and how to
/// fix that.
// Hard-won: 028b8327: Docker absence produced inconsistent raw error chains instead of the shared not-installed sentence.
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

/// The dashboard lists the built-in `docker` target (and downloads its image)
/// without any configured Docker target. Doctor checks the same target set:
/// it probes Docker and the image for the built-in target, and when Docker
/// is missing it reports the built-in target as unavailable, not as a fault.
// Hard-won: ecc10219: doctor omitted the built-in Docker target that the dashboard already listed and checked.
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
    assert!(all_ready(&checks));

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
    assert!(checks[0].remediation.is_some());
    assert!(!all_ready(&checks));

    let available = FakeExecutor::new([
        Ok(output(b"29.0.1 linux\n")),
        Ok(output(b"image metadata\n")),
        Ok(output(b"image metadata\n")),
    ]);
    let checks = docker_checks(Ok(&configured), &available, false);
    assert!(
        checks
            .iter()
            .all(|check| check.status == CheckStatus::Ready),
        "{checks:?}"
    );
    assert!(all_ready(&checks));
}

/// Launch finding R3-3: a Setup save once wrote the built-in `[targets.docker]`
/// and `[targets.podman]` blocks into config.toml, and doctor then reported a
/// missing engine as a fault to fix. A block identical to the built-in target
/// is still the built-in target.
// Hard-won: f15ebc51: Setup saved an unchanged built-in target and doctor wrongly treated it as user configured.
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
    let podman = podman_checks(
        Ok(&written),
        &AlwaysFailingExecutor,
        false,
        &ApplePlatform::Linux,
    );
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

// Hard-won: ecc10219: doctor omitted the built-in Podman target that the dashboard already listed and checked.
#[test]
fn podman_checks_cover_the_built_in_podman_target() {
    let checks = podman_checks(
        Ok(&Config::default()),
        &AlwaysFailingExecutor,
        false,
        &ApplePlatform::Linux,
    );
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Unsupported);
    assert!(
        checks[0].detail.contains("built-in `podman` target"),
        "{}",
        checks[0].detail
    );
}

/// Docker Desktop's daemon runs in a VM that cannot overlay host
/// directories, so the smoke test checks the read-only attachment sessions
/// get there instead of failing on the overlay mount (#1152).
#[cfg(target_os = "linux")]
// Hard-won: 3921dfb5: Docker Desktop smoke failed OverlayFS on a VM share and the smoke path lacked read-only attachment support.
#[test]
fn docker_desktop_smoke_verifies_the_read_only_attachment() {
    let executor = FakeExecutor::new([
        Ok(output(b"Docker Desktop 4.40.0 (187762)\n")),
        Ok(output(b"created\n")),
        Ok(output(b"ok\n")),
        Ok(output(b"removed\n")),
        Ok(output(b"Docker Desktop 4.40.0 (187762)\n")),
    ]);

    let check = docker_image_check("docker", "ubuntu:24.04", &executor, true);

    assert_eq!(check.status, CheckStatus::Ready, "{}", check.detail);
    assert!(
        check
            .detail
            .contains("read-only attachment smoke test passed")
    );
    let commands = executor.commands.borrow();
    assert!(commands.iter().all(|command| {
        !command
            .args
            .iter()
            .any(|arg| arg.starts_with("type=overlay"))
    }));
    let probe = commands
        .iter()
        .find(|command| command.args.first().map(String::as_str) == Some("exec"))
        .expect("smoke probe");
    assert!(probe.args.last().unwrap().contains("! printf"));
}

/// A requested smoke test that fails is a fault even for the built-in
/// `docker` target, so `mj doctor --smoke` exits non-zero (#1152).
// Hard-won: 3921dfb5: a failed requested Docker smoke test on the built-in target incorrectly left doctor exit status successful.
#[test]
fn failed_smoke_test_of_a_builtin_target_stays_fixable() {
    let failed = DoctorCheck::fixable(
        "runtime.docker.image.docker",
        "Docker image for target docker",
        "Disposable run/exec/remove smoke test failed",
        "Fix the configured image or Docker runtime",
    );
    let config = Config::default().with_local_targets();

    let smoke = builtin_image_check(Ok(&config), "docker", failed.clone(), true);
    let presence = builtin_image_check(Ok(&config), "docker", failed, false);

    assert_eq!(smoke.status, CheckStatus::Fixable);
    assert_eq!(presence.status, CheckStatus::Warning);
}

#[test]
fn host_limits_say_a_drop_in_may_override_an_unread_max_startups() {
    let limits = parse_host_limits(b"keys.used=10\nkeys.quota=4096\nmaxstartups.unreadable=1\n");

    assert!(limits.max_startups_unreadable);
    assert!(!limits.keyring_is_under_pressure());
    let sentence = limits.max_startups_sentence();
    assert!(sentence.contains("unreadable drop-in"), "{sentence}");
    assert!(!sentence.contains("10:30"), "{sentence}");

    let readable_default = parse_host_limits(b"keys.used=10\nkeys.quota=4096\n");
    assert!(!readable_default.max_startups_unreadable);
    assert_eq!(
        readable_default.max_startups_sentence(),
        "sshd MaxStartups is not set in sshd_config, so sshd's default applies."
    );

    let explicit = parse_host_limits(b"maxstartups=10:30:60\n");
    assert_eq!(
        explicit.max_startups_sentence(),
        "sshd MaxStartups is 10:30:60."
    );

    let empty = parse_host_limits(b"");
    assert!(empty.is_empty());
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

fn ssh_podman_probe_executor(linger: (i32, &str, &str)) -> FakeExecutor {
    let probes = crate::targets::ssh_podman_probe_fixture(&[
        ("version", 0, "podman version 5.4.2\n", ""),
        (
            "uid_map",
            0,
            "         0       1000          1\n         1     100000      65536\n",
            "",
        ),
        ("linger", linger.0, linger.1, linger.2),
    ]);
    FakeExecutor::new([Ok(output(b"")), Ok(output(probes))])
}

/// Known enabled, known disabled and unverified user lingering are distinct
/// durability states for a remote Podman worker.
#[test]
fn ssh_podman_check_explains_when_durability_cannot_be_verified() {
    let ssh = RuntimeSshTarget::from(&ssh_connection());

    let ready_executor = ssh_podman_probe_executor((0, "yes\n", ""));
    let (ready, _) = ssh_podman_check("remote", &ssh, "ubuntu:24.04", &ready_executor, false);
    assert_eq!(ready.id, "runtime.ssh-podman.remote");
    assert_eq!(ready.title, "Remote Podman for target remote");
    assert_eq!(ready.status, CheckStatus::Ready);
    assert!(ready.detail.contains("Remote rootless Podman 5.4.2"));
    assert!(ready.detail.contains("dev@example.test"));
    let commands = ready_executor.commands.borrow();
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

    let disabled_executor = ssh_podman_probe_executor((0, "no\n", ""));
    let (disabled, _) = ssh_podman_check("remote", &ssh, "ubuntu:24.04", &disabled_executor, false);
    assert_eq!(disabled.status, CheckStatus::Warning);
    assert!(all_ready(std::slice::from_ref(&disabled)));
    assert!(disabled.detail.contains("Podman 5.4.2 is available"));
    assert!(disabled.detail.contains("last SSH connection closes"));
    assert!(
        disabled
            .remediation
            .as_deref()
            .unwrap()
            .contains("sudo loginctl enable-linger")
    );

    let unknown_executor = ssh_podman_probe_executor((127, "", "sh: loginctl: not found\n"));
    let (unknown, _) = ssh_podman_check("remote", &ssh, "ubuntu:24.04", &unknown_executor, false);
    assert_eq!(unknown.status, CheckStatus::Warning);
    assert!(all_ready(std::slice::from_ref(&unknown)));
    assert!(unknown.detail.contains("durability check is unavailable"));
    assert!(unknown.detail.contains("may not use systemd"));
    assert!(unknown.detail.contains("cannot verify"));
    let remediation = unknown.remediation.as_deref().unwrap();
    assert!(remediation.contains("service manager"));
    assert!(!remediation.contains("sudo loginctl enable-linger"));
}

/// Permission denied on the SSH connection probe stops dependent Podman
/// probes and reports the shared SSH remediation.
#[test]
fn ssh_podman_check_reports_the_shared_ssh_remediation_before_probing_podman() {
    let executor = FakeExecutor::new([Ok(failed(
        b"dev@example.test: Permission denied (publickey).",
    ))]);

    let (check, _) = ssh_podman_check(
        "remote",
        &RuntimeSshTarget::from(&ssh_connection()),
        "ubuntu:24.04",
        &executor,
        false,
    );

    assert_eq!(check.status, CheckStatus::Fixable);
    assert_eq!(
        check.remediation.as_deref(),
        Some("Install your public key on the host with `ssh-copy-id dev@example.test`.")
    );
    assert_eq!(executor.commands.borrow().len(), 1);
}

/// precision-3260 had 0 B free for its user while `df` showed ~23 GB "free"
/// behind ext4's root reserve. Doctor says so plainly, with the reserve.
// Hard-won: 540c9202: a real full-root incident caused repeated worker replacement attempts while users saw only unreachable.
#[test]
fn storage_check_reports_a_full_disk_and_its_root_reserve() {
    let full = output(
        b"home=/home/dev\n\
storage=0\t491134172\t467026656\t/\t.local/share/hel/workers\n\
storage=41943040\t976762584\t900000000\t/home/dev/Projects\t/home/dev/Projects\n\
cpu.percent=3\nmemory.current=1\nmemory.max=2\nlogical.cores=8\n",
    );
    let roomy = output(b"home=/home/dev\nstorage=41943040\t491134172\t100000000\t/\t/tmp\n");
    let executor = FakeExecutor::new([Ok(full)]);
    let checks = storage_checks(Ok(&ssh_bare_config()), &executor);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "storage.ssh:example.test");
    assert_eq!(checks[0].status, CheckStatus::Fixable);
    assert_eq!(
        checks[0].detail,
        "Disk full on /: /: 0 B free, 24.69 GB reserved for root (full); \
/home/dev/Projects: 42.95 GB free, 35.66 GB reserved for root. \
Mjolnir refuses writes there, and sessions that write there wait instead of restarting."
    );
    // The probe measures the paths Mjolnir writes to on that host.
    let probe = executor.commands.borrow()[0].clone();
    let remote = probe.args.last().unwrap();
    for path in [
        ".local/share/hel/workers",
        ".local/share/hel/profiles",
        ".cache/mjolnir",
        ".cache/mbx",
        "/tmp",
    ] {
        assert!(remote.contains(path), "{path} in {remote}");
    }

    let executor = FakeExecutor::new([Ok(roomy)]);
    let checks = storage_checks(Ok(&ssh_bare_config()), &executor);
    assert_eq!(checks[0].status, CheckStatus::Ready, "{}", checks[0].detail);
}

// Hard-won: 9ef0cd60: a timed-out SSH probe was wrongly diagnosed as a missing OpenSSH installation.
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

// Hard-won: 9ef0cd60: OpenSSH connection-timeout output was wrongly diagnosed as a missing client instead of an unreachable host.
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

/// A host-key mismatch offers key installation only with a fingerprint check.
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

/// An unrecognized SSH result stays visible instead of being misreported as
/// a known reachability or installation problem.
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

/// An untyped launch failure must not turn into incorrect OpenSSH install
/// advice.
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

/// Image checks must stop when the host's Podman preflight is unsupported.
#[test]
fn image_checks_are_skipped_when_the_host_podman_preflight_fails() {
    let executor = FakeExecutor::new([Ok(output(b"")), Ok(output(b"podman version 3.4.7\n"))]);
    let config = config_with([(
        "podman",
        TargetTemplate::LocalPodman {
            container: container("ubuntu:24.04"),
        },
    )]);

    let checks = podman_checks(Ok(&config), &executor, false, &ApplePlatform::Linux);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "runtime.podman");

    let mut responses = vec![
        Ok(output(b"podman version 5.4.2\n")),
        Ok(output(
            b"         0       1000          1\n         1     100000      65536\n",
        )),
    ];
    responses.extend([Ok(output(b"")), Ok(failed(b"")), Ok(output(b""))]);
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
    let checks = podman_checks(Ok(&config), &executor, false, &ApplePlatform::Linux);
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

/// With no target blocks in config.toml, the dashboard still offers the
/// built-in `podman` target, and doctor's engine and image checks cover it;
/// the worker-binary checks said "No container target is configured" and the
/// freshness check for Linux targets was missing (launch finding R5-2). A
/// built-in target whose engine is unavailable needs no worker.
// Hard-won: 435bf565: doctor omitted worker and architecture checks for a ready built-in target absent from config.toml.
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

    let configured = config_with([(
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
    let offered = offered_targets(&configured, &engines);
    let ids = worker_binary_checks(Ok(&offered))
        .into_iter()
        .map(|check| check.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["worker.pd"]);
}

/// Settings opens with `prefix+s`; F7 is not bound, so no fix may send the
/// user there, and every fix names the key the configuration binds.
// Hard-won: 0f46ecf5: doctor fixes named F7 even though Settings is opened with the configured prefix and s.
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
// Hard-won: 527f018a: installed users were sent to a repository docs path and the Podman fix was printed twice.
#[test]
fn missing_podman_names_its_fix_once_and_links_the_published_guide() {
    for response in [
        Err(anyhow!("No such file or directory (os error 2)")),
        Ok(failed(b"podman: command not found")),
    ] {
        let check =
            local_podman_runtime_check(&FakeExecutor::new([response]), &ApplePlatform::Linux);
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

/// The README and the quickstart list five agents. The check that says none
/// was found named four and left out Muse Code, which discovery does look
/// for (launch finding R13-12).
// Hard-won: caf9d80a: setup and doctor omitted Muse from their supported-agent list despite discovering it.
#[test]
fn missing_harness_homes_name_every_supported_agent() {
    let check = harness_discovery_check_from(&[], false, "ctrl+b s");

    assert_eq!(
        check.detail,
        "No Codex, Claude Code, Kimi Code, Grok Build, or Muse Code home was found in the default or environment-overridden locations."
    );
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

// Hard-won: 592318d8: setup instructions used the old product name and misstated local-bare prerequisites.
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
    assert_eq!(
        std::fs::read_to_string(git_dir.join("refs/hel/review-capture")).unwrap(),
        "a".repeat(41)
    );
    assert_eq!(std::fs::read_to_string(&scratch).unwrap(), "scratch");

    let clean = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(clean.path().join(".git/refs/heads")).unwrap();
    let clean_residue = crate::doctor::review_residue(clean.path());
    assert!(clean_residue.refs.is_empty());
    assert!(clean_residue.scratch_indexes.is_empty());
}

/// R2-6: every running session's own managed clone holds
/// `refs/hel/review-baseline`, and doctor reported each one as left in a
/// repository Mjolnir does not own. A managed checkout is Mjolnir's working
/// state, and a repository a live session works in has its refs in use.
// Hard-won: c69618f4: doctor reported active sessions’ managed refs as user-owned leftover residue and failed.
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

// Hard-won: 871a4d9b: one unresolved profile credential stopped the whole config and daemon upgrade instead of a per-profile readiness refusal.
#[test]
fn doctor_reports_a_profile_or_target_that_cannot_start_with_the_fix() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("deepseek");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "model_provider = \"deepseek\"\n\n[model_providers.deepseek]\n\
         base_url = \"https://api.deepseek.com\"\nwire_api = \"responses\"\nenv_key = \"MJ_TEST_UNSET_PROVIDER_KEY\"\n",
    )
    .unwrap();
    let config_path = directory.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {}\n\n[profiles.deepseek]\nkind = \"codex\"\nhome = {:?}\n\n\
             [targets.boxed]\nkind = \"podman\"\nimage = \"example\"\n\n\
             [targets.boxed.environment]\nTOKEN = {{ from_secret = \"TOKEN\" }}\n",
            mj_core::config::CONFIG_VERSION,
            home.to_string_lossy()
        ),
    )
    .unwrap();
    // The configuration loads even though neither can start.
    let config = Config::load_from(&config_path).unwrap();

    let profile = harness_checks(Ok(&config), &AlwaysFailingExecutor)
        .into_iter()
        .find(|check| check.id == "harness.deepseek")
        .expect("the profile is reported");
    assert_eq!(profile.status, CheckStatus::Fixable);
    assert!(
        profile
            .detail
            .contains("export MJ_TEST_UNSET_PROVIDER_KEY and run `mj daemon restart`"),
        "{}",
        profile.detail
    );

    let target = secret_checks(Ok(&config), &config_path)
        .into_iter()
        .find(|check| check.id == "targets.boxed.environment")
        .expect("the target is reported");
    assert_eq!(target.status, CheckStatus::Fixable);
    assert!(
        target.detail.contains("TOKEN = { from_secret"),
        "{}",
        target.detail
    );
}

#[test]
fn doctor_points_plain_text_credentials_at_the_secrets_file_and_checks_its_mode() {
    use mj_core::config::{Environment, EnvironmentValue, SecretResolver, with_secret_resolver};

    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = directory.path().join(mj_core::config::SECRETS_FILE);
    let mut config = Config::default();
    let mut literal = Environment::new();
    literal.insert("ZAI_API_KEY".into(), "plain".into());
    literal.insert("PATH".into(), "/usr/bin".into());
    config.profiles.insert(
        "plain".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: "/profiles/plain".into(),
            environment: literal,
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        },
    );
    let referenced = with_secret_resolver(
        SecretResolver::fixed(
            Default::default(),
            [("ZAI_API_KEY".to_owned(), "stored".to_owned())].into(),
        ),
        || {
            Environment::from_sources(
                [(
                    "ZAI_API_KEY".to_owned(),
                    EnvironmentValue::FromSecret("ZAI_API_KEY".into()),
                )]
                .into(),
            )
        },
    );
    config.profiles.insert(
        "referenced".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: "/profiles/referenced".into(),
            environment: referenced,
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        },
    );
    let mut target = container("ghcr.io/example/agent:latest");
    target.environment.insert("GH_TOKEN".into(), "plain".into());
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman { container: target },
    );

    let checks = secret_checks(Ok(&config), &config_path);
    let ids: Vec<&str> = checks.iter().map(|check| check.id.as_str()).collect();
    assert_eq!(
        ids,
        ["profiles.plain.secrets", "targets.podman.secrets"],
        "{checks:#?}"
    );
    assert!(
        checks
            .iter()
            .all(|check| check.status == CheckStatus::Warning)
    );
    let remediation = checks[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("from_secret") && remediation.contains(&secrets.display().to_string()),
        "{remediation}"
    );
    assert!(checks[0].detail.contains("ZAI_API_KEY") && !checks[0].detail.contains("PATH"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&secrets, "ZAI_API_KEY = \"stored\"\n").unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
        let file_check = secret_checks(Ok(&config), &config_path).remove(0);
        assert_eq!(file_check.id, "secrets.file");
        assert_eq!(file_check.status, CheckStatus::Warning);
        assert!(
            file_check
                .remediation
                .as_deref()
                .unwrap()
                .contains("chmod 600")
        );
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file_check = secret_checks(Ok(&config), &config_path).remove(0);
        assert_eq!(file_check.status, CheckStatus::Ready, "{file_check:?}");
    }
}
