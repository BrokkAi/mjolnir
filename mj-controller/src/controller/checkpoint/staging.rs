use super::*;

pub(in crate::controller) async fn wait_for_relay_closed(
    relay: &mut StandaloneSession,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if relay.sync().await?.operational.execution == RelayExecutionState::Closed {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("ACP runtime did not close within 30 seconds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Hand ACP dispatch back as soon as target-owned state is sealed.
///
/// Proving the barrier first moves the workspace-consistency proof ahead of the
/// release: the same barrier still holding the same ready cursor means nothing
/// the harness could write reached the workspace while the stage was captured.
/// The recovery floor stays put, because nothing yet proves the archive reached
/// the controller's disk.
///
/// A worker that does not understand the release keeps its barrier, and the
/// caller falls back to ending it only after the archive is installed. That is
/// slower, not wrong, so it is not a checkpoint failure.
pub(super) async fn release_checkpoint_after_capture(
    relay: &mut ControllerRelayLease,
    session_id: &str,
    barrier_command_id: &str,
    cursor: &RelayCursor,
    harness: HarnessKind,
) -> Result<CheckpointCompletion> {
    relay
        .sync_snapshot()
        .await
        .and_then(|snapshot| {
            validate_automatic_checkpoint_barrier_snapshot(
                &snapshot,
                barrier_command_id,
                cursor,
                harness,
            )
        })
        .context("checkpoint barrier changed while capturing target state")?;
    match relay
        .submit(
            new_command_id("checkpoint-release")?,
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: barrier_command_id.to_owned(),
            },
        )
        .await
    {
        Ok(_) => Ok(CheckpointCompletion::ReleasedAfterCapture),
        Err(error) => {
            tracing::debug!(
                session_id,
                "relay kept the checkpoint barrier through the transfer: {error:#}"
            );
            Ok(CheckpointCompletion::HeldBarrier)
        }
    }
}

/// Run one checkpoint command on the target with its spec streamed over stdin.
///
/// When the installed worker is older than this controller and cannot read the
/// spec, its `mj` is replaced with the controller's binary once and the command
/// is retried. `worker_binary` names that binary; `None` resolves the one this
/// controller would install.
pub(super) fn run_checkpoint_staging_command<T: serde::Serialize>(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    spec: &T,
    command: fn(&targets::TargetLocator, &str) -> Result<CommandSpec>,
    operation: &str,
    worker_binary: Option<&Path>,
) -> Result<CommandOutput> {
    let body = serde_json::to_vec(spec).with_context(|| format!("serialize {operation} spec"))?;
    let mut replaced_worker = false;
    loop {
        let command = command(locator, session_id)?;
        let output = executor.execute_with_stdin(&command, &mut body.as_slice())?;
        if output.status == 0 {
            return Ok(output);
        }
        let failure = String::from_utf8_lossy(&output.stderr).into_owned();
        if staging_protocol_unsupported(&failure)
            && replace_stale_export_worker(
                executor,
                locator,
                session_id,
                worker_binary,
                &failure,
                &mut replaced_worker,
            )?
        {
            continue;
        }
        bail!(
            "{operation} failed with status {}: {failure}",
            output.status
        );
    }
}

/// When the installed worker cannot execute this export protocol, replace its
/// `mj` with the controller's current binary and tell the caller to retry. The
/// live daemon keeps the previous inode; only the next `export-checkpoint`
/// process changes.
pub(super) fn replace_stale_export_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: Option<&Path>,
    failure: &str,
    replaced_worker: &mut bool,
) -> Result<bool> {
    if *replaced_worker || !staging_protocol_unsupported(failure) {
        return Ok(false);
    }
    tracing::debug!(
        session_id,
        "target worker does not support this checkpoint export protocol; replacing the installed Mjolnir binary and retrying"
    );
    let owned_binary;
    let binary = if let Some(path) = worker_binary {
        path
    } else {
        owned_binary = crate::controller::worker_binary::worker_binary_for(locator, executor)?;
        owned_binary.as_path()
    };
    crate::controller::worker_binary::replace_installed_worker_binary(
        executor, locator, session_id, binary,
    )?;
    *replaced_worker = true;
    Ok(true)
}

/// Whether an export failure says the target's worker cannot deserialize this
/// spec. `CheckpointExportSpec` and its nested canonical snapshot use
/// `deny_unknown_fields`, so a controller that gained a field such as
/// `terminal_refs` cannot pause a session whose installed `mj` predates it.
pub(super) fn export_spec_schema_unsupported(failure: &str) -> bool {
    failure.contains("parse checkpoint")
        && (failure.contains("unknown field") || failure.contains("unknown variant"))
}

pub(super) fn export_protocol_unsupported(failure: &str) -> bool {
    export_spec_schema_unsupported(failure)
        || failure.contains("unsupported checkpoint export protocol version")
}

pub(super) fn staging_protocol_unsupported(failure: &str) -> bool {
    export_protocol_unsupported(failure)
        || failure.contains("unsupported checkpoint staging protocol version")
        || failure.contains("unrecognized subcommand")
        || failure.contains("unexpected argument")
}

pub(in crate::controller) fn upload_checkpoint_spec(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    local: &Path,
    remote: &str,
) -> Result<()> {
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            std::fs::copy(local, remote)
                .with_context(|| format!("copy checkpoint specification to {remote}"))?;
            Ok(())
        }
        targets::TargetLocator::LocalPodman { container_id, .. } => execute_checked(
            executor,
            CommandSpec::new(
                "podman",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::LocalDocker { container_id, .. } => execute_checked(
            executor,
            CommandSpec::new(
                "docker",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::AppleContainer { container_id, .. } => execute_checked(
            executor,
            CommandSpec::new(
                "container",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => execute_checked(
            executor,
            crate::targets::scp_upload(ssh, local, remote, false)
                .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | targets::TargetLocator::SshDocker {
            ssh, container_id, ..
        } => {
            let engine = match locator {
                targets::TargetLocator::SshPodman { .. } => "podman",
                targets::TargetLocator::SshDocker { .. } => "docker",
                _ => unreachable!("matched remote container target"),
            };
            let staging = format!(
                "{}/{session_id}-checkpoint.json",
                targets::REMOTE_UPLOAD_STAGING
            );
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["mkdir", "-p", targets::REMOTE_UPLOAD_STAGING])
                    .purpose("create remote checkpoint staging"),
            )?;
            execute_checked(
                executor,
                crate::targets::scp_upload(ssh, local, &staging, false)
                    .purpose("upload remote container checkpoint specification"),
            )?;
            execute_checked(
                executor,
                crate::targets::ssh_command(
                    ssh,
                    [engine, "cp", &staging, &format!("{container_id}:{remote}")],
                )
                .purpose("install remote container checkpoint specification"),
            )?;
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["rm", "-f", "--", &staging])
                    .purpose("remove remote checkpoint staging"),
            )?;
            Ok(())
        }
    }?;
    Ok(())
}

/// The artifact a latched checkpoint may keep instead of exporting a new one,
/// or `None` when a full export has to run.
///
/// Every relay command is journalled, checkpoint plumbing included, so the
/// event frontier always moves between two checkpoints. Session content is
/// what decides whether the installed archive still represents the session.
/// Every reason to decline is reported; none of them fails the checkpoint.
pub(super) fn reusable_installed_checkpoint(
    session_id: &str,
    installed: Option<&CheckpointMetadata>,
    native_session_id: &str,
    latched_ordinal: u64,
    latched_session: &CanonicalSessionSnapshot,
) -> Option<CheckpointArtifact> {
    let installed = installed?;
    if installed.event_frontier > latched_ordinal {
        tracing::warn!(
            session_id,
            installed_frontier = installed.event_frontier,
            latched_ordinal,
            "installed checkpoint is ahead of the latched cursor; exporting a fresh archive"
        );
        return None;
    }
    let verified = match verify_archive_streaming(&installed.archive_path) {
        Ok(verified) => verified,
        Err(error) => {
            tracing::warn!(
                session_id,
                path = %installed.archive_path.display(),
                "installed checkpoint could not be verified for reuse: {error:#}"
            );
            return None;
        }
    };
    if verified.archive_sha256 != installed.sha256
        || verified.manifest.session.id != session_id
        || verified.canonical_session.event_frontier != installed.event_frontier
    {
        tracing::warn!(
            session_id,
            path = %installed.archive_path.display(),
            "installed checkpoint no longer matches its controller metadata; exporting a fresh archive"
        );
        return None;
    }
    if !verified.canonical_session.content_matches(latched_session) {
        tracing::info!(
            session_id,
            archive_frontier = verified.canonical_session.event_frontier,
            latched_ordinal,
            "session content changed since the installed checkpoint; exporting a fresh archive"
        );
        return None;
    }
    tracing::info!(
        session_id,
        archive_frontier = verified.canonical_session.event_frontier,
        latched_ordinal,
        "reusing the installed checkpoint archive; only relay bookkeeping moved"
    );
    Some(CheckpointArtifact {
        metadata: installed.clone(),
        native_session_id: native_session_id.to_owned(),
        event_frontier_digest: verified.canonical_session.event_frontier_digest,
    })
}

pub(in crate::controller) fn verify_installed_checkpoint_gate(
    session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let sha256 = checkpoint_sha256(&checkpoint.archive_path).with_context(|| {
        format!(
            "hash installed checkpoint {} before target cleanup",
            checkpoint.archive_path.display()
        )
    })?;
    ensure!(
        sha256 == checkpoint.sha256,
        "refusing target cleanup for session {session_id}: installed checkpoint SHA changed"
    );
    Ok(())
}

pub(super) fn verify_checkpoint_artifact(
    session_id: &str,
    artifact: &CheckpointArtifact,
) -> Result<()> {
    let sha256 = checkpoint_sha256(&artifact.metadata.archive_path).with_context(|| {
        format!(
            "hash completed checkpoint {}",
            artifact.metadata.archive_path.display()
        )
    })?;
    ensure!(
        sha256 == artifact.metadata.sha256,
        "completed checkpoint SHA changed before persistence for session {session_id}"
    );
    Ok(())
}

/// Release the projection history the new checkpoint covers.
///
/// The checkpoint archive holds the complete transcript up to its frontier, so
/// the tool output stored below that frontier is a second copy of something
/// already durable. Reclaiming it is housekeeping: a checkpoint that is
/// verified and persisted stays good whether or not this succeeds, so a
/// failure is logged rather than returned.
pub(in crate::controller) fn release_projection_behind_checkpoint(
    session_id: &str,
    current: &CheckpointMetadata,
) {
    match crate::database::compact_materialized_transcript_through(
        session_id,
        current.event_frontier,
    ) {
        Ok(retention) if retention.items == 0 => {}
        Ok(retention) => tracing::info!(
            session_id,
            items = retention.items,
            bytes = retention.bytes,
            remaining = retention.remaining,
            event_frontier = current.event_frontier,
            "released projection history the checkpoint covers"
        ),
        Err(error) => tracing::warn!(
            session_id,
            "checkpoint was saved, but the projection history it covers could not be released: {error:#}"
        ),
    }
}

pub(in crate::controller) fn prune_replaced_checkpoint(
    previous: Option<&CheckpointMetadata>,
    current: &CheckpointMetadata,
) {
    let Some(previous) = previous.filter(|old| old.archive_path != current.archive_path) else {
        return;
    };
    match crate::database::move_checkpoint_is_retained(&previous.archive_path) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "could not check move retention; keeping superseded checkpoint");
            return;
        }
    }
    if let Err(error) = std::fs::remove_file(&previous.archive_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %previous.archive_path.display(),
            "could not remove superseded recovery copy: {error}"
        );
    }
    reap_finished_move_intents();
}

/// Release the durable rows of finished moves whose checkpoint has gone.
///
/// Called where an archive stops being retained: after a superseded archive is
/// pruned, and in the startup archive reconciliation. Like the prune itself this
/// is housekeeping, so a failure is logged rather than returned.
pub(in crate::controller) fn reap_finished_move_intents() {
    match crate::database::reap_finished_move_intents() {
        Ok(0) => {}
        Ok(reaped) => tracing::debug!(reaped, "reaped durable rows of finished moves"),
        Err(error) => {
            tracing::warn!("could not reap the durable rows of finished moves: {error:#}")
        }
    }
}
