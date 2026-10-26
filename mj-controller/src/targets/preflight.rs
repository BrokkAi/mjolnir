use super::*;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedResourceKind {
    Container,
    Ec2Instance,
}

/// Build command-line fragments that identify resources Hel owns for a session.
pub(super) fn managed_resource_identity_args(
    kind: ManagedResourceKind,
    session_id: &str,
) -> Vec<String> {
    let instance = mj_core::config::instance_identity();
    match kind {
        ManagedResourceKind::Container => vec![
            "--label".to_owned(),
            format!("{SESSION_LABEL}={session_id}"),
            "--label".to_owned(),
            format!("{MANAGED_LABEL}=true"),
            "--label".to_owned(),
            format!("{INSTANCE_LABEL}={instance}"),
        ],
        ManagedResourceKind::Ec2Instance => vec![
            "--tag-specifications".to_owned(),
            format!(
                "ResourceType=instance,Tags=[{{Key={SESSION_TAG},Value={session_id}}},{{Key={MANAGED_TAG},Value=true}},{{Key={INSTANCE_TAG},Value={instance}}}]"
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
pub(super) enum PodmanHost<'a> {
    Local,
    Ssh(&'a SshTarget),
}

impl PodmanHost<'_> {
    /// Sentence opener for every failure raised by these probes.
    pub(super) fn failure(self) -> String {
        match self {
            Self::Local => "Podman preflight failed".to_owned(),
            Self::Ssh(ssh) => format!("Remote Podman preflight failed on {}", ssh.destination),
        }
    }

    /// Prefix that says where a remediation must be applied.
    pub(super) fn remediation_scope(self) -> String {
        match self {
            Self::Local => String::new(),
            Self::Ssh(ssh) => format!("On {}: ", ssh.destination),
        }
    }

    pub(super) fn command(self, args: &[&str], purpose: &'static str) -> CommandSpec {
        self.command_owned(args.iter().map(|arg| (*arg).to_owned()).collect(), purpose)
    }

    pub(super) fn command_owned(self, args: Vec<String>, purpose: &'static str) -> CommandSpec {
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
/// `mj doctor --smoke` and by the subsequent target creation command.
pub fn verify_local_podman(executor: &impl CommandExecutor) -> Result<PodmanPreflight> {
    verify_podman(PodmanHost::Local, executor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerPreflight {
    pub version: String,
}

/// Verify that the Docker CLI can reach a Linux Docker daemon.
///
/// Image and OverlayFS support are exercised by `mj doctor --smoke`;
/// this fast probe runs before every launch and never pulls an image.
pub fn verify_local_docker(executor: &impl CommandExecutor) -> Result<DockerPreflight> {
    verify_docker(None, executor)
}

static APPLE_CONTAINER_ENSURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The single gate shared by startup, launch readiness, and image refresh.
#[cfg(test)]
pub(super) fn apple_container_runtime_gate() -> &'static std::sync::Mutex<()> {
    &APPLE_CONTAINER_ENSURE
}

/// What ensuring Apple's local runtime did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleContainerRuntime {
    /// The service was already answering health checks.
    Running,
    /// The service was stopped, and Mjolnir started it.
    Started,
}

/// Ensure Apple's local container service is running.
///
/// Several launch surfaces can ask at once. One process-wide gate makes the
/// check and start one decision, so two callers cannot both try to register
/// the same launchd service. `--disable-kernel-install` keeps this unattended
/// path from prompting for, or silently downloading, Apple's default kernel;
/// the manual command documented for first-time setup remains interactive.
pub fn ensure_apple_container_runtime(
    executor: &impl CommandExecutor,
) -> Result<AppleContainerRuntime> {
    // A launch can own this gate through the unbounded ProcessExecutor used by
    // the options API. Daemon shutdown joins the cancellable startup and image
    // refresh callers, so they must be able to leave while waiting for it.
    let mut backoff = Duration::from_millis(1);
    let _ensuring = loop {
        ensure!(
            !executor.cancellation_requested(),
            "operation cancelled while waiting to check Apple container runtime"
        );
        match APPLE_CONTAINER_ENSURE.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
        }
    };
    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while checking Apple container runtime"
    );

    let status = CommandSpec::new("container", ["system", "status"])
        .purpose("check Apple container runtime")
        .stage(ProvisionStage::Provisioning);
    if executor.execute(&status)?.status == 0 {
        return Ok(AppleContainerRuntime::Running);
    }

    let start = CommandSpec::new("container", ["system", "start", "--disable-kernel-install"])
        .purpose("start Apple container runtime")
        .stage(ProvisionStage::Provisioning);
    let output = executor.execute(&start)?;
    ensure!(
        output.status == 0,
        "container system start failed with status {}: {}",
        output.status,
        apple_container_output(&output)
    );

    let output = executor.execute(&status)?;
    ensure!(
        output.status == 0,
        "container system status still failed after start with status {}: {}",
        output.status,
        apple_container_output(&output)
    );
    Ok(AppleContainerRuntime::Started)
}

fn apple_container_output(output: &CommandOutput) -> String {
    [
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    ]
    .into_iter()
    .filter(|message| !message.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
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

/// What a reachable Apple container host reported about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleContainerPreflight {
    /// The macOS release, as `sw_vers -productVersion` prints it.
    pub macos_version: String,
    /// What `container --version` prints, when the CLI answers it.
    pub version: String,
}

/// Verify that an SSH host can run Apple containers.
///
/// Apple's `container` CLI drives a per-host VM disk image, so a session's
/// workspace lives on the SSH host exactly as a local one lives on this Mac.
/// There is no daemon socket to reach over a tunnel, which is why this target
/// exists as its own kind instead of a tunnel to a local runtime.
///
/// Platform support comes first: a Linux host has no `container` command and
/// no reason to be told to install one.
pub fn verify_ssh_apple_container(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<AppleContainerPreflight> {
    validate_ssh(ssh)?;
    let destination = &ssh.destination;
    let probe = |args: [&str; 2], purpose: &'static str| {
        ssh_validation_command(ssh, vec![args[0].to_owned(), args[1].to_owned()], purpose)
            .stage(ProvisionStage::Provisioning)
    };
    let reported = |command: &CommandSpec| -> Result<String> {
        let output = executor.execute(command).with_context(|| {
            format!(
                "Apple container preflight on {destination} could not run `{}`",
                command.purpose
            )
        })?;
        ensure!(
            output.status == 0,
            "Apple container preflight failed on {destination}: `{}` exited with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };

    let uname = reported(&probe(
        ["uname", "-sm"],
        "detect the Apple container host platform",
    ))?;
    let platform = TargetPlatform::parse(&uname).with_context(|| {
        format!("Apple container preflight failed on {destination}: the host reported {uname:?}")
    })?;
    ensure!(
        platform.os == TargetOs::Darwin,
        "Apple container preflight failed on {destination}: Apple container runs only on macOS, and this host reports {:?}.",
        uname
    );
    ensure!(
        platform.architecture == "aarch64",
        "Apple container preflight failed on {destination}: Apple container requires Apple silicon, and this host reports {uname:?}."
    );

    let macos_version = reported(&probe(
        ["sw_vers", "-productVersion"],
        "detect the remote macOS release",
    ))?;
    let major: u32 = macos_version
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .with_context(|| {
            format!(
                "Apple container preflight failed on {destination}: `sw_vers -productVersion` printed {macos_version:?}"
            )
        })?;
    ensure!(
        major >= APPLE_CONTAINER_MINIMUM_MACOS,
        "Apple container preflight failed on {destination}: Apple container requires macOS {APPLE_CONTAINER_MINIMUM_MACOS} or newer, and this host runs {macos_version}."
    );

    let version = reported(&probe(
        ["container", "--version"],
        "check the remote Apple container installation",
    ))?;
    // `container system status` exits nonzero while the daemon is stopped, so
    // it is checked for its status rather than its words.
    let status = ssh_validation_command(
        ssh,
        vec![
            "container".to_owned(),
            "system".to_owned(),
            "status".to_owned(),
        ],
        "check the remote Apple container daemon",
    )
    .stage(ProvisionStage::Provisioning);
    let output = executor.execute(&status).with_context(|| {
        format!("Apple container preflight on {destination} could not query the daemon")
    })?;
    ensure!(
        output.status == 0,
        "Apple container preflight failed on {destination}: the Apple container daemon is not running ({}). Start it there with `container system start`.",
        [
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ]
        .into_iter()
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    );
    Ok(AppleContainerPreflight {
        macos_version,
        version,
    })
}

/// The oldest macOS release Apple's official signed `container` package
/// supports.
pub const APPLE_CONTAINER_MINIMUM_MACOS: u32 = 26;

/// Why the local Docker daemon cannot overlay host directories, or `None`
/// when it runs on this Linux host and sees them directly.
///
/// On macOS every Docker daemon (Docker Desktop, Colima, OrbStack) runs in a
/// Linux VM, as Docker Desktop also does on Linux. The VM reaches host
/// directories through a file share (virtiofs, gRPC FUSE, sshfs, 9p) that
/// cannot back an overlay lower layer, and the local volume driver may not
/// see them at their host path at all (#1152). Bind mounts are translated
/// through the share, so these daemons still get read-only attachments.
pub fn local_docker_vm_share(executor: &impl CommandExecutor) -> Result<Option<&'static str>> {
    const REASON: &str = "Docker runs in a VM that reaches host directories through a file share";
    if !cfg!(target_os = "linux") {
        return Ok(Some(REASON));
    }
    let command = CommandSpec::new(
        "docker",
        ["version", "--format", "{{.Server.Platform.Name}}"],
    )
    .purpose("identify the Docker daemon platform")
    .stage(ProvisionStage::Provisioning);
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "{} failed with status {}: {}",
        command.purpose,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let platform = String::from_utf8_lossy(&output.stdout);
    Ok(platform
        .trim()
        .starts_with("Docker Desktop")
        .then_some(REASON))
}

/// How the launch options, the session wizard, doctor and Setup say that a
/// local container engine's command is not on this host.
pub fn engine_not_installed(engine: &str) -> String {
    format!("{engine} is not installed on this host")
}

/// The command a local container target's engine runs as. `None` for any
/// other target, whose readiness is not a local engine's.
pub fn local_engine_command(template: &mj_core::config::TargetTemplate) -> Option<&'static str> {
    use mj_core::config::TargetTemplate as Template;
    match template {
        Template::LocalPodman { .. } => Some("podman"),
        Template::LocalDocker { .. } => Some("docker"),
        Template::AppleContainer { .. } => Some("container"),
        _ => None,
    }
}

/// Whether `template` needs a local container engine whose command is not on
/// `path`, a PATH value. That is a fact about this host and stays true until
/// the engine is installed, unlike a host that did not answer its last check.
/// Every place that decides whether to offer a target reads this one answer:
/// the terminal wizards, the web viewer's snapshot, and the launch options.
pub fn runtime_missing_on_host(
    template: &mj_core::config::TargetTemplate,
    path: Option<&std::ffi::OsStr>,
) -> bool {
    local_engine_command(template).is_some_and(|engine| !program_on_path(engine, path))
}

/// Whether `program` is found in one of the directories of `path`, a PATH
/// value, under the platform's executable extensions (`docker.exe` on
/// Windows). A missing PATH finds nothing.
pub fn program_on_path(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    mj_core::program_path::find_program_on_path(program, path).is_some()
}

/// Why local Docker cannot run sessions: one sentence per case, where the
/// raw error chain said "run docker for check Docker daemon: No such file or
/// directory (os error 2)" (launch finding R5-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerUnavailable {
    /// `docker` is not on PATH.
    NotInstalled,
    /// The CLI ran but found no daemon to talk to.
    NotRunning { reported: String },
    /// The CLI ran and failed for another reason, such as a socket the user
    /// may not open.
    NotAnswering { status: i32, reported: String },
}

impl DockerUnavailable {
    /// What to do before Docker can run sessions, for a check made before
    /// anything is launched, such as the session wizard's target row. It
    /// does not mention Retry launch, which only the launch-failure dialog
    /// offers (launch finding R6-3).
    pub fn remedy(&self) -> &'static str {
        match self {
            Self::NotInstalled => "Install Docker or choose another target.",
            Self::NotRunning { .. } => "Start Docker.",
            Self::NotAnswering { .. } => "Fix what it reports.",
        }
    }

    /// What to do after a launch failed this check: the same advice, then
    /// the failure dialog's Retry launch.
    pub fn launch_remedy(&self) -> &'static str {
        match self {
            Self::NotInstalled => "Install Docker or choose another target, then Retry launch.",
            Self::NotRunning { .. } => "Start Docker, then Retry launch.",
            Self::NotAnswering { .. } => "Fix what it reports, then Retry launch.",
        }
    }

    /// What doctor and Setup advise.
    pub fn remediation(&self) -> String {
        match self {
            Self::NotInstalled => {
                format!("Install Docker ({DOCKER_DOCUMENTATION_URL}), or use another target.")
            }
            Self::NotRunning { .. } => {
                "Start Docker, then make sure `docker info` succeeds as the user running Mjolnir."
                    .to_owned()
            }
            Self::NotAnswering { .. } => format!(
                "Make sure `docker info` succeeds as the user running Mjolnir. See {DOCKER_DOCUMENTATION_URL}."
            ),
        }
    }
}

impl std::fmt::Display for DockerUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(formatter, "{}.", engine_not_installed("Docker")),
            Self::NotRunning { reported } => {
                write!(
                    formatter,
                    "Docker is installed, but its daemon is not running."
                )?;
                if !reported.is_empty() {
                    write!(formatter, " `docker version` said: {reported}")?;
                }
                Ok(())
            }
            Self::NotAnswering { status, reported } => {
                write!(
                    formatter,
                    "Docker did not answer its check on this host: `docker version` exited with status {status}"
                )?;
                if !reported.is_empty() {
                    write!(formatter, ": {reported}")?;
                }
                write!(
                    formatter,
                    ". Run `docker info` as the user running Mjolnir to see why."
                )
            }
        }
    }
}

