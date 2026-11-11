//! Move-only workspace transport. It never reads or writes a checkpoint payload.
use super::*;
use crate::controller::checkpoint::HOST_CHECKOUT_REPOSITORY;
use crate::targets::{self, CommandSpec};
use mj_core::move_workspace::*;
use std::path::Path;

fn repositories(
    layout: &super::super::checkpoint::SessionExportLayout,
) -> Vec<WorkspaceRepository> {
    layout
        .repositories
        .iter()
        .map(|repo| WorkspaceRepository {
            id: repo.id.clone(),
            root: mj_core::target_path::join(
                Path::new(&layout.workspace_root),
                &repo.relative_destination,
            ),
        })
        .collect()
}

fn utility(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    id: &str,
    command: &WorkspaceCommand,
) -> Result<serde_json::Value> {
    use super::super::worker_binary::{IMAGE_WORKER, WorkerInstall};
    let executable = match WorkerInstall::for_target(backend, executor)? {
        // The image's own worker is this build and needs no upload.
        WorkerInstall::Image => IMAGE_WORKER.to_owned(),
        WorkerInstall::Host(binary)
            if matches!(backend, targets::TargetLocator::LocalBare { .. }) =>
        {
            binary.to_string_lossy().into_owned()
        }
        WorkerInstall::Host(binary) => {
            let root = targets::worker_root(backend, id)?;
            let helper = format!(
                "{root}/move-helper-{}",
                mj_core::worker_build::WORKER_BUILD_ID
            );
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
        }
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

/// Options shared by GNU rsync, openrsync and older macOS rsync.
/// Preflight checks every option before interrupting the session.
const RSYNC_TRANSFER_OPTIONS: [&str; 5] = [
    "--recursive",
    "--times",
    "--perms",
    "--partial",
    "--partial-dir=.move-partial",
];

const RSYNC_REQUIREMENT: &str = "Move needs rsync with support for recursive transfers, \
permissions, timestamps and partial files (including --partial-dir); install rsync";

const CONTAINER_RSYNC_REQUIREMENT: &str = "Move needs rsync with support for recursive transfers, \
permissions, timestamps and partial files (including --partial-dir); add rsync to the container image";

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
///
/// If the SSH server turns the transport away, the whole transfer runs again
/// (see [`retry_refused_transport`]).
fn copy_workspace(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    remote: &Path,
    local: &Path,
    upload: bool,
) -> Result<()> {
    std::fs::create_dir_all(local)?;
    let local_path = format!("{}/", local.display());
    let base = targets::locator_command(backend, rsync_remote_arguments(remote));
    let command = CommandSpec::new("rsync", Vec::<String>::new())
        .purpose("transfer Move workspace with resumable files");
    let output = retry_refused_transport(
        executor,
        base.ssh_destination.as_deref(),
        &command.purpose,
        |walk| {
            let mut args: Vec<String> = RSYNC_TRANSFER_OPTIONS.map(String::from).into();
            // Rsync reads this ssh's stdout as its own protocol and appends its
            // server arguments to the remote line, so the session carries no
            // start marker.
            let session = base.open_ssh_session_without_marker(executor, walk)?;
            let wrapper = if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
                let remote_path = format!("{}/", remote.display());
                args.extend(if upload {
                    [local_path.clone(), remote_path]
                } else {
                    [remote_path, local_path.clone()]
                });
                None
            } else {
                let directory = rsync_shell(session.command())?;
                let path = directory.path().join("move-rsync-shell");
                args.extend(["--rsh".into(), path.to_string_lossy().into_owned()]);
                args.extend(if upload {
                    [local_path.clone(), "move:./".into()]
                } else {
                    ["move:./".into(), local_path.clone()]
                });
                Some(directory)
            };
            let mut attempt = command.clone();
            attempt.args = args;
            let output = executor.execute(&attempt);
            drop(wrapper);
            Ok((output?, session.into_parts().1))
        },
    )?;
    super::super::check_output(&command, output).map(|_| ())
}

/// Run a transfer whose `ssh` transport an SSH server may turn away, and run
/// it again when it does.
///
/// Rsync's `ssh` carries no start marker (its stdout is the rsync protocol),
/// so a refusal is read from the `ssh` messages rsync relays on stderr, and
/// rsync exits with a status of its own (12). A transfer is idempotent and
/// resumes from its partial files, so running it again is safe even when
/// the stderr reading was a hangup in the middle. `attempt` leases the session
/// from the walk it is given, runs the transfer and returns its output with the
/// lease; the lease records what the refusal means, and the next attempt
/// leases the next shard, or waits and tries the same way as every other
/// refused SSH command. A refusal that outlasts the retry bound fails the transfer
/// with the error naming its causes. Returns the last output.
fn retry_refused_transport(
    executor: &impl CommandExecutor,
    destination: Option<&str>,
    purpose: &str,
    mut attempt: impl FnMut(
        &mut targets::SshWalk,
    ) -> Result<(targets::CommandOutput, Option<targets::SshSessionLease>)>,
) -> Result<targets::CommandOutput> {
    let mut retry = targets::SshRefusalRetry::new();
    loop {
        let (output, lease) = attempt(retry.walk())?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let refusal = lease
            .as_ref()
            .filter(|_| output.status != 0)
            .and_then(|lease| {
                let refusal = targets::ssh_refusal_in_stderr(&stderr)?;
                Some((refusal, lease.note_refusal(refusal)))
            });
        drop(lease);
        let Some((refusal, walks_on)) = refusal else {
            return Ok(output);
        };
        let delay = retry
            .next(
                refusal,
                walks_on,
                destination.unwrap_or_default(),
                purpose,
                &stderr,
            )
            .with_context(|| format!("{purpose} failed"))?;
        ensure!(
            targets::sleep_unless_cancelled(delay, &|| executor.cancellation_requested()),
            "operation cancelled while {purpose}"
        );
    }
}

/// Keep the staging path in the locator's quoted argv, never in rsync's
/// remote command suffix. SSH parses that suffix as shell text; container
/// exec uses argv directly. Both receive only rsync's flags and `./` there.
fn rsync_remote_arguments(remote: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "cd -- \"$1\" && shift && exec rsync \"$@\"".into(),
        "move-rsync".into(),
        remote.to_string_lossy().into_owned(),
    ]
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
        crate::worker_lifecycle::run_blocking(
            session_id,
            "cleanup retained move source",
            executor,
            || {
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
                if let mj_core::state::Checkout::ManagedWorktree {
                    worktree: checkout, ..
                } = source.checkout()
                {
                    ensure!(
                        !self.state.sessions.keys().any(|session_id| {
                            matches!(
                                self.state.checkout(session_id),
                                Ok(mj_core::state::Checkout::ManagedWorktree {
                                    worktree: other,
                                    ..
                                }) if other == checkout
                            )
                        }),
                        "retained checkout is still referenced by a session"
                    );
                }
                let environment = crate::database::retained_move_environment(operation_id)?
                    .context("retained Move source has no recorded environment owner")?;
                super::super::environment::retire_owned_environment(
                    environment,
                    &self.config,
                    executor,
                    super::super::BranchDisposition::Keep,
                    super::super::CheckoutDisposition::Remove,
                )?;
                crate::database::forget_retained_move_source(operation_id)
            },
        )
    }

    /// Each repository of a Move's workspace transfer, under the id the source
    /// captured it as, with the repository of the session's accepted bundle
    /// that receives it on a destination that clones the bundle.
    ///
    /// The bundle names every destination folder, so the clone made while
    /// provisioning and the restore that fills it reach the same directory,
    /// the one [`Self::session_export_layout`] reports afterwards. A source
    /// that ran in a host checkout captured its one repository as
    /// [`HOST_CHECKOUT_REPOSITORY`], whatever the bundle calls it; moving into
    /// a bundle workspace makes that checkout the bundle's primary repository,
    /// as every raw-to-workspace conversion does.
    fn move_destination_repositories(
        &self,
        id: &str,
        transfer: &WorkspaceTransfer,
    ) -> Result<(
        &mj_core::config::ProjectBundle,
        Vec<(String, &mj_core::config::ProjectRepository)>,
    )> {
        let bundle = self.move_session_bundle(id)?;
        let repositories = if self.move_source_is_host_checkout(id, transfer)? {
            let primary = bundle.primary().context("bundle primary is missing")?;
            vec![(HOST_CHECKOUT_REPOSITORY.to_owned(), primary)]
        } else {
            bundle
                .repositories
                .iter()
                .map(|repository| (repository.id.clone(), repository))
                .collect()
        };
        Ok((bundle, repositories))
    }

    /// The id under which a Move's source captured the one repository that a
    /// host checkout destination holds.
    ///
    /// A host checkout's layout calls that repository
    /// [`HOST_CHECKOUT_REPOSITORY`]. A source that also ran in a host checkout
    /// captured it under the same id. A source that ran in a bundle workspace
    /// captured it under the bundle's id; the bundle has exactly one
    /// repository, its primary, as every workspace-to-raw conversion requires.
    fn move_host_checkout_source_id(
        &self,
        id: &str,
        transfer: &WorkspaceTransfer,
    ) -> Result<String> {
        if self.move_source_is_host_checkout(id, transfer)? {
            return Ok(HOST_CHECKOUT_REPOSITORY.to_owned());
        }
        Ok(self.move_session_bundle(id)?.primary_repo.clone())
    }

    /// Whether a Move's source ran in a host checkout, which is what decides
    /// how its layout named the repositories it captured.
    fn move_source_is_host_checkout(&self, id: &str, transfer: &WorkspaceTransfer) -> Result<bool> {
        Ok(self
            .state
            .checkout_for_record(id, &transfer.source)?
            .project_directory()
            .is_some())
    }

    fn move_session_bundle(&self, id: &str) -> Result<&mj_core::config::ProjectBundle> {
        self.state
            .sessions
            .get(id)
            .with_context(|| format!("unknown session {id}"))?
            .project_bundle(&self.config)
            .context("session bundle is missing")
    }

    /// What a Move that rebuilds into a bundle workspace clones: the folders
    /// come from the session's bundle, and only the clone and push URLs come
    /// from the handoff, which recorded the source's remotes.
    pub(in crate::controller) fn move_destination_bundle(
        &self,
        id: &str,
    ) -> Result<Option<targets::ProjectBundleSpec>> {
        let Some(operation) = crate::database::load_move_operation(id)? else {
            return Ok(None);
        };
        let Some(transfer) = operation.workspace_transfer.as_ref() else {
            return Ok(None);
        };
        if operation.phase != MovePhase::ResumingDestination {
            return Ok(None);
        }
        let handoff = operation.handoff.as_ref().context("Move handoff missing")?;
        let archive = super::super::resume::verify_resume_checkpoint(id, handoff)?;
        let (bundle, destinations) = self.move_destination_repositories(id, transfer)?;
        let primary = bundle
            .primary()
            .context("bundle primary is missing")?
            .destination
            .to_string_lossy()
            .into_owned();
        let repositories = archive
            .manifest
            .repositories
            .iter()
            .map(|repo| {
                let metadata = &repo.metadata;
                let (_, repository) = destinations
                    .iter()
                    .find(|(source_id, _)| *source_id == metadata.id)
                    .with_context(|| {
                        format!(
                            "the Move handoff's repository {:?} has no counterpart in the session's project, so Mjolnir cannot tell which folder to clone it into",
                            metadata.id
                        )
                    })?;
                mj_core::remote_git::validate_network_url(&metadata.origin)?;
                Ok(targets::RepositorySpec {
                    url: Some(metadata.origin.clone()),
                    push_urls: metadata.push_urls.clone(),
                    destination: repository.destination.to_string_lossy().into_owned(),
                    git_ref: None,
                    reference: None,
                    history_cutoff: None,
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
            && operation.holds_source_environment()
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
        // A sandbox is a VM behind the engine: the Move transport (an rsync
        // container run on the destination, or a mounted staging directory)
        // has no equivalent there yet. Say so now instead of failing in the
        // middle of a transfer.
        let destination = selection
            .target_template_id
            .as_deref()
            .and_then(|target| self.config.targets.get(target));
        if matches!(layout.backend, targets::TargetLocator::SshNitro { .. })
            || matches!(
                destination,
                Some(mj_core::config::TargetTemplate::SshNitro { .. })
            )
        {
            assessment.blockers.push(
                "Move does not support Nitro sandbox targets yet. Close this session and start a \
                 new one on the other target; the workspace stays in the checkpoint."
                    .into(),
            );
            return Ok(assessment);
        }
        let source_requirement = if targets::is_container(&layout.backend) {
            CONTAINER_RSYNC_REQUIREMENT
        } else {
            RSYNC_REQUIREMENT
        };
        for (command, requirement) in [
            (
                CommandSpec::new("rsync", rsync_probe_arguments())
                    .purpose("check controller Move transport"),
                RSYNC_REQUIREMENT,
            ),
            (
                targets::command_on_locator(
                    &layout.backend,
                    id,
                    rsync_probe(),
                    "check source Move transport",
                )?,
                source_requirement,
            ),
        ] {
            match executor.execute(&command) {
                Ok(output) if output.status == 0 => {}
                Ok(output) => assessment.blockers.push(format!(
                    "{}: {requirement}: {}",
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
        let target = self.config.ensure_target_enabled(
            selection
                .target_template_id
                .as_deref()
                .context("Move target missing")?,
        )?;
        if matches!(target, mj_core::config::TargetTemplate::LocalBare)
            && matches!(layout.backend, targets::TargetLocator::LocalBare { .. })
        {
            assessment.blockers.push("These local targets share the source worker storage, so no files can be transferred between them.".into());
        }
        if let mj_core::config::TargetTemplate::SshBare { ssh, .. } = target
            && let targets::TargetLocator::SshBare {
                ssh: source_ssh, ..
            } = &layout.backend
            && *source_ssh == targets::SshTarget::from(ssh)
        {
            assessment.blockers.push("These SSH targets share the source worker storage, so no files can be transferred between them.".into());
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
            mj_core::config::TargetTemplate::SshAppleContainer { ssh, container } => {
                (Some(ssh), Some("container"), Some(container.image.as_str()))
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
            Ok(output) if output.status == 0 => {}
            Ok(output) => assessment.blockers.push(format!(
                "Destination: {}. Update the target image or install rsync there: {}",
                if destination_engine.is_some() {
                    CONTAINER_RSYNC_REQUIREMENT
                } else {
                    RSYNC_REQUIREMENT
                },
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(error) => assessment
                .blockers
                .push(format!("Destination transport check failed: {error:#}")),
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
        } else {
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
            source_stage: mj_core::target_path::join(
                Path::new(&root),
                Path::new(&format!("move-{operation_id}")),
            ),
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
        executor.begin_resumable_work()?;
        let result = self.capture_move_workspace_inner(operation, executor);
        executor.end_resumable_work("Move control transition")?;
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
        executor.begin_resumable_work()?;
        let result = self.restore_move_workspace_inner(id, executor);
        executor.end_resumable_work("Move control transition")?;
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
        let stage = mj_core::target_path::join(
            Path::new(&targets::worker_root(&layout.backend, id)?),
            Path::new(&format!("move-{}", operation.operation_id)),
        );
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
        // The transfer names each repository as the source did. A host
        // checkout holds one repository; a bundle workspace was cloned where
        // `move_destination_bundle` said, under the bundle's ids.
        let repositories = if self.state.checkout(id)?.project_directory().is_some() {
            let source_id = self.move_host_checkout_source_id(id, &transfer)?;
            repositories(&layout)
                .into_iter()
                .map(|repository| WorkspaceRepository {
                    id: source_id.clone(),
                    root: repository.root,
                })
                .collect()
        } else {
            self.move_destination_repositories(id, &transfer)?
                .1
                .into_iter()
                .map(|(source_id, repository)| WorkspaceRepository {
                    id: source_id,
                    root: mj_core::target_path::join(
                        Path::new(&layout.workspace_root),
                        &repository.destination,
                    ),
                })
                .collect()
        };
        utility(
            executor,
            &layout.backend,
            id,
            &WorkspaceCommand::Restore {
                source: stage,
                repositories,
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
            // The accepted source owner survives publication of the destination.
            let environment = crate::database::move_source_environment(&operation.operation_id)?
                .context("Move source has no recorded environment owner")?;
            super::super::environment::retire_owned_environment(
                environment,
                &self.config,
                executor,
                super::super::BranchDisposition::Keep,
                super::super::CheckoutDisposition::Remove,
            )?;
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
        crate::worker_lifecycle::run_blocking(
            &operation.selection.session_id,
            "rollback move destination",
            executor,
            || {
                let transfer = operation
                    .workspace_transfer
                    .as_ref()
                    .context("Move transfer missing")?;
                let id = &operation.selection.session_id;
                let current = &self.state.sessions[id];
                if current.target == transfer.source.target {
                    return self.retain_failed_in_place_move(id, &transfer.source, error);
                }
                let prepared = operation
                    .prepared_destination
                    .as_ref()
                    .and_then(|d| d.target())
                    .cloned();
                if prepared.is_some() {
                    let mut saved = crate::database::load_move_operation(id)?
                        .context("Move cleanup intent missing")?;
                    self.cleanup_prepared_move_destination(&mut saved, executor)?;
                }
                if let Some(locator) = &current.target
                    && Some(locator) != prepared.as_ref()
                {
                    super::super::environment::retire_session_environment(
                        current,
                        &self.config,
                        executor,
                        super::super::BranchDisposition::Keep,
                    )?;
                    // The source becomes the session again, so it is the
                    // placement whose build state must survive.
                }
                let mut source = (*transfer.source).clone();
                source.state = SessionState::Error;
                source.last_error = Some(format!(
                    "{error:#}; source environment retained for Move retry"
                ));
                source.updated_at = now();
                let environment =
                    crate::database::move_source_environment(&operation.operation_id)?
                        .context("Move rollback lost its source environment")?;
                crate::database::restore_resumed_session(&source, &environment.id)?;
                self.state.sessions.insert(id.clone(), source);
                Ok(error)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: 049656544: retained-source cleanup deleted a referenced checkout before refusing.
    #[cfg(unix)]
    #[test]
    fn retained_source_reference_refusal_preserves_checkout_and_worker_files() {
        use crate::controller::test_support::{
            IsolatedTest, committed_repository, managed_worktree_session,
            resume_compatibility_config, test_name,
        };
        const TEST: &str = "retained_source_reference_refusal_preserves_checkout_and_worker_files";
        const CHILD: &str = "MJ_TEST_RETAINED_SOURCE_REFERENCE";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .isolated_store(directory.path())
                .env(CHILD, "1")
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let repository = committed_repository();
        let directory = tempfile::tempdir().unwrap();
        let id = "11111111111111111111111111111111";
        let mut source = managed_worktree_session(repository.path(), id);
        source.target_template_id = "local-bare".into();
        source.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join(id),
        });
        std::fs::create_dir_all(directory.path().join(id)).unwrap();
        std::fs::write(directory.path().join(id).join("worker-evidence"), "keep").unwrap();
        let checkout = source
            .managed_worktree
            .as_ref()
            .unwrap()
            .worktree_root
            .clone();
        std::fs::write(checkout.join("work-in-progress"), "keep").unwrap();
        crate::database::save_session(&source).unwrap();
        let mut operation = super::super::tests::source_recovery_operation(&source);
        operation.workspace_transfer = Some(WorkspaceTransfer {
            assessment: Default::default(),
            source: Box::new(source.clone()),
            source_stage: directory.path().join("source-stage"),
            controller_stage: directory.path().join("controller-stage"),
            phase: WorkspaceTransferPhase::Ready,
        });
        crate::database::save_move_operation(&operation).unwrap();
        crate::database::retain_move_source(&operation).unwrap();
        let mut destination = source.clone();
        destination.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join("destination").join(id),
        });
        crate::database::save_session(&destination).unwrap();
        let controller = Controller {
            config: resume_compatibility_config(),
            state: crate::database::load_state().unwrap(),
        };
        let error = controller
            .cleanup_retained_move_source(id, &operation.operation_id, &targets::ProcessExecutor)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retained checkout is still referenced"),
            "{error:#}"
        );
        assert!(checkout.join("work-in-progress").exists());
        assert!(directory.path().join(id).join("worker-evidence").exists());
        assert_eq!(crate::database::retained_move_sources(id).unwrap().len(), 1);
    }

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
    // Hard-won: #1383: stock macOS rsync rejects --protect-args.
    #[cfg(unix)]
    #[test]
    fn rsync_namespace_adapter_streams_large_files_with_spaces_in_paths() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory
            .path()
            .join("source with spaces, 'quotes', $(exit 91); &");
        let destination = directory
            .path()
            .join("destination with spaces and 'quotes'");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let payload = vec![0x71; 256 * 1024 + 13];
        std::fs::write(source.join("large file"), &payload).unwrap();
        for shell_boundary in [false, true] {
            for upload in [false, true] {
                let remote = if upload { &destination } else { &source };
                let remote_args = rsync_remote_arguments(remote);
                let command = if shell_boundary {
                    // OpenSSH joins its command arguments with spaces before
                    // the remote login shell interprets them. Model that here.
                    CommandSpec::new(
                        "sh",
                        vec![
                            "-c".into(),
                            "exec sh -c \"$*\"".into(),
                            "ssh".into(),
                            targets::remote_command_line(&remote_args),
                        ],
                    )
                } else {
                    let mut args = remote_args.into_iter();
                    CommandSpec::new(args.next().unwrap(), args)
                };
                let wrapper = rsync_shell(&command).unwrap();
                let mut args: Vec<String> = RSYNC_TRANSFER_OPTIONS.map(String::from).into();
                args.extend([
                    "--rsh".into(),
                    wrapper
                        .path()
                        .join("move-rsync-shell")
                        .display()
                        .to_string(),
                ]);
                args.extend(if upload {
                    [format!("{}/", source.display()), "move:./".into()]
                } else {
                    ["move:./".into(), format!("{}/", destination.display())]
                });
                super::super::super::execute_checked(
                    &targets::ProcessExecutor,
                    CommandSpec::new("rsync", args),
                )
                .unwrap();
                assert_eq!(
                    std::fs::read(destination.join("large file")).unwrap(),
                    payload
                );
                std::fs::remove_file(destination.join("large file")).unwrap();
            }
        }
    }

    /// Rsync exits 12 whatever its `ssh` transport reports, and its `ssh`
    /// carries no start marker, so the refusals are read from the stderr it
    /// relays. A session the server refuses (`MaxSessions`) runs the whole
    /// transfer again on the next shard at once; a connection dropped before
    /// authentication runs it again on the same shard, after a wait; a
    /// failure that shows no refusal is final.
    // Hard-won: 75da0c2b0: a refused SSH session failed the whole Move, because the rsync transport was untagged and never retried.
    #[cfg(unix)]
    #[test]
    fn a_move_transfer_the_ssh_server_refused_runs_again_on_the_next_shard() {
        use mj_core::targets::{
            CommandOutput, SshSessions, SshSharingForTest, set_ssh_connection_sharing_for_test,
            set_ssh_retry_backoff_for_test,
        };
        const SESSION_REFUSED: &str = "mux_client_request_session: session request failed: \
            Session open refused by peer\nrsync: connection unexpectedly closed \
            (0 bytes received so far) [Receiver]\nrsync error: error in rsync protocol data \
            stream (code 12)";
        const DROPPED: &str = "kex_exchange_identification: Connection closed by 10.0.0.1 port 22\n\
            rsync error: error in rsync protocol data stream (code 12)";
        let sockets = tempfile::tempdir_in("/tmp").unwrap();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            sockets.path().to_path_buf(),
        )));
        set_ssh_retry_backoff_for_test(Some(std::time::Duration::from_millis(5)));
        let ssh = targets::SshTarget {
            destination: "move-refused-host".into(),
            ssh_args: Vec::new(),
        };
        let run = |replies: &[(i32, &str)]| {
            let mut shards = Vec::new();
            let mut replies = replies.iter();
            let output = retry_refused_transport(
                &targets::ProcessExecutor,
                Some(&ssh.destination),
                "transfer Move workspace with resumable files",
                |walk| {
                    let lease = SshSessions::lease_probe(&ssh, walk);
                    let name = lease.control_path().unwrap().file_name().unwrap();
                    shards.push(
                        name.to_string_lossy()
                            .rsplit('-')
                            .next()
                            .unwrap()
                            .to_owned(),
                    );
                    let &(status, stderr) = replies.next().expect("an unexpected extra attempt");
                    Ok((
                        CommandOutput {
                            status,
                            stdout: Vec::new(),
                            stderr: stderr.as_bytes().to_vec(),
                        },
                        Some(lease),
                    ))
                },
            );
            (output.unwrap().status, shards)
        };
        let refused = run(&[
            (12, SESSION_REFUSED),
            (12, DROPPED),
            (12, SESSION_REFUSED),
            (0, ""),
        ]);
        let failed = run(&[(
            12,
            "rsync: write failed on \"/data/x\": No space left on device",
        )]);
        set_ssh_retry_backoff_for_test(None);
        set_ssh_connection_sharing_for_test(None);

        assert_eq!(refused.0, 0);
        assert_eq!(
            refused.1,
            ["0", "1", "1", "2"],
            "a refused session walks on; a dropped connection retries its shard"
        );
        assert_eq!(failed, (12, vec!["0".to_owned()]), "no refusal, no retry");
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
