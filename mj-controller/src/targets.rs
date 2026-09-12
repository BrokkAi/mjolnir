//! Declarative execution plans for Hel session targets.
//!
//! Plans deliberately contain argv vectors instead of local shell strings.  A
//! shell is used only at the SSH boundary, where OpenSSH necessarily sends a
//! command string; every remotely supplied argument is POSIX-quoted there.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::ImagePullPolicy;

pub use mj_core::targets::*;
pub const PODMAN_DOCUMENTATION_PATH: &str = "docs/PODMAN.md";
pub const DOCKER_DOCUMENTATION_PATH: &str = "docs/DOCKER.md";

// `mj doctor` prints a self-contained setup page that quotes these two pages in
// full. They are embedded here, beside the paths that name them, because this
// crate's `include` list is what carries `docs/` into the published package;
// the controller crate that renders the page cannot reach outside its own
// directory.
/// The rootless Podman postconditions page, verbatim.
pub const PODMAN_DOCUMENTATION: &str = include_str!("../docs/PODMAN.md");
/// The Docker postconditions page, verbatim.
pub const DOCKER_DOCUMENTATION: &str = include_str!("../docs/DOCKER.md");

const PODMAN_MINIMUM_MAJOR_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedResourceKind {
    Container,
    Ec2Instance,
}

