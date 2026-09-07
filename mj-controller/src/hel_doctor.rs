//! Actionable host and configuration prerequisite checks.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;

use crate::hel_controller::{
    WorkerBinaryAvailability, backend_ssh, worker_binary_prerequisite_for_arch,
};
use crate::hel_setup::{
    DiscoveredHome, discover_harness_homes_with_executor, harness_is_authenticated_with_executor,
};
use hel::hel_config::{
    ContainerTemplate, HarnessKind, HarnessProfile, HelConfig, TargetTemplate, config_path,
};
use hel::hel_credentials::login_command;
use hel::hel_targets::{
    BoundedProcessExecutor, CommandExecutor, CommandSpec,
    ContainerTemplate as RuntimeContainerTemplate, ProcessExecutor, SshTarget as RuntimeSshTarget,
    TargetTemplate as RuntimeTargetTemplate, run_setup_smoke_test, ssh_command,
    ssh_connectivity_probe, verify_local_docker, verify_local_podman, verify_ssh_docker,
    verify_ssh_podman,
};

// Only the image for the Apple container smoke test when the config has no
// apple-container target. This intentionally stays a small stock image rather
// than hel_setup::DEFAULT_IMAGE: the check just proves the runtime can start a
// container, and pulling the multi-gigabyte agent-dev image to do that would be
// a poor trade.
const DEFAULT_CONTAINER_IMAGE: &str = "ubuntu:24.04";
const APPLE_CONTAINER_INSTALL_URL: &str = "https://github.com/apple/container#initial-install";

/// How long a single prerequisite probe may take before doctor reports it as a
/// fixable check instead of waiting for it.
///
/// Every probe outside the opt-in smoke tests is a local or short network call,
/// so this only ever fires for a wedged runtime socket, a blackholed network,
/// or a credential helper waiting on something that will never arrive.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The executor `mj doctor` and `mj setup` run their prerequisite probes
/// through: one deadline per probe, so a wedged runtime cannot hang the run.
pub const fn probe_executor() -> BoundedProcessExecutor {
    BoundedProcessExecutor::new(PROBE_TIMEOUT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ready,
    Warning,
    Fixable,
    Unsupported,
}

impl CheckStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Warning => "warning",
            Self::Fixable => "fixable",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
    pub remediation: Option<String>,
}

