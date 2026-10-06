use super::*;

pub(crate) fn container_temporary_volume_name(container: &str) -> String {
    format!("{container}-tmp")
}

pub(crate) fn has_managed_temporary_volume(
    locator: &TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let (engine, container, ssh) = match locator {
        TargetLocator::LocalPodman { container_id, .. } => ("podman", container_id, None),
        TargetLocator::LocalDocker { container_id, .. } => ("docker", container_id, None),
        TargetLocator::SshPodman {
            container_id, ssh, ..
        } => ("podman", container_id, Some(ssh)),
        TargetLocator::SshDocker {
            container_id, ssh, ..
        } => ("docker", container_id, Some(ssh)),
        _ => return Ok(false),
    };
    let command = CommandSpec::new(
        engine,
        [
            "container",
            "inspect",
            "--format",
            "{{range .Mounts}}{{if eq .Destination \"/tmp\"}}{{.Name}}{{end}}{{end}}",
            container,
        ],
    )
    .purpose("inspect session temporary storage");
    let command = match ssh {
        Some(ssh) => command_over_ssh(command, ssh),
        None => command,
    };
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "inspect session temporary storage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)
        .context("decode session temporary volume")?
        .trim()
        == container_temporary_volume_name(container))
}

// Podman exec leaves `--user` unset, so each container's stored configured user
// controls initialization as well as later worker commands.
// Shared by all Podman workspace policies and Docker attachment launches.
// Storage is released only after its owning container has been removed.
pub(super) const TEMPORARY_VOLUME_FUNCTIONS: &str = r#"
prepare_temporary_volume() {
    [ -n "$temporary_volume" ] || return 0
    if ! "$engine" volume inspect "$temporary_volume" >/dev/null 2>&1; then
        "$engine" info >/dev/null
        "$engine" volume create --driver local --label "dev.mj.managed=true" --label "dev.mj.session=$session" "$temporary_volume" >/dev/null
    fi
    identity=$("$engine" volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$temporary_volume")
    [ "$identity" = "true|$session" ] || {
        echo "refusing foreign $engine temporary volume $temporary_volume" >&2
        return 1
    }
}
remove_temporary_volume() {
    [ -n "$temporary_volume" ] || return 0
    if identity=$("$engine" volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$temporary_volume" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing to remove foreign $engine temporary volume $temporary_volume" >&2
            return 1
        }
        "$engine" volume rm --force "$temporary_volume" >/dev/null
    else
        "$engine" info >/dev/null
    fi
}
remove_failed_container() {
    if identity=$("$engine" container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' "$container" 2>/dev/null); then
        [ "$identity" = "true|$session" ] || {
            echo "refusing to remove foreign $engine container $container" >&2
            return 1
        }
        "$engine" rm --force "$container" >/dev/null
    else
        "$engine" info >/dev/null
    fi
}
start_container() {
    "$@"
    if [ -n "$temporary_volume" ]; then
        if [ -n "$exec_user" ]; then
            "$engine" exec --user "$exec_user" "$container" sh -c 'set -eu; chown 0:0 /tmp; chmod 1777 /tmp'
        else
            "$engine" exec "$container" sh -c 'set -eu; chown 0:0 /tmp; chmod 1777 /tmp'
        fi
    fi
}
"#;

fn container_launch_script(engine: &str, script: &str) -> String {
    let exec_user = if engine == "docker" { "0" } else { "" };
    format!(
        "set -eu\nengine={engine}\nexec_user={exec_user}\n{TEMPORARY_VOLUME_FUNCTIONS}\n{script}"
    )
}

fn temporary_volume_argument(name: &str, mounts: &[AdditionalMount]) -> String {
    if mounts
        .iter()
        .any(|mount| mount.destination == Path::new("/tmp"))
    {
        String::new()
    } else {
        container_temporary_volume_name(name)
    }
}

pub(super) fn container_run(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    workspace_root: &str,
) -> Result<CommandSpec> {
    Ok(CommandSpec::new(
        engine,
        container_run_args(
            engine,
            template,
            name,
            session_id,
            additional_mounts,
            None,
            workspace_root,
        )?,
    )
    .purpose("start session container")
    .stage(ProvisionStage::Provisioning)
    .creates_target())
}

pub(super) const PODMAN_VOLUME_RUN_SCRIPT: &str = r#"set -eu
session=$1
container=$2
volume=$3
temporary_volume=$4
shift 4
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if remove_failed_container; then
            remove_temporary_volume || echo "Podman temporary storage cleanup failed for session $session" >&2
            if [ -n "$volume" ] && identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null) && [ "$identity" = "true|$session" ]; then
                podman volume rm --force "$volume" >/dev/null || echo "Podman workspace cleanup failed for session $session" >&2
            fi
        else
            echo "Podman container cleanup failed; retained storage for session $session" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
prepare_temporary_volume
if [ -z "$volume" ]; then
    start_container "$@"
    exit 0
elif identity=$(podman volume inspect --format '{{index .Labels "dev.mj.managed"}}|{{index .Labels "dev.mj.session"}}' "$volume" 2>/dev/null); then
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
start_container "$@"
"#;

pub(super) fn podman_host_helper_run_script(helper: &[String]) -> String {
    let helper = join_remote_command(helper);
    format!(
        r#"set -eu
session=$1
container=$2
resource=$3
temporary_volume=$4
shift 4
cleanup() {{
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        if remove_failed_container; then
            remove_temporary_volume || echo "Podman temporary storage cleanup failed for session $session" >&2
            {helper} destroy "$resource" >/dev/null || echo "Podman workspace cleanup failed for session $session" >&2
        else
            echo "Podman container cleanup failed; retained storage for session $session" >&2
        fi
    fi
    exit "$status"
}}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
prepare_temporary_volume
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
start_container "$@"
"#
    )
}

pub(super) fn podman_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    ssh: Option<&SshTarget>,
    workspace_root: &str,
) -> Result<CommandSpec> {
    let workspace = podman_workspace_locator_named(template, name)?;
    let run_args = container_run_args(
        "podman",
        template,
        name,
        session_id,
        additional_mounts,
        Some(&workspace),
        workspace_root,
    )?;
    let mut wrapped = match &workspace {
        PodmanWorkspaceLocator::ContainerLayer | PodmanWorkspaceLocator::Volume { .. } => {
            let volume = match &workspace {
                PodmanWorkspaceLocator::Volume { name } => name.clone(),
                _ => String::new(),
            };
            let mut command = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                container_launch_script("podman", PODMAN_VOLUME_RUN_SCRIPT),
                "mj-podman-run".to_owned(),
                session_id.to_owned(),
                name.to_owned(),
                volume,
                temporary_volume_argument(name, additional_mounts),
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
                container_launch_script("podman", &podman_host_helper_run_script(helper)),
                "mj-podman-run".to_owned(),
                session_id.to_owned(),
                name.to_owned(),
                resource.clone(),
                temporary_volume_argument(name, additional_mounts),
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

pub(super) const DOCKER_OVERLAY_RUN_SCRIPT: &str = r#"set -eu
session=$1
container=$2
image=$3
pull=$4
temporary_volume=$5
shift 5
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
        remove_failed_container || released=false
        if [ "$released" = true ]; then
            remove_temporary_volume || released=false
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
prepare_temporary_volume
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
start_container "$@"
"#;

pub(super) fn docker_overlay_volume_name(container_name: &str, ordinal: usize) -> String {
    format!("{container_name}-mount-{ordinal}")
}

pub(super) fn docker_pull_policy(template: &ContainerTemplate) -> &'static str {
    match template.pull_policy.at_launch(&template.image) {
        ImagePullPolicy::Auto => unreachable!("at_launch resolves auto"),
        ImagePullPolicy::Always | ImagePullPolicy::Newer => "always",
        ImagePullPolicy::Missing => "missing",
        ImagePullPolicy::Never => "never",
    }
}

pub(super) fn docker_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    workspace_root: &str,
) -> Result<CommandSpec> {
    let run_args = container_run_args(
        "docker",
        template,
        name,
        session_id,
        additional_mounts,
        None,
        workspace_root,
    )?;
    let overlaid = additional_mounts
        .iter()
        .enumerate()
        .filter(|(_, mount)| mount.access == MountAccess::Cow)
        .collect::<Vec<_>>();
    let mut args = vec![
        "-c".to_owned(),
        container_launch_script("docker", DOCKER_OVERLAY_RUN_SCRIPT),
        "hel-docker-run".to_owned(),
        session_id.to_owned(),
        name.to_owned(),
        template.image.clone(),
        docker_pull_policy(template).to_owned(),
        temporary_volume_argument(name, additional_mounts),
    ];
    for (ordinal, mount) in overlaid {
        args.extend([
            ordinal.to_string(),
            mount.source.to_string_lossy().into_owned(),
            docker_overlay_volume_name(name, ordinal),
        ]);
    }
    args.extend(["--".to_owned(), "docker".to_owned()]);
    args.extend(run_args);
    let purpose = if additional_mounts
        .iter()
        .any(|mount| mount.access == MountAccess::Cow)
    {
        "start Docker session container with isolated attachments"
    } else {
        "start session container"
    };
    Ok(CommandSpec::new("sh", args)
        .purpose(purpose)
        .stage(ProvisionStage::Provisioning)
        .creates_target())
}

/// Podman's `--pull` flag for this template, or `None` when the resolved
/// policy is Podman's own default. Every `podman run` Hel issues for a
/// template uses this one answer.
pub(super) fn podman_pull_argument(template: &ContainerTemplate) -> Option<String> {
    let pull_policy = template.pull_policy.at_launch(&template.image);
    (pull_policy != ImagePullPolicy::Missing)
        .then(|| format!("--pull={}", pull_policy.podman_value()))
}

/// Processes and threads a Mjolnir session container may hold.
///
/// Podman and Docker default to 2048, which was never a choice Mjolnir made
/// and is far too small for what it puts in one container. Measured on a
/// local Podman target: one session's tree is about 700 threads, because its
/// worker, its ACP supervisor and the harness's own Node process each hold a
/// thread pool sized to the host's CPU count. Two sessions in a container had
/// already taken 1521 of the 2048, and a parent may hold six sub-agent
/// children by default, which needs roughly 4,900.
///
/// Past the limit every `fork` fails with EAGAIN, which reached users as
/// "sh: 1: Cannot fork", as "Resource temporarily unavailable" launching the
/// ACP bridge, and as sub-agent starts that simply never reported a harness
/// inside their 300-second wait. A limit is not a reservation, so this costs
/// nothing until the container actually needs the room.
const CONTAINER_PIDS_LIMIT: u32 = 8192;

/// The pids-limit flag for a Mjolnir-created container, unless the template
/// already sets one: an operator who chose a number keeps it.
fn container_pids_limit_args(engine: &str, template: &ContainerTemplate) -> Vec<String> {
    if !matches!(engine, "podman" | "docker") {
        return Vec::new();
    }
    if template
        .extra_run_args
        .iter()
        .any(|argument| argument.starts_with("--pids-limit"))
    {
        return Vec::new();
    }
    vec![format!("--pids-limit={CONTAINER_PIDS_LIMIT}")]
}

// Every argument is an independent input the engine needs: the template, the
// resource identity, the mounts, and the workspace the session runs in.
#[allow(clippy::too_many_arguments)]
pub(super) fn container_run_args(
    engine: &str,
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    podman_workspace: Option<&PodmanWorkspaceLocator>,
    workspace_root: &str,
) -> Result<Vec<String>> {
    validate_additional_mounts(additional_mounts)?;
    let mut args = vec!["run".to_owned()];
    if engine == "podman" {
        args.extend(podman_pull_argument(template));
        // PID 1 is `sleep infinity`, which reaps nothing, so every exec that
        // outlives its parent leaves a zombie behind. Apple's `container`
        // engine is left alone: its support for the flag is unverified.
        args.push("--init".to_owned());
    } else if engine == "docker" {
        let pull = docker_pull_policy(template);
        args.push(format!("--pull={pull}"));
        args.push("--init".to_owned());
    }
    args.extend(container_pids_limit_args(engine, template));
    args.extend(["--detach".to_owned(), "--name".to_owned(), name.to_owned()]);
    args.extend(managed_resource_identity_args(
        ManagedResourceKind::Container,
        session_id,
    ));
    args.extend(template.extra_run_args.clone());
    if engine == "podman" {
        // Rootless Podman maps container uid 0 to the host user. Keeping the
        // image user's uid mapping would force Podman to chown every image
        // layer on storage drivers that cannot shift ids. Keep these after
        // custom args so the session identity and HOME remain deliberate.
        args.extend([
            "--user".to_owned(),
            "0:0".to_owned(),
            "--env".to_owned(),
            "HOME=/home/hel".to_owned(),
        ]);
    }
    let temporary_volume = temporary_volume_argument(name, additional_mounts);
    if !temporary_volume.is_empty() {
        match engine {
            // No `nocopy`: Podman passes it to the OCI runtime as bind-mount
            // data, which runc rejects. The volume is new, so copy-up is
            // harmless, and `start_container` resets /tmp ownership and mode.
            "podman" => args.extend(["--volume".to_owned(), format!("{temporary_volume}:/tmp:rw")]),
            "docker" => args.extend([
                "--mount".to_owned(),
                format!("type=volume,source={temporary_volume},target=/tmp,volume-nocopy"),
            ]),
            _ => {}
        }
    }
    if engine == "podman" {
        match podman_workspace.unwrap_or(&PodmanWorkspaceLocator::ContainerLayer) {
            PodmanWorkspaceLocator::ContainerLayer => {}
            // `:U` chowns the volume to container root, which maps to the host
            // user in rootless Podman's default namespace.
            PodmanWorkspaceLocator::Volume { name } => args.extend([
                "--volume".to_owned(),
                format!("{name}:{workspace_root}:rw,U"),
            ]),
            // The host directory belongs to the host user, which rootless
            // Podman's default namespace maps to container uid 0.
            PodmanWorkspaceLocator::HostPath { path, .. } => {
                args.extend(["--volume".to_owned(), format!("{path}:{workspace_root}:rw")])
            }
        }
    }
    for (ordinal, mount) in additional_mounts.iter().enumerate() {
        let source = mount.source.to_string_lossy();
        let destination = mount.destination.to_string_lossy();
        match engine {
            "podman" => {
                let mode = match mount.access {
                    MountAccess::Ro => "ro",
                    MountAccess::Cow => "O",
                    MountAccess::Rw => "rw",
                };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}:{mode}"),
                ]);
            }
            "docker" => {
                let source = if mount.access == MountAccess::Cow {
                    docker_overlay_volume_name(name, ordinal)
                } else {
                    source.into_owned()
                };
                let suffix = if mount.access == MountAccess::Ro {
                    ":ro"
                } else {
                    ""
                };
                args.extend([
                    "--volume".to_owned(),
                    format!("{source}:{destination}{suffix}"),
                ]);
            }
            // Apple's runtime has no copy-on-write overlay, so those mounts
            // stay read-only rather than silently writing through.
            "container" => args.extend([
                "--mount".to_owned(),
                if mount.access == MountAccess::Rw {
                    format!("type=bind,source={source},target={destination}")
                } else {
                    format!("type=bind,source={source},target={destination},readonly")
                },
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

pub(super) fn apple_image_prepare_commands(template: &ContainerTemplate) -> Vec<CommandSpec> {
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
pub(super) fn command_over_ssh(mut command: CommandSpec, ssh: &SshTarget) -> CommandSpec {
    let remote = std::iter::once(command.program)
        .chain(command.args)
        .collect();
    let wrapped = ssh_command_owned(ssh, remote);
    command.program = wrapped.program;
    command.args = wrapped.args;
    // The wrapped command now opens an SSH session, so it is admitted and
    // placed on a shared connection like any other.
    command.ssh_destination = wrapped.ssh_destination;
    command.ssh_session = wrapped.ssh_session;
    command
}

pub(super) fn at_boundary(boundary: ExecutionBoundary<'_>, args: Vec<String>) -> CommandSpec {
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
mod pids_limit_tests {
    use super::*;

    /// A session container holds a parent and its sub-agent children, each with a
    /// worker, an ACP supervisor and a harness process that all size their thread
    /// pools to the host. The engine default of 2048 fits about two sessions, so
    /// Mjolnir asks for room for the concurrency it allows (#1065).
    // Hard-won: #1065: Concurrent child launches must have more than the engine’s default process slots.
    #[test]
    fn a_session_container_asks_for_more_processes_than_the_engine_default() {
        for engine in ["podman", "docker"] {
            let args = container_run_args(
                engine,
                &pids_limit_template(Vec::new()),
                "mj-test",
                "0123456789abcdef0123456789abcdef",
                &[],
                None,
                "/workspace",
            )
            .unwrap();
            assert!(
                args.iter().any(|argument| argument == "--pids-limit=8192"),
                "{engine} run args must raise the process limit: {args:?}"
            );
        }
    }

    /// An operator who chose a number keeps it.
    #[test]
    fn a_configured_process_limit_is_left_alone() {
        let args = container_run_args(
            "podman",
            &pids_limit_template(vec!["--pids-limit=256".to_owned()]),
            "mj-test",
            "0123456789abcdef0123456789abcdef",
            &[],
            None,
            "/workspace",
        )
        .unwrap();

        assert!(args.iter().any(|argument| argument == "--pids-limit=256"));
        assert_eq!(
            args.iter()
                .filter(|argument| argument.starts_with("--pids-limit"))
                .count(),
            1,
            "{args:?}"
        );
    }

    fn pids_limit_template(extra_run_args: Vec<String>) -> ContainerTemplate {
        ContainerTemplate {
            build_cache: None,
            image: "example.invalid/agent:latest".into(),
            pull_policy: Default::default(),
            workspace_storage: Default::default(),
            extra_run_args,
        }
    }
}
