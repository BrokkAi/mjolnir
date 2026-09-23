use super::*;

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

pub(super) fn podman_host_helper_run_script(helper: &[String]) -> String {
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

pub(super) fn podman_container_run(
    template: &ContainerTemplate,
    name: &str,
    session_id: &str,
    additional_mounts: &[AdditionalMount],
    image_user: Option<ImageUser>,
    ssh: Option<&SshTarget>,
    workspace_root: &str,
) -> Result<CommandSpec> {
    let workspace = podman_workspace_locator(template, session_id)?;
    let run_args = container_run_args(
        "podman",
        template,
        name,
        session_id,
        additional_mounts,
        Some(&workspace),
        image_user,
        workspace_root,
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

pub(super) const DOCKER_OVERLAY_RUN_SCRIPT: &str = r#"set -eu
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
        None,
        workspace_root,
    )?;
    let overlaid = additional_mounts
        .iter()
        .enumerate()
        .filter(|(_, mount)| mount.access == MountAccess::Cow)
        .collect::<Vec<_>>();
    if overlaid.is_empty() {
        return container_run(
            "docker",
            template,
            name,
            session_id,
            additional_mounts,
            workspace_root,
        );
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
    for (ordinal, mount) in overlaid {
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

/// Podman's `--pull` flag for this template, or `None` when the resolved
/// policy is Podman's own default. Every `podman run` Hel issues for a
/// template — the session container and the image-user probe alike — uses this
/// one answer, so a probe never pulls an image the launch would not.
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
    image_user: Option<ImageUser>,
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
        // Rootless Podman otherwise runs the container's user as a
        // subordinate id, which cannot write to anything the host user owns
        // and leaves unreadable files behind where it can. Every container
        // whose image user is known maps that user back to the host user,
        // whatever its mounts are.
        args.extend(podman_userns_option(image_user));
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
        match podman_workspace.unwrap_or(&PodmanWorkspaceLocator::ContainerLayer) {
            PodmanWorkspaceLocator::ContainerLayer => {}
            // `:U` chowns the volume to the container user, so the session's
            // workspace is writable wherever it is mounted.
            PodmanWorkspaceLocator::Volume { name } => args.extend([
                "--volume".to_owned(),
                format!("{name}:{workspace_root}:rw,U"),
            ]),
            // The host directory belongs to the host user, which
            // `--userns=keep-id` maps to the container user.
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