impl DoctorCheck {
    fn ready(id: impl Into<String>, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Ready,
            detail: detail.into(),
            remediation: None,
        }
    }

    fn warning(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Warning,
            detail: detail.into(),
            remediation: Some(remediation.into()),
        }
    }

    pub(crate) fn fixable(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Fixable,
            detail: detail.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn unsupported(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Unsupported,
            detail: detail.into(),
            remediation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorOptions {
    pub smoke: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplePlatform {
    Linux,
    Macos {
        architecture: String,
        major_version: u32,
    },
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionsPlatform {
    Linux,
    Macos,
}

pub fn run_current(options: DoctorOptions) -> Vec<DoctorCheck> {
    if options.smoke {
        // A smoke test may legitimately pull a multi-gigabyte image, which no
        // probe deadline could tell apart from a hung runtime, so an opt-in
        // `--smoke` run keeps waiting for its commands.
        return run_with(
            &ProcessExecutor,
            current_apple_platform(&ProcessExecutor),
            options,
        );
    }
    let executor = probe_executor();
    run_with(&executor, current_apple_platform(&executor), options)
}

pub fn run_with(
    executor: &impl CommandExecutor,
    apple_platform: ApplePlatform,
    options: DoctorOptions,
) -> Vec<DoctorCheck> {
    run_with_config_path(&config_path(), executor, apple_platform, options)
}

/// The same checks as [`run_with`], against an explicit configuration file.
///
/// `mj setup` uses this to report on the configuration it just wrote, so a
/// first run ends with exactly the summary and remediations `mj doctor`
/// would print.
pub fn run_with_config_path(
    config_path: &Path,
    executor: &impl CommandExecutor,
    apple_platform: ApplePlatform,
    options: DoctorOptions,
) -> Vec<DoctorCheck> {
    let (config, mut checks) = configuration_checks(config_path);
    checks.push(harness_discovery_check(config.as_ref(), executor));
    checks.extend(harness_checks(config.as_ref(), executor));
    checks.extend(podman_checks(config.as_ref(), executor, options.smoke));
    checks.extend(docker_checks(config.as_ref(), executor, options.smoke));
    checks.extend(ssh_bare_checks(config.as_ref(), executor));
    checks.extend(ssh_podman_checks(config.as_ref(), executor, options.smoke));
    checks.extend(ssh_docker_checks(config.as_ref(), executor, options.smoke));
    checks.extend(aws_checks(config.as_ref(), executor));
    checks.extend(worker_binary_checks(config.as_ref()));
    checks.push(apple_container_check(
        &apple_platform,
        executor,
        options.smoke,
        apple_container_image(config.as_ref()),
    ));
    checks
}

fn harness_discovery_check(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let home = dirs::home_dir();
    let overrides = HarnessKind::ALL
        .into_iter()
        .filter_map(|kind| std::env::var_os(kind.home_env()).map(|path| (kind, path.into())));
    let discovered = discover_harness_homes_with_executor(home.as_deref(), overrides, executor);
    harness_discovery_check_from(
        &discovered,
        config.is_some_and(|config| !config.profiles.is_empty()),
    )
}

fn harness_discovery_check_from(
    discovered: &[DiscoveredHome],
    has_configured_profiles: bool,
) -> DoctorCheck {
    if discovered.is_empty() {
        return if has_configured_profiles {
            DoctorCheck::ready(
                "harness.discovery",
                "Harness home discovery",
                "No default or environment-overridden harness homes were found; configured profile homes are checked below.",
            )
        } else {
            DoctorCheck::fixable(
                "harness.discovery",
                "Harness home discovery",
                "No Codex, Claude Code, Kimi Code, or Grok Build home was found in the default or environment-overridden locations.",
                "Install and sign in to a supported harness, then run `mj setup`.",
            )
        };
    }

    let homes = discovered
        .iter()
        .map(|home| {
            let authentication = if home.authenticated {
                "authenticated"
            } else {
                "not authenticated"
            };
            format!(
                "{} at {} ({authentication})",
                home.kind.display_name(),
                home.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    DoctorCheck::ready(
        "harness.discovery",
        "Harness home discovery",
        format!("Discovered {homes}. Configured profile authentication is checked below."),
    )
}

pub fn all_ready(checks: &[DoctorCheck]) -> bool {
    checks
        .iter()
        .all(|check| check.status != CheckStatus::Fixable)
}

pub fn render_human(checks: &[DoctorCheck], output: &mut impl Write) -> Result<()> {
    for check in checks {
        writeln!(
            output,
            "{} {}: {}",
            check.status.label(),
            check.title,
            check.detail
        )?;
        if let Some(remediation) = &check.remediation {
            writeln!(output, "  remediation: {remediation}")?;
        }
    }
    Ok(())
}

pub fn setup_instructions(platform: InstructionsPlatform) -> String {
    match platform {
        InstructionsPlatform::Linux => format!(
            "# Hel setup instructions for Linux\n\n\
This page is self-contained. Follow this exact loop as the user who will run Hel:\n\n\
1. Run `mj doctor --json`.\n\
2. Follow every `fixable` remediation from its JSON output.\n\
3. Run `mj doctor --json` again. Repeat until no check is `fixable`.\n\
4. Finish with `mj doctor --json --smoke` to verify every configured container\n\
   image end to end, and resolve anything it reports as `fixable`.\n\n\
For a coding-agent handoff, provide this entire instructions page together with\n\
the latest `mj doctor --json` output.\n\n\
## Linux container-runtime postconditions\n\n{}\n\n{}",
            hel::hel_targets::PODMAN_DOCUMENTATION,
            hel::hel_targets::DOCKER_DOCUMENTATION
        ),
        InstructionsPlatform::Macos => format!(
            "# Hel setup instructions for macOS\n\n\
This page is self-contained. Follow this exact loop as the user who will run Hel:\n\n\
1. Run `mj doctor --json`.\n\
2. Follow every `fixable` remediation from its JSON output.\n\
3. Run `mj doctor --json` again. Repeat until no check is `fixable`.\n\n\
For a coding-agent handoff, provide this entire instructions page together with\n\
the latest `mj doctor --json` output.\n\n\
## Apple container runtime\n\n\
Hel's Apple container target requires Apple silicon and macOS 26 or newer.\n\
On an Intel Mac or an older macOS release, the target is unsupported; use a\n\
local Podman, SSH, or AWS target instead.\n\n\
If the `container` command is absent, install only the official signed package:\n\n\
<https://github.com/apple/container#initial-install>\n\n\
Hel never downloads or installs that package. If doctor reports a stopped\n\
daemon, run exactly:\n\n```console\ncontainer system start\n```\n\n\
Finish with the opt-in disposable runtime test in JSON mode:\n\n```console\nmj doctor --json --smoke\n```\n\n\
Apple container is ready only when that smoke test creates a disposable\n\
container, executes `true` in it, and removes it successfully. Use the image\n\
configured by an `apple-container` target; without one, doctor uses\n\
`{DEFAULT_CONTAINER_IMAGE}` for the smoke test.\n\n\
## Shared Hel prerequisites\n\n\
`mj doctor --json` also checks the configuration, each configured harness home\n\
and authentication marker, selected container worker binaries, and any relevant\n\
Podman prerequisites. Resolve every `fixable` status before starting a session."
        ),
    }
}

fn configuration_checks(path: &Path) -> (Option<HelConfig>, Vec<DoctorCheck>) {
    if !path.exists() {
        return (
            None,
            vec![DoctorCheck::fixable(
                "config",
                "Mjolnir configuration",
                format!("{} does not exist", path.display()),
                "Run `mj setup` to create config.toml.",
            )],
        );
    }
    match HelConfig::load_from(path) {
        Ok(config) => {
            let mut checks = vec![match config.newer_build_notice() {
                // Hel still runs on a config a newer build owns, but every
                // save refuses, so say so rather than reporting it as valid.
                Some(notice) => DoctorCheck::warning(
                    "config",
                    "Mjolnir configuration",
                    format!("{}: {notice}", path.display()),
                    "Update Mjolnir, or change settings with the newer build.",
                ),
                None => DoctorCheck::ready(
                    "config",
                    "Mjolnir configuration",
                    format!("{} is valid", path.display()),
                ),
            }];
            if config.profiles.is_empty() || config.bundles.is_empty() || config.targets.is_empty()
            {
                checks.push(DoctorCheck::fixable(
                    "config.session-prerequisites",
                    "Session configuration",
                    "At least one profile, bundle, and target are required.",
                    "Run `mj setup`, or add profiles, bundles, and targets to config.toml.",
                ));
            } else {
                checks.push(DoctorCheck::ready(
                    "config.session-prerequisites",
                    "Session configuration",
                    "At least one profile, bundle, and target are configured.",
                ));
            }
            (Some(config), checks)
        }
        Err(error) => (
            None,
            vec![DoctorCheck::fixable(
                "config",
                "Mjolnir configuration",
                format!("{} is invalid: {error:#}", path.display()),
                "Fix the reported TOML error in config.toml, or run `mj setup` to replace it.",
            )],
        ),
    }
}

fn harness_checks(config: Option<&HelConfig>, executor: &impl CommandExecutor) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return vec![DoctorCheck::fixable(
            "harness.profiles",
            "Harness profiles",
            "Harness homes cannot be checked until config.toml is valid.",
            "Fix config.toml, then rerun `mj doctor --json`.",
        )];
    };
    if config.profiles.is_empty() {
        return vec![DoctorCheck::fixable(
            "harness.profiles",
            "Harness profiles",
            "No harness profiles are configured.",
            "Run `mj setup` to discover homes, or add a profile to config.toml.",
        )];
    }
    config
        .profiles
        .iter()
        .map(|(id, profile)| {
            let title = format!("Harness profile {id}");
            if !profile.home.is_dir() {
                return DoctorCheck::fixable(
                    format!("harness.{id}"),
                    title,
                    format!("{} does not exist", profile.home.display()),
                    format!(
                        "Create or select the {} home, then set its `home` path in config.toml.",
                        profile.kind.display_name()
                    ),
                );
            }
            if !harness_is_authenticated_with_executor(profile.kind, &profile.home, executor) {
                return DoctorCheck::fixable(
                    format!("harness.{id}"),
                    title,
                    format!(
                        "No usable authentication was detected for {}",
                        profile.home.display()
                    ),
                    harness_login_remediation(id, profile),
                );
            }
            DoctorCheck::ready(
                format!("harness.{id}"),
                title,
                format!(
                    "{} is present and authentication is available",
                    profile.home.display()
                ),
            )
        })
        .collect()
}

/// Point an unauthenticated profile at `mj login`, which already knows how to
/// sign each harness in.
///
/// The underlying command is named only for the reader's benefit; it comes from
/// [`login_command`], the one place that tracks what each harness CLI actually
/// accepts, so this text cannot drift away from what `mj login` runs.
fn harness_login_remediation(id: &str, profile: &HarnessProfile) -> String {
    let (program, arguments) = login_command(profile);
    format!(
        "Run `mj login --profile {id}`; it runs `{program} {}` against {}.",
        arguments.join(" "),
        profile.home.display()
    )
}

/// Host Podman prerequisites, then one image check per `local-podman` target.
///
/// The image checks run only after the host preflight passes, because a broken
/// Podman installation already reports its own actionable check.
fn podman_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let preflight = podman_check(config, executor);
    let preflight_passed = preflight.status == CheckStatus::Ready;
    let mut checks = vec![preflight];
    if preflight_passed {
        checks.extend(podman_image_checks(config, executor, smoke));
    }
    checks
}

fn podman_check(config: Option<&HelConfig>, executor: &impl CommandExecutor) -> DoctorCheck {
    let Some(config) = config else {
        return DoctorCheck::unsupported(
            "runtime.podman",
            "Rootless Podman",
            "Podman prerequisites cannot be evaluated until config.toml is valid.",
        );
    };
    if local_podman_targets(config).is_empty() {
        return DoctorCheck::unsupported(
            "runtime.podman",
            "Rootless Podman",
            "No local-podman target is configured.",
        );
    }
    local_podman_runtime_check(executor)
}

/// Probe the local rootless Podman prerequisites and phrase the result as a
/// doctor check.
///
/// This is the single source of truth for Podman availability wording and
/// remediation. `mj setup` calls it directly so its runtime list reports the
/// same detail and fix that `mj doctor` would.
pub fn local_podman_runtime_check(executor: &impl CommandExecutor) -> DoctorCheck {
    match verify_local_podman(executor) {
        Ok(preflight) => DoctorCheck::ready(
            "runtime.podman",
            "Rootless Podman",
            format!("Podman {} has a valid rootless UID map.", preflight.version),
        ),
        Err(error) => {
            let detail = format!("{error:#}");
            DoctorCheck::fixable(
                "runtime.podman",
                "Rootless Podman",
                detail.clone(),
                podman_remediation(&detail),
            )
        }
    }
}

fn local_podman_targets(config: &HelConfig) -> Vec<(&String, &ContainerTemplate)> {
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalPodman { container } => Some((id, container)),
            _ => None,
        })
        .collect()
}

fn podman_image_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return Vec::new();
    };
    local_podman_targets(config)
        .into_iter()
        .map(|(id, container)| podman_image_check(id, &container.image, executor, smoke))
        .collect()
}

