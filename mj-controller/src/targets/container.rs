use super::*;

pub(super) fn container_run(
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

pub(super) fn container_run_args(
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