/// Build command-line fragments that identify resources Hel owns for a session.
fn managed_resource_identity_args(kind: ManagedResourceKind, session_id: &str) -> Vec<String> {
    match kind {
        ManagedResourceKind::Container => vec![
            "--label".to_owned(),
            format!("{SESSION_LABEL}={session_id}"),
            "--label".to_owned(),
            format!("{MANAGED_LABEL}=true"),
        ],
        ManagedResourceKind::Ec2Instance => vec![
            "--tag-specifications".to_owned(),
            format!(
                "ResourceType=instance,Tags=[{{Key={SESSION_TAG},Value={session_id}}},{{Key={MANAGED_TAG},Value=true}}]"
            ),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanPreflight {
    pub version: String,
    /// Non-fatal host configuration problems that can make sessions fragile.
    pub warnings: Vec<PodmanPreflightWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanPreflightWarning {
    pub detail: String,
    pub remediation: String,
}

impl PodmanPreflightWarning {
    pub fn notice(&self) -> String {
        format!("{} {}", self.detail, self.remediation)
    }
}

/// Where the Podman prerequisite probes run.
///
/// The same postconditions apply locally and over SSH; only the command
/// wrapping and the wording of a failure differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PodmanHost<'a> {
    Local,
    Ssh(&'a SshTarget),
}

impl PodmanHost<'_> {
    /// Sentence opener for every failure raised by these probes.
    fn failure(self) -> String {
        match self {
            Self::Local => "Podman preflight failed".to_owned(),
            Self::Ssh(ssh) => format!("Remote Podman preflight failed on {}", ssh.destination),
        }
    }

    /// Prefix that says where a remediation must be applied.
    fn remediation_scope(self) -> String {
        match self {
            Self::Local => String::new(),
            Self::Ssh(ssh) => format!("On {}: ", ssh.destination),
        }
    }

    fn command(self, args: &[&str], purpose: &'static str) -> CommandSpec {
        self.command_owned(args.iter().map(|arg| (*arg).to_owned()).collect(), purpose)
    }

    fn command_owned(self, args: Vec<String>, purpose: &'static str) -> CommandSpec {
        match self {
            Self::Local => {
                CommandSpec::new(args[0].clone(), args[1..].iter().cloned()).purpose(purpose)
            }
            Self::Ssh(ssh) => ssh_validation_command(ssh, args, purpose),
        }
        .stage(ProvisionStage::Provisioning)
    }
}

/// Verify the fast local preconditions for Hel's rootless Podman target.
///
/// This intentionally never pulls an image. Image availability is verified by
/// `mj setup`'s smoke test and by the subsequent target creation command.
pub fn verify_local_podman(executor: &impl CommandExecutor) -> Result<PodmanPreflight> {
    verify_podman(PodmanHost::Local, executor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerPreflight {
    pub version: String,
}

/// Verify that the Docker CLI can reach a Linux Docker daemon.
///
/// Image and OverlayFS support are exercised by the setup/doctor smoke test;
/// this fast probe runs before every launch and never pulls an image.
pub fn verify_local_docker(executor: &impl CommandExecutor) -> Result<DockerPreflight> {
    verify_docker(None, executor)
}

pub fn verify_ssh_docker(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<DockerPreflight> {
    validate_ssh(ssh)?;
    verify_docker(Some(ssh), executor).with_context(|| {
        format!(
            "Docker preflight on {} failed; run docker info on that SSH host",
            ssh.destination
        )
    })
}

fn verify_docker(
    ssh: Option<&SshTarget>,
    executor: &impl CommandExecutor,
) -> Result<DockerPreflight> {
    let command = CommandSpec::new(
        "docker",
        ["version", "--format", "{{.Server.Version}} {{.Server.Os}}"],
    )
    .purpose("check Docker daemon")
    .stage(ProvisionStage::Provisioning);
    let command = match ssh {
        Some(ssh) => command_over_ssh(command, ssh),
        None => command,
    };
    let output = executor
        .execute(&command)
        .context("Docker preflight failed: run `docker info` as the user running Mjolnir")?;
    ensure!(
        output.status == 0,
        "Docker preflight failed: `docker version` exited with status {}: {}. Run `docker info` as the user running Mjolnir. See {DOCKER_DOCUMENTATION_PATH}.",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let reported = String::from_utf8_lossy(&output.stdout);
    let mut fields = reported.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let os = fields.next().unwrap_or_default();
    ensure!(
        !version.is_empty() && os == "linux",
        "Docker preflight failed: expected a Linux Docker daemon, got {:?}. See {DOCKER_DOCUMENTATION_PATH}.",
        reported.trim()
    );
    Ok(DockerPreflight {
        version: version.to_owned(),
    })
}

/// Verify the same rootless Podman preconditions on an SSH host.
///
/// The probes run through the noninteractive SSH options, so an unreachable
/// host fails fast instead of blocking doctor or session preflight.
pub fn verify_ssh_podman(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<PodmanPreflight> {
    let host = PodmanHost::Ssh(ssh);
    validate_ssh(ssh).map_err(|error| {
        anyhow::anyhow!(
            "{}: the configured SSH destination is unusable ({error}). Set a valid `host` (and optional `user`) for this ssh-podman target. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure()
        )
    })?;
    let mut preflight = verify_podman(host, executor)?;
    if let Some(warning) = ssh_podman_linger_warning(ssh, executor) {
        preflight.warnings.push(warning);
    }
    Ok(preflight)
}

fn verify_podman(host: PodmanHost<'_>, executor: &impl CommandExecutor) -> Result<PodmanPreflight> {
    let version = execute_podman_preflight(
        executor,
        host,
        &["podman", "--version"],
        "check Podman version",
        "Postcondition `podman --version` succeeds with Podman 4.0.0 or newer",
        "Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`.",
    )?;
    let version = parse_podman_version(host, &version.stdout)?;

    let rootless = execute_podman_preflight(
        executor,
        host,
        &["podman", "info", "--format", "{{.Host.Security.Rootless}}"],
        "check rootless Podman mode",
        "Postcondition `podman info --format '{{.Host.Security.Rootless}}'` prints `true`",
        "Run Mjolnir as the ordinary user without `sudo`; if a remote Podman connection is configured, unset `CONTAINER_HOST` or select the rootless local connection.",
    )?;
    let rootless_output = String::from_utf8_lossy(&rootless.stdout);
    if rootless_output.trim() != "true" {
        bail!(
            "{}: Postcondition `podman info --format '{{{{.Host.Security.Rootless}}}}'` prints `true` returned {:?}. {}Run Mjolnir as the ordinary user without `sudo`; if a remote Podman connection is configured, unset `CONTAINER_HOST` or select the rootless local connection. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure(),
            rootless_output.trim(),
            host.remediation_scope(),
        );
    }

    let uid_map = execute_podman_preflight(
        executor,
        host,
        &["podman", "unshare", "cat", "/proc/self/uid_map"],
        "check rootless Podman UID map",
        "Postcondition `podman unshare cat /proc/self/uid_map` maps container UIDs 0 and 1",
        "Install UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora), then add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"` and start a fresh login session.",
    )?;
    if !valid_rootless_uid_map(&uid_map.stdout) {
        bail!(
            "{}: Postcondition `podman unshare cat /proc/self/uid_map` maps container UIDs 0 and 1 was not met. {}Add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"`, verify `/etc/subuid` and `/etc/subgid`, then log out and back in. See {PODMAN_DOCUMENTATION_PATH}.",
            host.failure(),
            host.remediation_scope(),
        );
    }

    Ok(PodmanPreflight {
        version,
        warnings: Vec::new(),
    })
}

/// Report either an explicitly unsafe systemd setting or an unavailable
/// durability check. Neither condition makes an otherwise usable target fail.
fn ssh_podman_linger_warning(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Option<PodmanPreflightWarning> {
    let command = PodmanHost::Ssh(ssh).command(
        &[
            "sh",
            "-c",
            "loginctl show-user \"$(id -u)\" --property=Linger --value",
        ],
        "check remote user lingering",
    );
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            return Some(linger_unavailable_warning(
                ssh,
                format!("the probe could not run: {error}"),
            ));
        }
    };
    let linger = String::from_utf8_lossy(&output.stdout);
    match (output.status, linger.trim().to_ascii_lowercase().as_str()) {
        (0, "yes") => None,
        (0, "no") => Some(PodmanPreflightWarning {
            detail: format!(
                "Remote user lingering is disabled on {}; SSH-Podman sessions may be terminated when the last SSH connection closes.",
                ssh.destination
            ),
            remediation: format!(
                "On {}, run `sudo loginctl enable-linger \"$(id -un)\"`.",
                ssh.destination
            ),
        }),
        (status, _) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let reason = if status == 127 || stderr.contains("loginctl: not found") {
                "`loginctl` was not found; this host may not use systemd".to_owned()
            } else if status != 0 {
                format!("`loginctl` exited with status {status}: {stderr}")
            } else {
                format!("`loginctl` returned an unrecognized Linger value {linger:?}")
            };
            Some(linger_unavailable_warning(ssh, reason))
        }
    }
}

fn linger_unavailable_warning(ssh: &SshTarget, reason: String) -> PodmanPreflightWarning {
    PodmanPreflightWarning {
        detail: format!(
            "Remote user-manager durability check is unavailable on {} because {reason}. Mjolnir cannot verify whether rootless Podman sessions survive logout.",
            ssh.destination
        ),
        remediation: format!(
            "Configure {}'s service manager to keep the user and rootless Podman services running after logout; if it uses systemd, make `loginctl` available and enable lingering.",
            ssh.destination
        ),
    }
}

fn execute_podman_preflight(
    executor: &impl CommandExecutor,
    host: PodmanHost<'_>,
    args: &[&str],
    purpose: &'static str,
    postcondition: &str,
    remediation: &str,
) -> Result<CommandOutput> {
    let command = host.command(args, purpose);
    let failure = host.failure();
    let scope = host.remediation_scope();
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => match ssh_transport_failure(host, &error.to_string()) {
            Some(message) => bail!("{message}"),
            None => bail!(
                "{failure}: {postcondition}. {scope}{remediation} See {PODMAN_DOCUMENTATION_PATH}. Underlying error: {error}"
            ),
        },
    };
    if output.status == SSH_TRANSPORT_EXIT_STATUS
        && let Some(message) =
            ssh_transport_failure(host, String::from_utf8_lossy(&output.stderr).trim())
    {
        bail!("{message}");
    }
    if output.status != 0 {
        bail!(
            "{failure}: {postcondition}. {scope}{remediation} See {PODMAN_DOCUMENTATION_PATH}. Podman reported: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// `ssh` reserves exit status 255 for its own connection failures; the Podman
/// probes never produce it. Reporting that case separately keeps an
/// unreachable host from being mistaken for a broken Podman installation.
const SSH_TRANSPORT_EXIT_STATUS: i32 = 255;

fn ssh_transport_failure(host: PodmanHost<'_>, reported: &str) -> Option<String> {
    let PodmanHost::Ssh(ssh) = host else {
        return None;
    };
    let destination = &ssh.destination;
    Some(format!(
        "{}: SSH could not run the probes on {destination}. Verify that `ssh {destination}` succeeds noninteractively from this host. See {PODMAN_DOCUMENTATION_PATH}. ssh reported: {reported}",
        host.failure()
    ))
}

fn parse_podman_version(host: PodmanHost<'_>, stdout: &[u8]) -> Result<String> {
    let failure = host.failure();
    let scope = host.remediation_scope();
    let version = String::from_utf8_lossy(stdout).trim().to_owned();
    let Some(candidate) = version
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
    else {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer returned {version:?}. {scope}Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    };
    let Some(major) = candidate
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
    else {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer returned {version:?}. {scope}Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    };
    if major < PODMAN_MINIMUM_MAJOR_VERSION {
        bail!(
            "{failure}: Postcondition `podman --version` succeeds with Podman 4.0.0 or newer was not met (found {candidate}). {scope}Upgrade Podman to 4.0.0 or newer: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`. See {PODMAN_DOCUMENTATION_PATH}."
        );
    }
    Ok(candidate.to_owned())
}

fn valid_rootless_uid_map(stdout: &[u8]) -> bool {
    let mappings = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    [0, 1].into_iter().all(|container_id| {
        mappings.iter().any(|(inside, _outside, length)| {
            inside
                .checked_add(*length)
                .is_some_and(|end| *inside <= container_id && container_id < end)
        })
    })
}

/// Create the initial resource. AWS address discovery and all SSH bootstrap
/// happen after parsing the `run-instances` response and constructing a locator.
pub fn provision_plan(
    template: &TargetTemplate,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandPlan> {
    bundle.validate()?;
    if !additional_mounts.is_empty()
        && !matches!(
            template,
            TargetTemplate::LocalPodman(_)
                | TargetTemplate::LocalDocker(_)
                | TargetTemplate::AppleContainer(_)
                | TargetTemplate::SshPodman { .. }
                | TargetTemplate::SshDocker { .. }
        )
    {
        bail!("additional mounts require a container-backed target");
    }
    if let TargetTemplate::SshDocker { ssh, container } = template {
        validate_ssh(ssh)?;
        let mut plan = provision_plan(
            &TargetTemplate::LocalDocker(container.clone()),
            session_id,
            bundle,
            additional_mounts,
        )?;
        plan.commands = plan
            .commands
            .into_iter()
            .map(|command| command_over_ssh(command, ssh))
            .collect();
        return Ok(plan);
    }
    let name = resource_name(session_id)?;
    let mut commands = Vec::new();
    match template {
        TargetTemplate::LocalBare => {
            bail!("local bare projects must use the existing-project provisioning path")
        }
        TargetTemplate::LocalPodman(container) => {
            validate_container_template(container)?;
            commands.push(podman_container_run(
                container,
                &name,
                session_id,
                additional_mounts,
                None,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "podman",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("podman", &name, args)
            }));
        }
        TargetTemplate::LocalDocker(container) => {
            validate_container_template(container)?;
            commands.push(docker_container_run(
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "docker",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("docker", &name, args)
            }));
        }
        TargetTemplate::AppleContainer(container) => {
            validate_container_template(container)?;
            commands.push(
                CommandSpec::new("container", ["system", "status"])
                    .purpose("check Apple container service")
                    .stage(ProvisionStage::Provisioning),
            );
            commands.extend(apple_image_prepare_commands(container));
            commands.push(container_run(
                "container",
                container,
                &name,
                session_id,
                additional_mounts,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "container",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                container_exec("container", &name, args)
            }));
        }
        TargetTemplate::AwsEc2(aws) => {
            validate_aws(aws)?;
            let launch_key = if aws.launch_template.starts_with("lt-") {
                "LaunchTemplateId"
            } else {
                "LaunchTemplateName"
            };
            let mut launch = format!("{launch_key}={}", aws.launch_template);
            if let Some(version) = &aws.launch_template_version {
                launch.push_str(",Version=");
                launch.push_str(version);
            }
            let mut args = vec![
                "--profile".to_owned(),
                aws.profile.clone(),
                "--region".to_owned(),
                aws.region.clone(),
                "ec2".to_owned(),
                "run-instances".to_owned(),
                "--launch-template".to_owned(),
                launch,
            ];
            if let Some(instance_type) = &aws.instance_type {
                args.extend(["--instance-type".to_owned(), instance_type.clone()]);
            }
            args.extend(managed_resource_identity_args(
                ManagedResourceKind::Ec2Instance,
                session_id,
            ));
            args.extend(["--output".to_owned(), "json".to_owned()]);
            commands.push(
                CommandSpec::new("aws", args)
                    .purpose("launch EC2 session instance")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
        }
        TargetTemplate::SshBare {
            ssh,
            workspace_prefix: _,
        } => {
            validate_ssh(ssh)?;
            let workspace = workspace_for(template, session_id)?;
            commands.push(
                ssh_command(ssh, ["mkdir", "-p", &workspace])
                    .purpose("create SSH session workspace")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
            commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
            commands.extend(clone_commands(bundle, &workspace, |args| {
                ssh_command_owned(ssh, args)
            }));
        }
        TargetTemplate::SshDocker { .. } => unreachable!("handled above"),
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            validate_container_template(container)?;
            commands.push(podman_container_run(
                container,
                &name,
                session_id,
                additional_mounts,
                Some(ssh),
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::SshContainer {
                    engine: "podman",
                    ssh,
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, CONTAINER_WORKSPACE, |args| {
                let mut remote = vec!["podman".to_owned(), "exec".to_owned(), name.clone()];
                remote.extend(args);
                ssh_command_owned(ssh, remote)
            }));
        }
    }
    Ok(CommandPlan {
        description: format!("provision Mjolnir session {session_id}"),
        commands,
    })
}

/// Build the no-op infrastructure plan for an existing bare project.
/// The wizard validates the project for early feedback; worker/ACP startup is
/// authoritative if it changes before launch. Worker state is installed later
/// under the dedicated worker and profile roots, not under a cloned workspace.
pub fn provision_bare_project_plan(
    template: &TargetTemplate,
    session_id: &str,
    project_directory: &str,
) -> Result<CommandPlan> {
    let project = std::path::Path::new(project_directory);
    validate_bare_project_path(project)?;
    match template {
        TargetTemplate::LocalBare => {}
        TargetTemplate::SshBare { ssh, .. } => {
            validate_ssh(ssh)?;
            workspace_for(template, session_id)?;
        }
        _ => bail!("raw project directories require a bare target"),
    }
    Ok(CommandPlan {
        description: format!("provision Mjolnir session {session_id}"),
        commands: Vec::new(),
    })
}

/// Create the short-lived local container used to verify a setup target.
///
/// This deliberately shares the same argv construction as session targets so
/// setup catches an unusable image or runtime before the first session exists.
pub fn setup_smoke_plan(template: &TargetTemplate, smoke_id: &str) -> Result<CommandPlan> {
    let name = resource_name(smoke_id)?;
    let (engine, container, boundary) = match template {
        TargetTemplate::LocalPodman(container) => ("podman", container, ExecutionBoundary::Direct),
        TargetTemplate::LocalDocker(container) => ("docker", container, ExecutionBoundary::Direct),
        TargetTemplate::AppleContainer(container) => {
            ("container", container, ExecutionBoundary::Direct)
        }
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            ("podman", container, ExecutionBoundary::Ssh(ssh))
        }
        TargetTemplate::SshDocker { ssh, container } => {
            validate_ssh(ssh)?;
            ("docker", container, ExecutionBoundary::Ssh(ssh))
        }
        _ => bail!("setup smoke tests require a local or SSH container target"),
    };
    validate_container_template(container)?;

    let mut run = vec![engine.to_owned()];
    run.extend(container_run_args(
        engine,
        container,
        &name,
        smoke_id,
        &[],
        None,
    )?);
    let exec = vec![
        engine.to_owned(),
        "exec".to_owned(),
        "-i".to_owned(),
        name.clone(),
        "true".to_owned(),
    ];
    let remove = vec![
        engine.to_owned(),
        "rm".to_owned(),
        "--force".to_owned(),
        name,
    ];

    Ok(CommandPlan {
        description: format!("smoke test Mjolnir setup target {smoke_id}"),
        commands: vec![
            at_boundary(boundary, run).purpose("create disposable setup container"),
            at_boundary(boundary, exec).purpose("execute setup smoke command"),
            at_boundary(boundary, remove).purpose("remove disposable setup container"),
        ],
    })
}

/// Run the disposable setup smoke test and always attempt container cleanup
/// after a successful create step.
pub fn run_setup_smoke_test(
    template: &TargetTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    if let TargetTemplate::LocalDocker(container) = template {
        return run_docker_overlay_smoke_test(container, smoke_id, executor);
    }
    if let TargetTemplate::SshDocker { ssh, container } = template {
        return run_ssh_docker_overlay_smoke_test(ssh, container, smoke_id, executor);
    }
    let plan = setup_smoke_plan(template, smoke_id)?;
    execute_checked(executor, &plan.commands[0])?;
    let smoke_result = execute_checked(executor, &plan.commands[1]);
    let cleanup_result = execute_checked(executor, &plan.commands[2]);
    smoke_result?;
    cleanup_result
}

fn run_ssh_docker_overlay_smoke_test(
    ssh: &SshTarget,
    container: &ContainerTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_ssh(ssh)?;
    validate_container_template(container)?;
    let name = resource_name(smoke_id)?;
    let prepare = ssh_command(ssh, ["sh", "-c",
        "set -eu; root=$(mktemp -d /tmp/mj-docker-overlay-smoke.XXXXXXXXXX); printf 'lower\\n' >\"$root/original.txt\"; chmod 777 \"$root\"; chmod 666 \"$root/original.txt\"; printf '%s\\n' \"$root\""])
        .purpose("create remote Docker OverlayFS smoke source");
    let output = executor.execute(&prepare)?;
    ensure!(
        output.status == 0,
        "{} failed on {}: {}",
        prepare.purpose,
        ssh.destination,
        String::from_utf8_lossy(&output.stderr)
    );
    let lower = String::from_utf8(output.stdout).context("decode remote smoke directory")?;
    let lower = lower.trim();
    ensure!(
        lower.starts_with("/tmp/mj-docker-overlay-smoke.")
            && !lower.contains(['\n', '\r'])
            && !lower.contains("/../"),
        "unexpected remote smoke directory {lower:?}"
    );
    let mount = AdditionalMount {
        source: PathBuf::from(lower),
        destination: PathBuf::from("/mnt/hel-overlay-smoke"),
        read_only: false,
    };
    let result = (|| {
        let create = command_over_ssh(
            docker_container_run(container, &name, smoke_id, &[mount])?,
            ssh,
        );
        execute_checked(executor, &create)?;
        let probe = command_over_ssh(
            container_exec("docker", &name, ["sh", "-c", DOCKER_OVERLAY_SMOKE_PROBE]),
            ssh,
        )
        .purpose("verify remote Docker OverlayFS copy-on-write attachment");
        execute_checked(executor, &probe)?;
        execute_checked(executor, &ssh_command(ssh, ["sh", "-c",
            "test \"$(cat \"$1/original.txt\")\" = lower && test ! -e \"$1/container-created.txt\"", "mj-check-smoke-source", lower])
            .purpose("verify original remote attachment is unchanged"))
    })();
    let cleanup = (|| {
        let plan = close_plan(
            &TargetLocator::SshDocker {
                ssh: ssh.clone(),
                container_id: name,
            },
            smoke_id,
        )?;
        for command in &plan.commands {
            execute_checked(executor, command)?;
        }
        // Never remove a lower directory until its container and volumes are gone.
        execute_checked(
            executor,
            &ssh_command(ssh, ["rm", "-rf", "--", lower])
                .purpose("remove remote Docker smoke source"),
        )
    })();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("remote smoke cleanup also failed: {cleanup:#}")))
        }
    }
}

const DOCKER_OVERLAY_SMOKE_PROBE: &str = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && printf 'changed\\n' >/mnt/hel-overlay-smoke/original.txt && printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt";

// macOS temporary directories are not normally shared into Docker VMs. The
// home directory is shared by Colima's default configuration.
fn docker_overlay_smoke_directory() -> Result<tempfile::TempDir> {
    let parent = if cfg!(target_os = "macos") {
        dirs::home_dir().context("locate shared home directory for Docker smoke test")?
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix(".mj-docker-overlay-smoke-")
        .tempdir_in(parent)
        .context("create Docker OverlayFS smoke directory")
}

fn run_docker_overlay_smoke_test(
    container: &ContainerTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_container_template(container)?;
    let lower = docker_overlay_smoke_directory()?;
    let original = lower.path().join("original.txt");
    let added = lower.path().join("container-created.txt");
    fs::write(&original, b"lower\n").context("write Docker OverlayFS smoke source")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The disposable probe tests the mount, independently of host/image UID.
        fs::set_permissions(lower.path(), fs::Permissions::from_mode(0o777))?;
        fs::set_permissions(&original, fs::Permissions::from_mode(0o666))?;
    }
    let name = resource_name(smoke_id)?;
    let mount = AdditionalMount {
        source: lower.path().to_path_buf(),
        destination: PathBuf::from("/mnt/hel-overlay-smoke"),
        read_only: false,
    };
    let create = docker_container_run(container, &name, smoke_id, &[mount])?
        .purpose("create disposable Docker OverlayFS smoke container");
    let probe = container_exec("docker", &name, ["sh", "-c", DOCKER_OVERLAY_SMOKE_PROBE])
        .purpose("verify Docker OverlayFS copy-on-write attachment");
    let cleanup = close_plan(&TargetLocator::LocalDocker { container_id: name }, smoke_id)?
        .commands
        .into_iter()
        .next()
        .context("Docker OverlayFS smoke cleanup plan is empty")?;

    let smoke_result =
        execute_checked(executor, &create).and_then(|()| execute_checked(executor, &probe));
    if let Err(cleanup_error) = execute_checked(executor, &cleanup) {
        // A surviving overlay still references its lower directory.
        let retained = lower.keep();
        let cleanup_error = cleanup_error.context(format!(
            "Docker smoke cleanup failed; retained source at {}",
            retained.display()
        ));
        return match smoke_result {
            Ok(()) => Err(cleanup_error),
            Err(error) => Err(error.context(format!("{cleanup_error:#}"))),
        };
    }
    smoke_result?;
    ensure!(
        fs::read(&original).context("read Docker OverlayFS smoke source after container write")?
            == b"lower\n",
        "Docker OverlayFS smoke test changed its lower source"
    );
    ensure!(
        !added.exists(),
        "Docker OverlayFS smoke test created a file in its lower source"
    );
    Ok(())
}

fn execute_checked(executor: &impl CommandExecutor, command: &CommandSpec) -> Result<()> {
    let output = executor.execute(command)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Clone/bootstrap commands for AWS once the exact instance ID and address are known.
pub fn provision_on_locator_plan(
    locator: &TargetLocator,
    session_id: &str,
    bundle: &ProjectBundleSpec,
) -> Result<CommandPlan> {
    bundle.validate()?;
    verify_locator(locator, session_id)?;
    let TargetLocator::AwsEc2 { ssh, workspace, .. } = locator else {
        bail!("post-launch provisioning is only required for AWS");
    };
    let mut commands = vec![
        ssh_command(ssh, ["mkdir", "-p", workspace])
            .purpose("create EC2 session workspace")
            .stage(ProvisionStage::Cloning),
    ];
    commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
    commands.extend(clone_commands(bundle, workspace, |args| {
        ssh_command_owned(ssh, args)
    }));
    Ok(CommandPlan {
        description: format!("initialize EC2 session {session_id}"),
        commands,
    })
}

pub fn reconnect_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    verify_locator(locator, session_id)?;
    let root = worker_root(locator, session_id)?;
    let binary = format!("{root}/hel");
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            CommandSpec::new(binary, ["worker", "proxy", "--root", root.as_str()])
        }
        TargetLocator::LocalPodman { container_id, .. } => container_exec(
            "podman",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::LocalDocker { container_id } => container_exec(
            "docker",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::AppleContainer { container_id } => container_exec(
            "container",
            container_id,
            [&binary, "worker", "proxy", "--root", &root],
        ),
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command(ssh, [&binary, "worker", "proxy", "--root", &root])
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => ssh_command(
            ssh,
            [
                locator.container_engine().expect("remote container"),
                "exec",
                "-i",
                container_id,
                &binary,
                "worker",
                "proxy",
                "--root",
                &root,
            ],
        ),
    }
    .purpose("connect to Mjolnir worker")
    .stage(ProvisionStage::Starting);
    Ok(CommandPlan {
        description: format!("reconnect Mjolnir session {session_id}"),
        commands: vec![command],
    })
}

/// Describe safe recovery for a container that belongs to an active
/// session. The inspect command is deliberately separate from `exec`: a host
/// crash can leave the container present but stopped, where `exec` cannot
/// distinguish that state from other transport failures.
pub fn target_recovery_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<TargetRecoveryPlan>> {
    verify_locator(locator, session_id)?;
    if let TargetLocator::SshDocker { ssh, container_id } = locator {
        let local = target_recovery_plan(
            &TargetLocator::LocalDocker {
                container_id: container_id.clone(),
            },
            session_id,
        )?;
        return Ok(local.map(|plan| TargetRecoveryPlan {
            exists: command_over_ssh(plan.exists, ssh),
            inspect: command_over_ssh(plan.inspect, ssh),
            start: command_over_ssh(plan.start, ssh),
            session_id: plan.session_id,
        }));
    }

    let (exists, inspect, start) = match locator {
        TargetLocator::LocalPodman { container_id, .. } => (
            CommandSpec::new("podman", ["container", "exists", container_id])
                .purpose("check for Mjolnir session container"),
            CommandSpec::new("podman", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir session container"),
            CommandSpec::new("podman", ["start", container_id])
                .purpose("start stopped Mjolnir session container"),
        ),
        TargetLocator::SshDocker { .. } => unreachable!("handled above"),
        TargetLocator::LocalDocker { container_id } => (
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "docker container inspect \"$1\" >/dev/null 2>&1 && exit 0; docker info >/dev/null 2>&1 && exit 1; exit 125",
                    "mj-docker-exists",
                    container_id,
                ],
            )
            .purpose("check for Mjolnir Docker session container"),
            CommandSpec::new("docker", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir Docker session container"),
            CommandSpec::new("docker", ["start", container_id])
                .purpose("start stopped Mjolnir Docker session container"),
        ),
        TargetLocator::SshPodman { ssh, container_id, .. } => (
            ssh_command(ssh, ["podman", "container", "exists", container_id])
                .purpose("check for remote Mjolnir session container"),
            ssh_command(ssh, ["podman", "container", "inspect", container_id])
                .purpose("inspect remote Mjolnir session container"),
            ssh_command(ssh, ["podman", "start", container_id])
                .purpose("start stopped remote Mjolnir session container"),
        ),
        TargetLocator::LocalBare { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::AwsEc2 { .. }
        | TargetLocator::SshBare { .. } => return Ok(None),
    };
    Ok(Some(TargetRecoveryPlan {
        exists,
        inspect,
        start,
        session_id: session_id.to_owned(),
    }))
}

/// Start a confirmed stopped container target and verify it reached `running`.
/// Missing or foreign containers, transport failures, and transitional states
/// fail without running the start command.
pub fn ensure_recovery_target_running(
    executor: &impl CommandExecutor,
    plan: Option<&TargetRecoveryPlan>,
) -> Result<TargetRecoveryOutcome> {
    let Some(plan) = plan else {
        return Ok(TargetRecoveryOutcome::NotRequired);
    };
    let existence = executor
        .execute(&plan.exists)
        .context("check whether container session target exists")?;
    match existence.status {
        0 => {}
        // `podman container exists` deliberately reserves 1 for absence and
        // uses 125 for invocation or storage failures. SSH preserves the
        // remote exit status, so this contract also covers remote Podman.
        1 => return Ok(TargetRecoveryOutcome::Missing),
        _ => {
            checked_command_output(&plan.exists, existence)
                .context("check whether container session target exists")?;
            unreachable!("a successful checked command has status zero");
        }
    }
    let status = inspect_recovery_target(executor, plan)?;
    match status.as_str() {
        "running" => Ok(TargetRecoveryOutcome::AlreadyRunning),
        "created" | "initialized" | "stopped" | "exited" => {
            let output = executor.execute(&plan.start)?;
            checked_command_output(&plan.start, output)
                .context("start confirmed stopped container session target")?;
            let after = inspect_recovery_target(executor, plan)
                .context("verify container session target after starting it")?;
            ensure!(
                after == "running",
                "container session target reported {after:?} after start"
            );
            Ok(TargetRecoveryOutcome::Started)
        }
        "paused" | "removing" | "stopping" | "unknown" => {
            bail!("refusing to start container session target in {status:?} state")
        }
        _ => bail!("container session target reported unexpected state {status:?}"),
    }
}

fn inspect_recovery_target(
    executor: &impl CommandExecutor,
    plan: &TargetRecoveryPlan,
) -> Result<String> {
    let output = executor.execute(&plan.inspect)?;
    let output = checked_command_output(&plan.inspect, output)
        .context("inspect container session target for recovery")?;
    let values: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parse container target inspection")?;
    ensure!(
        values.len() == 1,
        "container inspection returned {} targets instead of one",
        values.len()
    );
    let target = &values[0];
    let labels = target
        .pointer("/Config/Labels")
        .and_then(serde_json::Value::as_object)
        .context("container session target has no ownership labels")?;
    ensure!(
        labels
            .get(MANAGED_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some("true"),
        "refusing to start a container target Mjolnir does not own"
    );
    ensure!(
        labels
            .get(SESSION_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some(plan.session_id.as_str()),
        "refusing to start a container target owned by another session"
    );
    target
        .pointer("/State/Status")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("container session target inspection has no state")
}

const CGROUP_RESOURCE_USAGE_SCRIPT: &str = r#"
for file in memory.current memory.max memory.swap.current memory.swap.max; do
    path="/sys/fs/cgroup/$file"
    if [ -r "$path" ]; then
        printf "%s=%s\n" "$file" "$(cat "$path")"
    fi
done
if [ -r /sys/fs/cgroup/cpu.stat ]; then
    before=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    sleep 0.25
    after=$(awk '/^usage_usec / { print $2 }' /sys/fs/cgroup/cpu.stat)
    set -- $(cat /sys/fs/cgroup/cpu.max 2>/dev/null || printf 'max 100000')
    if [ "$1" = max ]; then
        cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')
    else
        cores=$(awk -v quota="$1" -v period="$2" 'BEGIN { print quota / period }')
    fi
    awk -v used="$((after - before))" -v cores="$cores" \
        'BEGIN { if (cores > 0) printf "cpu.percent=%.0f\n", used / 250000 / cores * 100 }'
fi
"#;

const HOST_RESOURCE_USAGE_SCRIPT: &str = r#"
memory_proc_root=${1:-/proc}
read_cpu() { awk '/^cpu / { total=0; for (i=2; i<=NF; i++) total += $i; print total, $5 + $6 }' /proc/stat; }
set -- $(read_cpu); total_before=$1; idle_before=$2
sleep 0.25
set -- $(read_cpu); total_after=$1; idle_after=$2
awk -v total="$((total_after - total_before))" -v idle="$((idle_after - idle_before))" \
    'BEGIN { if (total > 0) printf "cpu.percent=%.0f\n", (total - idle) * 100 / total }'
arc_size=0
arc_min=0
arcstats="$memory_proc_root/spl/kstat/zfs/arcstats"
if [ -r "$arcstats" ]; then
    set -- $(awk '
        $1 == "c_min" { arc_min = $3 }
        $1 == "size" { arc_size = $3 }
        END { printf "%.0f %.0f\n", arc_size, arc_min }
    ' "$arcstats")
    arc_size=$1
    arc_min=$2
fi
awk -v arc_size="$arc_size" -v arc_min="$arc_min" '
    /^MemTotal:/ { memory_total = $2 }
    /^MemAvailable:/ { memory_available = $2 }
    /^SwapTotal:/ { swap_total = $2 }
    /^SwapFree:/ { swap_free = $2 }
    END {
        memory_total *= 1024
        memory_available *= 1024
        # Like btop, count ARC above its minimum size as reclaimable cache.
        if (arc_size > arc_min) memory_available += arc_size - arc_min
        if (memory_available > memory_total) memory_available = memory_total
        printf "memory.current=%.0f\n", memory_total - memory_available
        printf "memory.max=%.0f\n", memory_total
        printf "memory.swap.current=%.0f\n", (swap_total - swap_free) * 1024
        printf "memory.swap.max=%.0f\n", swap_total * 1024
    }
' "$memory_proc_root/meminfo"
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
"#;

const AWS_ALLOCATED_CAPACITY_SCRIPT: &str = r#"
awk '/^MemTotal:/ { printf "memory.total=%.0f\n", $2 * 1024 }' /proc/meminfo
printf 'logical.cores=%s\n' "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
df -B1 -P -- "$1" | awk 'NR == 2 { print "disk.total=" $2 }'
"#;

// `du` is run on its own so a path it cannot measure fails the probe instead of
// being silently dropped from the total: a session that reports less disk than
// it uses is worse than one that reports none. Its stderr is deliberately left
// attached, so the caller's failure message names the path that could not be
// read.
const AWS_SESSION_DISK_USAGE_SCRIPT: &str = r#"
usage=$(du -sk "$@") || exit 1
printf '%s\n' "$usage" | awk '{ total += $1 * 1024 } END { print total + 0 }'
"#;

pub fn resource_probe(locator: &TargetLocator, session_id: &str) -> Result<SessionResourceProbe> {
    verify_locator(locator, session_id)?;
    let (memory, disk) = match locator {
        TargetLocator::LocalPodman { container_id, .. } => (
            container_exec(
                "podman",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Podman container resources"),
            Some(
                CommandSpec::new(
                    "podman",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Podman container writable disk"),
            ),
        ),
        TargetLocator::LocalDocker { container_id } => (
            container_exec(
                "docker",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample local Docker container resources"),
            Some(
                CommandSpec::new(
                    "docker",
                    [
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample local Docker container writable disk"),
            ),
        ),
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => (
            ssh_command(
                ssh,
                [
                    locator.container_engine().expect("remote container"),
                    "exec",
                    container_id,
                    "sh",
                    "-c",
                    CGROUP_RESOURCE_USAGE_SCRIPT,
                ],
            )
            .purpose("sample remote container resources"),
            Some(
                ssh_command(
                    ssh,
                    [
                        locator.container_engine().expect("remote container"),
                        "container",
                        "inspect",
                        "--size",
                        "--format",
                        "{{.SizeRw}}",
                        container_id,
                    ],
                )
                .purpose("sample remote container writable disk"),
            ),
        ),
        TargetLocator::AwsEc2 { ssh, workspace, .. } => {
            let worker_root = worker_root(locator, session_id)?;
            let profile_root = format!(".local/share/hel/profiles/{session_id}");
            (
                ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
                    .purpose("sample EC2 session resources"),
                Some(
                    ssh_command(
                        ssh,
                        [
                            "sh",
                            "-c",
                            AWS_SESSION_DISK_USAGE_SCRIPT,
                            "sh",
                            workspace.as_str(),
                            worker_root.as_str(),
                            profile_root.as_str(),
                        ],
                    )
                    .purpose("sample EC2 session disk"),
                ),
            )
        }
        TargetLocator::AppleContainer { container_id } => (
            container_exec(
                "container",
                container_id,
                ["sh", "-c", CGROUP_RESOURCE_USAGE_SCRIPT],
            )
            .purpose("sample Apple container resources"),
            None,
        ),
        TargetLocator::LocalBare { .. } | TargetLocator::SshBare { .. } => {
            bail!("resource sampling is unsupported for this target")
        }
    };
    Ok(SessionResourceProbe { memory, disk })
}

pub fn parse_resource_usage(
    memory_output: &[u8],
    disk_output: Option<&[u8]>,
) -> Result<SessionResourceUsage> {
    let mut values = BTreeMap::new();
    let memory_text = String::from_utf8_lossy(memory_output);
    for line in memory_text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(name, value.trim());
    }

    let memory_current_bytes = parse_cgroup_counter(
        values
            .get("memory.current")
            .context("resource probe did not expose memory.current")?,
    )?
    .context("resource probe reported memory.current as unlimited")?;
    let memory_limit_bytes = values
        .get("memory.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_current_bytes = values
        .get("memory.swap.current")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let swap_limit_bytes = values
        .get("memory.swap.max")
        .map(|value| parse_cgroup_counter(value))
        .transpose()?
        .flatten();
    let writable_disk_bytes = disk_output.map(parse_disk_usage).transpose()?;
    let cpu_percent = values
        .get("cpu.percent")
        .map(|value| parse_percent(value))
        .transpose()?;

    Ok(SessionResourceUsage {
        cpu_percent,
        memory_current_bytes,
        memory_limit_bytes,
        swap_current_bytes,
        swap_limit_bytes,
        writable_disk_bytes,
    })
}

/// Read the single byte count every writable-disk probe answers with.
///
/// A probe that ran and answered something else measured nothing, which must be
/// reported as a failure rather than silently becoming "disk usage unknown":
/// only a probe that was never run leaves the value unknown.
fn parse_disk_usage(output: &[u8]) -> Result<u64> {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    text.parse()
        .with_context(|| format!("disk usage probe answered {text:?} instead of a byte count"))
}

pub fn ssh_host_capacity_command(ssh: &SshTarget) -> CommandSpec {
    ssh_command(ssh, ["sh", "-c", HOST_RESOURCE_USAGE_SCRIPT])
        .purpose("sample deployment host capacity")
}

pub fn aws_allocated_capacity_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    let TargetLocator::AwsEc2 { workspace, .. } = locator else {
        bail!("AWS allocated-capacity probes require an EC2 locator");
    };
    command_on_locator(
        locator,
        session_id,
        vec![
            "sh".into(),
            "-c".into(),
            AWS_ALLOCATED_CAPACITY_SCRIPT.into(),
            "sh".into(),
            workspace.clone(),
        ],
        "sample EC2 allocated capacity",
    )
}

pub fn parse_host_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let total = parse_required_u64(&values, "memory.max")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(parse_percent(required_value(&values, "cpu.percent")?)?),
        memory_used_bytes: parse_required_u64(&values, "memory.current")?,
        memory_total_bytes: total,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: None,
    })
}

pub fn parse_aws_allocated_capacity(output: &[u8]) -> Result<DeploymentCapacityUsage> {
    let values = parse_key_values(output);
    let memory_total_bytes = parse_required_u64(&values, "memory.total")?;
    Ok(DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes,
        logical_cores: parse_required_u64(&values, "logical.cores")?,
        disk_total_bytes: Some(parse_required_u64(&values, "disk.total")?),
    })
}

fn parse_key_values(output: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim().to_owned()))
        .collect()
}

fn required_value<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("capacity probe did not expose {key}"))
}

fn parse_required_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    required_value(values, key)?
        .parse()
        .with_context(|| format!("capacity probe reported invalid {key}"))
}

fn parse_percent(value: &str) -> Result<u8> {
    let value: f64 = value
        .parse()
        .with_context(|| format!("invalid percentage {value:?}"))?;
    if !value.is_finite() {
        bail!("invalid percentage {value:?}");
    }
    Ok(value.round().clamp(0.0, 100.0) as u8)
}

fn parse_cgroup_counter(value: &str) -> Result<Option<u64>> {
    if value == "max" {
        return Ok(None);
    }
    Ok(Some(value.parse().with_context(|| {
        format!("invalid memory counter {value:?}")
    })?))
}

/// POSIX shell helpers that identify the daemon for one exact worker root.
/// The match is assembled at run time so the script's own command line cannot
/// select itself, and `worker proxy` command lines cannot match either.
fn worker_daemon_identity_script(worker_root: &str) -> String {
    format!(
        r#"hel_root={root}
hel_match="hel worker run --root $hel_root"
hel_match_home="hel worker run --root $HOME/$hel_root"
hel_ps() {{
    ps -ww "$@" 2>/dev/null || ps "$@" 2>/dev/null
}}
hel_is_worker() {{
    hel_args=$(hel_ps -o args= -p "$1") || return 1
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) return 0 ;;
    esac
    return 1
}}
hel_recorded_worker() {{
    [ -f "$hel_root/{pid_file}" ] || return 1
    hel_pid=$(cat "$hel_root/{pid_file}" 2>/dev/null)
    case "$hel_pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    hel_is_worker "$hel_pid" || return 1
    printf '%s\n' "$hel_pid"
}}"#,
        root = posix_quote(worker_root),
        pid_file = mj_core::relay::WORKER_PID_FILE,
    )
}