fn podman_image_check(
    id: &str,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.podman.image.{id}");
    let title = format!("Podman image for target {id}");
    if smoke {
        let target = RuntimeTargetTemplate::LocalPodman(RuntimeContainerTemplate {
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        });
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!("Disposable run/exec/remove smoke test passed for image {image}."),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image}: {error:#}"
                ),
                "Fix the configured image or Podman runtime, then run `mj doctor --json --smoke` again.",
            ),
        };
    }

    let command = CommandSpec::new("podman", ["image", "exists", image])
        .purpose("check Podman image presence");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!("Image {image} is present in local Podman storage."),
        ),
        Ok(_) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Image {image} is not present in local Podman storage."),
            missing_image_remediation(image),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Could not check whether image {image} is present in local Podman storage: {error}"
            ),
            missing_image_remediation(image),
        ),
    }
}

fn missing_image_remediation(image: &str) -> String {
    format!(
        "Pull it with `podman pull {image}`, build it from containers/Containerfile.agent-dev, or run `mj doctor --json --smoke` to verify the full pull-and-run path."
    )
}

/// Host Docker prerequisites, then one image check per `local-docker` target.
fn docker_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return vec![DoctorCheck::unsupported(
            "runtime.docker",
            "Docker",
            "Docker prerequisites cannot be evaluated until config.toml is valid.",
        )];
    };
    let targets = local_docker_targets(config);
    if targets.is_empty() {
        return vec![DoctorCheck::unsupported(
            "runtime.docker",
            "Docker",
            "No local-docker target is configured.",
        )];
    }
    let preflight = local_docker_runtime_check(executor);
    if preflight.status != CheckStatus::Ready {
        return vec![preflight];
    }
    let mut checks = vec![preflight];
    checks.extend(
        targets
            .into_iter()
            .map(|(id, container)| docker_image_check(id, &container.image, executor, smoke)),
    );
    checks
}

pub fn local_docker_runtime_check(executor: &impl CommandExecutor) -> DoctorCheck {
    match verify_local_docker(executor) {
        Ok(preflight) => DoctorCheck::ready(
            "runtime.docker",
            "Docker",
            format!(
                "Docker {} is connected to a Linux daemon.",
                preflight.version
            ),
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.docker",
            "Docker",
            format!("{error:#}"),
            "Install and start Docker, then make sure `docker info` succeeds as the user running mj.",
        ),
    }
}

fn local_docker_targets(config: &HelConfig) -> Vec<(&String, &ContainerTemplate)> {
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalDocker { container } => Some((id, container)),
            _ => None,
        })
        .collect()
}

fn docker_image_check(
    id: &str,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.docker.image.{id}");
    let title = format!("Docker image for target {id}");
    if smoke {
        let target = RuntimeTargetTemplate::LocalDocker(RuntimeContainerTemplate {
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        });
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove and OverlayFS attachment smoke test passed for image {image}."
                ),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image}: {error:#}"
                ),
                "Fix the configured image or Docker runtime, then run `mj doctor --json --smoke` again.",
            ),
        };
    }
    let command = CommandSpec::new("docker", ["image", "inspect", image])
        .purpose("check Docker image presence");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!("Image {image} is present in Docker storage."),
        ),
        Ok(_) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Image {image} is not present in Docker storage."),
            format!("Pull it with `docker pull {image}`, or run `mj doctor --json --smoke`."),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not inspect Docker image {image}: {error}"),
            format!("Make sure `docker info` succeeds, then run `docker pull {image}`."),
        ),
    }
}

/// The outcome of the shared SSH connectivity probe.
///
/// Both SSH-backed checks run this first: an unreachable host makes every
/// later probe fail with a misleading message.
enum SshConnectivity {
    Reachable,
    Failed { detail: String, remediation: String },
}

