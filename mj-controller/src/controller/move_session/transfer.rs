//! Move-only workspace transport. It never reads or writes a checkpoint payload.
use super::*;
use crate::targets::{self, CommandSpec};
use mj_core::move_workspace::*;
use std::path::{Path, PathBuf};

fn repositories(
    layout: &super::super::checkpoint::SessionExportLayout,
) -> Vec<WorkspaceRepository> {
    layout
        .repositories
        .iter()
        .map(|repo| WorkspaceRepository {
            id: repo.id.clone(),
            root: Path::new(&layout.workspace_root).join(&repo.relative_destination),
        })
        .collect()
}

fn utility(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    id: &str,
    command: &WorkspaceCommand,
) -> Result<serde_json::Value> {
    let binary = super::super::worker_binary::worker_binary_for(backend, executor)?;
    let executable = if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        binary.to_string_lossy().into_owned()
    } else {
        let root = targets::worker_root(backend, id)?;
        let helper = format!("{root}/move-helper-{}", mj_core::worker_build::BUILD_ID);
        let probe = executor.execute(&targets::command_on_locator(
            backend,
            id,
            vec!["test".into(), "-x".into(), helper.clone()],
            "check Move helper",
        )?)?;
        if probe.status != 0 {
            install_helper(executor, backend, id, &binary, &helper)?;
        }
        helper
    };
    let command_spec = targets::command_on_locator(
        backend,
        id,
        vec![executable, "worker".into(), "move-workspace".into()],
        "prepare or verify Move workspace",
    )?;
    let output = executor.execute_with_stdin(
        &command_spec,
        &mut std::io::Cursor::new(serde_json::to_vec(command)?),
    )?;
    ensure!(
        output.status == 0,
        "Move workspace operation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("read Move workspace result")
}

