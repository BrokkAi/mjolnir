use super::*;

/// `container_workspace` is the session record's recorded container workspace.
/// Container targets clone into it and mount their workspace storage there;
/// a session that predates per-session workspaces records none and keeps the
/// shared `/workspace`.
pub fn provision_plan(
    template: &TargetTemplate,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
    container_workspace: Option<&Path>,
) -> Result<CommandPlan> {
    provision_plan_named(
        template,
        session_id,
        bundle,
        additional_mounts,
        container_workspace,
        &resource_name(session_id)?,
        None,
    )
}

pub fn provision_plan_named(
    template: &TargetTemplate,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
    container_workspace: Option<&Path>,
    name: &str,
    github_token: Option<&str>,
) -> Result<CommandPlan> {
    ensure!(
        resource_name_belongs_to(name, session_id)?,
        "invalid target resource generation"
    );
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
                | TargetTemplate::SshAppleContainer { .. }
                | TargetTemplate::SshNitro { .. }
        )
    {
        bail!("additional mounts require a container-backed target");
    }
    // Apple's `container` CLI drives the host's own VM disk image, so the
    // remote plan is the local one with every command run over SSH.
    if let TargetTemplate::SshAppleContainer { ssh, container } = template {
        validate_ssh(ssh)?;
        let mut plan = provision_plan_named(
            &TargetTemplate::AppleContainer(container.clone()),
            session_id,
            bundle,
            additional_mounts,
            container_workspace,
            name,
            github_token,
        )?;
        plan.commands = plan
            .commands
            .into_iter()
            .map(|command| command_over_ssh(command, ssh))
            .collect();
        return Ok(plan);
    }
    if let TargetTemplate::SshDocker { ssh, container } = template {
        validate_ssh(ssh)?;
        let mut plan = provision_plan_named(
            &TargetTemplate::LocalDocker(container.clone()),
            session_id,
            bundle,
            additional_mounts,
            container_workspace,
            name,
            github_token,
        )?;
        plan.commands = plan
            .commands
            .into_iter()
            .map(|command| command_over_ssh(command, ssh))
            .collect();
        return Ok(plan);
    }
    let mut commands = Vec::new();
    match template {
        TargetTemplate::LocalBare => {
            bail!("local bare projects must use the existing-project provisioning path")
        }
        TargetTemplate::LocalPodman(container) => {
            validate_container_template(container)?;
            commands.push(podman_container_run(
                container,
                name,
                session_id,
                additional_mounts,
                None,
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "podman",
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                container_exec("podman", name, args)
            }));
        }
        TargetTemplate::LocalDocker(container) => {
            validate_container_template(container)?;
            commands.push(docker_container_run(
                container,
                name,
                session_id,
                additional_mounts,
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "docker",
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                container_exec("docker", name, args)
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
                name,
                session_id,
                additional_mounts,
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine: "container",
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                container_exec("container", name, args)
            }));
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
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                ssh_command_owned(ssh, args)
            }));
        }
        TargetTemplate::SshDocker { .. } | TargetTemplate::SshAppleContainer { .. } => {
            unreachable!("handled above")
        }
        TargetTemplate::SshNitro { ssh, container } => {
            validate_ssh(ssh)?;
            validate_container_template(container)?;
            // The engine creates the workspace and /tmp volumes and removes
            // them again if the launch fails, so the plan is the engine's
            // `run` directly, not a shell wrapper that does that itself.
            commands.extend(nitro_container_run(
                container,
                name,
                session_id,
                additional_mounts,
                ssh,
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::SshContainer {
                    engine: MICROVM_ENGINE,
                    ssh,
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                command_over_ssh(container_exec(MICROVM_ENGINE, name, args), ssh)
            }));
        }
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            validate_container_template(container)?;
            commands.push(podman_container_run(
                container,
                name,
                session_id,
                additional_mounts,
                Some(ssh),
                &workspace,
            )?);
            commands.extend(
                install_git_plan(ExecutionBoundary::SshContainer {
                    engine: "podman",
                    ssh,
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                command_over_ssh(container_exec("podman", name, args), ssh)
            }));
        }
    }
    Ok(CommandPlan {
        description: format!("provision Mjolnir session {session_id}"),
        commands,
    })
}