/// Probe `ssh <destination> true` and map any failure to a copy-paste fix.
///
/// Hel never generates keys, runs `ssh-copy-id`, or accepts a host key on the
/// user's behalf; it only says exactly which command would fix the failure.
fn ssh_connectivity(ssh: &RuntimeSshTarget, executor: &impl CommandExecutor) -> SshConnectivity {
    let destination = &ssh.destination;
    let command = ssh_connectivity_probe(ssh);
    match executor.execute(&command) {
        Err(error) => SshConnectivity::Failed {
            detail: format!("Could not run `ssh {destination} true`: {error}"),
            remediation: SSH_MISSING_REMEDIATION.to_owned(),
        },
        Ok(output) if output.status != 0 => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            SshConnectivity::Failed {
                detail: format!("`ssh {destination} true` failed: {stderr}"),
                remediation: ssh_failure_remediation(&stderr, ssh),
            }
        }
        Ok(_) => SshConnectivity::Reachable,
    }
}

const SSH_MISSING_REMEDIATION: &str = "Install an OpenSSH client and put `ssh` on PATH: `sudo apt update && sudo apt install -y openssh-client` (Debian/Ubuntu) or `sudo dnf install -y openssh-clients` (Fedora).";

/// Map `ssh -o BatchMode=yes` stderr to the command that fixes it.
fn ssh_failure_remediation(stderr: &str, ssh: &RuntimeSshTarget) -> String {
    let destination = &ssh.destination;
    if stderr.contains("Host key verification failed")
        || stderr.contains("No ECDSA host key is known")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        let host = ssh_host_only(destination);
        return format!(
            "Add the host key with `ssh-keyscan -H {host} >> ~/.ssh/known_hosts`. Verify the fingerprint out of band before trusting it; if the key changed, remove the stale entry with `ssh-keygen -R {host}` first."
        );
    }
    if stderr.contains("Permission denied")
        || stderr.contains("Too many authentication failures")
        || stderr.contains("no matching host key")
        || stderr.contains("Authentication failed")
    {
        return match ssh_identity_file(ssh) {
            Some(identity) => format!(
                "Install your public key on the host with `ssh-copy-id -i {identity}.pub {destination}`."
            ),
            None => {
                format!("Install your public key on the host with `ssh-copy-id {destination}`.")
            }
        };
    }
    if stderr.contains("ssh: command not found") || stderr.contains("No such file or directory") {
        return SSH_MISSING_REMEDIATION.to_owned();
    }
    format!("Run `ssh {destination} true` by hand and resolve the error it reports: {stderr}")
}

/// The host part of an OpenSSH destination, without any `user@` prefix.
fn ssh_host_only(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

/// The identity file provisioning passes, recovered from the built ssh args.
fn ssh_identity_file(ssh: &RuntimeSshTarget) -> Option<&str> {
    let position = ssh.ssh_args.iter().position(|arg| arg == "-i")?;
    ssh.ssh_args.get(position + 1).map(String::as_str)
}

/// One check per `ssh-bare` target: can Hel reach the host noninteractively?
fn ssh_bare_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::SshBare { ssh, .. } => {
                Some(ssh_bare_check(id, &backend_ssh(ssh), executor))
            }
            _ => None,
        })
        .collect()
}

fn ssh_bare_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-bare.{id}");
    let title = format!("SSH access for target {id}");
    match ssh_connectivity(ssh, executor) {
        SshConnectivity::Reachable => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "`ssh {} true` succeeds noninteractively from this host.",
                ssh.destination
            ),
        ),
        SshConnectivity::Failed {
            detail,
            remediation,
        } => DoctorCheck::fixable(check_id, title, detail, remediation),
    }
}

/// One check per `ssh-podman` target: the same Podman probes, run over SSH.
fn ssh_podman_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::SshPodman { ssh, container, .. } => Some(ssh_podman_check(
                id,
                &backend_ssh(ssh),
                &container.image,
                executor,
                smoke,
            )),
            _ => None,
        })
        .collect()
}

fn ssh_podman_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-podman.{id}");
    let title = format!("Remote Podman for target {id}");
    let destination = &ssh.destination;
    // Connectivity first: a remote Podman probe on an unreachable host reports
    // a Podman problem the user does not have.
    if let SshConnectivity::Failed {
        detail,
        remediation,
    } = ssh_connectivity(ssh, executor)
    {
        return DoctorCheck::fixable(check_id, title, detail, remediation);
    }
    let preflight = match verify_ssh_podman(ssh, executor) {
        Ok(preflight) => preflight,
        Err(error) => {
            let detail = format!("{error:#}");
            let remediation = match podman_remediation_match(&detail) {
                Some(remediation) => format!("On {destination}: {remediation}"),
                None => format!(
                    "Verify `ssh {destination}` succeeds noninteractively from this host, then install rootless Podman 4 or newer there (see docs/PODMAN.md)."
                ),
            };
            return DoctorCheck::fixable(check_id, title, detail, remediation);
        }
    };
    let linger_warning = preflight.warnings.first();
    if !smoke && let Some(warning) = linger_warning {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "Remote rootless Podman {} is available via {destination}, but {}",
                preflight.version, warning.detail
            ),
            &warning.remediation,
        );
    }
    if !smoke {
        return DoctorCheck::ready(
            check_id,
            title,
            format!(
                "Remote rootless Podman {} is available via {destination}. Run `mj doctor --json --smoke` to verify the image end to end.",
                preflight.version
            ),
        );
    }

    let target = RuntimeTargetTemplate::SshPodman {
        ssh: ssh.clone(),
        container: RuntimeContainerTemplate {
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        },
    };
    match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
        Ok(()) => match linger_warning {
            Some(warning) => DoctorCheck::warning(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test passed for image {image} on {destination}, but {}",
                    warning.detail
                ),
                &warning.remediation,
            ),
            None => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test passed for image {image} on {destination}."
                ),
            ),
        },
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Disposable run/exec/remove smoke test failed for image {image} on {destination}: {error:#}"
            ),
            format!(
                "Fix the configured image or Podman runtime on {destination}, then run `mj doctor --json --smoke` again."
            ),
        ),
    }
}