impl std::error::Error for DockerUnavailable {}

/// Whether the Docker CLI's own words say it found no daemon to talk to.
fn docker_daemon_not_running(reported: &str) -> bool {
    reported.contains("Cannot connect to the Docker daemon")
        || reported.contains("Is the docker daemon running")
}

/// Whether a command could not start because its program is not on PATH.
fn is_missing_program(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

pub(super) fn verify_docker(
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
    let output = match executor.execute(&command) {
        Ok(output) => output,
        // Over SSH a missing program would be `ssh` itself, which the SSH
        // checks report; only a local one means Docker is not installed.
        // The cause stays in the chain for callers that classify it.
        Err(error) if ssh.is_none() && is_missing_program(&error) => {
            return Err(error.context(DockerUnavailable::NotInstalled));
        }
        Err(error) => {
            return Err(error.context(
                "Docker preflight failed: run `docker info` as the user running Mjolnir",
            ));
        }
    };
    if output.status != 0 {
        let reported = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if ssh.is_none() {
            let problem = if docker_daemon_not_running(&reported) {
                DockerUnavailable::NotRunning { reported }
            } else {
                DockerUnavailable::NotAnswering {
                    status: output.status,
                    reported,
                }
            };
            return Err(problem.into());
        }
        bail!(
            "Docker preflight failed: `docker version` exited with status {}: {reported}. Run `docker info` as the user running Mjolnir. See {DOCKER_DOCUMENTATION_URL}.",
            output.status
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    let mut fields = reported.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let os = fields.next().unwrap_or_default();
    ensure!(
        !version.is_empty() && os == "linux",
        "Docker preflight failed: expected a Linux Docker daemon, got {:?}. See {DOCKER_DOCUMENTATION_URL}.",
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
            "{}: the configured SSH destination is unusable ({error}). Set a valid `host` (and optional `user`) for this ssh-podman target. See {PODMAN_DOCUMENTATION_URL}.",
            host.failure()
        )
    })?;
    // One SSH round trip carries every probe; a remote shell runs them in
    // sequence and frames each result so the checks below stay unchanged.
    let probes = run_ssh_podman_probes(host, executor)?;
    let mut preflight = verify_podman_probes(host, |probe| {
        let output = probes.get(probe.key()).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                ssh_transport_failure(
                    host,
                    &format!(
                        "the preflight output ended before the {} probe",
                        probe.key()
                    ),
                )
                .expect("SSH host always reports a transport failure")
            )
        })?;
        check_podman_probe_status(host, probe, output)
    })?;
    if let Some(warning) = ssh_podman_linger_warning(ssh, probes.get(LINGER_PROBE_KEY)) {
        preflight.warnings.push(warning);
    }
    Ok(preflight)
}