/// An independent bundle workspace on the controller's host. The controller
/// supplies the recorded worker root so provisioning and restoration agree.
pub(crate) fn provision_local_bundle_plan(
    bundle: &ProjectBundleSpec,
    worker_root: &str,
    github_token: Option<&str>,
) -> Result<CommandPlan> {
    bundle.validate()?;
    Ok(CommandPlan {
        description: "provision local bundle workspace".into(),
        commands: clone_commands(bundle, worker_root, github_token, |args| {
            CommandSpec::new(args[0].clone(), args[1..].to_vec())
        }),
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
    if let TargetTemplate::SshNitro { ssh, container } = template {
        validate_ssh(ssh)?;
        validate_container_template(container)?;
        let mut create =
            nitro_container_run(container, &name, smoke_id, &[], ssh, CONTAINER_WORKSPACE)?;
        create.truncate(1);
        return Ok(CommandPlan {
            description: format!("smoke test Mjolnir setup target {smoke_id}"),
            commands: vec![
                create.remove(0).purpose("create disposable setup sandbox"),
                ssh_engine_command(ssh, MICROVM_ENGINE, ["exec", "-i", name.as_str(), "true"])
                    .purpose("execute setup smoke command"),
                nitro_destroy_in_one_command(ssh, &name),
            ],
        });
    }
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
        TargetTemplate::SshAppleContainer { ssh, container } => {
            validate_ssh(ssh)?;
            ("container", container, ExecutionBoundary::Ssh(ssh))
        }
        _ => bail!("setup smoke tests require a local or SSH container target"),
    };
    validate_container_template(container)?;

    let (run, locator) = match engine {
        "podman" => (
            podman_container_run(container, &name, smoke_id, &[], None, CONTAINER_WORKSPACE)?,
            TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: name.clone(),
                workspace_storage: podman_workspace_locator_named(container, &name)?,
            },
        ),
        "docker" => (
            docker_container_run(container, &name, smoke_id, &[], CONTAINER_WORKSPACE)?,
            TargetLocator::LocalDocker {
                borrowed_from: None,
                container_id: name.clone(),
            },
        ),
        _ => (
            container_run(engine, container, &name, smoke_id, &[], CONTAINER_WORKSPACE)?,
            match boundary {
                ExecutionBoundary::Ssh(ssh) => TargetLocator::SshAppleContainer {
                    borrowed_from: None,
                    ssh: ssh.clone(),
                    container_id: name.clone(),
                },
                _ => TargetLocator::AppleContainer {
                    borrowed_from: None,
                    container_id: name.clone(),
                },
            },
        ),
    };
    let exec = vec![
        engine.to_owned(),
        "exec".to_owned(),
        "-i".to_owned(),
        name.clone(),
        "true".to_owned(),
    ];
    let cleanup = retire_move_target_plan(&locator, smoke_id)?;
    let mut script = String::from("set -eu\n");
    for command in cleanup.commands {
        let argv: Vec<_> = std::iter::once(command.program)
            .chain(command.args)
            .collect();
        script.push_str(&join_remote_command(&argv));
        script.push('\n');
    }
    let remove = if engine == "container" {
        vec![
            engine.to_owned(),
            "rm".to_owned(),
            "--force".to_owned(),
            name,
        ]
    } else {
        vec!["sh".to_owned(), "-c".to_owned(), script]
    };
    let run = match boundary {
        ExecutionBoundary::Ssh(ssh) => command_over_ssh(run, ssh),
        _ => run,
    };

    Ok(CommandPlan {
        description: format!("smoke test Mjolnir setup target {smoke_id}"),
        commands: vec![
            run.purpose("create disposable setup container"),
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

/// The attachment a VM-hosted Docker daemon gets instead of the overlay: the
/// container reads the host directory and cannot write to it.
pub(super) const DOCKER_READ_ONLY_SMOKE_PROBE: &str = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && ! printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt 2>/dev/null";

// macOS temporary directories are not normally shared into Docker VMs. The
// home directory is shared by Colima's default configuration.
pub(super) fn docker_overlay_smoke_directory() -> Result<tempfile::TempDir> {
    let parent = if cfg!(target_os = "macos") {
        mj_core::path_input::user_home()
            .context("locate shared home directory for Docker smoke test")?
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
    // Test the attachment sessions on this daemon actually get: a VM-hosted
    // daemon cannot overlay host directories, so they mount read-only there.
    let vm_share = local_docker_vm_share(executor)?.is_some();
    let (access, probe_script, probe_purpose) = if vm_share {
        (
            MountAccess::Ro,
            DOCKER_READ_ONLY_SMOKE_PROBE,
            "verify Docker read-only attachment",
        )
    } else {
        (
            MountAccess::Cow,
            DOCKER_OVERLAY_SMOKE_PROBE,
            "verify Docker OverlayFS copy-on-write attachment",
        )
    };
    let mount = AdditionalMount {
        source: lower.path().to_path_buf(),
        destination: PathBuf::from("/mnt/hel-overlay-smoke"),
        access,
    };
    let create = docker_container_run(container, &name, smoke_id, &[mount], CONTAINER_WORKSPACE)?
        .purpose("create disposable Docker attachment smoke container");
    let probe = container_exec("docker", &name, ["sh", "-c", probe_script]).purpose(probe_purpose);
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
        "Docker attachment smoke test changed its source directory"
    );
    ensure!(
        !added.exists(),
        "Docker attachment smoke test created a file in its source directory"
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