fn install_helper(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    id: &str,
    binary: &Path,
    helper: &str,
) -> Result<()> {
    // Each uploader owns its temporary inode. Rename publishes only a complete
    // immutable helper; neither checkpoint staging nor a running binary is overwritten.
    let script = r#"set -eu
temporary=$(mktemp "$1.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT
cat > "$temporary"
chmod 700 "$temporary"
mv -f -- "$temporary" "$1"
"#;
    let command = targets::command_on_locator(
        backend,
        id,
        vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "install-move-helper".into(),
            helper.into(),
        ],
        "install immutable Move helper",
    )?;
    let output = executor.execute_with_stdin(&command, &mut std::fs::File::open(binary)?)?;
    ensure!(
        output.status == 0,
        "Move helper installation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Options for every Move rsync. `--protect-args` sends paths inside the rsync
/// protocol, so the namespace wrapper passes paths with spaces unchanged. The
/// preflight probes these options, so an rsync that rejects one blocks Move.
const RSYNC_TRANSFER_OPTIONS: [&str; 6] = [
    "--recursive",
    "--times",
    "--perms",
    "--protect-args",
    "--partial",
    "--partial-dir=.move-partial",
];

/// Why an rsync failed the probe. macOS ships openrsync, or rsync 2.6.9 on
/// older releases, and neither accepts `--protect-args`.
const RSYNC_REQUIREMENT: &str = "Move needs rsync 3.0 or newer, which accepts --protect-args; \
the rsync that ships with macOS does not (install one with `brew install rsync`)";

/// Rsync arguments that succeed only if rsync accepts every transfer option.
/// `--version` makes rsync exit once it has parsed them.
fn rsync_probe_arguments() -> impl Iterator<Item = String> {
    RSYNC_TRANSFER_OPTIONS
        .into_iter()
        .chain(["--version"])
        .map(String::from)
}

fn rsync_probe() -> Vec<String> {
    std::iter::once("rsync".into())
        .chain(rsync_probe_arguments())
        .collect()
}

/// Rsync's remote shell is the existing locator command, including its SSH
/// session lease and container namespace. The remote endpoint sees only Move
/// staging. Partial files belong to this operation and survive retry.
fn copy_workspace(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    remote: &Path,
    local: &Path,
    upload: bool,
) -> Result<()> {
    std::fs::create_dir_all(local)?;
    let source = format!("{}/", if upload { local } else { remote }.display());
    let destination = format!("{}/", if upload { remote } else { local }.display());
    let mut args: Vec<String> = RSYNC_TRANSFER_OPTIONS.map(String::from).into();
    let base = targets::locator_command(backend, vec!["rsync".into()]);
    let lease = base.open_ssh_session(executor)?;
    let wrapper = if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        args.extend([source, destination]);
        None
    } else {
        let directory = rsync_shell(lease.command())?;
        let path = directory.path().join("move-rsync-shell");
        args.extend(["--rsh".into(), path.to_string_lossy().into_owned()]);
        args.extend(if upload {
            [source, format!("move:{destination}")]
        } else {
            [format!("move:{source}"), destination]
        });
        Some(directory)
    };
    let result = super::super::execute_checked(
        executor,
        CommandSpec::new("rsync", args).purpose("transfer Move workspace with resumable files"),
    );
    drop(wrapper);
    drop(lease);
    result.map(|_| ())
}

fn rsync_shell(command: &CommandSpec) -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("move-rsync-shell");
    let quoted = std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .map(targets::posix_quote)
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nset -eu\nshift\nshift\nexec {quoted} \"$@\"\n"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn storage_probe(
    executor: &(impl CommandExecutor + Sync),
    command: CommandSpec,
    location: &str,
    allocation: &str,
    copies: u64,
    assessment: &mut WorkspaceAssessment,
) {
    let result = executor.execute(&command).and_then(|output| {
        ensure!(
            output.status == 0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = String::from_utf8_lossy(&output.stdout);
        let available_bytes = super::super::mbx::available_bytes(&report)
            .map(|blocks| blocks.saturating_mul(1024))
            .context("cannot read available disk space")?;
        let device = report
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().next())
            .context("cannot read storage identity")?;
        Ok((
            format!(
                "{}:{device}",
                command.ssh_destination.as_deref().unwrap_or("local")
            ),
            available_bytes,
        ))
    });
    match result {
        Ok((filesystem_key, available_bytes)) => {
            if let Some(storage) = assessment
                .storage
                .iter_mut()
                .find(|storage| storage.filesystem_key == filesystem_key)
            {
                storage.location.push_str(&format!(" + {location}"));
                if storage.allocations.insert(allocation.into()) {
                    storage.copies = storage.copies.saturating_add(copies);
                }
                storage.available_bytes = storage.available_bytes.min(available_bytes);
            } else {
                assessment.storage.push(WorkspaceStorage {
                    filesystem_key,
                    allocations: [allocation.into()].into_iter().collect(),
                    location: location.into(),
                    available_bytes,
                    copies,
                });
            }
        }
        Err(error) => assessment
            .blockers
            .push(format!("Check {location} storage: {error:#}")),
    }
}

fn at_host(ssh: Option<&targets::SshTarget>, mut arguments: Vec<String>) -> CommandSpec {
    if let Some(ssh) = ssh {
        targets::ssh_command_owned(ssh, arguments)
    } else {
        let program = arguments.remove(0);
        CommandSpec::new(program, arguments)
    }
}

fn container_storage_arguments(engine: &str) -> Option<Vec<String>> {
    let format = match engine {
        "podman" => "{{.Store.GraphRoot}}",
        "docker" => "{{.DockerRootDir}}",
        _ => return None,
    };
    Some(vec![
        "sh".into(),
        "-c".into(),
        "set -eu; p=$(\"$1\" info --format \"$2\"); test -n \"$p\"; df -Pk -- \"$p\"".into(),
        "move-container-storage".into(),
        engine.into(),
        format.into(),
    ])
}

fn storage_arguments(path: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "set -eu; p=$1; while [ ! -e \"$p\" ]; do p=$(dirname -- \"$p\"); done; df -Pk -- \"$p\""
            .into(),
        "move-storage".into(),
        path.to_string_lossy().into_owned(),
    ]
}

impl Controller {
    pub fn cleanup_retained_move_source(
        &self,
        session_id: &str,
        operation_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let retained = crate::database::retained_move_sources(session_id)?
            .into_iter()
            .find(|source| source.operation_id == operation_id)
            .context("retained Move source is missing")?;
        let source = &retained.source;
        ensure!(
            !self
                .state
                .sessions
                .values()
                .any(|session| session.target.is_some() && session.target == source.target),
            "retained source is still referenced by an active session"
        );
        if let Some(locator) = &source.target {
            let backend = super::super::backend::backend_locator(locator, source, &self.config)?;
            targets::retire_move_target_plan(&backend, session_id)?.execute(executor)?;
        }
        if let Some(checkout) = &source.managed_worktree {
            ensure!(
                !self
                    .state
                    .sessions
                    .values()
                    .any(|session| session.managed_worktree.as_ref() == Some(checkout)),
                "retained checkout is still referenced by a session"
            );
            super::super::worktree::retire_managed_worktree(executor, checkout)?;
        }
        crate::database::forget_retained_move_source(operation_id)
    }

