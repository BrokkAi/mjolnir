//! Nitro Sandbox sessions: one Firecracker microVM per session on an EC2 host
//! reached over SSH, driven through `mj-worker microvm` as root.
//!
//! The engine implements the subset of Podman's verbs, flags and output
//! formats that Mjolnir uses over SSH, so recovery, worker start, copies and
//! image refresh reuse the Podman call sites with a different command prefix
//! ([`engine_argv`]). Two things differ on purpose:
//!
//! * The engine owns compound operations. `run` creates the named volumes with
//!   the session's labels and removes everything it created when the launch
//!   fails, so there is no shell wrapper like `PODMAN_VOLUME_RUN_SCRIPT`.
//! * A sandbox is a VM. It has no host bind mounts, so additional directories
//!   and the shared build cache are dropped before this module is reached
//!   (with a notice), and this module refuses any that arrive.

use super::*;

/// The oldest host kernel the sandbox boot path was proven on (Amazon Linux
/// 2023 `6.18.51-120.162`).
const KERNEL_MINIMUM: (u32, u32, u32) = (6, 18, 51);

/// What `microvm info --format json` reports, as the controller reads it.
///
/// These key names are the controller's expectation of the engine's output;
/// the engine contract names the facts (versions, pool, slots, CPUs) and this
/// pins their spelling.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct NitroEngineInfo {
    pub engine_version: String,
    pub state_format: u32,
    pub firecracker_version: String,
    pub kernel_version: String,
    /// Memory the host reserved for sandboxes at boot, in MiB.
    pub memory_pool_total_mib: u64,
    pub memory_pool_free_mib: u64,
    pub slots_in_use: u32,
    pub slots_total: u32,
    pub cpus: u32,
}

/// A Nitro host that can run sandboxes, for `mj doctor` and the Targets pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NitroPreflight {
    pub info: NitroEngineInfo,
}

impl NitroPreflight {
    /// One line naming what doctor reports as ready.
    pub fn summary(&self) -> String {
        format!(
            "Firecracker {}, {} MiB sandbox memory pool ({} MiB free), kernel {}",
            self.info.firecracker_version,
            self.info.memory_pool_total_mib,
            self.info.memory_pool_free_mib,
            self.info.kernel_version
        )
    }
}

/// The sandbox memory pool as `(used, total)` bytes, from the engine's report.
pub(super) fn nitro_pool_memory(info: &str) -> Result<(u64, u64)> {
    const MIB: u64 = 1024 * 1024;
    let info: NitroEngineInfo =
        serde_json::from_str(info).context("decode the Nitro sandbox engine report")?;
    ensure!(
        info.memory_pool_free_mib <= info.memory_pool_total_mib,
        "the Nitro sandbox engine reports more free memory than its pool holds"
    );
    Ok((
        (info.memory_pool_total_mib - info.memory_pool_free_mib) * MIB,
        info.memory_pool_total_mib * MIB,
    ))
}

pub fn nitro_info_command(ssh: &SshTarget) -> CommandSpec {
    ssh_engine_command(ssh, MICROVM_ENGINE, ["info", "--format", "json"])
        .purpose("read the Nitro sandbox engine's host report")
        .stage(ProvisionStage::Provisioning)
}

/// Parse the engine's report, and say what is wrong with a host that cannot
/// run a sandbox.
pub fn parse_nitro_info(destination: &str, stdout: &[u8]) -> Result<NitroPreflight> {
    let info: NitroEngineInfo = serde_json::from_slice(stdout)
        .with_context(|| format!("decode the Nitro sandbox engine report from {destination}"))?;
    ensure!(
        !info.firecracker_version.trim().is_empty(),
        "Nitro sandbox preflight failed on {destination}: Firecracker is not installed. \
         Create the host with `mj ec2 create --runtime nitro`."
    );
    ensure!(
        info.memory_pool_total_mib > 0,
        "Nitro sandbox preflight failed on {destination}: no memory is reserved for sandboxes. \
         Boot the host with `nitro_enclaves.mempool=95%-1G` and restart it."
    );
    let kernel = kernel_release(&info.kernel_version).with_context(|| {
        format!(
            "Nitro sandbox preflight failed on {destination}: the host reports kernel {:?}",
            info.kernel_version
        )
    })?;
    ensure!(
        kernel >= KERNEL_MINIMUM,
        "Nitro sandbox preflight failed on {destination}: kernel {} is older than {}.{}.{}, \
         the oldest with Nitro Sandbox support.",
        info.kernel_version,
        KERNEL_MINIMUM.0,
        KERNEL_MINIMUM.1,
        KERNEL_MINIMUM.2,
    );
    Ok(NitroPreflight { info })
}

fn kernel_release(release: &str) -> Result<(u32, u32, u32)> {
    let mut parts = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(str::parse::<u32>);
    let mut next = || {
        parts
            .next()
            .context("kernel release has fewer than three numbers")?
            .context("kernel release number is out of range")
    };
    Ok((next()?, next()?, next()?))
}

