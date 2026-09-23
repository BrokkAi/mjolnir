use super::*;

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
    let output = executor
        .execute(&command)
        .context("Docker preflight failed: run `docker info` as the user running Mjolnir")?;
    ensure!(
        output.status == 0,
        "Docker preflight failed: `docker version` exited with status {}: {}. Run `docker info` as the user running Mjolnir. See {DOCKER_DOCUMENTATION_URL}.",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
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
pub(crate) enum PodmanProbe {
    Version,
    Rootless,
    UidMap,
}

/// A rootless Podman postcondition that was not met, carrying which one.
///
/// `mj doctor` needs the probe, not its wording, to name the fix. Carrying the
/// probe on the error means the diagnosis never depends on matching message
/// text that this repository itself produces.
#[derive(Debug)]
pub(crate) struct PodmanProbeFailure {
    probe: PodmanProbe,
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

/// The probe whose postcondition failed, when the error came from one.
pub(crate) fn failed_podman_probe(error: &anyhow::Error) -> Option<PodmanProbe> {
    error
        .downcast_ref::<PodmanProbeFailure>()
        .map(|failure| failure.probe)
}

/// What a failed probe observed, without the fix, so a report that prints
/// the fix separately does not repeat it.
pub(crate) fn podman_probe_observation(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<PodmanProbeFailure>()
        .map(|failure| failure.observation.as_str())
}

/// Fail a probe with what it observed, followed by that probe's fix and the
/// published guide, keeping the probe machine-readable.
fn probe_failure(host: PodmanHost<'_>, probe: PodmanProbe, observation: String) -> anyhow::Error {
    let message = format!(
        "{observation} {}{} See {PODMAN_DOCUMENTATION_URL}.",
        host.remediation_scope(),
        probe.remediation()
    );
    anyhow::Error::new(PodmanProbeFailure {
        probe,
        observation,
        message,
    })
}

impl PodmanProbe {
    /// Name of this probe in the batched remote script's output.
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Rootless => "rootless",
            Self::UidMap => "uid_map",
        }
    }

    pub(super) fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["podman", "--version"],
            Self::Rootless => &["podman", "info", "--format", "{{.Host.Security.Rootless}}"],
            Self::UidMap => &["podman", "unshare", "cat", "/proc/self/uid_map"],
        }
    }

    pub(super) fn purpose(self) -> &'static str {
        match self {
            Self::Version => "check Podman version",
            Self::Rootless => "check rootless Podman mode",
            Self::UidMap => "check rootless Podman UID map",
        }
    }

    pub(super) fn postcondition(self) -> &'static str {
        match self {
            Self::Version => "Postcondition `podman --version` succeeds with Podman 4.3.0 or newer",
            Self::Rootless => {
                "Postcondition `podman info --format '{{.Host.Security.Rootless}}'` prints `true`"
            }
            Self::UidMap => {
                "Postcondition `podman unshare cat /proc/self/uid_map` maps container UIDs 0 and 1"
            }
        }
    }

    pub(crate) fn remediation(self) -> &'static str {
        match self {
            Self::Version => {
                "Install or upgrade Podman: Debian/Ubuntu `sudo apt update && sudo apt install -y podman uidmap`; Fedora `sudo dnf install -y podman shadow-utils`."
            }
            Self::Rootless => {
                "Run Mjolnir as the ordinary user without `sudo`; if a remote Podman connection is configured, unset `CONTAINER_HOST` or select the rootless local connection."
            }
            Self::UidMap => {
                "Install UID-map helpers (`sudo apt install -y uidmap` on Debian/Ubuntu or `sudo dnf install -y shadow-utils` on Fedora), then add subordinate ranges with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \"$USER\"` and start a fresh login session."
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

    let rootless = probe_output(PodmanProbe::Rootless)?;
    let rootless_output = String::from_utf8_lossy(&rootless.stdout);
    if rootless_output.trim() != "true" {
        return Err(probe_failure(
            host,
            PodmanProbe::Rootless,
            format!(
                "{}: {} returned {:?}.",
                host.failure(),
                PodmanProbe::Rootless.postcondition(),
                rootless_output.trim(),
            ),
        ));
    }

    let uid_map = probe_output(PodmanProbe::UidMap)?;
    if !valid_rootless_uid_map(&uid_map.stdout) {
        return Err(probe_failure(
            host,
            PodmanProbe::UidMap,
            format!(
                "{}: {} was not met.",
                host.failure(),
                PodmanProbe::UidMap.postcondition(),
            ),
        ));
    }

    Ok(PodmanPreflight {
        version,
        warnings: Vec::new(),
    })
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
            return Err(podman_probe_run_failure(host, probe, &error.to_string()));
        }
    };
    check_podman_probe_status(host, probe, output)
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
            probe,
            format!(
                "{}: {} could not be checked: {reported}.",
                host.failure(),
                probe.postcondition(),
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
        return Err(probe_failure(
            host,
            probe,
            format!(
                "{}: {} failed. Podman reported: {}",
                host.failure(),
                probe.postcondition(),
                String::from_utf8_lossy(&output.stderr).trim()
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
probe rootless podman info --format '{{.Host.Security.Rootless}}'
probe uid_map podman unshare cat /proc/self/uid_map
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
            PodmanProbe::Version,
            format!(
                "{failure}: {} returned {version:?}.",
                PodmanProbe::Version.postcondition()
            ),
        ));
    };
    let mut numbers = candidate.split('.').map(|part| part.parse::<u32>().ok());
    let Some(Some(major)) = numbers.next() else {
        return Err(probe_failure(
            host,
            PodmanProbe::Version,
            format!(
                "{failure}: {} returned {version:?}.",
                PodmanProbe::Version.postcondition()
            ),
        ));
    };
    // A version with no minor component, such as `podman version 4`, names
    // the earliest release of that series.
    let minor = numbers.next().flatten().unwrap_or(0);
    if (major, minor) < PODMAN_MINIMUM_VERSION {
        return Err(probe_failure(
            host,
            PodmanProbe::Version,
            format!(
                "{failure}: {} was not met (found {candidate}).",
                PodmanProbe::Version.postcondition()
            ),
        ));
    }
    Ok(candidate.to_owned())
}