/// Report whether the exact session worker is alive without signaling it.
/// A successful probe prints one stable token; transport or shell failures
/// stay distinguishable from a confirmed absent worker.
pub fn worker_daemon_liveness_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_report_worker_state() {
    if [ -S "$hel_root/control.sock" ]; then
        printf 'alive\n'
    else
        printf 'starting\n'
    fi
}
if hel_recorded_worker >/dev/null; then
    hel_report_worker_state
    exit 0
fi
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_report_worker_state; exit 0 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
printf 'dead\n'
"#,
    );
    script
}

/// Stop the detached worker daemon rooted at `worker_root`.
///
/// The daemon leads its own process group, so the signal goes to the group
/// first to take the agent down with it. Shells disagree about how to write a
/// negative PID (`dash` rejects `--`), hence the two forms before the
/// single-process fallback for daemons predating the group leadership.
pub fn stop_worker_daemon_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_signal() {
    kill -"$1" -- "-$2" 2>/dev/null && return 0
    kill -"$1" "-$2" 2>/dev/null && return 0
    kill -"$1" "$2" 2>/dev/null
}
hel_stop() {
    hel_signal TERM "$1" || return 0
    hel_waited=0
    while [ "$hel_waited" -lt 2 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
    kill -0 "$1" 2>/dev/null || return 0
    hel_signal KILL "$1" || true
    hel_waited=0
    while [ "$hel_waited" -lt 3 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
}
if hel_pid=$(hel_recorded_worker); then
    hel_stop "$hel_pid"
fi
hel_ps -eo pid=,args= | while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_stop "$hel_pid" ;;
    esac
done
hel_left=0
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_left=1 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
if [ "$hel_left" -ne 0 ]; then
    echo "worker still running after stop: $hel_root" >&2
    exit 1
fi
"#,
    );
    script
}

