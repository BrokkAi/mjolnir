use super::*;

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
        TargetLocator::SshBare { ssh, workspace, .. } => {
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

/// Stop and remove only a child session's private worker state from a target
/// owned by its parent. This never removes the target or project workspace.
pub fn borrowed_worker_cleanup_plan(
    locator: &TargetLocator,
    child_session_id: &str,
) -> Result<CommandPlan> {
    verify_locator(locator, child_session_id)?;
    let worker_root = worker_root(locator, child_session_id)?;
    let mut script = stop_worker_daemon_script(&worker_root);
    script.push_str(&format!("rm -rf -- {}\n", posix_quote(&worker_root)));
    if !matches!(locator, TargetLocator::LocalBare { .. }) {
        script.push_str(&format!(
            "rm -rf -- {} {}\n",
            posix_quote(&format!("/var/lib/hel/profiles/{child_session_id}")),
            posix_quote(&format!(".local/share/hel/profiles/{child_session_id}")),
        ));
    }
    let command = command_on_locator(
        locator,
        child_session_id,
        vec!["sh".into(), "-c".into(), script],
        "stop a borrowed-target sub-agent and remove its private worker state",
    )?;
    Ok(CommandPlan {
        description: format!("clean up sub-agent worker {child_session_id}"),
        commands: vec![command],
    })
}

pub(super) const PODMAN_CONTAINER_IDENTITY_SCRIPT: &str = r#"set -eu
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

pub(super) fn podman_cleanup_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandPlan> {
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

pub(super) fn podman_absence_command(
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