pub(super) fn valid_rootless_uid_map(stdout: &[u8]) -> bool {
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

/// The uid and gid of the container image's configured user, read on the host
/// that runs the container engine. `ssh` names that host for a remote Podman
/// target; `None` reads it on this machine.
///
/// The image is asked rather than assumed, because `--userns=keep-id` has to
/// name the ids the container will actually run as. The probe carries the
/// template's own pull policy, so it reads the same image the launch will run
/// and never pulls one the launch would not. The entrypoint is cleared so the
/// answer comes from an image whose entrypoint is a long-running program.
pub fn probe_image_user(
    ssh: Option<&SshTarget>,
    template: &ContainerTemplate,
    executor: &impl CommandExecutor,
) -> Result<ImageUser> {
    let host = match ssh {
        Some(ssh) => PodmanHost::Ssh(ssh),
        None => PodmanHost::Local,
    };
    let mut args = vec!["podman".to_owned(), "run".to_owned(), "--rm".to_owned()];
    args.extend(podman_pull_argument(template));
    args.extend([
        "--entrypoint".to_owned(),
        String::new(),
        template.image.clone(),
        "sh".to_owned(),
        "-c".to_owned(),
        "id -u; id -g".to_owned(),
    ]);
    let output = executor.execute(&host.command_owned(args, "read the container image user"))?;
    if output.status != 0 {
        bail!(
            "image user probe failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ids = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let mut next = |field: &str| -> Result<u32> {
        ids.next()
            .with_context(|| format!("image user probe reported no {field}"))?
            .parse()
            .with_context(|| format!("image user probe reported an unreadable {field}"))
    };
    let uid = next("uid")?;
    let gid = next("gid")?;
    Ok(ImageUser { uid, gid })
}

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