/// Stop a leaked worker and delete the durable relay state under its root.
///
/// A resume seeds fresh relay state into the same root a closed session used.
/// Leftover state wins over that seed at startup, so it has to go, and
/// whatever might still be writing it has to go first. Container and instance
/// targets are rebuilt from scratch on resume, so they need nothing here.
pub fn clear_relay_state_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<CommandSpec>> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let script = format!(
        "{}\nrm -rf -- {} {}\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_JOURNAL_DIR
        )),
    );
    Ok(match locator {
        TargetLocator::LocalBare { .. } => Some(
            CommandSpec::new("sh", ["-c", script.as_str()])
                .purpose("stop a leaked local Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::SshBare { ssh, .. } => Some(
            ssh_command(ssh, ["sh", "-c", script.as_str()])
                .purpose("stop a leaked remote Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::AwsEc2 { .. } => None,
    })
}

pub fn close_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    verify_locator(locator, session_id)?;
    if let TargetLocator::SshDocker { ssh, container_id } = locator {
        let local = close_plan(
            &TargetLocator::LocalDocker {
                container_id: container_id.clone(),
            },
            session_id,
        )?;
        return Ok(CommandPlan {
            description: local.description,
            commands: local
                .commands
                .into_iter()
                .map(|command| command_over_ssh(command, ssh))
                .collect(),
        });
    }

    let session_worker_root = worker_root(locator, session_id)?;
    let session_profile_home = format!(".local/share/hel/profiles/{session_id}");
    if matches!(
        locator,
        TargetLocator::LocalPodman { .. } | TargetLocator::SshPodman { .. }
    ) {
        return podman_cleanup_plan(locator, session_id);
    }
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            // The daemon dies before its root does: a survivor's next durable
            // write would recreate the directory this command removes.
            let script = format!(
                "{}\nrm -rf -- {}\n",
                stop_worker_daemon_script(&session_worker_root),
                posix_quote(&session_worker_root),
            );
            CommandSpec::new("sh", ["-c", script.as_str()]).purpose(
                "stop the local Mjolnir worker and remove exact local Mjolnir worker state",
            )
        }
        TargetLocator::LocalPodman { .. } => unreachable!("handled above"),
        TargetLocator::SshDocker { .. } => unreachable!("handled above"),
        TargetLocator::LocalDocker { container_id } => {
            let script = r#"status=0
helper="$1-mount-init"
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.attachment-helper"}}|{{index .Config.Labels "dev.mj.session"}}' "$helper" 2>/dev/null); then
    if [ "$identity" = "true|$2" ]; then
        docker rm --force "$helper" || status=$?
    else
        echo 'refusing to remove a foreign Docker attachment helper' >&2
        status=2
    fi
elif ! docker info >/dev/null 2>&1; then
    echo 'could not determine whether the Docker attachment helper exists' >&2
    status=1
fi
if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$1" 2>/dev/null); then
    if [ "$identity" = "true|$2" ]; then
        docker rm --force "$1" || status=$?
    else
        echo 'refusing to remove a Docker container Mjolnir does not own for this session' >&2
        status=2
    fi
elif ! docker info >/dev/null 2>&1; then
    echo 'could not determine whether the Docker session container exists' >&2
    status=1
fi
if [ "$status" -eq 0 ]; then
    volumes=$(docker volume ls --quiet --filter "label=dev.mj.managed=true" --filter "label=dev.mj.session=$2") || status=$?
    if [ "$status" -eq 0 ]; then
        backings=
        for volume in $volumes; do
            backing=$(docker volume inspect --format '{{index .Labels "dev.mj.attachment-backing"}}' "$volume") || { status=$?; continue; }
            if [ "$backing" = true ]; then
                backings="$backings $volume"
            else
                docker volume rm --force "$volume" || status=$?
            fi
        done
        if [ "$status" -eq 0 ]; then
            for backing in $backings; do docker volume rm --force "$backing" || status=$?; done
        fi
    fi
fi
if [ "$status" -eq 0 ]; then
    rm -rf -- "$HOME/.cache/mjolnir/git/sessions/$2" || status=$?
fi
if [ "$status" -eq 0 ]; then
    root="$HOME/.cache/mjolnir/docker-overlays/$1"
    if [ "$(cat "$root/.hel-session" 2>/dev/null || true)" = "$2" ]; then
        case $1 in mj-*|hel-*) rm -rf -- "$root" || status=$? ;; *) status=2 ;; esac
    fi