    pub(in crate::controller) fn move_destination_bundle(
        &self,
        id: &str,
    ) -> Result<Option<targets::ProjectBundleSpec>> {
        let Some(operation) = crate::database::load_move_operation(id)? else {
            return Ok(None);
        };
        if operation.workspace_transfer.is_none()
            || operation.phase != MovePhase::ResumingDestination
        {
            return Ok(None);
        }
        let handoff = operation.handoff.context("Move handoff missing")?;
        let archive = super::super::resume::verify_resume_checkpoint(id, &handoff)?;
        let primary = archive
            .manifest
            .repositories
            .iter()
            .find(|repo| repo.metadata.id == archive.manifest.bundle.primary_repository)
            .context("Move primary repository missing")?
            .metadata
            .relative_destination
            .to_string_lossy()
            .into_owned();
        let repositories = archive
            .manifest
            .repositories
            .iter()
            .map(|repo| {
                let metadata = &repo.metadata;
                mj_core::remote_git::validate_network_url(&metadata.origin)?;
                Ok(targets::RepositorySpec {
                    url: Some(metadata.origin.clone()),
                    push_urls: metadata.push_urls.clone(),
                    destination: metadata.relative_destination.to_string_lossy().into_owned(),
                    git_ref: None,
                    reference: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(targets::ProjectBundleSpec {
            primary,
            repositories,
        }))
    }

    pub(super) fn assess_move_workspace(
        &self,
        selection: &MoveSelection,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<WorkspaceAssessment> {
        let id = &selection.session_id;
        if let Some(operation) = crate::database::load_move_operation(id)?
            && operation.phase != MovePhase::Completed
            && let Some(transfer) = operation.workspace_transfer
        {
            return Ok(transfer.assessment);
        }
        let layout = self.session_export_layout(id, executor)?;
        let mut assessment: WorkspaceAssessment = serde_json::from_value(utility(
            executor,
            &layout.backend,
            id,
            &WorkspaceCommand::Inspect {
                repositories: repositories(&layout),
            },
        )?)?;
        for command in [
            CommandSpec::new("rsync", rsync_probe_arguments())
                .purpose("check controller Move transport"),
            targets::command_on_locator(
                &layout.backend,
                id,
                rsync_probe(),
                "check source Move transport",
            )?,
        ] {
            match executor.execute(&command) {
                Ok(output) if output.status == 0 => {}
                Ok(output) => assessment.blockers.push(format!(
                    "{}: {RSYNC_REQUIREMENT}: {}",
                    command.purpose,
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => assessment
                    .blockers
                    .push(format!("{}: {error:#}", command.purpose)),
            }
        }
        let source_root = targets::worker_root(&layout.backend, id)?;
        storage_probe(
            executor,
            targets::command_on_locator(
                &layout.backend,
                id,
                storage_arguments(Path::new(&source_root)),
                "check source Move staging space",
            )?,
            "Source",
            "source",
            1,
            &mut assessment,
        );
        if let Some(arguments) = layout
            .backend
            .container_engine()
            .and_then(container_storage_arguments)
        {
            let host = targets::locator_command(&layout.backend, vec!["true".into()]).ssh_session;
            storage_probe(
                executor,
                at_host(host.as_ref(), arguments).purpose("check source container backing storage"),
                "Source backing storage",
                "source",
                1,
                &mut assessment,
            );
        }
        let mut local_args = storage_arguments(&mj_core::config::data_dir());
        let program = local_args.remove(0);
        storage_probe(
            executor,
            CommandSpec::new(program, local_args).purpose("check controller Move staging space"),
            "Controller",
            "controller",
            1,
            &mut assessment,
        );
        let target = &self.config.targets[selection
            .target_template_id
            .as_ref()
            .context("Move target missing")?];
        if matches!(target, mj_core::config::TargetTemplate::AwsEc2 { .. }) {
            assessment.blockers.push("Move cannot inspect a not-yet-created EC2 instance's transfer tools or free disk space. Use a configured SSH target on an existing instance.".into());
            return Ok(assessment);
        }
        if matches!(target, mj_core::config::TargetTemplate::LocalBare)
            && matches!(layout.backend, targets::TargetLocator::LocalBare { .. })
        {
            assessment.blockers.push("These local targets share the source worker storage. Keep the current resource settings to switch profiles in place.".into());
        }
        if let mj_core::config::TargetTemplate::SshBare { ssh, .. } = target
            && let targets::TargetLocator::SshBare {
                ssh: source_ssh, ..
            } = &layout.backend
            && *source_ssh == targets::SshTarget::from(ssh)
        {
            assessment.blockers.push("These SSH targets share the source worker storage. Select the current target to keep the environment while changing profiles.".into());
        }
        let (destination_ssh, destination_engine, destination_image) = match target {
            mj_core::config::TargetTemplate::LocalPodman { container } => {
                (None, Some("podman"), Some(container.image.as_str()))
            }
            mj_core::config::TargetTemplate::LocalDocker { container } => {
                (None, Some("docker"), Some(container.image.as_str()))
            }
            mj_core::config::TargetTemplate::AppleContainer { container } => {
                (None, Some("container"), Some(container.image.as_str()))
            }
            mj_core::config::TargetTemplate::SshPodman { ssh, container } => {
                (Some(ssh), Some("podman"), Some(container.image.as_str()))
            }
            mj_core::config::TargetTemplate::SshDocker { ssh, container } => {
                (Some(ssh), Some("docker"), Some(container.image.as_str()))
            }
            mj_core::config::TargetTemplate::SshBare { ssh, .. } => (Some(ssh), None, None),
            _ => (None, None, None),
        };
        let mut arguments =
            if let (Some(engine), Some(image)) = (destination_engine, destination_image) {
                let mut arguments = vec![
                    engine.into(),
                    "run".into(),
                    "--rm".into(),
                    "--entrypoint".into(),
                    "rsync".into(),
                    image.into(),
                ];
                arguments.extend(rsync_probe_arguments());
                arguments
            } else {
                rsync_probe()
            };
        let command = if let Some(ssh) = destination_ssh {
            targets::ssh_command_owned(&targets::SshTarget::from(ssh), arguments)
        } else {
            let program = arguments.remove(0);
            CommandSpec::new(program, arguments)
        };
        match executor.execute(&command.purpose("check destination Move transport")) {
            Ok(output) if output.status == 0 => {},
            Ok(output) => assessment.blockers.push(format!("Destination: {RSYNC_REQUIREMENT}. Update the target image or install rsync there: {}", String::from_utf8_lossy(&output.stderr))),
            Err(error) => assessment.blockers.push(format!("Destination transport check failed: {error:#}")),
        }
        let destination_ssh = destination_ssh.map(targets::SshTarget::from);
        if let mj_core::config::TargetTemplate::LocalPodman { container }
        | mj_core::config::TargetTemplate::SshPodman { container, .. } = target
            && let mj_core::config::PodmanWorkspaceStorage::HostHelper { root, .. } =
                &container.workspace_storage
        {
            storage_probe(
                executor,
                at_host(destination_ssh.as_ref(), storage_arguments(Path::new(root)))
                    .purpose("check destination workspace storage"),
                "Destination workspace",
                "destination",
                2,
                &mut assessment,
            );
        }
        if let Some(engine) = destination_engine {
            let image = destination_image.context("Move image missing")?;
            let arguments = vec![
                engine.into(),
                "run".into(),
                "--rm".into(),
                "--entrypoint".into(),
                "sh".into(),
                image.into(),
                "-c".into(),
                "df -Pk /".into(),
            ];
            storage_probe(
                executor,
                at_host(destination_ssh.as_ref(), arguments)
                    .purpose("check destination container space"),
                "Destination container",
                "destination",
                2,
                &mut assessment,
            );
            if let Some(arguments) = container_storage_arguments(engine) {
                storage_probe(
                    executor,
                    at_host(destination_ssh.as_ref(), arguments)
                        .purpose("check destination container backing storage"),
                    "Destination backing storage",
                    "destination",
                    2,
                    &mut assessment,
                );
            }
        } else if !matches!(target, mj_core::config::TargetTemplate::AwsEc2 { .. }) {
            let path = match target {
                mj_core::config::TargetTemplate::SshBare {
                    workspace_prefix, ..
                } => workspace_prefix.clone(),
                _ => mj_core::config::data_dir(),
            };
            storage_probe(
                executor,
                at_host(destination_ssh.as_ref(), storage_arguments(&path))
                    .purpose("check destination Move staging and workspace space"),
                "Destination",
                "destination",
                2,
                &mut assessment,
            );
        }
        Ok(assessment)
    }

    pub(super) fn new_workspace_transfer(
        &self,
        id: &str,
        operation_id: &str,
        assessment: WorkspaceAssessment,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<WorkspaceTransfer> {
        let layout = self.session_export_layout(id, executor)?;
        let root = targets::worker_root(&layout.backend, id)?;
        Ok(WorkspaceTransfer {
            assessment,
            source: Box::new(self.state.sessions[id].clone()),
            source_stage: Path::new(&root).join(format!("move-{operation_id}")),
            controller_stage: mj_core::config::data_dir()
                .join("moves")
                .join(operation_id)
                .join("workspace"),
            phase: WorkspaceTransferPhase::Planned,
        })
    }

    pub(super) fn capture_move_workspace(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        executor.begin_resumable_move_work()?;
        let result = self.capture_move_workspace_inner(operation, executor);
        executor.end_resumable_move_work()?;
        result
    }

    fn capture_move_workspace_inner(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let id = &operation.selection.session_id;
        let transfer = operation
            .workspace_transfer
            .as_ref()
            .context("Move transfer missing")?
            .clone();
        let layout = self.session_export_layout(id, executor)?;
        if transfer.phase == WorkspaceTransferPhase::Planned {
            // The target utility holds the stage's file lock before reusing
            // or replacing an incomplete capture, including after a lost ACK.
            utility(
                executor,
                &layout.backend,
                id,
                &WorkspaceCommand::Capture {
                    repositories: repositories(&layout),
                    selection: operation.selection.workspace.clone(),
                    destination: transfer.source_stage.clone(),
                },
            )?;
            operation.workspace_transfer.as_mut().unwrap().phase = WorkspaceTransferPhase::Captured;
            crate::database::save_move_operation(operation)?;
        }
        if operation.workspace_transfer.as_ref().unwrap().phase == WorkspaceTransferPhase::Captured
        {
            executor.notify_notice("Transferring workspace to temporary Move storage");
            copy_workspace(
                executor,
                &layout.backend,
                &transfer.source_stage,
                &transfer.controller_stage,
                false,
            )?;
            // Verification uses the same installed utility on the controller.
            let local = targets::TargetLocator::LocalBare {
                worker_root: mj_core::config::data_dir()
                    .join("workers")
                    .join(id)
                    .to_string_lossy()
                    .into_owned(),
            };
            utility(
                executor,
                &local,
                id,
                &WorkspaceCommand::Verify {
                    source: transfer.controller_stage.clone(),
                },
            )?;
            operation.workspace_transfer.as_mut().unwrap().phase =
                WorkspaceTransferPhase::Downloaded;
            crate::database::save_move_operation(operation)?;
        }
        if operation.workspace_transfer.as_ref().unwrap().phase
            == WorkspaceTransferPhase::Downloaded
        {
            let root = targets::worker_root(&layout.backend, id)?;
            super::super::execute_checked(
                executor,
                targets::command_on_locator(
                    &layout.backend,
                    id,
                    vec![
                        "sh".into(),
                        "-c".into(),
                        targets::stop_worker_daemon_script(&root),
                    ],
                    "stop sealed Move source worker, retaining workspace",
                )?,
            )?;
            operation.workspace_transfer.as_mut().unwrap().phase =
                WorkspaceTransferPhase::SourceStopped;
            crate::database::save_move_operation(operation)?;
        }
        Ok(())
    }

    pub(in crate::controller) fn restore_move_workspace(
        &self,
        id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        executor.begin_resumable_move_work()?;
        let result = self.restore_move_workspace_inner(id, executor);
        executor.end_resumable_move_work()?;
        result
    }

    fn restore_move_workspace_inner(
        &self,
        id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let mut operation =
            crate::database::load_move_operation(id)?.context("Move intent missing")?;
        let transfer = operation
            .workspace_transfer
            .as_ref()
            .context("Move transfer missing")?
            .clone();
        let layout = self.session_export_layout(id, executor)?;
        ensure!(
            self.state.sessions[id].target != transfer.source.target,
            "Move destination reuses the source environment; refusing to overwrite it"
        );
        let stage = PathBuf::from(targets::worker_root(&layout.backend, id)?)
            .join(format!("move-{}", operation.operation_id));
        super::super::execute_checked(
            executor,
            targets::command_on_locator(
                &layout.backend,
                id,
                vec![
                    "mkdir".into(),
                    "-p".into(),
                    stage.to_string_lossy().into_owned(),
                ],
                "create destination Move staging",
            )?,
        )?;
        copy_workspace(
            executor,
            &layout.backend,
            &stage,
            &transfer.controller_stage,
            true,
        )?;
        utility(
            executor,
            &layout.backend,
            id,
            &WorkspaceCommand::Restore {
                source: stage,
                repositories: repositories(&layout),
            },
        )?;
        operation.workspace_transfer.as_mut().unwrap().phase = WorkspaceTransferPhase::Restored;
        crate::database::save_move_operation(&operation)?;
        Ok(())
    }

    pub(super) fn finish_workspace_transfer(
        &mut self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let Some(transfer) = operation.workspace_transfer.as_ref().cloned() else {
            return Ok(());
        };
        if transfer.phase == WorkspaceTransferPhase::Ready {
            return Ok(());
        }
        let id = &operation.selection.session_id;
        ensure!(
            self.state.sessions[id].state == SessionState::Running
                && self.state.sessions[id].target != transfer.source.target,
            "Move destination is not independently ready; source retained"
        );
        if !operation.selection.workspace.exclusions.is_empty() {
            crate::database::retain_move_source(operation)?;
            if let Some(locator) = &transfer.source.target {
                let backend = super::super::backend::backend_locator(
                    locator,
                    &transfer.source,
                    &self.config,
                )?;
                super::super::execute_checked(
                    executor,
                    targets::command_on_locator(
                        &backend,
                        id,
                        vec![
                            "rm".into(),
                            "-rf".into(),
                            "--".into(),
                            transfer.source_stage.to_string_lossy().into_owned(),
                            transfer
                                .source_stage
                                .with_extension("move-lock")
                                .to_string_lossy()
                                .into_owned(),
                        ],
                        "remove completed Move source staging",
                    )?,
                )?;
            }
            executor.notify_notice(&format!("Excluded files remain in the stopped source. Inspect with mj move-sources --session {id}; remove with mj move-sources --session {id} --cleanup {} --yes", operation.operation_id));
        } else {
            // Operate on the saved resource identity, never on the destination.
            // cleanup_stopped_target persists, so use the target/checkout
            // primitives directly and leave the active session row untouched.
            let source = &transfer.source;
            if let Some(locator) = &source.target {
                let backend =
                    super::super::backend::backend_locator(locator, source, &self.config)?;
                targets::retire_move_target_plan(&backend, id)?.execute(executor)?;
            }
            if let Some(checkout) = &source.managed_worktree {
                super::super::worktree::retire_managed_worktree(executor, checkout)?;
            }
        }
        if transfer.controller_stage.exists() {
            std::fs::remove_dir_all(&transfer.controller_stage)?;
        }
        operation.workspace_transfer.as_mut().unwrap().phase = WorkspaceTransferPhase::Ready;
        crate::database::save_move_operation(operation)?;
        Ok(())
    }

    pub(in crate::controller) fn rollback_move_destination(
        &mut self,
        operation: &MoveOperation,
        error: anyhow::Error,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<anyhow::Error> {
        let transfer = operation
            .workspace_transfer
            .as_ref()
            .context("Move transfer missing")?;
        let id = &operation.selection.session_id;
        let current = &self.state.sessions[id];
        if current.target == transfer.source.target {
            return self.retain_failed_in_place_move(id, &transfer.source, error);
        }
        if let Some(locator) = &current.target {
            let backend = super::super::backend::backend_locator(locator, current, &self.config)?;
            targets::retire_move_target_plan(&backend, id)?.execute(executor)?;
        }
        if let Some(checkout) = &current.managed_worktree
            && Some(checkout) != transfer.source.managed_worktree.as_ref()
        {
            super::super::worktree::retire_managed_worktree(executor, checkout)?;
        }
        let mut source = (*transfer.source).clone();
        source.state = SessionState::Error;
        source.last_error = Some(format!(
            "{error:#}; source environment retained for Move retry"
        ));
        source.updated_at = now();
        crate::database::save_resumed_session(&source, None)?;
        self.state.sessions.insert(id.clone(), source);
        Ok(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs a POSIX shell.
    #[cfg(unix)]
    #[test]
    fn concurrent_move_helper_uploads_publish_complete_files_without_shared_staging() {
        let directory = tempfile::tempdir().unwrap();
        let id = "move-helper-upload";
        let root = directory.path().join(id);
        std::fs::create_dir(&root).unwrap();
        let binary = directory.path().join("binary");
        let body = vec![0x5a; 512 * 1024 + 17];
        std::fs::write(&binary, &body).unwrap();
        let helper = root
            .join("move-helper-build")
            .to_string_lossy()
            .into_owned();
        let backend = targets::TargetLocator::LocalBare {
            worker_root: root.to_string_lossy().into_owned(),
        };
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                install_helper(&targets::ProcessExecutor, &backend, id, &binary, &helper)
            });
            let second = scope.spawn(|| {
                install_helper(&targets::ProcessExecutor, &backend, id, &binary, &helper)
            });
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });
        assert_eq!(std::fs::read(&helper).unwrap(), body);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    }

    #[test]
    fn shared_filesystem_capacity_counts_allocations_once_and_changes_with_selection() {
        struct Disk;
        impl CommandExecutor for Disk {
            fn execute(&self, _: &CommandSpec) -> Result<targets::CommandOutput> {
                Ok(targets::CommandOutput {
                    status: 0,
                    stdout: b"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/test 2000000 0 1500000 0% /\n".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        let path = WorkspacePath {
            repository: "project".into(),
            path: "large".into(),
        };
        let mut assessment = WorkspaceAssessment {
            files: vec![WorkspaceFile {
                location: path.clone(),
                bytes: 400_000_000,
            }],
            ..Default::default()
        };
        for (location, allocation, copies) in [
            ("Source", "source", 1),
            ("Controller", "controller", 1),
            ("Destination", "destination", 2),
            ("Destination backing", "destination", 2),
        ] {
            storage_probe(
                &Disk,
                CommandSpec::new("df", ["-Pk"]),
                location,
                allocation,
                copies,
                &mut assessment,
            );
        }
        assert_eq!(assessment.storage.len(), 1);
        assert_eq!(assessment.storage[0].copies, 4);
        let mut selection = WorkspaceSelection::default();
        assert!(
            assessment
                .selection_problem(&selection)
                .unwrap()
                .contains("free")
        );
        selection.set_included(&assessment, &path, false);
        assert!(assessment.selection_problem(&selection).is_none());
    }

    // Needs a POSIX shell and rsync.
    #[cfg(unix)]
    #[test]
    fn interrupted_copy_retries_and_exclusions_retain_only_the_source() {
        use crate::controller::test_support::{
            IsolatedTest, checkpoint_test_session, committed_repository,
            resume_compatibility_config,
        };
        if std::env::var_os("MJ_MOVE_TRANSFER_LIFECYCLE_TEST").is_none() {
            let directory = tempfile::tempdir().unwrap();
            let name = crate::controller::test_support::test_name(
                module_path!(),
                "interrupted_copy_retries_and_exclusions_retain_only_the_source",
            );
            IsolatedTest::new(name)
                .isolated_store(directory.path())
                .env("MJ_MOVE_TRANSFER_LIFECYCLE_TEST", "1")
                .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        struct Executor(std::sync::atomic::AtomicBool);
        impl CommandExecutor for Executor {
            fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
                if command.purpose == "transfer Move workspace with resumable files"
                    && self.0.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    anyhow::bail!("interrupted copy");
                }
                targets::ProcessExecutor.execute(command)
            }
            fn execute_with_stdin(
                &self,
                _: &CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> Result<targets::CommandOutput> {
                let request = serde_json::from_reader(input)?;
                Ok(targets::CommandOutput {
                    status: 0,
                    stdout: serde_json::to_vec(&mj_worker::move_workspace::execute(request)?)?,
                    stderr: Vec::new(),
                })
            }
        }
        let source = committed_repository();
        let destination = committed_repository();
        std::fs::write(source.path().join("selected"), vec![7; 256 * 1024 + 1]).unwrap();
        std::fs::write(source.path().join("excluded"), b"retain me").unwrap();
        let id = "move-transfer-lifecycle";
        let mut session = checkpoint_test_session(id);
        session.project_directory = Some(source.path().into());
        session.target_template_id = "local-bare".into();
        session.state = SessionState::Closing;
        let source_root = mj_core::config::data_dir().join("source-workers").join(id);
        std::fs::create_dir_all(&source_root).unwrap();
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: source_root.clone(),
        });
        let mut operation = super::super::tests::source_recovery_operation(&session);
        operation
            .selection
            .workspace
            .exclusions
            .push(WorkspacePath {
                repository: "project".into(),
                path: "excluded".into(),
            });
        operation.phase = MovePhase::ResumingDestination;
        let executor = Executor(std::sync::atomic::AtomicBool::new(true));
        let mut controller = Controller {
            config: resume_compatibility_config(),
            state: mj_core::state::State::default(),
        };
        controller.state.sessions.insert(id.into(), session.clone());
        crate::database::save_session(&session).unwrap();
        let assessment = mj_worker::move_workspace::inspect(&[WorkspaceRepository {
            id: "project".into(),
            root: source.path().into(),
        }])
        .unwrap();
        operation.workspace_transfer = Some(
            controller
                .new_workspace_transfer(id, &operation.operation_id, assessment, &executor)
                .unwrap(),
        );
        crate::database::save_move_operation(&operation).unwrap();
        assert!(
            controller
                .capture_move_workspace(&mut operation, &executor)
                .is_err()
        );
        operation = crate::database::load_move_operation(id).unwrap().unwrap();
        assert_eq!(
            operation.workspace_transfer.as_ref().unwrap().phase,
            WorkspaceTransferPhase::Captured
        );
        controller
            .capture_move_workspace(&mut operation, &executor)
            .unwrap();
        let destination_root = mj_core::config::data_dir()
            .join("destination-workers")
            .join(id);
        std::fs::create_dir_all(&destination_root).unwrap();
        let destination_session = controller.state.sessions.get_mut(id).unwrap();
        destination_session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: destination_root.clone(),
        });
        destination_session.project_directory = Some(destination.path().into());
        controller.restore_move_workspace(id, &executor).unwrap();
        operation = crate::database::load_move_operation(id).unwrap().unwrap();
        controller.state.sessions.get_mut(id).unwrap().state = SessionState::Running;
        controller
            .finish_workspace_transfer(&mut operation, &executor)
            .unwrap();
        assert!(destination.path().join("selected").exists());
        assert!(!destination.path().join("excluded").exists());
        assert!(source.path().join("excluded").exists());
        assert_eq!(crate::database::retained_move_sources(id).unwrap().len(), 1);
        assert!(
            !operation
                .workspace_transfer
                .as_ref()
                .unwrap()
                .controller_stage
                .exists()
        );
        controller
            .cleanup_retained_move_source(id, &operation.operation_id, &executor)
            .unwrap();
        assert!(
            crate::database::retained_move_sources(id)
                .unwrap()
                .is_empty()
        );
        assert!(!source_root.exists());
        assert!(destination_root.exists());
        assert!(
            source.path().join("excluded").exists(),
            "user-owned checkout must survive cleanup"
        );
    }

    // Needs a POSIX shell and rsync.
    #[cfg(unix)]
    #[test]
    fn rsync_namespace_adapter_streams_large_files_with_spaces_in_paths() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source with spaces");
        let destination = directory.path().join("destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let payload = vec![0x71; 256 * 1024 + 13];
        std::fs::write(source.join("large file"), &payload).unwrap();
        let wrapper = rsync_shell(&CommandSpec::new(
            "sh",
            ["-c", "exec \"$@\"", "namespace", "rsync"],
        ))
        .unwrap();
        super::super::super::execute_checked(
            &targets::ProcessExecutor,
            CommandSpec::new(
                "rsync",
                vec![
                    "--recursive".into(),
                    "--protect-args".into(),
                    "--rsh".into(),
                    wrapper
                        .path()
                        .join("move-rsync-shell")
                        .display()
                        .to_string(),
                    format!("move:{}/", source.display()),
                    format!("{}/", destination.display()),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("large file")).unwrap(),
            payload
        );
    }

    // Needs a POSIX shell and rsync.
    #[cfg(unix)]
    #[test]
    fn resumable_local_copy_preserves_large_payloads_and_repairs_partial_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let bytes = vec![0x35; 192 * 1024 + 17];
        std::fs::write(source.join("payload"), &bytes).unwrap();
        std::fs::write(destination.join("payload"), &bytes[..70_000]).unwrap();
        let backend = targets::TargetLocator::LocalBare {
            worker_root: directory.path().display().to_string(),
        };
        copy_workspace(
            &targets::ProcessExecutor,
            &backend,
            &source,
            &destination,
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(destination.join("payload")).unwrap(), bytes);
        copy_workspace(
            &targets::ProcessExecutor,
            &backend,
            &source,
            &destination,
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(destination.join("payload")).unwrap(), bytes);
    }
}