/// One check per `ssh-docker` target: Docker daemon, image, and optional
/// remote OverlayFS smoke test, all executed on the SSH host.
fn ssh_docker_checks(
    config: Option<&HelConfig>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::SshDocker { ssh, container } => Some(ssh_docker_check(
                id,
                &backend_ssh(ssh),
                &container.image,
                executor,
                smoke,
            )),
            _ => None,
        })
        .collect()
}

fn ssh_docker_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-docker.{id}");
    let title = format!("Remote Docker for target {id}");
    let destination = &ssh.destination;
    if let SshConnectivity::Failed {
        detail,
        remediation,
    } = ssh_connectivity(ssh, executor)
    {
        return DoctorCheck::fixable(check_id, title, detail, remediation);
    }

    let preflight = match verify_ssh_docker(ssh, executor) {
        Ok(preflight) => preflight,
        Err(error) => {
            let detail = format!("{error:#}");
            return DoctorCheck::fixable(
                check_id,
                title,
                detail,
                format!(
                    "Verify `ssh {destination}` succeeds noninteractively from this host, then install and start Docker Engine there; make sure `docker info` succeeds for the configured SSH user."
                ),
            );
        }
    };

    if smoke {
        let target = RuntimeTargetTemplate::SshDocker {
            ssh: ssh.clone(),
            container: RuntimeContainerTemplate {
                image: image.to_owned(),
                pull_policy: Default::default(),
                extra_run_args: vec![],
                workspace_storage: Default::default(),
            },
        };
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Remote Docker {} is available via {destination}; disposable run/exec/remove and remote OverlayFS attachment smoke test passed for image {image}.",
                    preflight.version
                ),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image} on {destination}: {error:#}"
                ),
                format!(
                    "Fix the configured image or Docker runtime on {destination}, then run `mj doctor --json --smoke` again."
                ),
            ),
        };
    }

    let image_command = ssh_command(
        ssh,
        [
            "docker".to_owned(),
            "image".to_owned(),
            "inspect".to_owned(),
            image.to_owned(),
        ]
        .to_vec(),
    )
    .purpose("check remote Docker image presence");
    match executor.execute(&image_command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "Remote Docker {} is available via {destination}; image {image} is present. Run `mj doctor --json --smoke` to verify remote OverlayFS attachments.",
                preflight.version
            ),
        ),
        Ok(output) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Image {image} is not present in remote Docker storage on {destination}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            format!(
                "Pull it on {destination} with `ssh {destination} docker pull {image}`, or run `mj doctor --json --smoke`."
            ),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not inspect remote Docker image {image} on {destination}: {error}"),
            format!(
                "Verify `ssh {destination} docker info` succeeds, then pull {image} on that host."
            ),
        ),
    }
}

/// Shared disposable-container identity for every doctor smoke test.
fn doctor_smoke_id() -> String {
    format!(
        "doctor-{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn podman_remediation(detail: &str) -> &'static str {
    podman_remediation_match(detail).unwrap_or(
        "Install Podman with `sudo apt update && sudo apt install -y podman uidmap` (Debian/Ubuntu) or `sudo dnf install -y podman shadow-utils` (Fedora).",
    )
}

/// Map a Podman preflight failure to its specific remediation, if one applies.
fn podman_remediation_match(detail: &str) -> Option<&'static str> {
    if detail.contains("Podman 4.0.0") {
        Some(
            "Upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`.",
        )
    } else if detail.contains("podman unshare") {
        Some(
            "Install UID mapping support with `sudo apt install -y uidmap` (Debian/Ubuntu) or `sudo dnf install -y shadow-utils` (Fedora), add `/etc/subuid` and `/etc/subgid` entries, then log out and back in.",
        )
    } else if detail.contains("Rootless") {
        Some(
            "Run mj without `sudo`; unset `CONTAINER_HOST` and select the rootless local Podman connection.",
        )
    } else {
        None
    }
}

const AWS_CLI_INSTALL_URL: &str =
    "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";

/// One check per `aws-ec2` target: the AWS CLI, its credentials, and the
/// configured launch template.
fn aws_checks(config: Option<&HelConfig>, executor: &impl CommandExecutor) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                ..
            } => Some(aws_target_check(
                id,
                aws_profile.as_deref(),
                region,
                launch_template,
                executor,
            )),
            _ => None,
        })
        .collect()
}

/// The profile and region every AWS probe carries, applied exactly the way
/// provisioning applies them in `hel_targets`.
fn aws_global_args<'a>(profile: Option<&'a str>, region: &'a str) -> Vec<String> {
    vec![
        "--profile".to_owned(),
        profile.unwrap_or("default").to_owned(),
        "--region".to_owned(),
        region.to_owned(),
    ]
}

fn aws_target_check(
    id: &str,
    profile: Option<&str>,
    region: &str,
    launch_template: &str,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let check_id = format!("runtime.aws-ec2.{id}");
    let title = format!("AWS EC2 target {id}");
    let profile_label = profile.unwrap_or("default");

    let version = CommandSpec::new("aws", ["--version"]).purpose("check AWS CLI installation");
    match executor.execute(&version) {
        Err(error) => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!("The `aws` command is not available: {error}"),
                format!("Install the AWS CLI and put `aws` on PATH: {AWS_CLI_INSTALL_URL}"),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "`aws --version` failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!("Reinstall the AWS CLI: {AWS_CLI_INSTALL_URL}"),
            );
        }
        Ok(_) => {}
    }

    let mut identity_args = aws_global_args(profile, region);
    identity_args.extend(["sts".to_owned(), "get-caller-identity".to_owned()]);
    identity_args.extend(["--output".to_owned(), "json".to_owned()]);
    let identity =
        CommandSpec::new("aws", identity_args).purpose("check AWS credentials for a doctor target");
    match executor.execute(&identity) {
        Err(error) => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!("Could not run `aws sts get-caller-identity`: {error}"),
                format!(
                    "Configure credentials with `aws configure --profile {profile_label}`, or sign in with `aws sso login --profile {profile_label}`."
                ),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "AWS credentials for profile {profile_label} are not usable: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!(
                    "Configure credentials with `aws configure --profile {profile_label}`, or sign in with `aws sso login --profile {profile_label}`."
                ),
            );
        }
        Ok(_) => {}
    }

    // Launch templates are addressed by id when they carry the `lt-` prefix
    // and by name otherwise, the same split provisioning uses.
    let by_id = launch_template.starts_with("lt-");
    let mut template_args = aws_global_args(profile, region);
    template_args.extend(["ec2".to_owned(), "describe-launch-templates".to_owned()]);
    template_args.extend([
        if by_id {
            "--launch-template-ids".to_owned()
        } else {
            "--launch-template-names".to_owned()
        },
        launch_template.to_owned(),
    ]);
    template_args.extend(["--output".to_owned(), "json".to_owned()]);
    let template =
        CommandSpec::new("aws", template_args).purpose("check the configured AWS launch template");
    let template_remediation = format!(
        "Create the launch template in {region}, or point this target at an existing one; `aws --profile {profile_label} --region {region} ec2 describe-launch-templates` lists them."
    );
    match executor.execute(&template) {
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not query launch template {launch_template}: {error}"),
            template_remediation,
        ),
        Ok(output) if output.status != 0 => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Launch template {launch_template} was not found in {region}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            template_remediation,
        ),
        Ok(_) => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "The AWS CLI is installed, profile {profile_label} has valid credentials, and launch template {launch_template} exists in {region}."
            ),
        ),
    }
}