fi
exit "$status""#;
            CommandSpec::new("sh", ["-c", script, "mj-close", container_id, session_id])
                .purpose("remove local Docker session container, overlay volumes, and cache state")
        }
        TargetLocator::AppleContainer { container_id } => {
            let script = "status=0; container rm --force \"$1\" || status=$?; rm -rf -- \"$HOME/.cache/mjolnir/git/sessions/$2\"; exit \"$status\"";
            CommandSpec::new("sh", ["-c", script, "mj-close", container_id, session_id])
                .purpose("remove Apple session container and Git cache snapshot")
        }
        TargetLocator::AwsEc2 {
            profile,
            region,
            instance_id,
            ..
        } => {
            // EC2 TerminateInstances is explicitly idempotent, including a
            // repeated request for an already-terminated instance.
            CommandSpec::new(
                "aws",
                [
                    "--profile",
                    profile,
                    "--region",
                    region,
                    "ec2",
                    "terminate-instances",
                    "--instance-ids",
                    instance_id,
                ],
            )
            .purpose("terminate exact EC2 session instance")
        }
        TargetLocator::SshBare { ssh, workspace } => {
            // Same ordering constraint as the local bare target: stop the
            // daemon before deleting the root it keeps writing to.
            let script = format!(
                "{}\nrm -rf -- {} {} {}\n",
                stop_worker_daemon_script(&session_worker_root),
                posix_quote(workspace),
                posix_quote(&session_worker_root),
                posix_quote(&session_profile_home),
            );
            ssh_command(ssh, ["sh", "-c", script.as_str()]).purpose(
                "stop the remote Mjolnir worker and remove exact SSH session workspace and runtime state",
            )
        }
        TargetLocator::SshPodman { .. } => unreachable!("handled above"),
    };
    Ok(CommandPlan {
        description: format!("close Mjolnir session {session_id}"),
        commands: vec![command],
    })
}

