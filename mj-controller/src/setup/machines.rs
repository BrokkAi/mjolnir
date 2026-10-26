//! The runtimes a machine can run, found by asking the machine.
//!
//! A machine is only a host: sessions run on its runtimes, so a machine with
//! none never appears as a session target. Saving a machine, starting the
//! dashboard, Settings' Detect runtimes and `mj setup` all probe the machine
//! here and add what it runs. `mj doctor` probes a machine the same way to
//! say why it still has no runtime.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use mj_core::config::{
    Config, ContainerTemplate, Machine, PermissionMode, SshConnection, TargetTemplate,
};
use mj_core::targets::{CommandOutput, CommandSpec, join_remote_command, posix_quote};

use super::{DEFAULT_IMAGE, RuntimeKind, RuntimeProbe};
use crate::doctor::{
    APPLE_CONTAINER_INSTALL_URL, CheckStatus, SshConnectivity, ssh_connectivity,
    ssh_docker_failure_remediation, ssh_podman_failure,
};
use crate::targets::{
    APPLE_CONTAINER_MACOS_PROBE, APPLE_CONTAINER_MINIMUM_MACOS, APPLE_CONTAINER_STATUS_PROBE,
    APPLE_CONTAINER_VERSION_PROBE, CommandExecutor, DOCKER_SERVER_VERSION, PLATFORM_PROBE,
    ProbeFrames, SSH_PODMAN_PREFLIGHT_SCRIPT, SshTarget, TargetOs, TargetPlatform,
    parse_framed_probes, ssh_validation_command, verify_ssh_apple_container, verify_ssh_docker,
    verify_ssh_podman,
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
        // until then, and an EC2 machine runs a bare harness only. What can
        // be checked is what a launch checks first: the AWS credentials and
        // the launch template.
        Machine::AwsEc2 { .. } => {
            let (_, template) = machine_runtime(machine_id, machine, RuntimeKind::Bare)
                .expect("an EC2 machine has a bare runtime");
            MachineProbeOutcome::Answered(vec![
                match crate::controller::verify_target_before_launch(&template, executor) {
                    Ok(()) => RuntimeProbe {
                        kind: RuntimeKind::Bare,
                        status: CheckStatus::Ready,
                        detail: "Each session launches its own instance from this template."
                            .to_owned(),
                        remediation: None,
                    },
                    Err(error) => RuntimeProbe {
                        kind: RuntimeKind::Bare,
                        status: CheckStatus::Fixable,
                        detail: format!("{error:#}"),
                        remediation: Some(
                            "Sign in to AWS for this profile (for example `aws sso login`) and check that the launch template exists in its region.".to_owned(),
                        ),
                    },
                },
            ])
        }
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

/// The markers the machine probe frames each result with. They differ from
/// the Podman script's, which runs inside this one as a single probe.
const MACHINE_PROBE_FRAMES: ProbeFrames = ProbeFrames {
    begin: "__mj_machine_begin__",
    end: "__mj_machine_end__",
    status: "__mj_machine_status__",
};

/// Every remote command line the runtime checks send to an SSH host, built
/// from the same constants the checks use, and whether it runs only on an
/// Apple-silicon Mac. The machine probe runs them all in one SSH call and
/// replays the answers to the checks.
fn machine_probe_commands() -> Vec<(String, bool)> {
    let line = |args: &[&str]| {
        join_remote_command(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    };
    vec![
        (line(&PLATFORM_PROBE), false),
        (line(&["sh", "-c", SSH_PODMAN_PREFLIGHT_SCRIPT]), false),
        (line(&DOCKER_SERVER_VERSION), false),
        (line(&APPLE_CONTAINER_MACOS_PROBE), true),
        (line(&APPLE_CONTAINER_VERSION_PROBE), true),
        (line(&APPLE_CONTAINER_STATUS_PROBE), true),
    ]
}

/// One shell script that runs each command as the remote login shell would
/// and prints its stdout, stderr and status between markers. Each command
/// reads no input, so a probe that waits on stdin cannot stall the rest. The
/// Apple container commands run only where `uname -sm` reports an
/// Apple-silicon Mac: another host has no `container` command to ask about.
fn machine_probe_script(commands: &[(String, bool)]) -> String {
    let ProbeFrames { begin, end, status } = MACHINE_PROBE_FRAMES;
    let mut script = format!(
        r#"exec 3>&1
probe() {{
    name=$1
    shift
    printf '{begin} %s.stderr\n' "$name"
    out=$("$@" 2>&3 </dev/null)
    status=$?
    printf '\n{end}\n'
    printf '{begin} %s.stdout\n%s\n{end}\n' "$name" "$out"
    printf '{status} %s %s\n' "$name" "$status"
}}
apple=no
[ "$(uname -sm 2>/dev/null)" = "Darwin arm64" ] && apple=yes
"#
    );
    for (index, (command, apple_only)) in commands.iter().enumerate() {
        let guard = if *apple_only {
            "[ $apple = yes ] && "
        } else {
            ""
        };
        script.push_str(&format!(
            "{guard}probe {index} sh -c {}\n",
            posix_quote(command)
        ));
    }
    script.push_str("exit 0\n");
    script
}

/// What the machine probe's batch prints for the given results, named
/// `platform`, `podman`, `docker`, `macos`, `container-version` and
/// `container-status`. Tests across this crate stand in for a remote host,
/// so they need the real framing rather than a second description of it.
#[cfg(test)]
pub(crate) fn machine_probe_fixture(results: &[(&str, i32, &[u8], &str)]) -> Vec<u8> {
    const NAMES: [&str; 6] = [
        "platform",
        "podman",
        "docker",
        "macos",
        "container-version",
        "container-status",
    ];
    let ProbeFrames { begin, end, status } = MACHINE_PROBE_FRAMES;
    let mut output = Vec::new();
    for (name, code, stdout, stderr) in results {
        let index = NAMES
            .iter()
            .position(|known| known == name)
            .unwrap_or_else(|| panic!("no machine probe is named {name:?}"));
        output.extend(format!("{begin} {index}.stderr\n{stderr}\n{end}\n").into_bytes());
        output.extend(format!("{begin} {index}.stdout\n").into_bytes());
        output.extend(stdout.strip_suffix(b"\n").unwrap_or(stdout));
        output.extend(format!("\n{end}\n{status} {index} {code}\n").into_bytes());
    }
    output
}

/// Answers the runtime checks' SSH commands from one recorded batch. A
/// command the batch did not record is an error, so a check that starts
/// sending something new fails visibly instead of being judged on nothing.
struct RecordedMachine {
    answers: BTreeMap<String, CommandOutput>,
    /// Why the batch itself did not run, handed to the connectivity check.
    failure: RefCell<Option<anyhow::Error>>,
}

impl CommandExecutor for RecordedMachine {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if let Some(error) = self.failure.borrow_mut().take() {
            return Err(error);
        }
        ensure!(
            command.program == "ssh",
            "the machine probe cannot answer `{}`",
            command.program
        );
        let remote = command
            .args
            .last()
            .context("an SSH command names its remote command last")?;
        self.answers
            .get(remote)
            .cloned()
            .with_context(|| format!("the machine probe did not run `{remote}`"))
    }
}

/// Run every runtime check against the host in one SSH call, then judge the
/// answers with the checks a launch uses.
fn record_machine(target: &SshTarget, executor: &impl CommandExecutor) -> RecordedMachine {
    let commands = machine_probe_commands();
    let batch = ssh_validation_command(
        target,
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            machine_probe_script(&commands),
        ],
        "check what the machine can run",
    );
    let connected = join_remote_command(&["true".to_owned()]);
    match executor.execute(&batch) {
        Err(error) => RecordedMachine {
            answers: BTreeMap::new(),
            failure: RefCell::new(Some(error)),
        },
        // The script always exits 0, so any other status is SSH's own: the
        // connectivity check classifies it from what SSH printed.
        Ok(output) if output.status != 0 => RecordedMachine {
            answers: BTreeMap::from([(connected, output)]),
            failure: RefCell::new(None),
        },
        Ok(output) => {
            let mut found = parse_framed_probes(&output.stdout, MACHINE_PROBE_FRAMES);
            let mut answers: BTreeMap<String, CommandOutput> = commands
                .into_iter()
                .enumerate()
                .filter_map(|(index, (command, _))| {
                    Some((command, found.remove(&index.to_string())?))
                })
                .collect();
            answers.insert(
                connected,
                CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            );
            RecordedMachine {
                answers,
                failure: RefCell::new(None),
            }
        }
    }
}