/// One rootless Podman postcondition, with the wording used to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodmanPostcondition {
    Version,
    Rootless,
    Remote,
    Info,
    UidMap,
}

/// One Podman command whose result is checked.
///
/// `podman info` works through a remote client, unlike `podman unshare`. That
/// matters on macOS, where Podman always talks to a `podman machine` VM and
/// the client is remote by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodmanProbe {
    Version,
    Info,
}

/// A rootless Podman postcondition that was not met, carrying which one.
///
/// `mj doctor` needs the postcondition, not its wording, to name the fix.
/// Carrying it on the error means the diagnosis never depends on matching
/// message text that this repository itself produces.
#[derive(Debug)]
pub(crate) struct PodmanProbeFailure {
    postcondition: PodmanPostcondition,
    /// What was observed, without the fix.
    observation: String,
    /// The observation followed by the fix and the guide link.
    message: String,
}

impl std::fmt::Display for PodmanProbeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PodmanProbeFailure {}

/// The postcondition that failed, when the error came from a probe.
pub(crate) fn failed_podman_postcondition(error: &anyhow::Error) -> Option<PodmanPostcondition> {
    error
        .downcast_ref::<PodmanProbeFailure>()
        .map(|failure| failure.postcondition)
}

/// What a failed probe observed, without the fix, so a report that prints
/// the fix separately does not repeat it.
pub(crate) fn podman_probe_observation(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<PodmanProbeFailure>()
        .map(|failure| failure.observation.as_str())
}

