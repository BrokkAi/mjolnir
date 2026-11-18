//! Move-only workspace transport. Only checkpoint metadata finalizes the checkout.
use super::*;
use crate::controller::checkpoint::HOST_CHECKOUT_REPOSITORY;
use crate::targets::{self, CommandSpec};
use mj_core::move_workspace::*;
use mj_core::targets::SshTrafficClass;
use std::path::{Path, PathBuf};

fn repositories(
    layout: &super::super::checkpoint::SessionExportLayout,
) -> Vec<WorkspaceRepository> {
    layout
        .repositories
        .iter()
        .map(|repo| WorkspaceRepository {
            base_commit: None,
            id: repo.id.clone(),
            root: mj_core::target_path::join(
                Path::new(&layout.workspace_root),
                &repo.relative_destination,
            ),
        })
        .collect()
}

fn move_handoff_base(
    source: &mj_core::state::SessionRecord,
    metadata: &mj_checkpoint::archive::RepositoryMetadata,
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
    executor: &impl CommandExecutor,
) -> Result<String> {
    if let Some(worktree) = source.managed_worktree.as_ref() {
        return super::super::worktree::managed_worktree_base_commit(worktree, executor);
    }
    if metadata.remote_workspace {
        return Ok(metadata.base_commit.clone());
    }
    if let Some(checkout) = source.checkout().project_directory() {
        return Ok(super::super::worktree::raw_checkout_base(git, checkout, &metadata.id)?.commit);
    }
    // Remote captures own their immutable base. Moves require an idle source,
    // whose worker is upgraded before capture; do not infer a base at restore.
    Ok(metadata.base_commit.clone())
}

fn record_move_handoff_bases(
    source: &mj_core::state::SessionRecord,
    handoff: &mj_core::state::CheckpointMetadata,
    backend: &targets::TargetLocator,
    executor: &(impl CommandExecutor + Sync),
) -> Result<mj_core::state::CheckpointMetadata> {
    let archive = super::super::resume::verify_resume_checkpoint(&source.id, handoff)?;
    let git = super::super::worktree::ExecutorGit::for_session(executor, backend, &source.id);
    let mut changed = Vec::new();
    for repository in &archive.manifest.repositories {
        let base = move_handoff_base(source, &repository.metadata, &git, executor)?;
        let mut metadata = repository.metadata.clone();
        metadata.base_commit = base;
        if source.checkout().project_directory().is_some() {
            metadata.remote_workspace = true;
        }
        if metadata != repository.metadata {
            changed.push(metadata);
        }
    }
    if changed.is_empty() {
        return Ok(handoff.clone());
    }
    ensure!(
        archive.manifest.repositories.len() == 1 && changed.len() == 1,
        "a host Move handoff must contain its one repository"
    );
    let metadata = changed.pop().unwrap();
    ensure!(
        metadata.review_refs_metadata_only,
        "Move handoff contains repository payloads"
    );
    let output = handoff
        .archive_path
        .parent()
        .context("Move handoff has no parent")?
        .join(format!(
            "{}-{}-{}.hel.zip",
            source.id,
            handoff.event_frontier,
            new_command_id("archive")?,
        ));
    super::super::resume::conversion_checkpoint(
        &handoff.archive_path,
        mj_checkpoint::archive::RepositorySnapshot {
            metadata,
            committed_bundle: Vec::new(),
            staged_patch: Vec::new(),
            unstaged_patch: Vec::new(),
            untracked_tar: Vec::new(),
            operation_payloads: None,
            linked_worktrees: Vec::new(),
        },
        &output,
    )
}

fn move_repository_base<'a>(
    manifest: &'a mj_checkpoint::archive::ArchiveManifest,
    id: &str,
) -> Result<&'a str> {
    let metadata = &manifest
        .repositories
        .iter()
        .find(|entry| entry.metadata.id == id)
        .with_context(|| format!("Move checkpoint repository {id} missing"))?
        .metadata;
    ensure!(
        !metadata.base_commit.is_empty(),
        "Move checkpoint repository {id} has no base"
    );
    Ok(&metadata.base_commit)
}