const PODMAN_CONTAINER_IDENTITY_SCRIPT: &str = r#"set -eu
container=$1
session=$2
if identity=$(podman container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo 'refusing to operate on a Podman container Mjolnir does not own for this session' >&2
        exit 2
    }
elif ! podman info >/dev/null 2>&1; then
    echo 'could not determine whether the Podman session container exists' >&2
    exit 1
else
    exit 0
fi
"#;

/// Stop a Podman target without deleting its potentially large writable layer.
/// A successful return means the exact owned container is absent or not running.
pub fn quiesce_plan(locator: &TargetLocator, session_id: &str) -> Result<Option<CommandPlan>> {
    verify_locator(locator, session_id)?;
    let (ssh, container_id) = match locator {
        TargetLocator::LocalPodman { container_id, .. } => (None, container_id),
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => (Some(ssh), container_id),
        _ => return Ok(None),
    };
    let script = format!(
        "{PODMAN_CONTAINER_IDENTITY_SCRIPT}\nif podman container inspect \"$container\" >/dev/null 2>&1; then\n    podman stop --time 0 --ignore \"$container\" >/dev/null\n    running=$(podman container inspect --format '{{{{.State.Running}}}}' \"$container\")\n    [ \"$running\" = false ] || {{ echo 'Podman session container is still running' >&2; exit 1; }}\nfi\n"
    );
    let command = match ssh {
        Some(ssh) => ssh_command(
            ssh,
            [
                "sh",
                "-c",
                script.as_str(),
                "mj-quiesce",
                container_id,
                session_id,
            ],
        ),
        None => CommandSpec::new(
            "sh",
            [
                "-c",
                script.as_str(),
                "mj-quiesce",
                container_id,
                session_id,
            ],
        ),
    }
    .purpose("stop exact Podman session container without removing storage")
    .stage(ProvisionStage::StoppingTarget);
    Ok(Some(CommandPlan {
        description: format!("quiesce Mjolnir session {session_id}"),
        commands: vec![command],
    }))
}

fn podman_cleanup_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    let (ssh, container_id, workspace_storage) = match locator {
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
        } => (None, container_id, workspace_storage),
        TargetLocator::SshPodman {
            ssh,
            container_id,
            workspace_storage,
        } => (Some(ssh), container_id, workspace_storage),
        _ => unreachable!("Podman cleanup requires a Podman locator"),
    };
    let remove_container_script = format!(
        "{PODMAN_CONTAINER_IDENTITY_SCRIPT}\npodman rm --force --ignore \"$container\" >/dev/null\n"
    );
    let at_host = |args: Vec<String>| match ssh {
        Some(ssh) => ssh_command_owned(ssh, args),
        None => {
            let mut args = args;
            CommandSpec::new(args.remove(0), args)
        }
    };
    let mut commands = vec![
        at_host(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            remove_container_script,
            "mj-remove-container".to_owned(),
            container_id.clone(),
            session_id.to_owned(),
        ])
        .purpose("remove exact stopped Podman session container")
        .stage(ProvisionStage::RemovingContainer),
    ];
    match workspace_storage {
        PodmanWorkspaceLocator::ContainerLayer => {}
        PodmanWorkspaceLocator::Volume { name } => {
            let script = r#"set -eu
volume=$1
session=$2
if identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo 'refusing to remove a Podman volume Mjolnir does not own for this session' >&2
        exit 2
    }
    podman volume rm --force "$volume" >/dev/null
elif ! podman info >/dev/null 2>&1; then
    echo 'could not determine whether the Podman workspace volume exists' >&2
    exit 1
fi
"#;
            commands.push(
                at_host(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script.to_owned(),
                    "mj-remove-volume".to_owned(),
                    name.clone(),
                    session_id.to_owned(),
                ])
                .purpose("remove exact Podman session workspace volume")
                .stage(ProvisionStage::RemovingStorage),
            );
        }
        PodmanWorkspaceLocator::HostPath {
            helper, resource, ..
        } => {
            let helper = join_remote_command(helper);
            let script = format!(
                r#"set -eu
resource=$1
state=$({helper} status "$resource")
case $state in
    present) {helper} destroy "$resource" ;;
    absent) ;;
    *) echo "workspace helper returned invalid status $state for $resource" >&2; exit 1 ;;
esac
[ "$({helper} status "$resource")" = absent ] || {{
    echo "workspace helper did not destroy $resource" >&2
    exit 1
}}
"#
            );
            commands.push(
                at_host(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script,
                    "mj-remove-host-workspace".to_owned(),
                    resource.clone(),
                ])
                .purpose("remove exact helper-managed Podman session workspace")
                .stage(ProvisionStage::RemovingStorage),
            );
        }
    }
    commands.push(
        at_host(vec![
            "rm".to_owned(),
            "-rf".to_owned(),
            "--".to_owned(),
            format!(".cache/mjolnir/git/sessions/{session_id}"),
        ])
        .purpose("remove Podman session Git cache snapshot")
        .stage(ProvisionStage::CleaningCache),
    );
    Ok(CommandPlan {
        description: format!("clean up stopped Mjolnir session {session_id}"),
        commands,
    })
}

/// Confirm that a container is absent after its exact delete command failed.
/// Other target deletion commands are already idempotent: filesystem removal
/// uses `rm -rf`, Podman uses `--ignore`, and EC2 termination is an idempotent
/// API operation. Apple lists exact container IDs; Docker checks both the exact
/// container name and exact session-labeled volumes while distinguishing an
/// unavailable daemon from absence.
pub fn cleanup_target_is_confirmed_absent(
    locator: &TargetLocator,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    verify_locator(locator, session_id)?;
    let (command, status_is_answer) = match locator {
        TargetLocator::AppleContainer { .. } => (
            CommandSpec::new("container", ["list", "--all", "--quiet"])
                .purpose("confirm exact Apple session container is absent"),
            false,
        ),
        TargetLocator::LocalDocker { container_id } | TargetLocator::SshDocker { container_id, .. } => (
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "if docker container inspect \"$1\" >/dev/null 2>&1; then exit 1; fi; docker info >/dev/null 2>&1 || exit 2; test -z \"$(docker volume ls --quiet --filter label=dev.mj.managed=true --filter label=dev.mj.session=$2)\"",
                    "hel-confirm-absent",
                    container_id,
                    session_id,
                ],
            )
            .purpose("confirm exact Docker session resources are absent"),
            true,
        ),
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
        } => (
            podman_absence_command(None, container_id, workspace_storage, session_id),
            true,
        ),
        TargetLocator::SshPodman {
            ssh,
            container_id,
            workspace_storage,
        } => (
            podman_absence_command(Some(ssh), container_id, workspace_storage, session_id),
            true,
        ),
        _ => return Ok(false),
    };
    let command = match locator {
        TargetLocator::SshDocker { ssh, .. } => command_over_ssh(command, ssh),
        _ => command,
    };
    let output = executor.execute(&command)?;
    if status_is_answer {
        return match output.status {
            0 => Ok(true),
            1 => Ok(false),
            _ => bail!(
                "{} failed with status {}: {}",
                command.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        };
    }
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listed = String::from_utf8(output.stdout).context("decode Apple container list")?;
    let TargetLocator::AppleContainer { container_id } = locator else {
        unreachable!("engine selected from locator")
    };
    Ok(!listed.lines().any(|id| id.trim() == container_id))
}

fn podman_absence_command(
    ssh: Option<&SshTarget>,
    container_id: &str,
    workspace_storage: &PodmanWorkspaceLocator,
    session_id: &str,
) -> CommandSpec {
    let storage_check = match workspace_storage {
        PodmanWorkspaceLocator::ContainerLayer => "exit 0".to_owned(),
        PodmanWorkspaceLocator::Volume { .. } => r#"podman volume exists "$3"
case $? in
    0) exit 1 ;;
    1) exit 0 ;;
    *) exit 2 ;;