/// Fail a postcondition with what was observed, followed by its fix and the
/// published guide, keeping the postcondition machine-readable.
fn probe_failure(
    host: PodmanHost<'_>,
    postcondition: PodmanPostcondition,
    observation: String,
) -> anyhow::Error {
    let message = format!(
        "{observation} {}{} See {PODMAN_DOCUMENTATION_URL}.",
        host.remediation_scope(),
        postcondition.remediation()
    );
    anyhow::Error::new(PodmanProbeFailure {
        postcondition,
        observation,
        message,
    })
}

impl PodmanProbe {
    /// Name of this probe in the batched remote script's output.
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Info => "info",
        }
    }

    pub(super) fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["podman", "--version"],
            Self::Info => &["podman", "info", "--format", "json"],
        }
    }

    pub(super) fn purpose(self) -> &'static str {
        match self {
            Self::Version => "check Podman version",
            Self::Info => "check rootless Podman mode and UID map",
        }
    }

    pub(super) fn postcondition(self) -> PodmanPostcondition {
        match self {
            Self::Version => PodmanPostcondition::Version,
            Self::Info => PodmanPostcondition::Info,
        }
    }

    /// What running this probe checks, in words that follow "to check".
    fn checks(self) -> &'static str {
        match self {
            Self::Version => "that Podman 4.0.0 or newer is installed",
            Self::Info => "that Podman is rootless and maps container UIDs 0 and 1",
        }
    }
}