fn worker_binary_checks(config: Option<&HelConfig>) -> Vec<DoctorCheck> {
    let Some(config) = config else {
        return vec![DoctorCheck::fixable(
            "worker.containers",
            "Container worker binary",
            "Worker availability cannot be checked until config.toml is valid.",
            "Fix config.toml, then rerun `mj doctor --json`.",
        )];
    };
    let containers = config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::AppleContainer { container } => Some((id, container, None)),
            TargetTemplate::SshPodman { container, .. } => {
                Some((id, container, Some("ssh-podman")))
            }
            TargetTemplate::SshDocker { container, .. } => {
                Some((id, container, Some("ssh-docker")))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if containers.is_empty() {
        return vec![DoctorCheck::unsupported(
            "worker.containers",
            "Container worker binary",
            "No container target is configured.",
        )];
    }
    containers
        .into_iter()
        .map(|(id, container, remote_kind)| {
            if let Some(remote_kind) = remote_kind
                && container.platform.is_none()
            {
                // The remote CPU architecture is only observable once the host
                // is reachable, so an explicit `platform` is required here.
                return DoctorCheck::unsupported(
                    format!("worker.{id}"),
                    format!("Container worker binary for target {id}"),
                    format!(
                        "Set `platform` on this {remote_kind} target to check its worker binary; the remote architecture is unknown until provisioning."
                    ),
                );
            }
            worker_binary_check(id, container)
        })
        .collect()
}

fn worker_binary_check(id: &str, container: &ContainerTemplate) -> DoctorCheck {
    let title = format!("Container worker binary for target {id}");
    let arch = match container_architecture(container.platform.as_deref()) {
        Ok(arch) => arch,
        Err(reason) => {
            return DoctorCheck::unsupported(format!("worker.{id}"), title, reason);
        }
    };
    let triple = format!("{arch}-unknown-linux-musl");
    match worker_binary_prerequisite_for_arch(arch) {
        Ok(WorkerBinaryAvailability::Local { path, source }) => DoctorCheck::ready(
            format!("worker.{id}"),
            title,
            format!(
                "{triple} worker is available from {source}: {}",
                path.display()
            ),
        ),
        Ok(WorkerBinaryAvailability::Remote { url, .. }) => DoctorCheck::ready(
            format!("worker.{id}"),
            title,
            format!("{triple} worker will be verified and downloaded from {url} when needed."),
        ),
        Err(error) => DoctorCheck::fixable(
            format!("worker.{id}"),
            title,
            format!("No usable {triple} worker source: {error:#}"),
            format!(
                "Build it with `cargo build --release --target {triple} -p brokk-mj-worker --bin mj-worker`, install `mj-worker-{triple}` beside `mj`, or set MJ_WORKER_BINARY, MJ_WORKER_DIR, or MJ_WORKER_URL with MJ_WORKER_SHA256."
            ),
        ),
    }
}

fn container_architecture(platform: Option<&str>) -> std::result::Result<&'static str, String> {
    let candidate = platform.unwrap_or(std::env::consts::ARCH);
    let candidate = candidate
        .split('/')
        .rev()
        .find(|part| matches!(*part, "x86_64" | "amd64" | "aarch64" | "arm64"))
        .unwrap_or(candidate);
    match candidate {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        other => Err(format!(
            "Container architecture {other:?} is unsupported; Mjolnir supports x86_64 and aarch64 Linux workers."
        )),
    }
}

fn apple_container_image(config: Option<&HelConfig>) -> String {
    config
        .and_then(|config| {
            config.targets.values().find_map(|target| match target {
                TargetTemplate::AppleContainer { container } => Some(container.image.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.into())
}

pub fn apple_container_check(
    platform: &ApplePlatform,
    executor: &impl CommandExecutor,
    smoke: bool,
    image: String,
) -> DoctorCheck {
    match platform {
        ApplePlatform::Linux => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                "macOS only",
            );
        }
        ApplePlatform::Other(current) => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                format!("macOS only (current platform: {current})"),
            );
        }
        ApplePlatform::Macos {
            architecture,
            major_version,
        } if architecture != "aarch64" && architecture != "arm64" => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                "Apple container requires Apple silicon; Intel Macs are unsupported.",
            );
        }
        ApplePlatform::Macos { major_version, .. } if *major_version < 26 => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                format!("Apple container requires macOS 26 or newer (found {major_version})."),
            );
        }
        ApplePlatform::Macos { .. } => {}
    }

    let daemon = apple_container_daemon_check(executor);
    if daemon.status != CheckStatus::Ready {
        return daemon;
    }

    if !smoke {
        return DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            "The daemon is running, but the required disposable smoke test was not requested.",
            "Run `mj doctor --json --smoke`.",
        );
    }

    let target = RuntimeTargetTemplate::AppleContainer(RuntimeContainerTemplate {
        image,
        pull_policy: Default::default(),
        extra_run_args: vec![],
        workspace_storage: Default::default(),
    });
    match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
        Ok(()) => DoctorCheck::ready(
            "runtime.apple-container",
            "Apple container runtime",
            "Installed, daemon running, and disposable run/exec/remove smoke test passed.",
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!("Disposable run/exec/remove smoke test failed: {error:#}"),
            "Fix the configured image or container runtime, then run `mj doctor --json --smoke` again.",
        ),
    }
}

