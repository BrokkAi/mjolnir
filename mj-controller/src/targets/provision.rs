use super::*;

/// Create the initial resource. AWS address discovery and all SSH bootstrap
/// happen after parsing the `run-instances` response and constructing a locator.
///
/// `container_workspace` is the session record's recorded container workspace.
/// Container targets clone into it and mount their workspace storage there;
/// a session that predates per-session workspaces records none and keeps the
/// shared `/workspace`.
pub fn provision_plan(
    template: &TargetTemplate,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
    image_user: Option<ImageUser>,
    container_workspace: Option<&Path>,
) -> Result<CommandPlan> {
    bundle.validate()?;
    let workspace = container_workspace_root(container_workspace);
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
            image_user,
            container_workspace,
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
                image_user,
                None,
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "podman",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, |args| {
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
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "docker",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, |args| {
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
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "container",
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, |args| {
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
                image_user,
                Some(ssh),
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::SshContainer {
                    engine: "podman",
                    ssh,
                    container_id: &name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, |args| {
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
        None,
        CONTAINER_WORKSPACE,
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
///
/// The test runs the target's image, which the engine pulls if it is
/// missing. It holds the image download lock while it runs, so it waits for
/// a daemon that is downloading the same image instead of pulling it a
/// second time at once (J-13).
pub fn run_setup_smoke_test(
    template: &TargetTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    crate::image_pull_gate::with_image_ready(template, executor, || {
        run_setup_smoke_test_unlocked(template, smoke_id, executor)
    })
}

fn run_setup_smoke_test_unlocked(
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

pub(super) fn run_ssh_docker_overlay_smoke_test(
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
        access: MountAccess::Cow,
    };
    let result = (|| {
        let create = command_over_ssh(
            docker_container_run(container, &name, smoke_id, &[mount], CONTAINER_WORKSPACE)?,
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
                borrowed_from: None,
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

pub(super) const DOCKER_OVERLAY_SMOKE_PROBE: &str = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && printf 'changed\\n' >/mnt/hel-overlay-smoke/original.txt && printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt";

// macOS temporary directories are not normally shared into Docker VMs. The
// home directory is shared by Colima's default configuration.
pub(super) fn docker_overlay_smoke_directory() -> Result<tempfile::TempDir> {
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

pub(super) fn run_docker_overlay_smoke_test(
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
        access: MountAccess::Cow,
    };
    let create = docker_container_run(container, &name, smoke_id, &[mount], CONTAINER_WORKSPACE)?
        .purpose("create disposable Docker OverlayFS smoke container");
    let probe = container_exec("docker", &name, ["sh", "-c", DOCKER_OVERLAY_SMOKE_PROBE])
        .purpose("verify Docker OverlayFS copy-on-write attachment");
    let cleanup = close_plan(
        &TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id: name,
        },
        smoke_id,
    )?
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

pub(super) fn execute_checked(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
) -> Result<()> {
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