fn repositories_with_bases(
    mut repositories: Vec<WorkspaceRepository>,
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<Vec<WorkspaceRepository>> {
    for repository in &mut repositories {
        repository.base_commit = Some(move_repository_base(manifest, &repository.id)?.to_owned());
    }
    Ok(repositories)
}

fn finalize_move_repositories(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    id: &str,
    repositories: &[WorkspaceRepository],
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<()> {
    for repository in repositories {
        let base = move_repository_base(manifest, &repository.id)?;
        super::super::network_git::configure_workspace(
            executor,
            backend,
            id,
            &repository.root,
            base,
        )
        .with_context(|| {
            format!(
                "Move could not finalize repository '{}' with recorded base '{base}'; session stays on its source",
                repository.id
            )
        })?;
    }
    Ok(())
}

/// Where a Move's copied files wait on the destination host.
///
/// The durable Move owns this directory, keyed by its operation id. It sits
/// beside the worker roots in the host's state directory, not inside the
/// destination container: a failed attempt removes the container, and the
/// resumable copy must outlive that. Bare destinations restore from it
/// directly; container destinations load it into the container afterwards.
struct DestinationStage {
    /// The destination's host, addressed as a bare target.
    host: targets::TargetLocator,
    stage: PathBuf,
    /// Names what the stage holds; a stage whose marker differs is discarded.
    marker: PathBuf,
    container: Option<ContainerStage>,
}

struct ContainerStage {
    engine: &'static str,
    container_id: String,
    /// The Move's staging inside the container, the source of the restore.
    stage: PathBuf,
}

/// The destination host of a target template, addressed as a bare target.
fn template_host(template: &mj_core::config::TargetTemplate) -> Option<targets::TargetLocator> {
    use mj_core::config::TargetTemplate as T;
    let ssh = |ssh: &mj_core::config::SshConnection| targets::TargetLocator::SshBare {
        ssh: targets::SshTarget::from(ssh),
        workspace: String::new(),
        worker_id: None,
    };
    Some(match template {
        T::SshBare { ssh: target, .. }
        | T::SshPodman { ssh: target, .. }
        | T::SshDocker { ssh: target, .. }
        | T::SshAppleContainer { ssh: target, .. } => ssh(target),
        T::LocalBare | T::LocalPodman { .. } | T::LocalDocker { .. } | T::AppleContainer { .. } => {
            targets::TargetLocator::LocalBare {
                worker_root: String::new(),
            }
        }
        _ => return None,
    })
}

/// Where a host holds a Move's staging and marker: the state directory of
/// the host's account, or the controller's data directory for a local host.
fn host_stage_paths(host: &targets::TargetLocator, operation_id: &str) -> (PathBuf, PathBuf) {
    let root = match host {
        targets::TargetLocator::LocalBare { .. } => mj_core::config::data_dir().join("move-stage"),
        _ => PathBuf::from(".local/share/hel/move-stage"),
    };
    (
        root.join(operation_id),
        root.join(format!("{operation_id}.marker")),
    )
}

fn destination_stage(
    backend: &targets::TargetLocator,
    session_id: &str,
    operation_id: &str,
) -> Result<DestinationStage> {
    use targets::TargetLocator as L;
    let host = match backend {
        L::LocalBare { .. } | L::AwsEc2 { .. } | L::SshBare { .. } => backend.clone(),
        L::LocalPodman { .. } | L::LocalDocker { .. } | L::AppleContainer { .. } => L::LocalBare {
            worker_root: String::new(),
        },
        L::SshPodman { ssh, .. }
        | L::SshDocker { ssh, .. }
        | L::SshAppleContainer { ssh, .. }
        | L::SshNitro { ssh, .. } => L::SshBare {
            ssh: ssh.clone(),
            workspace: String::new(),
            worker_id: None,
        },
    };
    let (stage, marker) = host_stage_paths(&host, operation_id);
    let container = match (backend.container_engine(), backend) {
        (
            Some(engine),
            L::LocalPodman { container_id, .. }
            | L::LocalDocker { container_id, .. }
            | L::AppleContainer { container_id, .. }
            | L::SshPodman { container_id, .. }
            | L::SshDocker { container_id, .. }
            | L::SshAppleContainer { container_id, .. }
            | L::SshNitro { container_id, .. },
        ) => Some(ContainerStage {
            engine,
            container_id: container_id.clone(),
            // Where the copy always went inside the container. The session's
            // worker root does not exist before the worker is installed; the
            // load creates it, and the restore's helper lands beside it.
            stage: mj_core::target_path::join(
                Path::new(&targets::worker_root(backend, session_id)?),
                Path::new(&format!("move-{operation_id}")),
            ),
        }),
        _ => None,
    };
    Ok(DestinationStage {
        host,
        stage,
        marker,
        container,
    })
}

/// What the staged files are: the Move, its sealed checkpoint, and the exact
/// capture being copied. A retry that sealed again, or captured different
/// files, gets an empty stage instead of rsync's comparison against another
/// Move's files.
fn destination_stage_identity(
    operation: &MoveOperation,
    transfer: &WorkspaceTransfer,
) -> Result<String> {
    let checkpoint = operation
        .restore_artifact()
        .context("Move has no sealed checkpoint")?;
    let manifest = std::fs::read(transfer.controller_stage.join("move-workspace.json"))
        .context("read the captured workspace manifest")?;
    Ok(format!(
        "{} {} {}",
        operation.operation_id,
        checkpoint.sha256,
        lower_hex(Sha256::digest(manifest))
    ))
}

/// Keep the stage when it holds what this Move copies; otherwise empty it.
fn prepare_destination_stage(
    executor: &(impl CommandExecutor + Sync),
    destination: &DestinationStage,
    identity: &str,
) -> Result<()> {
    let script = r#"set -eu
stage=$1; marker=$2; identity=$3
if [ "$(cat -- "$marker" 2>/dev/null || true)" != "$identity" ]; then
    rm -rf -- "$stage" "$marker"
fi
# The capture holds private files; rsync restores the source's directory
# modes inside the stage, so the stage's parent is what keeps others out.
umask 077
parent=$(dirname -- "$stage")
mkdir -p -- "$parent"
chmod -- 700 "$parent"
mkdir -p -- "$stage"
printf '%s' "$identity" > "$marker"
"#;
    super::super::execute_checked(
        executor,
        targets::locator_command(
            SshTrafficClass::Control,
            &destination.host,
            vec![
                "sh".into(),
                "-c".into(),
                script.into(),
                "prepare-move-stage".into(),
                destination.stage.to_string_lossy().into_owned(),
                destination.marker.to_string_lossy().into_owned(),
                identity.into(),
            ],
        )
        .purpose("prepare destination Move staging"),
    )
    .map(|_| ())
}

/// Replace the container's staging with the host's complete copy. The
/// container is disposable and the copy is local to its host.
fn load_container_stage(
    executor: &(impl CommandExecutor + Sync),
    destination: &DestinationStage,
    container: &ContainerStage,
) -> Result<()> {
    let engine = targets::engine_shell_words(container.engine);
    let script = format!(
        r#"set -euo pipefail
{engine} exec "$2" rm -rf -- "$3"
{engine} exec "$2" mkdir -p -- "$3"
tar cf - -C "$1" . | {engine} exec -i "$2" tar xpf - --no-same-owner -C "$3"
"#
    );
    super::super::execute_checked(
        executor,
        targets::locator_command(
            SshTrafficClass::Bulk,
            &destination.host,
            vec![
                "bash".into(),
                "-c".into(),
                script,
                "load-move-stage".into(),
                destination.stage.to_string_lossy().into_owned(),
                container.container_id.clone(),
                container.stage.to_string_lossy().into_owned(),
            ],
        )
        .purpose("load destination Move staging into the container"),
    )
    .map(|_| ())
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
            let name = format!("move-helper-{}", mj_core::worker_build::WORKER_BUILD_ID);
            let helper = format!("{root}/{name}");
            let probe = executor.execute(&targets::command_on_locator(
                SshTrafficClass::Control,
                backend,
                id,
                vec!["test".into(), "-x".into(), helper.clone()],
                "check Move helper",
            )?)?;
            if probe.status != 0 {
                install_helper(executor, backend, id, &binary, &name)?;
            }
            helper
        }
    };
    let command_spec = targets::command_on_locator(
        SshTrafficClass::Bulk,
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
    if !output.stderr.is_empty() {
        tracing::warn!(session_id = id, detail = %String::from_utf8_lossy(&output.stderr),
            "Move workspace helper reported diagnostics");
    }
    serde_json::from_slice(&output.stdout).context("read Move workspace result")
}

fn install_helper(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    id: &str,
    binary: &Path,
    name: &str,
) -> Result<()> {
    use super::super::worker_binary::{WorkerInstall, install_worker_binary};
    install_worker_binary(
        executor,
        backend,
        id,
        &WorkerInstall::Host(binary.to_owned()),
        name,
    )
    .context("install immutable Move helper")
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

const DESTINATION_HOST_RSYNC_REQUIREMENT: &str = "Move needs rsync with support for recursive transfers, \
permissions, timestamps and partial files (including --partial-dir) on the destination host; install rsync there";

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
    let base = targets::locator_command(
        SshTrafficClass::Bulk,
        backend,
        rsync_remote_arguments(remote),
    );
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
        targets::ssh_command_owned(SshTrafficClass::Control, ssh, arguments)
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
                    SshTrafficClass::Control,
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
                SshTrafficClass::Control,
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
            let host = targets::locator_command(
                SshTrafficClass::Control,
                &layout.backend,
                vec!["true".into()],
            )
            .ssh_session;
            storage_probe(
                executor,
                at_host(host.as_ref().map(|request| &request.target), arguments)
                    .purpose("check source container backing storage"),
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
        // The copy lands on the destination host (rsync, and bash for loading
        // a container from it); a container image only has to carry tar.
        let at_destination_host = |arguments: Vec<String>| {
            if let Some(ssh) = destination_ssh {
                targets::ssh_command_owned(
                    SshTrafficClass::Control,
                    &targets::SshTarget::from(ssh),
                    arguments,
                )
            } else {
                let mut arguments = arguments;
                let program = arguments.remove(0);
                CommandSpec::new(program, arguments)
            }
        };
        let mut destination_checks = vec![(
            at_destination_host(rsync_probe()).purpose("check destination Move transport"),
            if destination_engine.is_some() {
                DESTINATION_HOST_RSYNC_REQUIREMENT
            } else {
                RSYNC_REQUIREMENT
            },
        )];
        if let (Some(engine), Some(image)) = (destination_engine, destination_image) {
            destination_checks.push((
                at_destination_host(vec![
                    "sh".into(),
                    "-c".into(),
                    "command -v bash && command -v tar".into(),
                ])
                .purpose("check destination Move shell"),
                "Move needs bash and tar on the destination host to load its container",
            ));
            destination_checks.push((
                at_destination_host(vec![
                    engine.into(),
                    "run".into(),
                    "--rm".into(),
                    "--entrypoint".into(),
                    "sh".into(),
                    image.into(),
                    "-c".into(),
                    "command -v tar".into(),
                ])
                .purpose("check destination Move container image"),
                "Move needs tar in the container image; add tar to the image",
            ));
        }
        for (command, requirement) in destination_checks {
            match executor.execute(&command) {
                Ok(output) if output.status == 0 => {}
                Ok(output) => assessment.blockers.push(format!(
                    "Destination: {requirement}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => assessment
                    .blockers
                    .push(format!("{}: {error:#}", command.purpose)),
            }
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
            // The resumable copy waits on the host's state directory until
            // the container holds its own.
            let staging_root = if destination_ssh.is_some() {
                PathBuf::from(".")
            } else {
                mj_core::config::data_dir()
            };
            storage_probe(
                executor,
                at_host(destination_ssh.as_ref(), storage_arguments(&staging_root))
                    .purpose("check destination host staging space"),
                "Destination host staging",
                // Its own allocation: on a shared filesystem it is a third
                // simultaneous copy, not the container's two again.
                "destination-host-staging",
                1,
                &mut assessment,
            );
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
            let destination = operation
                .selection
                .target_template_id
                .as_deref()
                .unwrap_or(&transfer.source.target_template_id);
            let target = self
                .config
                .targets
                .get(destination)
                .context("Move target missing")?;
            if !mj_core::config::is_bare_project_target(target) {
                let handoff = operation.handoff.as_ref().context("Move handoff missing")?;
                operation.handoff = Some(record_move_handoff_bases(
                    &transfer.source,
                    handoff,
                    &layout.backend,
                    executor,
                )?);
                crate::database::save_move_operation(operation)?;
            }
            let handoff = operation.handoff.as_ref().context("Move handoff missing")?;
            let archive = super::super::resume::verify_resume_checkpoint(id, handoff)?;
            let capture_repositories =
                repositories_with_bases(repositories(&layout), &archive.manifest)?;
            // The target utility holds the stage's file lock before reusing
            // or replacing an incomplete capture, including after a lost ACK.
            utility(
                executor,
                &layout.backend,
                id,
                &WorkspaceCommand::Capture {
                    repositories: capture_repositories,
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
            targets::stop_worker_plan(
                &layout.backend,
                id,
                "stop sealed Move source worker, retaining workspace",
            )?
            .execute(executor, &crate::controller::ControlWorker)?;
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
        // The copy waits on the destination host, outside the container that
        // a failed attempt removes, so a retry or the next daemon resumes it.
        let destination = destination_stage(&layout.backend, id, &operation.operation_id)?;
        let identity = destination_stage_identity(&operation, &transfer)?;
        prepare_destination_stage(executor, &destination, &identity)?;
        copy_workspace(
            executor,
            &destination.host,
            &destination.stage,
            &transfer.controller_stage,
            true,
        )?;
        // The restore's helper installs into the session's worker root, which
        // nothing else creates before the worker is installed.
        super::super::execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Control,
                &layout.backend,
                id,
                vec![
                    "mkdir".into(),
                    "-p".into(),
                    targets::worker_root(&layout.backend, id)?,
                ],
                "create destination worker root",
            )?,
        )?;
        let stage = match &destination.container {
            Some(container) => {
                load_container_stage(executor, &destination, container)?;
                container.stage.clone()
            }
            None => destination.stage.clone(),
        };
        // The transfer names each repository as the source did. A host
        // checkout holds one repository; a bundle workspace was cloned where
        // `move_destination_bundle` said, under the bundle's ids.
        let repositories: Vec<_> = if self.state.checkout(id)?.project_directory().is_some() {
            let source_id = self.move_host_checkout_source_id(id, &transfer)?;
            repositories(&layout)
                .into_iter()
                .map(|repository| WorkspaceRepository {
                    base_commit: None,
                    id: source_id.clone(),
                    root: repository.root,
                })
                .collect()
        } else {
            self.move_destination_repositories(id, &transfer)?
                .1
                .into_iter()
                .map(|(source_id, repository)| WorkspaceRepository {
                    base_commit: None,
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
                repositories: repositories.clone(),
            },
        )?;
        if self.state.checkout(id)?.project_directory().is_none() {
            let handoff = operation.handoff.as_ref().context("Move handoff missing")?;
            let archive = super::super::resume::verify_resume_checkpoint(id, handoff)?;
            finalize_move_repositories(
                executor,
                &layout.backend,
                id,
                &repositories,
                &archive.manifest,
            )?;
        }
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
                        SshTrafficClass::Control,
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
        self.remove_destination_stage(&operation.operation_id, id, executor);
        operation.workspace_transfer.as_mut().unwrap().phase = WorkspaceTransferPhase::Ready;
        crate::database::save_move_operation(operation)?;
        Ok(())
    }

    /// The Move finished: its host staging has no further use. A leftover
    /// does not undo the Move, so the failure is reported, not returned.
    fn remove_destination_stage(
        &self,
        operation_id: &str,
        id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) {
        let removed = self.session_export_layout(id, executor).and_then(|layout| {
            let destination = destination_stage(&layout.backend, id, operation_id)?;
            // The container's copy is a second full capture; it goes with
            // the host's once the restored workspace is the only one.
            if let Some(container) = &destination.container {
                super::super::execute_checked(
                    executor,
                    targets::locator_command(
                        SshTrafficClass::Control,
                        &layout.backend,
                        vec![
                            "rm".into(),
                            "-rf".into(),
                            "--".into(),
                            container.stage.to_string_lossy().into_owned(),
                        ],
                    )
                    .purpose("remove completed container Move staging"),
                )?;
            }
            super::super::execute_checked(
                executor,
                targets::locator_command(
                    SshTrafficClass::Control,
                    &destination.host,
                    vec![
                        "rm".into(),
                        "-rf".into(),
                        "--".into(),
                        destination.stage.to_string_lossy().into_owned(),
                        destination.marker.to_string_lossy().into_owned(),
                    ],
                )
                .purpose("remove completed destination Move staging"),
            )
        });
        if let Err(error) = removed {
            tracing::warn!(session_id = id, operation_id, error = %format!("{error:#}"),
                "could not remove the completed Move's staging on the destination host");
            executor.notify_notice("The Move finished, but its temporary staging on the destination host could not be removed");
        }
    }

    /// A Move that will never resume (destroyed, or replaced by another):
    /// remove its host staging. The host is the one the Move's own target
    /// names, so no record of where it copied is needed. The container copy
    /// went with its container. Best effort: the host may be gone with the
    /// environment, and a leftover must not block the destroy or the new Move.
    pub(in crate::controller) fn remove_abandoned_move_stage(
        &self,
        operation: &MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) {
        if operation.workspace_transfer.is_none() {
            return;
        }
        let Some(template) = operation
            .selection
            .target_template_id
            .as_deref()
            .and_then(|target| self.config.targets.get(target))
        else {
            return;
        };
        let backend = match template_host(template) {
            Some(backend) => backend,
            None => return,
        };
        let (stage, marker) = host_stage_paths(&backend, &operation.operation_id);
        let removed = super::super::execute_checked(
            executor,
            targets::locator_command(
                SshTrafficClass::Control,
                &backend,
                vec![
                    "rm".into(),
                    "-rf".into(),
                    "--".into(),
                    stage.to_string_lossy().into_owned(),
                    marker.to_string_lossy().into_owned(),
                ],
            )
            .purpose("remove abandoned destination Move staging"),
        );
        if let Err(error) = removed {
            tracing::warn!(session_id = %operation.selection.session_id,
                operation_id = %operation.operation_id, error = %format!("{error:#}"),
                "could not remove an abandoned Move's staging on its destination host");
        }
    }

    pub(in crate::controller) fn rollback_move_destination(
        &mut self,
        operation: &MoveOperation,
        error: anyhow::Error,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<anyhow::Error> {
        self.rollback_move_destination_with_persist(
            operation,
            error,
            executor,
            |source, environment| match environment {
                Some(environment) => crate::database::restore_resumed_session(source, environment),
                None => crate::database::save_resumed_session(source, None),
            },
        )
    }

    pub(in crate::controller) fn rollback_move_destination_with_persist(
        &mut self,
        operation: &MoveOperation,
        error: anyhow::Error,
        executor: &(impl CommandExecutor + Sync),
        persist: impl Fn(&mj_core::state::SessionRecord, Option<&str>) -> Result<()>,
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
                    return self.retain_failed_in_place_move_with_persist(
                        id,
                        &transfer.source,
                        error,
                        |source| persist(source, None),
                    );
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
                    && Some(locator.as_ref()) != prepared.as_ref()
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
                let latest =
                    crate::database::load_session_record(id)?.context("Move source disappeared")?;
                source.resource_allocation = latest.resource_allocation;
                source.additional_mounts = latest.additional_mounts;
                let environment =
                    crate::database::move_source_environment(&operation.operation_id)?
                        .context("Move rollback lost its source environment")?;
                persist(&source, Some(&environment.id))?;
                self.state.sessions.insert(id.clone(), source);
                Ok(error)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1579: a resumed Move re-sent every file because its partial copy lived in the
    // container that rollback removes; the host stage must keep it, and only for the same capture.
    // Needs a POSIX shell and rsync.
    #[cfg(unix)]
    #[test]
    fn destination_stage_resumes_the_same_capture_and_discards_a_different_one() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let capture = directory.path().join("capture");
        std::fs::create_dir_all(&capture).unwrap();
        // More than a pipe buffer, so a re-send could not be mistaken for a tiny rewrite.
        std::fs::write(capture.join("data"), vec![9; 256 * 1024 + 1]).unwrap();
        let stage = |root: &Path| DestinationStage {
            host: targets::TargetLocator::LocalBare {
                worker_root: String::new(),
            },
            stage: root.join("stage"),
            marker: root.join("stage.marker"),
            container: None,
        };
        let first_daemon = stage(directory.path());
        let executor = &targets::ProcessExecutor;
        prepare_destination_stage(executor, &first_daemon, "move-1 checkpoint-a manifest-a")
            .unwrap();
        copy_workspace(
            executor,
            &first_daemon.host,
            &first_daemon.stage,
            &capture,
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(directory.path()).unwrap().mode() & 0o777,
            0o700,
            "the staging parent must keep other host users out of the capture"
        );
        let sent = std::fs::metadata(first_daemon.stage.join("data")).unwrap();
        // The interrupted attempt also left its partial file for rsync to continue.
        std::fs::create_dir_all(first_daemon.stage.join(".move-partial")).unwrap();
        std::fs::write(first_daemon.stage.join(".move-partial/half"), b"partial").unwrap();
        drop(first_daemon);

        // The next daemon derives the same stage from the persisted Move alone.
        let next_daemon = stage(directory.path());
        prepare_destination_stage(executor, &next_daemon, "move-1 checkpoint-a manifest-a")
            .unwrap();
        assert!(next_daemon.stage.join(".move-partial/half").exists());
        copy_workspace(
            executor,
            &next_daemon.host,
            &next_daemon.stage,
            &capture,
            true,
        )
        .unwrap();
        let resumed = std::fs::metadata(next_daemon.stage.join("data")).unwrap();
        assert_eq!(
            sent.ino(),
            resumed.ino(),
            "rsync rewrote a file the destination already had"
        );

        // A Move that sealed a different checkpoint must not reuse these files.
        prepare_destination_stage(executor, &next_daemon, "move-1 checkpoint-b manifest-a")
            .unwrap();
        assert!(!next_daemon.stage.join("data").exists());
        assert!(!next_daemon.stage.join(".move-partial").exists());
        assert!(next_daemon.stage.is_dir());
    }

    // Hard-won: #1579: a destroyed or replaced Move left its host staging, a full workspace copy,
    // behind, because only a finished Move removed it.
    // Needs a POSIX shell.
    #[cfg(unix)]
    #[test]
    fn an_abandoned_move_removes_the_staging_it_left_on_the_host() {
        use crate::controller::test_support::{
            IsolatedTest, checkpoint_test_session, resume_compatibility_config,
        };
        if std::env::var_os("MJ_MOVE_ABANDONED_STAGE_TEST").is_none() {
            let directory = tempfile::tempdir().unwrap();
            let name = crate::controller::test_support::test_name(
                module_path!(),
                "an_abandoned_move_removes_the_staging_it_left_on_the_host",
            );
            IsolatedTest::new(name)
                .isolated_store(directory.path())
                .env("MJ_MOVE_ABANDONED_STAGE_TEST", "1")
                .run();
            return;
        }
        let mut session = checkpoint_test_session("abandoned-move");
        session.target_template_id = "local-bare".into();
        let mut operation = super::super::tests::source_recovery_operation(&session);
        operation.selection.target_template_id = Some("local-bare".into());
        operation.workspace_transfer = Some(WorkspaceTransfer {
            assessment: Default::default(),
            source: Box::new(session),
            source_stage: "unused".into(),
            controller_stage: "unused".into(),
            phase: WorkspaceTransferPhase::SourceStopped,
        });
        let host = targets::TargetLocator::LocalBare {
            worker_root: String::new(),
        };
        let (stage, marker) = host_stage_paths(&host, &operation.operation_id);
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("copied"), vec![1; 256 * 1024 + 1]).unwrap();
        std::fs::write(&marker, "identity").unwrap();
        let other = host_stage_paths(&host, "another-move").0;
        std::fs::create_dir_all(&other).unwrap();

        let controller = Controller {
            config: resume_compatibility_config(),
            state: Default::default(),
        };
        controller.remove_abandoned_move_stage(&operation, &targets::ProcessExecutor);

        assert!(!stage.exists() && !marker.exists());
        assert!(
            other.exists(),
            "another Move's staging is not this Move's to remove"
        );
    }

    // The stage's location is fixed by the Move's operation id, so a daemon that never saw the
    // first attempt finds it.
    #[test]
    fn destination_stage_is_found_again_from_the_operation_id_alone() {
        let backend = targets::TargetLocator::LocalBare {
            worker_root: "/destination/workers/s".into(),
        };
        let a = destination_stage(&backend, "session", "move-abc").unwrap();
        let b = destination_stage(&backend, "session", "move-abc").unwrap();
        assert_eq!(a.stage, b.stage);
        assert_eq!(a.marker, b.marker);
        assert_ne!(
            a.stage,
            destination_stage(&backend, "session", "move-def")
                .unwrap()
                .stage
        );
        assert!(a.container.is_none());
    }

    // Hard-won: #1579: the container staging sat outside the session's worker root, which does not
    // exist before the worker is installed, so the restore's helper had nowhere to land.
    #[test]
    fn container_staging_lives_in_the_session_worker_root_the_load_creates() {
        let session = "0123456789abcdef0123456789abcdef";
        let backend = targets::TargetLocator::LocalPodman {
            container_id: mj_core::targets::move_resource_name(session, "move-abc").unwrap(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let stage = destination_stage(&backend, session, "move-abc").unwrap();
        assert_eq!(
            stage.container.unwrap().stage,
            Path::new(&targets::worker_root(&backend, session).unwrap()).join("move-move-abc")
        );
    }

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
        source.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join(id),
        }));
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
        destination.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join("destination").join(id),
        }));
        crate::database::save_session(&destination).unwrap();
        let controller = Controller {
            config: resume_compatibility_config(),
            state: crate::database::load_state().unwrap(),
        };
        let error = crate::controller::test_support::worker_test_scope(id, || {
            controller.cleanup_retained_move_source(
                id,
                &operation.operation_id,
                &targets::ProcessExecutor,
            )
        })
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
    fn move_helper_reuses_a_published_worker_without_uploading() {
        use super::super::super::worker_binary::{WorkerInstall, install_worker_binary};
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let id = "move-helper-upload";
        let root = directory.path().join(id);
        std::fs::create_dir(&root).unwrap();
        let binary = directory.path().join("binary");
        let mut body = vec![0x5a; 512 * 1024 + 17];
        body.extend_from_slice(mj_core::worker_build::WORKER_BUILD_STAMP.as_bytes());
        std::fs::write(&binary, &body).unwrap();
        let backend = targets::TargetLocator::LocalBare {
            worker_root: root.to_string_lossy().into_owned(),
        };
        let executor = targets::ProcessExecutor;
        install_worker_binary(
            &executor,
            &backend,
            id,
            &WorkerInstall::Host(binary.clone()),
            "hel",
        )
        .unwrap();
        install_helper(&executor, &backend, id, &binary, "move-helper-build").unwrap();
        let worker = root.join("hel");
        let helper = root.join("move-helper-build");
        assert_eq!(std::fs::read(&helper).unwrap(), body);
        assert_eq!(
            std::fs::metadata(worker).unwrap().ino(),
            std::fs::metadata(helper).unwrap().ino()
        );
    }

    #[test]
    fn shared_filesystem_capacity_counts_allocations_once_and_changes_with_selection() {
        struct Disk;
        impl CommandExecutor for Disk {
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

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

    // Unix only: one source is read through a session locator, and a local
    // bare worker root is a POSIX path, which no Windows directory is.
    // Hard-won: #1542: a Move into a fresh environment left the checkout unmarked, so Suspend could not export it
    #[cfg(unix)]
    #[test]
    fn moved_bundle_exports_each_repository_with_its_original_base() {
        use crate::controller::test_support::{
            checkpoint_archive_input, checkpoint_test_session, committed_repository, test_git,
        };
        use mj_checkpoint::archive::{
            GitCollectionSpec, GitCommand, GitCommandRunner, GitHistoryMode, SystemGit,
            collect_git_metadata_snapshot, collect_git_snapshot,
        };
        use mj_checkpoint::checkpoint::{
            CHECKPOINT_EXPORT_PROTOCOL_VERSION, CheckpointExportSpec, CheckpointRepositoryCapture,
            CheckpointRepositorySpec, export_checkpoint,
        };

        struct RemoteGit {
            remote_path: std::path::PathBuf,
            source_path: std::path::PathBuf,
        }
        impl CommandExecutor for RemoteGit {
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

            fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
                ensure!(
                    command.program == "ssh",
                    "the remote source must be read through SSH"
                );
                let remote = command.args.last().context("SSH command missing")?;
                // Hard-won: #1542: target Git could prompt while Move sealed its source.
                for (name, value) in mj_checkpoint::archive::NON_INTERACTIVE_GIT_ENV {
                    ensure!(
                        remote.contains(&targets::posix_quote(&format!("{name}={value}"))),
                        "Git environment did not reach the SSH target: {remote}"
                    );
                }
                let ssh = mj_core::local_git::remote_git_ssh_command()?;
                ensure!(
                    remote.contains(&targets::posix_quote(&format!("GIT_SSH_COMMAND={ssh}"))),
                    "Git SSH policy did not reach the SSH target: {remote}"
                );

                let from = targets::posix_quote(self.remote_path.to_str().unwrap());
                let to = targets::posix_quote(self.source_path.to_str().unwrap());
                ensure!(
                    remote.contains(&from),
                    "source Git used the wrong target path"
                );
                targets::ProcessExecutor
                    .execute(&CommandSpec::new("sh", ["-c", &remote.replace(&from, &to)]))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let relay = directory.path().join("move-managed-bundle");
        let home = directory.path().join("home");
        for path in [&workspace, &relay, &home] {
            std::fs::create_dir_all(path).unwrap();
        }
        let sources = [
            committed_repository(),
            committed_repository(),
            committed_repository(),
            committed_repository(),
        ];
        let mut source_repositories = Vec::new();
        let mut destinations = Vec::new();
        let mut snapshots = Vec::new();
        let mut bases = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            let id = format!("repo{index}");
            // Distinct launch bases and later commits catch both repository
            // mixups and accidentally substituting the transferred HEAD.
            test_git(source.path(), &["commit", "--allow-empty", "-m", &id]);
            let base = test_git(source.path(), &["rev-parse", "HEAD"]);
            if index == 3 {
                // A legacy worktree's session branch starts after existing
                // local work; its reflog, rather than origin, owns the base.
                test_git(source.path(), &["checkout", "-b", "mj/legacy-base"]);
                test_git(source.path(), &["branch", "-D", "master"]);
            }
            if index == 1 {
                test_git(source.path(), &["config", "mj.baseCommit", &base]);
                test_git(source.path(), &["config", "mj.remoteWorkspace", "true"]);
            }
            if index != 2 {
                // Both host and remote sessions can abandon the launch commit.
                test_git(source.path(), &["reset", "--hard", "HEAD^"]);
            }
            test_git(
                source.path(),
                &["commit", "--allow-empty", "-m", "later work"],
            );
            if index == 2 {
                // An unmanaged host's local commit must remain in future bundles.
                test_git(
                    source.path(),
                    &["update-ref", "refs/remotes/origin/master", &base],
                );
            }
            let mut snapshot = collect_git_metadata_snapshot(
                &SystemGit,
                source.path(),
                &GitCollectionSpec {
                    id: id.clone(),
                    relative_destination: id.clone().into(),
                    history: GitHistoryMode::NoBundle,
                    origin_override: Some(format!("https://example.invalid/{id}.git")),
                },
            )
            .unwrap();
            let source_id = format!("move-source-{index}");
            let mut source_record = checkpoint_test_session(&source_id);
            if index != 1 {
                source_record.project_directory = Some(source.path().into());
            }
            if index == 0 || index == 3 {
                // A host worktree (or an older capture) reports HEAD as its
                // metadata base; the controller's persisted launch base wins.
                source_record.managed_worktree = Some(Box::new(mj_core::state::ManagedWorktree {
                    kind: mj_core::state::ManagedCheckoutKind::Worktree,
                    source_project_directory: source.path().into(),
                    source_repository: source.path().into(),
                    worktree_root: source.path().into(),
                    branch: if index == 3 {
                        "mj/legacy-base"
                    } else {
                        "master"
                    }
                    .into(),
                    target: mj_core::state::ManagedWorktreeTarget::Local,
                    base_commit: (index != 3).then(|| base.clone()),
                }));
            }
            let mut handoff_input =
                checkpoint_archive_input(&source_id, 0, vec![snapshot.clone()], Vec::new());
            handoff_input.bundle.primary_repository = id.clone();
            let handoff = crate::controller::test_support::write_checkpoint_archive_input(
                &directory.path().join("handoffs"),
                &source_id,
                &handoff_input,
            );
            let local = targets::TargetLocator::LocalBare {
                worker_root: directory
                    .path()
                    .join(&source_id)
                    .to_string_lossy()
                    .into_owned(),
            };
            let handoff = if index == 2 {
                let remote_path = directory.path().join("exists-only-on-source");
                assert!(!remote_path.exists());
                source_record.project_directory = Some(remote_path.clone());
                let backend = targets::TargetLocator::SshBare {
                    ssh: targets::SshTarget {
                        destination: "source.invalid".into(),
                        ssh_args: Vec::new(),
                    },
                    workspace: format!("/remote/{source_id}"),
                    worker_id: None,
                };
                record_move_handoff_bases(
                    &source_record,
                    &handoff,
                    &backend,
                    &RemoteGit {
                        remote_path,
                        source_path: source.path().into(),
                    },
                )
                .unwrap()
            } else {
                record_move_handoff_bases(
                    &source_record,
                    &handoff,
                    &local,
                    &targets::ProcessExecutor,
                )
                .unwrap()
            };
            snapshot.metadata =
                mj_checkpoint::archive::read_checkpoint_manifest(&handoff.archive_path)
                    .unwrap()
                    .repositories
                    .remove(0)
                    .metadata;
            snapshots.push(snapshot);
            bases.push(base.clone());
            let destination = workspace.join(&id);
            test_git(
                directory.path(),
                &[
                    "clone",
                    "--no-local",
                    source.path().to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            if index != 2 {
                let probe = SystemGit
                    .run(
                        &destination,
                        &GitCommand {
                            arguments: vec!["cat-file".into(), "-e".into(), base.into()],
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_ne!(
                    probe.status, 0,
                    "the fresh clone must lack the abandoned base"
                );
            }
            test_git(
                &destination,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    &format!("https://example.invalid/{id}.git"),
                ],
            );
            assert!(
                mj_checkpoint::archive::remote_workspace_base(&SystemGit, &destination)
                    .unwrap()
                    .is_none()
            );
            source_repositories.push(WorkspaceRepository {
                base_commit: None,
                id: id.clone(),
                root: source.path().into(),
            });
            destinations.push(WorkspaceRepository {
                base_commit: None,
                id,
                root: destination,
            });
        }
        let mut input = checkpoint_archive_input("move-managed-bundle", 0, snapshots, Vec::new());
        input.bundle.primary_repository = "repo0".into();
        let checkpoint = crate::controller::test_support::write_checkpoint_archive_input(
            directory.path(),
            "move-managed-bundle",
            &input,
        );
        let manifest =
            mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path).unwrap();
        let source_repositories = repositories_with_bases(source_repositories, &manifest).unwrap();
        let stage = directory.path().join("transfer");
        // A managed source carries its GC root through transfer, while the
        // raw destination gets no pin until managed finalization owns it.
        test_git(
            sources[0].path(),
            &[
                "update-ref",
                mj_core::targets::HistoryCutoff::BASE_REF,
                &bases[0],
            ],
        );
        mj_worker::move_workspace::capture(
            &source_repositories,
            &WorkspaceSelection::default(),
            &stage,
        )
        .unwrap();
        let transfer = mj_worker::move_workspace::verify(&stage).unwrap();
        assert!(
            transfer.repositories[0]
                .refs
                .iter()
                .any(|(name, _)| name == mj_core::targets::HistoryCutoff::BASE_REF)
        );
        mj_worker::move_workspace::restore(&stage, &destinations).unwrap();
        for destination in &destinations {
            assert!(
                test_git(
                    &destination.root,
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        mj_core::targets::HistoryCutoff::BASE_REF
                    ]
                )
                .is_empty(),
                "restore must not install a managed base pin in a raw checkout"
            );
            assert!(
                mj_checkpoint::archive::remote_workspace_base(&SystemGit, &destination.root)
                    .unwrap()
                    .is_none()
            );
        }

        finalize_move_repositories(
            &targets::ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: relay.to_string_lossy().into_owned(),
            },
            "move-managed-bundle",
            &destinations,
            &manifest,
        )
        .unwrap();
        for repository in &destinations {
            test_git(
                &repository.root,
                &["reflog", "expire", "--expire=now", "--all"],
            );
            test_git(&repository.root, &["gc", "--prune=now"]);
        }
        let export = export_checkpoint(&CheckpointExportSpec {
            protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
            ignore_nested_repositories: false,
            session: input.session,
            target: input.target,
            bundle: input.bundle,
            relay_root: relay.clone(),
            harness_home: home,
            workspace_root: workspace,
            repositories: destinations
                .iter()
                .map(|repository| CheckpointRepositorySpec {
                    id: repository.id.clone(),
                    relative_destination: repository.id.clone().into(),
                    capture: CheckpointRepositoryCapture::RemoteWorkspace,
                    origin_override: None,
                })
                .collect(),
            report_directories: None,
            canonical_session: input.canonical_session,
            output_path: relay.join("suspend.hel.zip"),
        })
        .unwrap();
        let exported = mj_checkpoint::archive::read_checkpoint_manifest(&export.path).unwrap();
        for (repository, base) in exported.repositories.iter().zip(bases) {
            assert!(repository.metadata.remote_workspace);
            assert_eq!(repository.metadata.base_commit, base);
            assert_ne!(repository.metadata.head_commit, base);
        }
        // Restore a pinless legacy archive, then Move its available history
        // into a host checkout even though its recorded base is absent.
        let missing_base = test_git(sources[1].path(), &["config", "mj.baseCommit"]);
        let mut legacy_snapshot = collect_git_snapshot(
            &SystemGit,
            sources[1].path(),
            &GitCollectionSpec {
                id: "legacy".into(),
                relative_destination: "legacy".into(),
                history: GitHistoryMode::CloneFrom(missing_base.clone()),
                origin_override: Some("https://example.invalid/legacy.git".into()),
            },
        )
        .unwrap();
        legacy_snapshot.metadata.remote_workspace = true;
        assert!(
            !legacy_snapshot
                .metadata
                .saved_refs
                .contains_key(mj_core::targets::HistoryCutoff::BASE_REF)
        );
        let mut legacy_input =
            checkpoint_archive_input("legacy-move-session", 0, vec![legacy_snapshot], Vec::new());
        legacy_input.bundle.primary_repository = "legacy".into();
        let legacy_archive = crate::controller::test_support::write_checkpoint_archive_input(
            directory.path(),
            "legacy-move-session",
            &legacy_input,
        );
        let legacy_workspace = directory.path().join("legacy-source-workspace");
        std::fs::create_dir_all(&legacy_workspace).unwrap();
        let legacy_repository = legacy_workspace.join("legacy");
        test_git(
            directory.path(),
            &[
                "clone",
                "--no-local",
                sources[1].path().to_str().unwrap(),
                legacy_repository.to_str().unwrap(),
            ],
        );
        assert!(
            !mj_checkpoint::archive::managed_base_is_available(
                &SystemGit,
                &legacy_repository,
                &missing_base
            )
            .unwrap()
        );
        mj_checkpoint::checkpoint::restore_repositories(
            &legacy_archive.archive_path,
            &legacy_workspace,
            &SystemGit,
        )
        .unwrap();
        assert_eq!(
            mj_checkpoint::archive::remote_workspace_base(&SystemGit, &legacy_repository).unwrap(),
            Some(missing_base.clone())
        );
        let legacy_stage = directory.path().join("legacy-move-stage");
        mj_worker::move_workspace::capture(
            &[WorkspaceRepository {
                id: "legacy".into(),
                root: legacy_repository.clone(),
                base_commit: Some(missing_base),
            }],
            &WorkspaceSelection::default(),
            &legacy_stage,
        )
        .unwrap();
        let host = committed_repository();
        mj_worker::move_workspace::restore(
            &legacy_stage,
            &[WorkspaceRepository {
                id: "legacy".into(),
                root: host.path().into(),
                base_commit: None,
            }],
        )
        .unwrap();
        assert_eq!(
            test_git(host.path(), &["rev-parse", "HEAD"]),
            test_git(&legacy_repository, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            std::fs::read(host.path().join("nested/file.txt")).unwrap(),
            std::fs::read(legacy_repository.join("nested/file.txt")).unwrap()
        );
        assert!(
            mj_checkpoint::archive::remote_workspace_base(&SystemGit, host.path())
                .unwrap()
                .is_none()
        );
        let legacy_manifest =
            mj_checkpoint::archive::read_checkpoint_manifest(&legacy_archive.archive_path).unwrap();
        let error = finalize_move_repositories(
            &targets::ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: relay.to_string_lossy().into_owned(),
            },
            "move-managed-bundle",
            &[WorkspaceRepository {
                id: "legacy".into(),
                root: host.path().into(),
                base_commit: None,
            }],
            &legacy_manifest,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("repository 'legacy'"), "{message}");
        assert!(
            message.contains(&legacy_manifest.repositories[0].metadata.base_commit),
            "{message}"
        );
        assert!(message.contains("session stays on its source"), "{message}");
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
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

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
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: source_root.clone(),
        }));
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
        let snapshot = mj_checkpoint::archive::collect_git_metadata_snapshot(
            &mj_checkpoint::archive::SystemGit,
            source.path(),
            &mj_checkpoint::archive::GitCollectionSpec {
                id: "project".into(),
                relative_destination: "project".into(),
                history: mj_checkpoint::archive::GitHistoryMode::NoBundle,
                origin_override: None,
            },
        )
        .unwrap();
        let archive_input = crate::controller::test_support::checkpoint_archive_input(
            id,
            0,
            vec![snapshot],
            Vec::new(),
        );
        operation.handoff = Some(
            crate::controller::test_support::write_checkpoint_archive_input(
                &mj_core::config::data_dir(),
                id,
                &archive_input,
            ),
        );
        let executor = Executor(std::sync::atomic::AtomicBool::new(true));
        let mut controller = Controller {
            config: resume_compatibility_config(),
            state: mj_core::state::State::default(),
        };
        controller.state.sessions.insert(id.into(), session.clone());
        crate::database::save_session(&session).unwrap();
        let assessment = mj_worker::move_workspace::inspect(&[WorkspaceRepository {
            base_commit: None,
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
        destination_session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: destination_root.clone(),
        }));
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
        crate::controller::test_support::worker_test_scope(id, || {
            controller.cleanup_retained_move_source(id, &operation.operation_id, &executor)
        })
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
