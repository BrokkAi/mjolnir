//! The runtimes a machine can run, found by asking the machine.
//!
//! A machine is only a host: sessions run on its runtimes, so a machine with
//! none never appears as a session target. Saving a machine, starting the
//! dashboard, Settings' Detect runtimes and `mj setup` all probe the machine
//! here and add what it runs. `mj doctor` probes a machine the same way to
//! say why it still has no runtime.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use mj_core::config::{
    Config, ContainerTemplate, Machine, PermissionMode, SshConnection, TargetTemplate,
};

use super::{DEFAULT_IMAGE, RuntimeKind, RuntimeProbe};
use crate::doctor::{
    CheckStatus, SshConnectivity, ssh_connectivity, ssh_docker_failure_remediation,
    ssh_podman_failure,
};
use crate::targets::{
    CommandExecutor, SshTarget, TargetOs, TargetPlatform, ssh_validation_command,
    verify_ssh_docker, verify_ssh_podman,
};

/// What probing one machine found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineProbe {
    pub machine_id: String,
    /// The machine as it was probed. What was found describes this host
    /// only, so it is applied only while the configuration still names the
    /// machine unchanged.
    pub machine: Machine,
    pub outcome: MachineProbeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineProbeOutcome {
    /// The machine answered: each runtime it could run, usable or not.
    Answered(Vec<RuntimeProbe>),
    /// The machine did not answer over SSH, so nothing is known about what
    /// it runs.
    Unreachable { detail: String, remediation: String },
}

impl MachineProbe {
    /// The usable runtimes, as configuration entries under the ids they
    /// would like.
    pub fn runtimes(&self) -> Vec<(String, TargetTemplate)> {
        self.usable()
            .filter_map(|runtime| machine_runtime(&self.machine_id, &self.machine, runtime.kind))
            .collect()
    }

    /// The runtimes the machine answered it can run.
    pub fn usable(&self) -> impl Iterator<Item = &RuntimeProbe> {
        self.found().iter().filter(|runtime| runtime.usable())
    }

    /// The runtimes the machine answered it cannot run, with why.
    pub fn rejected(&self) -> impl Iterator<Item = &RuntimeProbe> {
        self.found().iter().filter(|runtime| !runtime.usable())
    }

    fn found(&self) -> &[RuntimeProbe] {
        match &self.outcome {
            MachineProbeOutcome::Answered(found) => found,
            MachineProbeOutcome::Unreachable { .. } => &[],
        }
    }
}

/// Probe the runtimes `machine` can run. `None` for this machine, whose
/// runtimes the local discovery finds.
pub fn probe_machine(
    machine_id: &str,
    machine: &Machine,
    executor: &impl CommandExecutor,
) -> Option<MachineProbe> {
    let outcome = match machine {
        Machine::Local { .. } => return None,
        // An instance is launched per session, so there is no host to ask
        // until then, and an EC2 machine runs a bare harness only.
        Machine::AwsEc2 { .. } => MachineProbeOutcome::Answered(vec![RuntimeProbe {
            kind: RuntimeKind::Bare,
            status: CheckStatus::Ready,
            detail: "Each session launches its own instance from this template.".to_owned(),
            remediation: None,
        }]),
        Machine::Ssh { ssh, .. } => probe_ssh_machine(ssh, executor),
    };
    Some(MachineProbe {
        machine_id: machine_id.to_owned(),
        machine: machine.clone(),
        outcome,
    })
}