/// Probe that the Apple `container` command is installed and its daemon is
/// running, phrased as a doctor check.
///
/// Split out of [`apple_container_check`] so `mj setup` can reuse the same
/// probes and remediation text without also demanding the opt-in smoke test.
/// The caller is responsible for platform gating.
pub fn apple_container_daemon_check(executor: &impl CommandExecutor) -> DoctorCheck {
    let installed =
        CommandSpec::new("container", ["--version"]).purpose("check Apple container installation");
    match executor.execute(&installed) {
        Err(error) => {
            return DoctorCheck::fixable(
                "runtime.apple-container",
                "Apple container runtime",
                format!("The `container` command is not available: {error}"),
                format!("Install the official signed package: {APPLE_CONTAINER_INSTALL_URL}"),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                "runtime.apple-container",
                "Apple container runtime",
                format!(
                    "The installed `container --version` command failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!("Reinstall the official signed package: {APPLE_CONTAINER_INSTALL_URL}"),
            );
        }
        Ok(_) => {}
    }

    let status =
        CommandSpec::new("container", ["system", "status"]).purpose("check Apple container daemon");
    match executor.execute(&status) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            "runtime.apple-container",
            "Apple container runtime",
            "Installed, and the Apple container daemon is running.",
        ),
        Ok(output) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!(
                "The Apple container daemon is stopped: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            "Run `container system start`.",
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!("Could not query the Apple container daemon: {error}"),
            "Run `container system start`.",
        ),
    }
}