impl PodmanPostcondition {
    pub(super) fn statement(self) -> &'static str {
        match self {
            Self::Version => "Postcondition `podman --version` succeeds with Podman 4.0.0 or newer",
            Self::Rootless => "Postcondition Podman is rootless",
            Self::Remote => "Postcondition Podman uses a local engine",
            Self::Info => "Postcondition `podman info` succeeds",
            Self::UidMap => "Postcondition `podman info` maps container UIDs 0 and 1",
        }
    }

    pub(crate) fn remediation(self) -> &'static str {
        match self {
            Self::Version => {
                "Install or upgrade Podman: on macOS `brew install podman` then `podman machine init --now`; Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`."
            }
            Self::Rootless => {
                "Run Mjolnir as the ordinary user without `sudo`, and use a rootless Podman. On macOS run `podman machine set --rootful=false` and restart the machine; on Linux do not run Podman as root and unset `CONTAINER_HOST` if a remote connection is selected."
            }
            Self::Remote => {
                "Unset `CONTAINER_HOST` and `CONTAINER_CONNECTION`, then select the local rootless Podman connection. On macOS the Podman machine connection is the supported local client; a manually configured remote connection is not."
            }
            Self::Info => {
                "Make sure `podman info` succeeds as the user running Mjolnir. On macOS start the Podman machine with `podman machine start`, creating it with `podman machine init --now` if none exists; on Linux install the UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora) if Podman reports one is missing."
            }
            Self::UidMap => {
                "Install UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora), then add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"` and start a fresh login session. On macOS, update Podman and recreate the machine if its rootless UID map does not cover container IDs 0 and 1."
            }
        }
    }
}

pub(super) fn verify_podman(
    host: PodmanHost<'_>,
    executor: &impl CommandExecutor,
) -> Result<PodmanPreflight> {
    verify_podman_probes(host, |probe| execute_podman_probe(executor, host, probe))
}

/// Apply the rootless Podman postconditions to probe results, however they
/// were obtained: one command each locally, one batched command over SSH.
pub(super) fn verify_podman_probes(
    host: PodmanHost<'_>,
    probe_output: impl Fn(PodmanProbe) -> Result<CommandOutput>,
) -> Result<PodmanPreflight> {
    let version = probe_output(PodmanProbe::Version)?;
    let version = parse_podman_version(host, &version.stdout)?;

    let info = probe_output(PodmanProbe::Info)?;
    let info = parse_podman_info(host, &info.stdout)?;

    if !info.rootless {
        return Err(probe_failure(
            host,
            PodmanPostcondition::Rootless,
            format!(
                "{}: {} was not met. `podman info` reports rootful mode.",
                host.failure(),
                PodmanPostcondition::Rootless.statement(),
            ),
        ));
    }

    // A remote client is expected only for a local macOS controller using a
    // Podman machine. On Linux, `serviceIsRemote` means CONTAINER_HOST or a
    // selected connection points this target at another host's engine, whose
    // paths and containers are not the local ones Mjolnir records.
    let macos_machine = matches!(host, PodmanHost::Local) && cfg!(target_os = "macos");
    if info.service_is_remote && !macos_machine {
        return Err(probe_failure(
            host,
            PodmanPostcondition::Remote,
            format!(
                "{}: {} was not met. `podman info` reports serviceIsRemote=true.",
                host.failure(),
                PodmanPostcondition::Remote.statement(),
            ),
        ));
    }

    for container_id in [0, 1] {
        if !info.covers_uid(container_id) {
            return Err(probe_failure(
                host,
                PodmanPostcondition::UidMap,
                format!(
                    "{}: {} was not met. `podman info` reports UID map {}.",
                    host.failure(),
                    PodmanPostcondition::UidMap.statement(),
                    info.uid_map_description(),
                ),
            ));
        }
    }

    let mut warnings = Vec::new();
    if !info.unenforceable_limits.is_empty() {
        warnings.push(PodmanPreflightWarning {
            detail: unenforceable_limits_detail(&info.unenforceable_limits),
            remediation: format!(
                "{}{}",
                host.remediation_scope(),
                CGROUP_DELEGATION_REMEDIATION
            ),
        });
    }
    Ok(PodmanPreflight { version, warnings })
}

/// A session container limit that rootless Podman applies through a cgroup v2
/// controller, which systemd must delegate to the user's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupLimit {
    Cpu,
    Memory,
}

impl CgroupLimit {
    pub const ALL: [Self; 2] = [Self::Cpu, Self::Memory];

    fn controller(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
        }
    }

    /// The `podman run` flag, as `backend_container` writes it, `--cpus=4`.
    pub fn run_flag_prefix(self) -> &'static str {
        match self {
            Self::Cpu => "--cpus=",
            Self::Memory => "--memory=",
        }
    }
}

/// openSUSE and SLE ship `Delegate=` for `user@.service`, so their rootless
/// Podman runs with only the pids controller. runc then fails every container
/// that sets `--cpus` or `--memory`, naming a missing `cpu.max` or
/// `memory.swap.max`.
pub const CGROUP_DELEGATION_REMEDIATION: &str = "To enforce them, have systemd delegate the controllers to user sessions: create `/etc/systemd/system/user@.service.d/80-delegate.conf` containing `[Service]` and `Delegate=pids memory cpu`, run `sudo systemctl daemon-reload`, then log in again.";

pub fn unenforceable_limits_detail(limits: &[CgroupLimit]) -> String {
    let controllers = limits
        .iter()
        .map(|limit| limit.controller())
        .collect::<Vec<_>>()
        .join(" and ");
    let noun = if limits.len() == 1 {
        "controller"
    } else {
        "controllers"
    };
    format!(
        "Rootless Podman cannot enforce session {controllers} limits because systemd does not delegate the {controllers} cgroup {noun} to this user, so sessions start without them."
    )
}

/// The limits Podman cannot enforce, read from the `host` object of
/// `podman info`. Only cgroup v2 is judged: rootless Podman on v1 already
/// ignores resource limits itself, and an absent controller list is unknown,
/// not empty.
fn unenforceable_limits(host: &serde_json::Value) -> Vec<CgroupLimit> {
    if host
        .get("cgroupVersion")
        .and_then(serde_json::Value::as_str)
        != Some("v2")
    {
        return Vec::new();
    }
    let Some(controllers) = host
        .get("cgroupControllers")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    CgroupLimit::ALL
        .into_iter()
        .filter(|limit| {
            !controllers
                .iter()
                .any(|controller| controller.as_str() == Some(limit.controller()))
        })
        .collect()
}

/// Ask the Podman that will run a container which limits it cannot enforce.
pub fn probe_podman_unenforceable_limits(
    ssh: Option<&SshTarget>,
    executor: &impl CommandExecutor,
) -> Result<Vec<CgroupLimit>> {
    let host = match ssh {
        Some(ssh) => PodmanHost::Ssh(ssh),
        None => PodmanHost::Local,
    };
    let output = executor.execute(&host.command(
        &["podman", "info", "--format", "{{json .Host}}"],
        "check Podman cgroup controllers",
    ))?;
    if output.status != 0 {
        bail!(
            "`podman info` failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let host: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("`podman info --format '{{json .Host}}'` returned unparsable output")?;
    Ok(unenforceable_limits(&host))
}

/// The parts of `podman info` the preflight checks.
#[derive(Debug)]
struct PodmanInfo {
    rootless: bool,
    service_is_remote: bool,
    /// `(container_id, host_id, size)` triples from `host.idMappings.uidmap`.
    uid_map: Vec<(u64, u64, u64)>,
    unenforceable_limits: Vec<CgroupLimit>,
}

impl PodmanInfo {
    fn covers_uid(&self, container_id: u64) -> bool {
        self.uid_map.iter().any(|(start, _, size)| {
            start
                .checked_add(*size)
                .is_some_and(|end| *start <= container_id && container_id < end)
        })
    }

    fn uid_map_description(&self) -> String {
        if self.uid_map.is_empty() {
            "no UID mappings".to_owned()
        } else {
            self.uid_map
                .iter()
                .map(|(container, host, size)| format!("{container}:{host}:{size}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// Read rootless mode and the UID map from `podman info --format json`.
///
/// `podman info` reports the server's view, so it works for local Rootless
/// Podman, a remote Podman machine, and an SSH host alike.
fn parse_podman_info(host: PodmanHost<'_>, stdout: &[u8]) -> Result<PodmanInfo> {
    let value: serde_json::Value = serde_json::from_slice(stdout).map_err(|error| {
        probe_failure(
            host,
            PodmanPostcondition::Info,
            format!(
                "{}: `podman info --format json` returned unparsable output: {error}.",
                host.failure()
            ),
        )
    })?;
    let rootless = value
        .pointer("/host/security/rootless")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| podman_info_failure(host, "it did not report rootless mode"))?;
    let service_is_remote = value
        .pointer("/host/serviceIsRemote")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let entries = value
        .pointer("/host/idMappings/uidmap")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| podman_info_failure(host, "it did not report a UID map"))?;
    let mut uid_map = Vec::with_capacity(entries.len());
    for entry in entries {
        let field = |name: &str| {
            entry
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    podman_info_failure(host, &format!("it reported a UID mapping without {name}"))
                })
        };
        uid_map.push((field("container_id")?, field("host_id")?, field("size")?));
    }
    Ok(PodmanInfo {
        rootless,
        service_is_remote,
        uid_map,
        unenforceable_limits: value
            .get("host")
            .map(unenforceable_limits)
            .unwrap_or_default(),
    })
}

fn podman_info_failure(host: PodmanHost<'_>, observation: &str) -> anyhow::Error {
    probe_failure(
        host,
        PodmanPostcondition::Info,
        format!(
            "{}: `podman info` could not be used: {observation}.",
            host.failure()
        ),
    )
}

/// Report either an explicitly unsafe systemd setting or an unavailable
/// durability check. Neither condition makes an otherwise usable target fail.
pub(super) fn ssh_podman_linger_warning(
    ssh: &SshTarget,
    output: Option<&CommandOutput>,
) -> Option<PodmanPreflightWarning> {
    let Some(output) = output else {
        return Some(linger_unavailable_warning(
            ssh,
            "the probe could not run: the preflight output did not include it".to_owned(),
        ));
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

pub(super) fn linger_unavailable_warning(
    ssh: &SshTarget,
    reason: String,
) -> PodmanPreflightWarning {
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

pub(super) fn execute_podman_probe(
    executor: &impl CommandExecutor,
    host: PodmanHost<'_>,
    probe: PodmanProbe,
) -> Result<CommandOutput> {
    let command = host.command(probe.args(), probe.purpose());
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            return Err(podman_probe_run_failure(
                host,
                probe,
                &probe_run_reason(&error),
            ));
        }
    };
    check_podman_probe_status(host, probe, output)
}

/// Why a probe command could not be started: a missing `podman` in plain
/// words, else the whole error chain, whose outer layer ("run podman for
/// check Podman version") says nothing on its own.
fn probe_run_reason(error: &anyhow::Error) -> String {
    if is_missing_program(error) {
        "`podman` is not installed or not on PATH".to_owned()
    } else {
        format!("{error:#}")
    }
}

/// Failure for a probe that could not be run at all.
pub(super) fn podman_probe_run_failure(
    host: PodmanHost<'_>,
    probe: PodmanProbe,
    reported: &str,
) -> anyhow::Error {
    match ssh_transport_failure(host, reported) {
        Some(message) => anyhow::anyhow!(message),
        None => probe_failure(
            host,
            probe.postcondition(),
            format!(
                "{}: could not run `{}` to check {}: {reported}.",
                host.failure(),
                probe.args().join(" "),
                probe.checks(),
            ),
        ),
    }
}

pub(super) fn check_podman_probe_status(
    host: PodmanHost<'_>,
    probe: PodmanProbe,
    output: CommandOutput,
) -> Result<CommandOutput> {
    // `ssh` reserves this status for its own connection failures; the Podman
    // probes never produce it. Reporting that case separately keeps an
    // unreachable host from being mistaken for a broken Podman installation.
    if output.status == SSH_TRANSPORT_EXIT_STATUS
        && let Some(message) =
            ssh_transport_failure(host, String::from_utf8_lossy(&output.stderr).trim())
    {
        bail!("{message}");
    }
    if output.status != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let postcondition = probe.postcondition();
        // Podman's message is usually a sentence without a final period.
        // End it before the remediation starts, so the fix does not read as
        // part of the command output.
        let reported = if stderr.ends_with(['.', '!', '?']) {
            stderr.to_owned()
        } else {
            format!("{stderr}.")
        };
        return Err(probe_failure(
            host,
            postcondition,
            format!(
                "{}: {} failed. Podman reported: {reported}",
                host.failure(),
                postcondition.statement(),
            ),
        ));
    }
    Ok(output)
}

pub(super) const LINGER_PROBE_KEY: &str = "linger";
pub(super) const PROBE_BLOCK_BEGIN: &str = "__mj_probe_begin__";
pub(super) const PROBE_BLOCK_END: &str = "__mj_probe_end__";
pub(super) const PROBE_STATUS_PREFIX: &str = "__mj_probe_status__";

/// Run every remote Podman probe in one SSH round trip.
///
/// Each probe's stdout is captured in a shell variable and reprinted between
/// framing markers, while its stderr is written straight to the saved stdout
/// inside its own frame, so multi-line and arbitrary output survives intact.
/// The version probe short-circuits the rest: without Podman the later probes
/// can only repeat its failure.
pub(super) const SSH_PODMAN_PREFLIGHT_SCRIPT: &str = r#"
exec 3>&1
probe() {
    name=$1
    shift
    printf '__mj_probe_begin__ %s.stderr\n' "$name"
    out=$("$@" 2>&3)
    status=$?
    printf '\n__mj_probe_end__\n'
    printf '__mj_probe_begin__ %s.stdout\n%s\n__mj_probe_end__\n' "$name" "$out"
    printf '__mj_probe_status__ %s %s\n' "$name" "$status"
    return "$status"
}
probe version podman --version || exit 0
probe info podman info --format json
probe linger sh -c 'loginctl show-user "$(id -u)" --property=Linger --value'
exit 0
"#;

pub(super) fn run_ssh_podman_probes(
    host: PodmanHost<'_>,
    executor: &impl CommandExecutor,
) -> Result<BTreeMap<String, CommandOutput>> {
    let command = host.command(
        &["sh", "-c", SSH_PODMAN_PREFLIGHT_SCRIPT],
        "check remote Podman prerequisites",
    );
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            return Err(podman_probe_run_failure(
                host,
                PodmanProbe::Version,
                &error.to_string(),
            ));
        }
    };
    if output.status == SSH_TRANSPORT_EXIT_STATUS
        && let Some(message) =
            ssh_transport_failure(host, String::from_utf8_lossy(&output.stderr).trim())
    {
        bail!("{message}");
    }
    let probes = parse_podman_probe_output(&output.stdout);
    if !probes.contains_key(PodmanProbe::Version.key()) {
        return Err(podman_probe_run_failure(
            host,
            PodmanProbe::Version,
            &format!(
                "the preflight probes returned unparsable output (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(probes)
}

/// Build what the batched remote script prints for the given probe results.
///
/// Tests across this crate stand in for a remote host, so they need the real
/// framing rather than a second, drifting description of it.
#[cfg(test)]
pub(crate) fn ssh_podman_probe_fixture(probes: &[(&str, i32, &str, &str)]) -> Vec<u8> {
    let mut output = String::new();
    for (name, status, stdout, stderr) in probes {
        output.push_str(&format!("{PROBE_BLOCK_BEGIN} {name}.stderr\n"));
        output.push_str(stderr);
        output.push_str(&format!("\n{PROBE_BLOCK_END}\n"));
        output.push_str(&format!("{PROBE_BLOCK_BEGIN} {name}.stdout\n"));
        output.push_str(stdout.strip_suffix('\n').unwrap_or(stdout));
        output.push_str(&format!("\n{PROBE_BLOCK_END}\n"));
        output.push_str(&format!("{PROBE_STATUS_PREFIX} {name} {status}\n"));
    }
    output.into_bytes()
}

/// Split the batched script's framed output into one result per probe.
///
/// A probe appears only once its status line has been read, so output truncated
/// mid-probe is reported as a missing probe rather than a partial result.
pub(super) fn parse_podman_probe_output(stdout: &[u8]) -> BTreeMap<String, CommandOutput> {
    let text = String::from_utf8_lossy(stdout);
    let mut blocks: BTreeMap<String, String> = BTreeMap::new();
    let mut probes = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some(name) = line.strip_prefix(PROBE_BLOCK_BEGIN).and_then(|rest| {
            rest.strip_prefix(' ')
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        }) {
            let mut body = Vec::new();
            let mut closed = false;
            for line in lines.by_ref() {
                if line == PROBE_BLOCK_END {
                    closed = true;
                    break;
                }
                body.push(line);
            }
            if closed {
                blocks.insert(name, body.join("\n"));
            }
            continue;
        }
        let Some(rest) = line.strip_prefix(PROBE_STATUS_PREFIX) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (Some(name), Some(status)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(status), Some(out), Some(err)) = (
            status.parse::<i32>(),
            blocks.remove(&format!("{name}.stdout")),
            blocks.remove(&format!("{name}.stderr")),
        ) else {
            continue;
        };
        probes.insert(
            name.to_owned(),
            CommandOutput {
                status,
                stdout: out.into_bytes(),
                stderr: err.into_bytes(),
            },
        );
    }
    probes
}

pub(super) fn ssh_transport_failure(host: PodmanHost<'_>, reported: &str) -> Option<String> {
    let PodmanHost::Ssh(ssh) = host else {
        return None;
    };
    let destination = &ssh.destination;
    Some(format!(
        "{}: SSH could not run the probes on {destination}. Verify that `ssh {destination}` succeeds noninteractively from this host. See {PODMAN_DOCUMENTATION_URL}. ssh reported: {reported}",
        host.failure()
    ))
}

pub(super) fn parse_podman_version(host: PodmanHost<'_>, stdout: &[u8]) -> Result<String> {
    let failure = host.failure();
    let version = String::from_utf8_lossy(stdout).trim().to_owned();
    let Some(candidate) = version
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
    else {
        return Err(probe_failure(
            host,
            PodmanPostcondition::Version,
            format!(
                "{failure}: {} returned {version:?}.",
                PodmanPostcondition::Version.statement()
            ),
        ));
    };
    let mut numbers = candidate.split('.').map(|part| part.parse::<u32>().ok());
    let Some(Some(major)) = numbers.next() else {
        return Err(probe_failure(
            host,
            PodmanPostcondition::Version,
            format!(
                "{failure}: {} returned {version:?}.",
                PodmanPostcondition::Version.statement()
            ),
        ));
    };
    // A version with no minor component, such as `podman version 4`, names
    // the earliest release of that series.
    let minor = numbers.next().flatten().unwrap_or(0);
    if (major, minor) < PODMAN_MINIMUM_VERSION {
        return Err(probe_failure(
            host,
            PodmanPostcondition::Version,
            format!(
                "{failure}: {} was not met (found {candidate}).",
                PodmanPostcondition::Version.statement()
            ),
        ));
    }
    Ok(candidate.to_owned())
}

/// Print the filesystem type of each argument, one per line. macOS `stat`
/// cannot name one, so there each `mount` entry, `<device> on <point>
/// (<type>, ...)`, is matched against the end of the path's `df` line: the
/// longest point it ends with holds the path, and among entries for that
/// point the last, the one on top, names the type. Both device names and
/// mount points can contain spaces and `%`, so no `df` column is parsed.
/// That match is a function rather than inline in `$(...)`: macOS `/bin/sh`
/// is bash 3.2, which ends a command substitution at the first `)`, so it
/// cannot parse a `case` pattern inside one.
pub(super) const FILESYSTEM_TYPES_SCRIPT: &str = r#"if [ "$(uname -s)" != Darwin ]; then
    exec stat -f -c %T -- "$@"
fi
mounted_type() {
    best= found=
    while IFS= read -r line; do
        head=${line% (*}
        [ "$head" != "$line" ] || continue
        case $head in *" on /"*) ;; *) continue ;; esac
        point=/${head#* on /}
        case $1 in *" $point") ;; *) continue ;; esac
        [ "${#point}" -ge "${#best}" ] || continue
        options=${line#"$head ("}
        best=$point found=${options%%[,)]*}
    done
    printf '%s' "$found"
}
mounts=$(mount) || exit 1
for path do
    usage=$(df -P -- "$path" | sed -n 2p)
    [ -n "$usage" ] || { printf 'df reported no filesystem for %s\n' "$path" >&2; exit 1; }
    filesystem=$(printf '%s\n' "$mounts" | mounted_type "$usage")
    [ -n "$filesystem" ] || { printf 'mount lists no filesystem holding %s\n' "$path" >&2; exit 1; }
    printf '%s\n' "$filesystem"
done"#;

/// Filesystem type of each directory, probed on the host that owns it: the
/// one that runs the container engine, or a bare session's own machine.
/// `ssh` names that host; `None` probes this machine.
///
/// The reply is positional, so the whole batch fails unless the host answered
/// for every directory in order.
pub fn probe_filesystem_types(
    ssh: Option<&SshTarget>,
    paths: &[PathBuf],
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        FILESYSTEM_TYPES_SCRIPT.to_owned(),
        "hel-filesystem".to_owned(),
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