esac"#
            .to_owned(),
        PodmanWorkspaceLocator::HostPath { helper, .. } => {
            let helper = join_remote_command(helper);
            format!(
                r#"state=$({helper} status "$3") || exit 2
case $state in
    absent) exit 0 ;;
    present) exit 1 ;;
    *) exit 2 ;;
esac"#
            )
        }
    };
    let script = format!(
        r#"podman container exists "$1"
case $? in
    0) exit 1 ;;
    1) ;;
    *) exit 2 ;;
esac
{storage_check}"#
    );
    let storage = match workspace_storage {
        PodmanWorkspaceLocator::ContainerLayer => "-",
        PodmanWorkspaceLocator::Volume { name } => name,
        PodmanWorkspaceLocator::HostPath { resource, .. } => resource,
    };
    let args = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        "mj-confirm-podman-absent".to_owned(),
        container_id.to_owned(),
        session_id.to_owned(),
        storage.to_owned(),
    ];
    match ssh {
        Some(ssh) => ssh_command_owned(ssh, args),
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
    }
    .purpose("confirm exact Podman session resources are absent")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBoundary<'a> {
    Direct,
    Container {
        engine: &'a str,
        container_id: &'a str,
    },
    Ssh(&'a SshTarget),
    SshContainer {
        engine: &'a str,
        ssh: &'a SshTarget,
        container_id: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessProbe<'a> {
    pub executable: &'a str,
    pub version_args: &'a [&'a str],
    pub bridge_executable: Option<&'a str>,
}

/// Compatibility is intentionally interpreted by the controller. A successful
/// probe permits an image-baked tool to be reused; a missing/incompatible tool
/// causes the controller to upload/install its release-owned copy.
pub fn bootstrap_probe_plan(
    boundary: ExecutionBoundary<'_>,
    harness: HarnessProbe<'_>,
) -> Result<CommandPlan> {
    validate_executable(harness.executable)?;
    let mut commands = vec![
        at_boundary(
            boundary,
            std::iter::once(harness.executable)
                .chain(harness.version_args.iter().copied())
                .map(str::to_owned)
                .collect(),
        )
        .purpose("probe harness version"),
    ];
    if let Some(bridge) = harness.bridge_executable {
        validate_executable(bridge)?;
        commands.push(
            at_boundary(boundary, vec![bridge.to_owned(), "--version".to_owned()])
                .purpose("probe ACP bridge version"),
        );
    }
    commands.push(
        at_boundary(boundary, vec!["git".to_owned(), "--version".to_owned()]).purpose("probe Git"),
    );
    Ok(CommandPlan {
        description: "probe reusable target tools".to_owned(),
        commands,
    })
}

/// Thin Linux Git bootstrap. Managed containers also receive GitHub CLI and
/// its HTTPS credential helper so an injected `GH_TOKEN` works before clone.
pub fn install_git_plan(boundary: ExecutionBoundary<'_>) -> CommandPlan {
    let managed_container = matches!(
        boundary,
        ExecutionBoundary::Container { .. } | ExecutionBoundary::SshContainer { .. }
    );
    let script = if managed_container {
        "set -eu; if ! command -v git >/dev/null 2>&1 || ! command -v gh >/dev/null 2>&1; then SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git and GitHub CLI installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git gh ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git gh ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git gh ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git github-cli ca-certificates curl; else echo 'Unsupported package manager; install Git and GitHub CLI in the image' >&2; exit 1; fi; fi; git config --global credential.https://github.com.helper '!gh auth git-credential'; git config --global credential.https://gist.github.com.helper '!gh auth git-credential'"
    } else {
        "set -eu; if command -v git >/dev/null 2>&1; then exit 0; fi; SUDO=''; if [ \"$(id -u)\" != 0 ]; then command -v sudo >/dev/null 2>&1 && sudo -n true || { echo 'Git installation requires root or passwordless sudo' >&2; exit 1; }; SUDO='sudo -n'; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update; $SUDO apt-get install -y git ca-certificates curl; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git ca-certificates curl; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git ca-certificates curl; elif command -v apk >/dev/null 2>&1; then $SUDO apk add --no-cache git ca-certificates curl; else echo 'Unsupported package manager; install Git manually' >&2; exit 1; fi"
    };
    CommandPlan {
        description: "install missing Git".to_owned(),
        commands: vec![
            at_boundary(
                boundary,
                vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            )
            .purpose("install Git")
            .stage(ProvisionStage::Cloning),
        ],
    }
}

/// Shared [`CommandSpec::parallel_group`] marker for one bundle's per-repository
/// clone/init commands. Every `clone_commands` call builds its own
/// [`CommandPlan`], so a single fixed marker never mixes batches across plans.
const BUNDLE_REPOSITORIES_PARALLEL_GROUP: u32 = 1;

fn clone_commands(
    bundle: &ProjectBundleSpec,
    workspace: &str,
    wrap: impl Fn(Vec<String>) -> CommandSpec,
) -> Vec<CommandSpec> {
    let mut commands = vec![
        wrap(vec![
            "mkdir".to_owned(),
            "-p".to_owned(),
            workspace.to_owned(),
        ])
        .purpose("create bundle workspace")
        .stage(ProvisionStage::Cloning),
    ];
    for repository in &bundle.repositories {
        let destination = format!("{workspace}/{}", repository.destination);
        let url = repository
            .url
            .as_ref()
            .expect("validated network repository");
        let mut args = vec!["git".to_owned(), "clone".to_owned()];
        for push_url in &repository.push_urls {
            args.extend([
                "--config".into(),
                format!("remote.origin.pushurl={push_url}"),
            ]);
        }
        if let Some(reference) = &repository.reference {
            args.extend(["--reference-if-able".to_owned(), reference.clone()]);
        }
        args.push("--".to_owned());
        args.push(url.clone());
        args.push(destination);
        commands.push(
            wrap(args)
                .purpose(format!("clone {}", repository.destination))
                .stage(ProvisionStage::Cloning)
                .parallel_group(BUNDLE_REPOSITORIES_PARALLEL_GROUP),
        );
    }
    commands
}

fn container_run(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandSpec> {
    Ok(CommandSpec::new(
        engine,
        container_run_args(engine, template, name, session_id, additional_mounts, None)?,
    )
    .purpose("start session container")
    .stage(ProvisionStage::Provisioning)
    .creates_target())
}

const PODMAN_VOLUME_RUN_SCRIPT: &str = r#"set -eu
session=$1
container=$2
volume=$3
shift 3
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if identity=$(podman container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null) && [ "$identity" = "true|$session" ]; then
            podman rm --force "$container" >/dev/null 2>&1 || true
        fi
        if identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null) && [ "$identity" = "true|$session" ]; then
            podman volume rm --force "$volume" >/dev/null 2>&1 || true
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
if identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null); then
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign Podman volume $volume" >&2
        exit 1
    }
else
    podman info >/dev/null
    podman volume create --label "dev.mj.managed=true" --label "dev.mj.session=$session" "$volume" >/dev/null
    identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume")
    [ "$identity" = "true|$session" ] || {
        echo "Podman volume $volume does not carry the expected Mjolnir identity" >&2
        exit 1
    }
fi
"$@"
"#;

fn podman_host_helper_run_script(helper: &[String]) -> String {
    let helper = join_remote_command(helper);
    format!(
        r#"set -eu
session=$1
container=$2
resource=$3
shift 3
cleanup() {{
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if identity=$(podman container inspect --format '{{{{index .Config.Labels "dev.mj.managed"}}}}|{{{{index .Config.Labels "dev.mj.session"}}}}' "$container" 2>/dev/null) && [ "$identity" = "true|$session" ]; then
            podman rm --force "$container" >/dev/null 2>&1 || true
        fi
        {helper} destroy "$resource" >/dev/null 2>&1 || true
    fi
    exit "$status"
}}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
state=$({helper} status "$resource")
case $state in
    absent) {helper} create "$resource" ;;
    present) ;;
    *) echo "workspace helper returned invalid status $state for $resource" >&2; exit 1 ;;
esac
[ "$({helper} status "$resource")" = present ] || {{
    echo "workspace helper did not create $resource" >&2
    exit 1
}}
"$@"
"#
    )
}

fn podman_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    ssh: Option<&SshTarget>,
) -> Result<CommandSpec> {
    let workspace = podman_workspace_locator(template, session_id)?;
    let run_args = container_run_args(
        "podman",
        template,
        name,
        session_id,
        additional_mounts,
        Some(&workspace),
    )?;
    let mut wrapped = match &workspace {
        PodmanWorkspaceLocator::ContainerLayer => {
            let mut command = vec!["podman".to_owned()];
            command.extend(run_args);
            command
        }
        PodmanWorkspaceLocator::Volume { name: volume } => {
            let mut command = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                PODMAN_VOLUME_RUN_SCRIPT.to_owned(),
                "mj-podman-run".to_owned(),
                session_id.to_owned(),
                name.to_owned(),
                volume.clone(),
                "podman".to_owned(),
            ];
            command.extend(run_args);
            command
        }
        PodmanWorkspaceLocator::HostPath {
            helper, resource, ..
        } => {
            let mut command = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                podman_host_helper_run_script(helper),
                "mj-podman-run".to_owned(),
                session_id.to_owned(),
                name.to_owned(),
                resource.clone(),
                "podman".to_owned(),
            ];
            command.extend(run_args);
            command
        }
    };
    let command = match ssh {
        Some(ssh) => ssh_command_owned(ssh, wrapped),
        None => {
            let program = wrapped.remove(0);
            CommandSpec::new(program, wrapped)
        }
    };
    let purpose = match (&workspace, ssh) {
        (PodmanWorkspaceLocator::ContainerLayer, Some(_)) => "start remote Podman container",
        (PodmanWorkspaceLocator::ContainerLayer, None) => "start session container",
        (_, Some(_)) => "start remote Podman container with isolated workspace storage",
        (_, None) => "start Podman container with isolated workspace storage",
    };
    Ok(command
        .purpose(purpose)
        .stage(ProvisionStage::Provisioning)
        .creates_target())
}