/// Probe several machines at once. Each one is a separate host with its own
/// timeouts, so a host that does not answer delays only its own result.
pub fn probe_machines(
    machines: &[(String, Machine)],
    executor: &(impl CommandExecutor + Sync),
) -> Vec<MachineProbe> {
    std::thread::scope(|scope| {
        let probes: Vec<_> = machines
            .iter()
            .map(|(id, machine)| scope.spawn(move || probe_machine(id, machine, executor)))
            .collect();
        probes
            .into_iter()
            .filter_map(|probe| {
                probe
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// Connectivity first: a runtime probe on a host that does not answer would
/// report a runtime problem the user does not have.
fn probe_ssh_machine(ssh: &SshConnection, executor: &impl CommandExecutor) -> MachineProbeOutcome {
    let target = SshTarget::from(ssh);
    if let SshConnectivity::Failed {
        detail,
        remediation,
    } = ssh_connectivity(&target, executor)
    {
        return MachineProbeOutcome::Unreachable {
            detail,
            remediation,
        };
    }
    let destination = &target.destination;
    let bare = match probe_platform(&target, executor) {
        Ok(platform) => RuntimeProbe {
            kind: RuntimeKind::Bare,
            status: CheckStatus::Ready,
            detail: format!(
                "{destination} runs {} on {}, which the Mjolnir worker supports.",
                match platform.os {
                    TargetOs::Linux => "Linux",
                    TargetOs::Darwin => "macOS",
                },
                platform.architecture
            ),
            remediation: None,
        },
        Err(error) => RuntimeProbe {
            kind: RuntimeKind::Bare,
            status: CheckStatus::Unsupported,
            detail: format!("{error:#}"),
            remediation: Some(
                "Mjolnir workers run on Linux and macOS, on x86_64 or arm64.".to_owned(),
            ),
        },
    };
    let podman = match verify_ssh_podman(&target, executor) {
        Ok(preflight) => RuntimeProbe {
            kind: RuntimeKind::Podman,
            status: CheckStatus::Ready,
            detail: format!("Rootless Podman {} is available.", preflight.version),
            remediation: None,
        },
        Err(error) => {
            let (detail, remediation) = ssh_podman_failure(destination, &error);
            RuntimeProbe {
                kind: RuntimeKind::Podman,
                status: CheckStatus::Fixable,
                detail,
                remediation: Some(remediation),
            }
        }
    };
    let docker = match verify_ssh_docker(&target, executor) {
        Ok(preflight) => RuntimeProbe {
            kind: RuntimeKind::Docker,
            status: CheckStatus::Ready,
            detail: format!("Docker {} is available.", preflight.version),
            remediation: None,
        },
        Err(error) => RuntimeProbe {
            kind: RuntimeKind::Docker,
            status: CheckStatus::Fixable,
            detail: format!("{error:#}"),
            remediation: Some(ssh_docker_failure_remediation(destination)),
        },
    };
    MachineProbeOutcome::Answered(vec![bare, podman, docker])
}

/// The platform the host reports, which decides whether a worker can run on
/// it directly.
fn probe_platform(target: &SshTarget, executor: &impl CommandExecutor) -> Result<TargetPlatform> {
    let command = ssh_validation_command(
        target,
        vec!["uname".to_owned(), "-sm".to_owned()],
        "detect machine platform",
    );
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "`uname -sm` on {} exited with status {}: {}",
        target.destination,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let reported = String::from_utf8_lossy(&output.stdout);
    TargetPlatform::parse(&reported)
        .with_context(|| format!("{} reported {:?}", target.destination, reported.trim()))
}

/// One runtime on `machine` as a configuration entry, under the id it would
/// like: the bare runtime takes the machine's own id, and a container
/// runtime adds its engine.
fn machine_runtime(
    machine_id: &str,
    machine: &Machine,
    kind: RuntimeKind,
) -> Option<(String, TargetTemplate)> {
    let container = |machine: &Machine| ContainerTemplate {
        // In memory a container runtime carries its machine's cache settings.
        build_cache: machine.build_cache().cloned(),
        image: DEFAULT_IMAGE.to_owned(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: Default::default(),
        workspace_storage: Default::default(),
    };
    let target = match (machine, kind) {
        (
            Machine::Ssh {
                ssh,
                workspace_prefix,
                ..
            },
            RuntimeKind::Bare,
        ) => TargetTemplate::SshBare {
            ssh: ssh.clone(),
            permissions: PermissionMode::Guardian,
            workspace_prefix: workspace_prefix.clone(),
        },
        (Machine::Ssh { ssh, .. }, RuntimeKind::Podman) => TargetTemplate::SshPodman {
            ssh: ssh.clone(),
            container: container(machine),
        },
        (Machine::Ssh { ssh, .. }, RuntimeKind::Docker) => TargetTemplate::SshDocker {
            ssh: ssh.clone(),
            container: container(machine),
        },
        (
            Machine::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                launch_template_version,
                ssh_user,
                address_source,
                identity_file,
                ssh_args,
            },
            RuntimeKind::Bare,
        ) => TargetTemplate::AwsEc2 {
            aws_profile: aws_profile.clone(),
            region: region.clone(),
            launch_template: launch_template.clone(),
            launch_template_version: launch_template_version.clone(),
            ssh_user: ssh_user.clone(),
            address_source: address_source.clone(),
            identity_file: identity_file.clone(),
            ssh_args: ssh_args.clone(),
        },
        _ => return None,
    };
    let id = match kind {
        RuntimeKind::Bare => machine_id.to_owned(),
        RuntimeKind::Podman => engine_runtime_id(machine_id, "podman"),
        RuntimeKind::Docker => engine_runtime_id(machine_id, "docker"),
        RuntimeKind::AppleContainer => return None,
    };
    Some((id, target))
}

/// `<machine>-<engine>`, shortened to the 64 bytes an id may have.
fn engine_runtime_id(machine_id: &str, engine: &str) -> String {
    let suffix = format!("-{engine}");
    let prefix: String = machine_id.chars().take(64 - suffix.len()).collect();
    format!("{prefix}{suffix}")
}

/// Add the runtimes `probes` found to every probed machine that still has
/// none, under the configuration file's lock. A machine that gained a
/// runtime, changed, or was removed while it was probed is left as it is
/// now. Returns the runtimes added, by machine; the file is not touched when
/// there are none.
pub fn add_machine_runtimes(
    config_path: &Path,
    probes: &[MachineProbe],
    cancelled: &dyn Fn() -> bool,
) -> Result<BTreeMap<String, Vec<String>>> {
    let additions = |config: &Config| {
        probes
            .iter()
            .filter(|probe| config.machine_runtimes(&probe.machine_id).next().is_none())
            .map(|probe| {
                let added = config.machine_runtime_additions(
                    &probe.machine_id,
                    &probe.machine,
                    probe.runtimes(),
                );
                (probe.machine_id.clone(), added)
            })
            .filter(|(_, added)| !added.is_empty())
            .collect::<BTreeMap<_, _>>()
    };
    ensure!(!cancelled(), "machine runtime detection cancelled");
    // Leave the file untouched on the common launch that adds nothing.
    if additions(&Config::load_from(config_path)?).is_empty() {
        return Ok(BTreeMap::new());
    }
    let (_, added) = Config::update_to(config_path, |latest| {
        // Decide against the latest file under its lock, so a concurrent
        // Settings save or another client's probe is preserved and never
        // doubled.
        let mut added = BTreeMap::new();
        for (machine_id, runtimes) in additions(latest) {
            added.insert(machine_id, runtimes.keys().cloned().collect());
            latest.targets.extend(runtimes);
        }
        Ok(added)
    })?;
    Ok(added)
}

#[cfg(test)]
mod tests;