/// Connectivity first: a runtime probe on a host that does not answer would
/// report a runtime problem the user does not have.
fn probe_ssh_machine(ssh: &SshConnection, executor: &impl CommandExecutor) -> MachineProbeOutcome {
    let target = SshTarget::from(ssh);
    let recorded = record_machine(&target, executor);
    let executor = &recorded;
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
    let platform = probe_platform(&target, executor);
    // Apple container runs only on Apple-silicon macOS, so a host that reported
    // another platform is not asked for it: a Linux host has no `container`
    // command and no reason to be told to install one.
    let apple_host = platform.as_ref().ok().is_some_and(|platform| {
        platform.os == TargetOs::Darwin && platform.architecture == "aarch64"
    });
    let bare = match &platform {
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
    let apple_container = if apple_host {
        match verify_ssh_apple_container(&target, executor) {
            Ok(preflight) => RuntimeProbe {
                kind: RuntimeKind::AppleContainer,
                status: CheckStatus::Ready,
                detail: format!(
                    "Apple container {} is available on macOS {}.",
                    preflight.version, preflight.macos_version
                ),
                remediation: None,
            },
            Err(error) => RuntimeProbe {
                kind: RuntimeKind::AppleContainer,
                status: CheckStatus::Fixable,
                detail: format!("{error:#}"),
                remediation: Some(format!(
                    "On {destination}: install the Apple container runtime from {APPLE_CONTAINER_INSTALL_URL}, start it with `container system start`, and use macOS {APPLE_CONTAINER_MINIMUM_MACOS} or newer. Run `mj doctor` to see which of these is missing."
                )),
            },
        }
    } else {
        RuntimeProbe {
            kind: RuntimeKind::AppleContainer,
            status: CheckStatus::Unsupported,
            detail: format!(
                "Apple container runs only on Apple-silicon macOS {APPLE_CONTAINER_MINIMUM_MACOS} or newer, and {}.",
                match &platform {
                    Ok(platform) => format!(
                        "{destination} runs {} on {}",
                        match platform.os {
                            TargetOs::Linux => "Linux",
                            TargetOs::Darwin => "macOS",
                        },
                        platform.architecture
                    ),
                    Err(error) => format!("{error:#}"),
                }
            ),
            remediation: None,
        }
    };
    MachineProbeOutcome::Answered(vec![bare, podman, docker, apple_container])
}

/// The platform the host reports, which decides whether a worker can run on
/// it directly.
fn probe_platform(target: &SshTarget, executor: &impl CommandExecutor) -> Result<TargetPlatform> {
    let command = ssh_validation_command(
        target,
        PLATFORM_PROBE.map(str::to_owned).to_vec(),
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
        (Machine::Ssh { ssh, .. }, RuntimeKind::AppleContainer) => {
            TargetTemplate::SshAppleContainer {
                ssh: ssh.clone(),
                container: container(machine),
            }
        }
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
        RuntimeKind::AppleContainer => engine_runtime_id(machine_id, "apple-container"),
    };
    Some((id, target))
}

/// `<machine>-<engine>`, shortened to the 64 bytes an id may have.
fn engine_runtime_id(machine_id: &str, engine: &str) -> String {
    let suffix = format!("-{engine}");
    let prefix: String = machine_id.chars().take(64 - suffix.len()).collect();
    format!("{prefix}{suffix}")
}

/// Add each runtime kind `probes` found that its machine does not run yet,
/// under the configuration file's lock. A runtime switched off with
/// `enabled = false` is still configured, so its kind is never added again.
/// A machine that changed or was removed while it was probed is left as it
/// is now. Returns the runtimes added, by machine; the file is not touched
/// when there are none.
pub fn add_machine_runtimes(
    config_path: &Path,
    probes: &[MachineProbe],
    cancelled: &dyn Fn() -> bool,
) -> Result<BTreeMap<String, Vec<String>>> {
    let additions = |config: &Config| {
        probes
            .iter()
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