const DOCKER_OVERLAY_RUN_SCRIPT: &str = r#"set -eu
session=$1
container=$2
image=$3
pull=$4
shift 4
helper="$container-mount-init"
volumes=
backings=
remove_helper() {
    if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.attachment-helper"}}|{{index .Config.Labels "dev.mj.session"}}' "$helper" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing foreign Docker attachment helper $helper" >&2
            return 1
        }
        docker rm --force "$helper" >/dev/null
    else
        docker info >/dev/null
    fi
}
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        released=true
        remove_helper || released=false
        if identity=$(docker container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
            if [ "$identity" = "true|$session" ]; then
                docker rm --force "$container" >/dev/null || released=false
            else
                released=false
            fi
        elif ! docker info >/dev/null 2>&1; then
            released=false
        fi
        if [ "$released" = true ]; then
            for volume in $volumes; do
                docker volume rm --force "$volume" >/dev/null || released=false
            done
        fi
        if [ "$released" = true ]; then
            for backing in $backings; do
                docker volume rm --force "$backing" >/dev/null || released=false
            done
        fi
        if [ "$released" != true ]; then
            echo "Docker attachment cleanup failed; retained backing storage for session $session" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
owned_volume() {
    identity=$(docker volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$1")
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign Docker volume $1" >&2
        return 1
    }
}
remove_helper
while [ "$1" != -- ]; do
    ordinal=$1
    source=$2
    volume=$3
    shift 3
    backing="$volume-backing"
    if docker volume inspect "$volume" >/dev/null 2>&1; then
        owned_volume "$volume"
        owned_volume "$backing"
        volumes="$volumes $volume"
        backings="$backings $backing"
        continue
    fi
    if ! docker volume inspect "$backing" >/dev/null 2>&1; then
        docker volume create --driver local \
            --label "dev.mj.managed=true" --label "dev.mj.session=$session" \
            --label "dev.mj.attachment-backing=true" "$backing" >/dev/null
    fi
    owned_volume "$backing"
    backings="$backings $backing"
    docker run --name "$helper" --pull="$pull" --network none --user 0 \
        --label "dev.mj.attachment-helper=true" --label "dev.mj.session=$session" \
        --volume "$backing:/mj-attachment" \
        --mount "type=bind,source=$source,target=/mj-source,readonly" \
        --entrypoint sh "$image" -c '
            set -eu
            mkdir -p /mj-attachment/upper /mj-attachment/work
            chown "$(stat -c %u:%g /mj-source)" /mj-attachment/upper
            chmod "$(stat -c %a /mj-source)" /mj-attachment/upper
        ' >/dev/null
    remove_helper
    root=$(docker volume inspect --format '{{.Mountpoint}}' "$backing")
    case $root in /*) ;; *) echo "invalid Docker backing volume mountpoint: $root" >&2; exit 1 ;; esac
    upper="$root/upper"
    work="$root/work"
    docker volume create \
        --driver local \
        --label "dev.mj.managed=true" \
        --label "dev.mj.session=$session" \
        --opt type=overlay \
        --opt device=overlay \
        --opt "o=lowerdir=$source,upperdir=$upper,workdir=$work" \
        "$volume" >/dev/null
    owned_volume "$volume"
    volumes="$volumes $volume"
done
shift
"$@"
"#;

fn docker_overlay_volume_name(container_name: &str, ordinal: usize) -> String {
    format!("{container_name}-mount-{ordinal}")
}

fn docker_pull_policy(template: &ContainerTemplate) -> &'static str {
    match template.pull_policy.at_launch(&template.image) {
        ImagePullPolicy::Auto => unreachable!("at_launch resolves auto"),
        ImagePullPolicy::Always | ImagePullPolicy::Newer => "always",
        ImagePullPolicy::Missing => "missing",
        ImagePullPolicy::Never => "never",
    }
}

fn docker_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
) -> Result<CommandSpec> {
    let run_args = container_run_args(
        "docker",
        template,
        name,
        session_id,
        additional_mounts,
        None,
    )?;
    let writable = additional_mounts
        .iter()
        .enumerate()
        .filter(|(_, mount)| !mount.read_only)
        .collect::<Vec<_>>();
    if writable.is_empty() {
        return container_run("docker", template, name, session_id, additional_mounts);
    }
    let mut args = vec![
        "-c".to_owned(),
        DOCKER_OVERLAY_RUN_SCRIPT.to_owned(),
        "hel-docker-run".to_owned(),
        session_id.to_owned(),
        name.to_owned(),
        template.image.clone(),
        docker_pull_policy(template).to_owned(),
    ];
    for (ordinal, mount) in writable {
        args.extend([
            ordinal.to_string(),
            mount.source.to_string_lossy().into_owned(),
            docker_overlay_volume_name(name, ordinal),
        ]);
    }
    args.extend(["--".to_owned(), "docker".to_owned()]);
    args.extend(run_args);
    Ok(CommandSpec::new("sh", args)
        .purpose("start Docker session container with isolated attachments")
        .stage(ProvisionStage::Provisioning)
        .creates_target())
}

fn container_run_args(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    podman_workspace: Option<&PodmanWorkspaceLocator>,
) -> Result<Vec<String>> {
    validate_additional_mounts(additional_mounts)?;
    let mut args = vec!["run".to_owned()];
    if engine == "podman" {
        let pull_policy = template.pull_policy.at_launch(&template.image);
        if pull_policy != ImagePullPolicy::Missing {
            args.push(format!("--pull={}", pull_policy.podman_value()));
        }
        // PID 1 is `sleep infinity`, which reaps nothing, so every exec that
        // outlives its parent leaves a zombie behind. Apple's `container`
        // engine is left alone: its support for the flag is unverified.
        args.push("--init".to_owned());
    } else if engine == "docker" {
        let pull = docker_pull_policy(template);
        args.push(format!("--pull={pull}"));
        args.push("--init".to_owned());
    }
    args.extend(["--detach".to_owned(), "--name".to_owned(), name.to_owned()]);
    args.extend(managed_resource_identity_args(
        ManagedResourceKind::Container,
        session_id,
    ));
    args.extend(template.extra_run_args.clone());
    if engine == "podman" {
        match podman_workspace.unwrap_or(&PodmanWorkspaceLocator::ContainerLayer) {
            PodmanWorkspaceLocator::ContainerLayer => {}
            PodmanWorkspaceLocator::Volume { name } => args.extend([
                "--volume".to_owned(),
                format!("{name}:{CONTAINER_WORKSPACE}:rw,U"),
            ]),
            PodmanWorkspaceLocator::HostPath { path, .. } => args.extend([
                "--volume".to_owned(),
                format!("{path}:{CONTAINER_WORKSPACE}:rw"),
            ]),
        }
    }
    for (ordinal, mount) in additional_mounts.iter().enumerate() {
        let source = mount.source.to_string_lossy();
        let destination = mount.destination.to_string_lossy();
        match engine {
            "podman" => {
                let mode = if mount.read_only { "ro" } else { "O" };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}:{mode}"),
                ]);
            }
            "docker" => {
                let source = if mount.read_only {
                    source.into_owned()
                } else {
                    docker_overlay_volume_name(name, ordinal)
                };
                let suffix = if mount.read_only { ":ro" } else { "" };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}{suffix}"),
                ]);
            }
            "container" => args.extend([
                "--mount".to_owned(),
                format!("type=bind,source={source},target={destination},readonly"),
            ]),
            _ => bail!("additional mounts are unsupported for container engine {engine:?}"),
        }
    }
    args.extend([
        template.image.clone(),
        "sleep".to_owned(),
        "infinity".to_owned(),
    ]);
    Ok(args)
}

fn apple_image_prepare_commands(template: &ContainerTemplate) -> Vec<CommandSpec> {
    let command = match template.pull_policy.resolve(&template.image) {
        ImagePullPolicy::Always | ImagePullPolicy::Newer => {
            CommandSpec::new("container", ["image", "pull", template.image.as_str()])
                .purpose(format!("refresh container image {}", template.image))
        }
        ImagePullPolicy::Never => {
            CommandSpec::new("container", ["image", "inspect", template.image.as_str()])
                .purpose(format!("find pinned container image {}", template.image))
        }
        ImagePullPolicy::Missing => return Vec::new(),
        ImagePullPolicy::Auto => unreachable!("auto pull policy must resolve"),
    };
    vec![command.stage(ProvisionStage::Provisioning)]
}

/// Move a command to the remote host without losing its input or lifecycle metadata.
fn command_over_ssh(mut command: CommandSpec, ssh: &SshTarget) -> CommandSpec {
    let remote = std::iter::once(command.program)
        .chain(command.args)
        .collect();
    let wrapped = ssh_command_owned(ssh, remote);
    command.program = wrapped.program;
    command.args = wrapped.args;
    command
}

fn at_boundary(boundary: ExecutionBoundary<'_>, args: Vec<String>) -> CommandSpec {
    match boundary {
        ExecutionBoundary::Direct => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        ExecutionBoundary::Container {
            engine,
            container_id,
        } => container_exec(engine, container_id, args),
        ExecutionBoundary::Ssh(ssh) => ssh_command_owned(ssh, args),
        ExecutionBoundary::SshContainer {
            engine,
            ssh,
            container_id,
        } => {
            let mut remote = vec![
                engine.to_owned(),
                "exec".to_owned(),
                "-i".to_owned(),
                container_id.to_owned(),
            ];
            remote.extend(args);
            ssh_command_owned(ssh, remote)
        }
    }
}

#[cfg(test)]
mod tests;

/// Filesystem type of each directory, probed on the host that runs the
/// container engine. `ssh` names that host for a remote Podman target; `None`
/// probes this machine.
///
/// The reply is positional, so the whole batch fails unless `stat` answered for
/// every directory in order.
pub fn probe_filesystem_types(
    ssh: Option<&SshTarget>,
    paths: &[PathBuf],
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "stat".to_owned(),
        "-f".to_owned(),
        "-c".to_owned(),
        "%T".to_owned(),
        "--".to_owned(),
    ];
    args.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
    let host = match ssh {
        Some(ssh) => PodmanHost::Ssh(ssh),
        None => PodmanHost::Local,
    };
    let output = executor.execute(&host.command_owned(args, "probe mount source filesystem"))?;
    if output.status != 0 {
        bail!(
            "filesystem probe failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let types = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect::<Vec<_>>();
    if types.len() != paths.len() {
        bail!(
            "filesystem probe named {} filesystems for {} directories",
            types.len(),
            paths.len()
        );
    }
    Ok(types)
}