/// The engine did not answer its report: it is not installed on the host yet,
/// or it cannot run there. Distinct from a report that says the host cannot
/// run sandboxes, because the fix differs.
#[derive(Debug)]
pub struct NitroEngineUnavailable(pub String);

impl std::fmt::Display for NitroEngineUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NitroEngineUnavailable {}

/// Check that the host can run sandboxes. The caller installs the engine
/// first, because the engine is the worker binary and nothing here can run
/// without it.
pub fn verify_ssh_nitro(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<NitroPreflight> {
    validate_ssh(ssh)?;
    let command = nitro_info_command(ssh);
    let output = executor.execute(&command).with_context(|| {
        format!(
            "Nitro sandbox preflight on {} could not run `{}`",
            ssh.destination, command.purpose
        )
    })?;
    if output.status != 0 {
        return Err(NitroEngineUnavailable(format!(
            "Nitro sandbox preflight failed on {}: `{}` exited with status {}: {}",
            ssh.destination,
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    parse_nitro_info(&ssh.destination, &output.stdout)
}

/// The volumes the engine creates for a sandbox, by name.
pub(crate) fn nitro_workspace_volume(container: &str) -> String {
    format!("{container}-workspace")
}

/// Run arguments for the engine: the shape `container_run_args` gives Podman,
/// minus what a VM has no use for (capabilities, security options, a pids
/// limit, host mounts, user namespaces) and with options spelled the way the
/// engine contract lists them.
pub(super) fn nitro_run_args(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    workspace_root: &str,
) -> Result<Vec<String>> {
    ensure!(
        additional_mounts.is_empty(),
        "Nitro sandboxes cannot attach host directories"
    );
    let mut args = vec!["run".to_owned()];
    args.push(format!("--pull={}", docker_pull_policy(template)));
    // The guest agent is PID 1 and reaps; stopping always kills.
    args.push("--init".to_owned());
    args.extend(["--stop-signal".to_owned(), "KILL".to_owned()]);
    args.extend(["--detach".to_owned(), "--name".to_owned(), name.to_owned()]);
    args.extend(managed_resource_identity_args(
        ManagedResourceKind::Container,
        session_id,
    ));
    // The size, platform and environment the shared template builder wrote in
    // Podman's `--flag=value` spelling. A VM runs the host's architecture, so
    // the platform belongs to the image pull, not to `run`.
    let mut extra = template.extra_run_args.iter();
    while let Some(argument) = extra.next() {
        match argument.split_once('=') {
            Some(("--platform", _)) => {}
            Some((flag @ ("--cpus" | "--memory"), value)) => {
                args.extend([flag.to_owned(), value.to_owned()]);
            }
            _ if argument == "--env" => {
                args.push(argument.clone());
                args.push(
                    extra
                        .next()
                        .context("--env has no value in the run arguments")?
                        .clone(),
                );
            }
            _ => bail!("Nitro sandboxes do not support the run argument {argument:?}"),
        }
    }
    // As on Podman, the session runs as root with the image user's home.
    args.extend([
        "--user".to_owned(),
        "0:0".to_owned(),
        "--env".to_owned(),
        "HOME=/home/hel".to_owned(),
    ]);
    args.extend([
        "--volume".to_owned(),
        format!("{}:/tmp:rw", container_temporary_volume_name(name)),
        "--volume".to_owned(),
        format!("{}:{workspace_root}:rw", nitro_workspace_volume(name)),
    ]);
    args.push(template.image.clone());
    args.extend([
        "sh".to_owned(),
        "-c".to_owned(),
        KEEPALIVE_SCRIPT.to_owned(),
    ]);
    Ok(args)
}

/// Create the sandbox, then make `/tmp` the world-writable sticky directory
/// the Podman launch script makes it (the fresh volume is root-owned 0755).
pub(super) fn nitro_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    ssh: &SshTarget,
    workspace_root: &str,
) -> Result<Vec<CommandSpec>> {
    let run = ssh_engine_command(
        ssh,
        MICROVM_ENGINE,
        nitro_run_args(
            template,
            name,
            session_id,
            additional_mounts,
            workspace_root,
        )?,
    )
    .purpose("start remote Nitro sandbox")
    .stage(ProvisionStage::Provisioning)
    .creates_target();
    let tmp = ssh_engine_command(
        ssh,
        MICROVM_ENGINE,
        [
            "exec",
            name,
            "sh",
            "-c",
            "set -eu; chown 0:0 /tmp; chmod 1777 /tmp",
        ],
    )
    .purpose("prepare the Nitro sandbox's /tmp")
    .stage(ProvisionStage::Provisioning);
    Ok(vec![run, tmp])
}

/// A script that runs the engine as `mj_microvm`. It is the same command as
/// [`engine_argv`], spelled for a script the login shell hands to `sh -c`.
fn engine_script(body: &str) -> String {
    format!(
        "set -eu\nmj_microvm() {{ {} \"$@\"; }}\n{body}",
        engine_shell_words(MICROVM_ENGINE)
    )
}

fn nitro_script_command(
    ssh: &SshTarget,
    body: &str,
    arguments: impl IntoIterator<Item = String>,
) -> CommandSpec {
    let mut words = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        engine_script(body),
        "mj-nitro".to_owned(),
    ];
    words.extend(arguments);
    ssh_command_owned(ssh, words)
}

const NITRO_STOP_SCRIPT: &str = r#"container=$1
mj_microvm stop --time 0 --ignore "$container" >/dev/null
"#;

const NITRO_QUIESCE_SCRIPT: &str = r#"container=$1
mj_microvm stop --time 0 --ignore "$container" >/dev/null
# `container exists` exits 0 for present, 1 for absent, and anything else for an
# engine or storage failure.
if mj_microvm container exists "$container"; then
    running=$(mj_microvm container inspect --format '{{.State.Running}}' "$container")
    [ "$running" = false ] || { echo 'Nitro session sandbox is still running' >&2; exit 1; }
else
    status=$?
    [ "$status" = 1 ] || exit "$status"
fi
"#;

const NITRO_REMOVE_SCRIPT: &str = r#"container=$1
mj_microvm rm --force --ignore "$container" >/dev/null
"#;

const NITRO_REMOVE_VOLUMES_SCRIPT: &str = r#"for volume in "$@"; do
    mj_microvm volume rm --force "$volume" >/dev/null
done
"#;

// Like `podman container exists`, `exists` exits 0 for present, 1 for absent,
// and anything else for an engine failure.
const NITRO_ABSENT_SCRIPT: &str = r#"for resource in "container $1" "volume $2" "volume $3"; do
    if mj_microvm ${resource% *} exists "${resource#* }"; then
        exit 1
    else
        status=$?
        [ "$status" = 1 ] || exit 2
    fi
done
"#;

/// Stop a sandbox without deleting its disks, and prove it is not running.
pub(super) fn nitro_quiesce_command(ssh: &SshTarget, container: &str) -> CommandSpec {
    nitro_script_command(ssh, NITRO_QUIESCE_SCRIPT, [container.to_owned()])
        .purpose("stop exact Nitro session sandbox without removing storage")
        .stage(ProvisionStage::StoppingTarget)
}

/// Stop, then remove the sandbox, then its volumes. The order matters: the
/// engine kills the Firecracker process before it deletes a disk, and a
/// volume goes only after the sandbox that mounts it is gone.
pub(super) fn nitro_destroy_commands(ssh: &SshTarget, container: &str) -> Vec<CommandSpec> {
    vec![
        nitro_script_command(
            ssh,
            &format!("{NITRO_STOP_SCRIPT}{NITRO_REMOVE_SCRIPT}"),
            [container.to_owned()],
        )
        .purpose("remove exact stopped Nitro session sandbox")
        .stage(ProvisionStage::RemovingContainer),
        nitro_script_command(
            ssh,
            NITRO_REMOVE_VOLUMES_SCRIPT,
            [
                nitro_workspace_volume(container),
                container_temporary_volume_name(container),
            ],
        )
        .purpose("remove exact Nitro session volumes")
        .stage(ProvisionStage::RemovingStorage),
    ]
}

/// The same removal as one command, for a disposable smoke test that runs
/// its cleanup as a single step.
pub(super) fn nitro_destroy_in_one_command(ssh: &SshTarget, container: &str) -> CommandSpec {
    nitro_script_command(
        ssh,
        &format!("{NITRO_STOP_SCRIPT}{NITRO_REMOVE_SCRIPT}shift\n{NITRO_REMOVE_VOLUMES_SCRIPT}"),
        [
            container.to_owned(),
            nitro_workspace_volume(container),
            container_temporary_volume_name(container),
        ],
    )
    .purpose("remove disposable setup sandbox")
}

/// Exit 0 when nothing of the session remains, 1 when something does, and
/// anything else when the engine could not answer.
pub(super) fn nitro_absence_command(ssh: &SshTarget, container: &str) -> CommandSpec {
    nitro_script_command(
        ssh,
        NITRO_ABSENT_SCRIPT,
        [
            container.to_owned(),
            nitro_workspace_volume(container),
            container_temporary_volume_name(container),
        ],
    )
    .purpose("confirm exact Nitro session resources are absent")
}

/// The sandbox's mounts, from the engine's full `container inspect` JSON (a
/// one-element array), so the controller needs no `--format` template.
pub(super) fn nitro_inspect_command(ssh: &SshTarget, container: &str) -> CommandSpec {
    ssh_engine_command(ssh, MICROVM_ENGINE, ["container", "inspect", container])
        .purpose("inspect session container storage")
}