pub fn current_apple_platform(executor: &impl CommandExecutor) -> ApplePlatform {
    if cfg!(target_os = "linux") {
        return ApplePlatform::Linux;
    }
    if !cfg!(target_os = "macos") {
        return ApplePlatform::Other(std::env::consts::OS.into());
    }
    let major_version = executor
        .execute(&CommandSpec::new("sw_vers", ["-productVersion"]).purpose("detect macOS version"))
        .ok()
        .filter(|output| output.status == 0)
        .and_then(|output| {
            String::from_utf8(output.stdout)
                .ok()
                .and_then(|value| value.trim().split('.').next()?.parse().ok())
        })
        .unwrap_or(0);
    ApplePlatform::Macos {
        architecture: std::env::consts::ARCH.into(),
        major_version,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use anyhow::anyhow;

    use super::*;
    use hel::hel_targets::CommandOutput;

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

    fn passing_ssh_podman_probes() -> Vec<Result<CommandOutput>> {
        let mut responses = passing_podman_probes();
        responses.push(Ok(output(b"yes\n")));
        responses
    }

    fn passing_podman_probes() -> Vec<Result<CommandOutput>> {
        vec![
            Ok(output(b"podman version 5.4.2\n")),
            Ok(output(b"true\n")),
            Ok(output(
                b"         0       1000          1\n         1     100000      65536\n",
            )),
        ]
    }

    fn container(image: &str) -> ContainerTemplate {
        ContainerTemplate {
            image: image.to_owned(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: std::collections::BTreeMap::new(),
            workspace_storage: Default::default(),
        }
    }

    fn ssh_connection() -> hel::hel_config::SshConnection {
        hel::hel_config::SshConnection {
            host: "example.test".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: vec![],
        }
    }

    #[test]
    fn doctor_reports_a_config_owned_by_a_newer_hel_as_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "version = {}\n\n[targets.localhost]\nkind = \"local-bare\"\n",
                hel::hel_config::CONFIG_VERSION + 1
            ),
        )
        .unwrap();

        let (config, checks) = configuration_checks(&path);

        assert!(config.is_some());
        let check = checks.iter().find(|check| check.id == "config").unwrap();
        assert_eq!(check.status, CheckStatus::Warning);
        assert!(check.detail.contains("read-only"), "{}", check.detail);
    }

    fn config_with(targets: impl IntoIterator<Item = (&'static str, TargetTemplate)>) -> HelConfig {
        HelConfig {
            targets: targets
                .into_iter()
                .map(|(id, target)| (id.to_owned(), target))
                .collect(),
            ..HelConfig::default()
        }
    }

    fn runtime_ssh() -> RuntimeSshTarget {
        backend_ssh(&ssh_connection())
    }

    #[test]
    fn podman_check_is_unsupported_without_a_valid_config() {
        let executor = FakeExecutor::new([]);

        let check = podman_check(None, &executor);

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

        let check = podman_check(Some(&config), &executor);

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

        let check = podman_check(Some(&config), &executor);

        assert_eq!(check.status, CheckStatus::Ready);
        assert!(check.detail.contains("Podman 5.4.2"));
        assert_eq!(executor.commands.borrow().len(), 3);
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

        let check = podman_check(Some(&config), &executor);

        assert_eq!(check.status, CheckStatus::Fixable);
        assert!(
            check
                .remediation
                .as_deref()
                .unwrap()
                .contains("Upgrade Podman")
        );
    }

    #[test]
    fn podman_image_check_is_ready_when_the_image_is_present() {
        let executor = FakeExecutor::new([Ok(output(b""))]);

        let check =
            podman_image_check("podman", "localhost/hel/agent-dev:latest", &executor, false);

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

        let checks = podman_checks(Some(&config), &executor, false);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].id, "runtime.podman");
    }

    #[test]
    fn image_checks_follow_a_passing_preflight_for_each_local_podman_target() {
        let mut responses = passing_podman_probes();
        responses.push(Ok(output(b"")));
        responses.push(Ok(failed(b"")));
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

        let checks = podman_checks(Some(&config), &executor, false);

        assert_eq!(
            checks
                .iter()
                .map(|check| check.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "runtime.podman",
                "runtime.podman.image.alpha",
                "runtime.podman.image.beta"
            ]
        );
        assert_eq!(checks[1].status, CheckStatus::Ready);
        assert_eq!(checks[2].status, CheckStatus::Fixable);
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

        let checks = docker_checks(Some(&config), &executor, false);

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

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

        assert_eq!(check.id, "runtime.ssh-podman.remote");
        assert_eq!(check.title, "Remote Podman for target remote");
        assert_eq!(check.status, CheckStatus::Ready);
        assert!(check.detail.contains("Remote rootless Podman 5.4.2"));
        assert!(check.detail.contains("dev@example.test"));
        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 5);
        assert_eq!(commands[0].args.last().unwrap(), "'true'");
        for command in commands.iter().skip(1) {
            assert_eq!(command.program, "ssh");
            assert!(command.args.contains(&"dev@example.test".to_owned()));
        }
        assert!(
            commands[4]
                .args
                .last()
                .unwrap()
                .contains("'loginctl show-user")
        );
    }

    #[test]
    fn ssh_podman_check_warns_when_remote_user_lingering_is_disabled() {
        let mut responses = passing_podman_probes();
        responses.push(Ok(output(b"no\n")));
        let executor = FakeExecutor::new(reachable_then(responses));

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

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
        let mut responses = passing_podman_probes();
        responses.push(Ok(CommandOutput {
            status: 127,
            stdout: vec![],
            stderr: b"sh: loginctl: not found\n".to_vec(),
        }));
        let executor = FakeExecutor::new(reachable_then(responses));

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

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
        let executor = FakeExecutor::new(reachable_then([Ok(output(b"podman version 3.4.7\n"))]));

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

        assert_eq!(check.status, CheckStatus::Fixable);
        assert!(check.detail.contains("dev@example.test"));
        assert!(
            check
                .remediation
                .as_deref()
                .unwrap()
                .starts_with("On dev@example.test: Upgrade Podman")
        );
    }

    #[test]
    fn ssh_podman_check_reports_the_shared_ssh_remediation_before_probing_podman() {
        let executor = FakeExecutor::new([Ok(failed(
            b"dev@example.test: Permission denied (publickey).",
        ))]);

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, false);

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

        let check = ssh_podman_check("remote", &runtime_ssh(), "ubuntu:24.04", &executor, true);

        assert_eq!(check.status, CheckStatus::Ready);
        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 8);
        for command in commands.iter().skip(5) {
            assert_eq!(command.program, "ssh");
            assert!(command.args.contains(&"dev@example.test".to_owned()));
        }
        assert!(commands[5].args.last().unwrap().contains("'run' '--init'"));
        assert!(commands[6].args.last().unwrap().ends_with("'true'"));
        assert!(commands[7].args.last().unwrap().contains("'rm' '--force'"));
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

        assert!(ssh_podman_checks(None, &executor, false).is_empty());
        assert!(executor.commands.borrow().is_empty());
    }

    fn ssh_bare_config() -> HelConfig {
        config_with([(
            "builder",
            TargetTemplate::SshBare {
                ssh: hel::hel_config::SshConnection {
                    host: "example.test".into(),
                    user: Some("dev".into()),
                    identity_file: Some(PathBuf::from("/home/dev/.ssh/id_ed25519")),
                    extra_args: vec![],
                },
                permissions: hel::hel_config::PermissionMode::Yolo,
                workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
            },
        )])
    }

    #[test]
    fn ssh_bare_check_is_ready_when_the_batch_mode_probe_succeeds() {
        let executor = FakeExecutor::new([Ok(output(b""))]);

        let checks = ssh_bare_checks(Some(&ssh_bare_config()), &executor);

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

        let checks = ssh_bare_checks(Some(&ssh_bare_config()), &executor);

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

        let checks = ssh_bare_checks(Some(&ssh_bare_config()), &executor);

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
        let executor = FakeExecutor::new([Err(anyhow!("No such file or directory (os error 2)"))]);

        let checks = ssh_bare_checks(Some(&ssh_bare_config()), &executor);

        assert_eq!(checks[0].status, CheckStatus::Fixable);
        assert_eq!(
            checks[0].remediation.as_deref(),
            Some(SSH_MISSING_REMEDIATION)
        );
    }

    #[test]
    fn ssh_bare_check_falls_back_to_quoting_an_unrecognized_ssh_failure() {
        let executor = FakeExecutor::new([Ok(failed(
            b"ssh: connect to host example.test port 22: Connection timed out",
        ))]);

        let checks = ssh_bare_checks(Some(&ssh_bare_config()), &executor);

        let remediation = checks[0].remediation.as_deref().unwrap();
        assert!(
            remediation.contains("Connection timed out"),
            "{remediation}"
        );
        assert!(
            remediation.contains("Run `ssh dev@example.test true` by hand"),
            "{remediation}"
        );
    }

    #[test]
    fn ssh_bare_checks_are_skipped_without_a_valid_config() {
        let executor = FakeExecutor::new([]);

        assert!(ssh_bare_checks(None, &executor).is_empty());
        assert!(executor.commands.borrow().is_empty());
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

        let checks = worker_binary_checks(Some(&config));

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

        let checks = worker_binary_checks(Some(&config));

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

        let checks = worker_binary_checks(Some(&config));

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
        let home = directory.path().join("claude-home");
        std::fs::create_dir_all(&home).unwrap();
        let profile = HarnessProfile {
            kind: HarnessKind::Claude,
            home,
            environment: std::collections::BTreeMap::new(),
            context_window_bytes: None,
        };
        let config = HelConfig {
            profiles: [("work".to_owned(), profile.clone())].into_iter().collect(),
            ..HelConfig::default()
        };

        let executor = FakeExecutor::new([Ok(output(br#"{"loggedIn":false}"#))]);
        let checks = harness_checks(Some(&config), &executor);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Fixable);
        let remediation = checks[0].remediation.as_deref().unwrap();
        assert!(
            remediation.contains("mj login --profile work"),
            "{remediation}"
        );
        // The underlying command is quoted from the one place that verified it,
        // so doctor cannot recommend something `mj login` does not run.
        let (program, arguments) = login_command(&profile);
        assert!(
            remediation.contains(&format!("`{program} {}`", arguments.join(" "))),
            "{remediation}"
        );
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

    #[test]
    fn missing_harness_homes_are_fixable_without_a_configured_profile() {
        let check = harness_discovery_check_from(&[], false);

        assert_eq!(check.status, CheckStatus::Fixable);
        assert_eq!(
            check.remediation.as_deref(),
            Some("Install and sign in to a supported harness, then run `mj setup`.")
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
                format!("Install the official signed package: {APPLE_CONTAINER_INSTALL_URL}")
                    .as_str()
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
            address_source: hel::hel_config::AwsAddressSource::default(),
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

        let checks = aws_checks(Some(&config), &executor);

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

        assert!(aws_checks(Some(&config), &executor).is_empty());
        assert!(aws_checks(None, &executor).is_empty());
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
    fn linux_instructions_embed_podman_postconditions_and_doctor_loop() {
        let instructions = setup_instructions(InstructionsPlatform::Linux);
        assert!(instructions.contains("mj doctor --json"));
        assert!(instructions.contains("mj doctor --json --smoke"));
        assert!(instructions.contains("podman unshare cat /proc/self/uid_map"));
        assert!(instructions.contains("Podman **4.0.0 or newer**"));
        assert!(instructions.contains("kind = \"local-docker\""));
        assert!(instructions.contains("--opt type=overlay"));
    }
}
