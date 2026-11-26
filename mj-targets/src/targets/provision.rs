use super::*;
use mj_core::targets::SshTrafficClass;

/// `container_workspace` is the session record's recorded container workspace.
/// Container targets clone into it and mount their workspace storage there;
/// a session that predates per-session workspaces records none and keeps the
/// shared `/workspace`.
pub fn provision_plan(
    template: &TargetTemplate,
    session_id: &str,
    instance: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
    container_workspace: Option<&Path>,
) -> Result<CommandPlan> {
    provision_plan_named(
        template,
        session_id,
        instance,
        bundle,
        additional_mounts,
        container_workspace,
        &resource_name(session_id)?,
        ProvisionOptions::default(),
    )
}

#[derive(Clone, Copy, Default)]
pub struct ProvisionOptions<'a> {
    pub github_token: Option<&'a str>,
    pub nitro_mbx_remote: Option<&'a NitroMbxRemote<'a>>,
}

/// `instance` is the owning daemon's ownership identity, stamped on the
/// session's container.
// Each argument is an independent input of one container's creation; the
// owning instance's identity is one more of them.
#[allow(clippy::too_many_arguments)]
pub fn provision_plan_named(
    template: &TargetTemplate,
    session_id: &str,
    instance: &str,
    bundle: &ProjectBundleSpec,
    additional_mounts: &[AdditionalMount],
    container_workspace: Option<&Path>,
    name: &str,
    options: ProvisionOptions<'_>,
) -> Result<CommandPlan> {
    let github_token = options.github_token;
    ensure!(
        resource_name_belongs_to(name, session_id)?,
        "invalid target resource generation"
    );
    bundle.validate()?;
    if template.container_engine().is_some()
        && let Some(source) = bundle.repositories.iter().find_map(|repository| {
            repository.url.as_deref().filter(|source| {
                repository.reference.is_none()
                    && mj_core::remote_git::is_host_only_git_source(source)
            })
        })
    {
        return Err(mj_core::refusal::Refusal::precondition(format!(
            "Git source {} is host-only and needs a container host with the Git cache. Choose a target that can mount the cache or configure a reachable Git source.",
            mj_core::remote_git::display_url(source)
        )).into());
    }
    let workspace = container_workspace_root(container_workspace);
    if !additional_mounts.is_empty() && template.container_engine().is_none() {
        bail!("additional mounts require a container-backed target");
    }
    let mut commands = Vec::new();
    match template {
        TargetTemplate::LocalBare => {
            bail!("local bare projects must use the existing-project provisioning path")
        }
        TargetTemplate::LocalPodman(container) => {
            validate_container_template(container)?;
            commands.push(podman_container_run_gated(
                container,
                name,
                session_id,
                instance,
                additional_mounts,
                None,
                &workspace,
                crate::image_pull_gate::launch_image_gate(template),
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
        TargetTemplate::LocalDocker(container)
        | TargetTemplate::SshDocker { container, .. }
        | TargetTemplate::AppleContainer(container)
        | TargetTemplate::SshAppleContainer { container, .. } => {
            let ssh = match template {
                TargetTemplate::SshDocker { ssh, .. }
                | TargetTemplate::SshAppleContainer { ssh, .. } => {
                    validate_ssh(ssh)?;
                    Some(ssh)
                }
                _ => None,
            };
            validate_container_template(container)?;
            let engine = template
                .container_engine()
                .expect("Docker or Apple container");
            let run = if engine == "container" {
                commands.extend(apple_image_prepare_commands(container));
                container_run(
                    engine,
                    container,
                    name,
                    session_id,
                    instance,
                    additional_mounts,
                    &workspace,
                )?
            } else {
                docker_container_run(
                    container,
                    name,
                    session_id,
                    instance,
                    additional_mounts,
                    &workspace,
                )?
            };
            commands.push(run);
            commands.extend(
                install_git_plan(ExecutionBoundary::Container {
                    engine,
                    container_id: name,
                })
                .commands,
            );
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                container_exec(engine, name, args)
            }));
            if let Some(ssh) = ssh {
                commands = commands
                    .into_iter()
                    .map(|command| command_over_ssh(SshTrafficClass::Bulk, command, ssh))
                    .collect();
            }
        }
        TargetTemplate::SshBare {
            ssh,
            workspace_prefix: _,
        } => {
            validate_ssh(ssh)?;
            let workspace = workspace_for(template, session_id)?;
            commands.push(
                ssh_command(SshTrafficClass::Control, ssh, ["mkdir", "-p", &workspace])
                    .purpose("create SSH session workspace")
                    .stage(ProvisionStage::Provisioning)
                    .creates_target(),
            );
            commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
            commands.extend(clone_commands(bundle, &workspace, github_token, |args| {
                ssh_command_owned(SshTrafficClass::Bulk, ssh, args)
            }));
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
                instance,
                additional_mounts,
                ssh,
                &workspace,
                options.nitro_mbx_remote,
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
                command_over_ssh(
                    SshTrafficClass::Bulk,
                    container_exec(MICROVM_ENGINE, name, args),
                    ssh,
                )
            }));
        }
        TargetTemplate::SshPodman { ssh, container } => {
            validate_ssh(ssh)?;
            validate_container_template(container)?;
            commands.push(podman_container_run_gated(
                container,
                name,
                session_id,
                instance,
                additional_mounts,
                Some(ssh),
                &workspace,
                crate::image_pull_gate::launch_image_gate(template),
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
                command_over_ssh(
                    SshTrafficClass::Bulk,
                    container_exec("podman", name, args),
                    ssh,
                )
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
pub fn provision_local_bundle_plan(
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
        let mut create = nitro_container_run(
            container,
            &name,
            smoke_id,
            SETUP_SMOKE_INSTANCE,
            &[],
            ssh,
            CONTAINER_WORKSPACE,
            None,
        )?;
        create.truncate(1);
        return Ok(CommandPlan {
            description: format!("smoke test Mjolnir setup target {smoke_id}"),
            commands: vec![
                create.remove(0).purpose("create disposable setup sandbox"),
                ssh_engine_command(
                    SshTrafficClass::Control,
                    ssh,
                    MICROVM_ENGINE,
                    ["exec", "-i", name.as_str(), "true"],
                )
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
            podman_container_run(
                container,
                &name,
                smoke_id,
                SETUP_SMOKE_INSTANCE,
                &[],
                None,
                CONTAINER_WORKSPACE,
            )?,
            Some(TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: name.clone(),
                workspace_storage: podman_workspace_locator_named(container, &name)?,
            }),
        ),
        "docker" => (
            docker_container_run(
                container,
                &name,
                smoke_id,
                SETUP_SMOKE_INSTANCE,
                &[],
                CONTAINER_WORKSPACE,
            )?,
            Some(TargetLocator::LocalDocker {
                borrowed_from: None,
                container_id: name.clone(),
            }),
        ),
        _ => (
            container_run(
                engine,
                container,
                &name,
                smoke_id,
                SETUP_SMOKE_INSTANCE,
                &[],
                CONTAINER_WORKSPACE,
            )?,
            None,
        ),
    };
    let exec = vec![
        engine.to_owned(),
        "exec".to_owned(),
        "-i".to_owned(),
        name.clone(),
        "true".to_owned(),
    ];
    let remove = if let Some(locator) = locator {
        let cleanup = retire_move_target_plan(&locator, smoke_id)?;
        let mut script = String::from("set -eu\n");
        for command in cleanup.commands_without_staging()? {
            let argv: Vec<_> = std::iter::once(command.program.clone())
                .chain(command.args.iter().cloned())
                .collect();
            script.push_str(&join_remote_command(&argv));
            script.push('\n');
        }
        vec!["sh".to_owned(), "-c".to_owned(), script]
    } else {
        vec![
            engine.to_owned(),
            "rm".to_owned(),
            "--force".to_owned(),
            name,
        ]
    };
    let run = match boundary {
        ExecutionBoundary::Ssh(ssh) => command_over_ssh(SshTrafficClass::Control, run, ssh),
        _ => run,
    };

    Ok(CommandPlan {
        description: format!("smoke test Mjolnir setup target {smoke_id}"),
        commands: vec![
            run.purpose("create disposable setup container"),
            at_boundary(SshTrafficClass::Control, boundary, exec)
                .purpose("execute setup smoke command"),
            at_boundary(SshTrafficClass::Control, boundary, remove)
                .purpose("remove disposable setup container"),
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
) -> Result<Option<DockerAttachmentMode>> {
    crate::image_pull_gate::with_image_ready(template, executor, || {
        match template {
            TargetTemplate::LocalDocker(container) => {
                return run_docker_attachment_smoke_test(None, container, smoke_id, executor)
                    .map(Some);
            }
            TargetTemplate::SshDocker { ssh, container } => {
                return run_docker_attachment_smoke_test(Some(ssh), container, smoke_id, executor)
                    .map(Some);
            }
            _ => {}
        }
        let plan = setup_smoke_plan(template, smoke_id)?;
        execute_checked(executor, &plan.commands[0])?;
        let smoke_result = execute_checked(executor, &plan.commands[1]);
        let cleanup_result = execute_checked(executor, &plan.commands[2]);
        smoke_result?;
        cleanup_result?;
        Ok(None)
    })
}

pub(super) const DOCKER_OVERLAY_SMOKE_PROBE: &str = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && printf 'changed\\n' >/mnt/hel-overlay-smoke/original.txt && printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt";

/// The attachment a VM-hosted Docker daemon gets instead of the overlay: the
/// container reads the host directory and cannot write to it.
pub(super) const DOCKER_READ_ONLY_SMOKE_PROBE: &str = "test \"$(cat /mnt/hel-overlay-smoke/original.txt)\" = lower && ! printf 'created\\n' >/mnt/hel-overlay-smoke/container-created.txt 2>/dev/null";

// macOS temporary directories are not normally shared into Docker VMs. The
// home directory is shared by Colima's default configuration.
pub(super) fn docker_overlay_smoke_directory(darwin: bool) -> Result<tempfile::TempDir> {
    let parent = if darwin {
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

enum DockerSmokeSource<'a> {
    Local(tempfile::TempDir),
    Ssh { host: &'a SshTarget, path: PathBuf },
}

impl<'a> DockerSmokeSource<'a> {
    fn prepare(
        ssh: Option<&'a SshTarget>,
        darwin: bool,
        executor: &impl CommandExecutor,
    ) -> Result<Self> {
        if let Some(host) = ssh {
            let template = if darwin {
                r#""$HOME/.mj-docker-overlay-smoke-XXXXXXXXXX""#
            } else {
                "/tmp/mj-docker-overlay-smoke.XXXXXXXXXX"
            };
            let script = format!(
                r#"set -eu
root=$(mktemp -d {template})
trap 'rm -rf -- "$root"' EXIT
printf 'lower\n' >"$root/original.txt"
chmod 777 "$root"
chmod 666 "$root/original.txt"
printf '%s\n' "$root"
trap - EXIT"#
            );
            let prepare = ssh_command(SshTrafficClass::Control, host, ["sh", "-c", &script])
                .purpose("create remote Docker attachment smoke source");
            let output = executor.execute(&prepare)?;
            ensure!(
                output.status == 0,
                "{} failed on {}: {}",
                prepare.purpose,
                host.destination,
                String::from_utf8_lossy(&output.stderr)
            );
            let path = String::from_utf8(output.stdout).context("decode remote smoke directory")?;
            let path = path.trim_end_matches('\n');
            let prefix = if darwin {
                ".mj-docker-overlay-smoke-"
            } else {
                "mj-docker-overlay-smoke."
            };
            ensure!(
                path.starts_with('/')
                    && (darwin || path.starts_with("/tmp/"))
                    && !path.contains(['\n', '\r'])
                    && !path.split('/').any(|part| part == "..")
                    && path
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.starts_with(prefix)),
                "unexpected remote smoke directory {path:?}"
            );
            return Ok(Self::Ssh {
                host,
                path: PathBuf::from(path),
            });
        }

        let lower = docker_overlay_smoke_directory(darwin)?;
        let original = lower.path().join("original.txt");
        fs::write(&original, b"lower\n").context("write Docker attachment smoke source")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Test the mount independently of host and image UIDs.
            fs::set_permissions(lower.path(), fs::Permissions::from_mode(0o777))?;
            fs::set_permissions(&original, fs::Permissions::from_mode(0o666))?;
        }
        Ok(Self::Local(lower))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Local(lower) => lower.path(),
            Self::Ssh { path, .. } => path,
        }
    }

    fn verify(&self, executor: &impl CommandExecutor) -> Result<()> {
        match self {
            Self::Local(lower) => {
                ensure!(
                    fs::read(lower.path().join("original.txt"))
                        .context("read Docker attachment smoke source after container write")?
                        == b"lower\n"
                        && !lower.path().join("container-created.txt").exists(),
                    "Docker attachment smoke test changed its source directory"
                );
                Ok(())
            }
            Self::Ssh { host, path } => execute_checked(
                executor,
                &ssh_command(SshTrafficClass::Control, host, ["sh", "-c",
                    r#"test "$(cat "$1/original.txt")" = lower && test ! -e "$1/container-created.txt""#,
                    "mj-check-smoke-source", path.to_str().context("invalid smoke source path")?])
                    .purpose("verify original remote attachment is unchanged"),
            ),
        }
    }

    fn remove(self, executor: &impl CommandExecutor) -> Result<()> {
        match self {
            Self::Local(lower) => lower.close().context("remove Docker smoke source"),
            Self::Ssh { host, path } => execute_checked(
                executor,
                &ssh_command(
                    SshTrafficClass::Control,
                    host,
                    [
                        "rm",
                        "-rf",
                        "--",
                        path.to_str().context("invalid smoke source path")?,
                    ],
                )
                .purpose("remove remote Docker smoke source"),
            ),
        }
    }

    fn retain(self) -> PathBuf {
        match self {
            Self::Local(lower) => lower.keep(),
            Self::Ssh { path, .. } => path,
        }
    }
}

fn run_docker_attachment_smoke_test(
    ssh: Option<&SshTarget>,
    container: &ContainerTemplate,
    smoke_id: &str,
    executor: &impl CommandExecutor,
) -> Result<DockerAttachmentMode> {
    if let Some(ssh) = ssh {
        validate_ssh(ssh)?;
    }
    validate_container_template(container)?;
    let name = resource_name(smoke_id)?;
    let (mode, darwin) = docker_attachment_mode(ssh, executor)?;
    let (access, probe_script, probe_purpose) = match mode {
        DockerAttachmentMode::ReadOnly { .. } => (
            MountAccess::Ro,
            DOCKER_READ_ONLY_SMOKE_PROBE,
            "verify Docker read-only attachment",
        ),
        DockerAttachmentMode::Overlay => (
            MountAccess::Cow,
            DOCKER_OVERLAY_SMOKE_PROBE,
            "verify Docker OverlayFS copy-on-write attachment",
        ),
    };
    let locator = match ssh {
        Some(ssh) => TargetLocator::SshDocker {
            borrowed_from: None,
            ssh: ssh.clone(),
            container_id: name.clone(),
        },
        None => TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id: name.clone(),
        },
    };
    let cleanup = close_plan(&locator, smoke_id)?;
    let cleanup = cleanup.commands_without_staging()?;
    let source = DockerSmokeSource::prepare(ssh, darwin, executor)?;
    let mount = AdditionalMount {
        source: source.path().to_owned(),
        destination: PathBuf::from("/mnt/hel-overlay-smoke"),
        access,
    };
    let at_host = |command| match ssh {
        Some(ssh) => command_over_ssh(SshTrafficClass::Control, command, ssh),
        None => command,
    };
    let result = (|| {
        let create = at_host(docker_container_run(
            container,
            &name,
            smoke_id,
            SETUP_SMOKE_INSTANCE,
            &[mount],
            CONTAINER_WORKSPACE,
        )?)
        .purpose("create disposable Docker attachment smoke container");
        execute_checked(executor, &create)?;
        let probe = at_host(container_exec("docker", &name, ["sh", "-c", probe_script]))
            .purpose(probe_purpose);
        execute_checked(executor, &probe)?;
        source.verify(executor)
    })();
    let cleanup = cleanup
        .iter()
        .try_for_each(|command| execute_checked(executor, command));
    let cleanup = match cleanup {
        Ok(()) => source.remove(executor),
        // A surviving overlay still references its lower directory.
        Err(error) => Err(error.context(format!(
            "Docker smoke cleanup failed; retained source at {}",
            source.retain().display()
        ))),
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(mode),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Docker smoke cleanup also failed: {cleanup:#}")))
        }
    }
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
    mj_core::targets::log_command_warnings(command, &output);
    Ok(())
}
