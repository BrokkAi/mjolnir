use super::*;

/// The latched projection must sit exactly at the barrier's ready cursor.
///
/// The barrier was admitted, but the relay can record more events before the
/// controller latches - the harness spoke again in the gap. The archive would
/// not be an exact cut of the session, so the attempt is dropped and the next
/// idle observation copies the settled session instead. This is not a fault in
/// the session, the target, or the last archive.
pub(super) fn ensure_exact_checkpoint_cut(
    cursor: &RelayCursor,
    expected_ordinal: u64,
    expected_digest: &str,
) -> Result<()> {
    if cursor.ordinal != expected_ordinal || cursor.digest != expected_digest {
        bail!(CheckpointDeferred::frontier_moved());
    }
    Ok(())
}

/// Prove the barrier that latched an archive is still the same barrier, still
/// held at the same ready cursor.
///
/// The relay frontier may have moved past that cursor: an active ordinary
/// barrier still accepts and journals submissions, it only freezes ACP
/// dispatch. Nothing the harness could write reaches the workspace while
/// dispatch is frozen, so an advanced frontier does not invalidate the archive.
/// Requiring frontier equality here would fail every checkpoint that overlapped
/// a prompt.
///
/// A turn the harness starts on its own is the exception. The barrier freezes
/// Mjolnir's dispatch, not the harness, so a harness turn that opened after the
/// cursor was captured means the agent may have been writing to the workspace
/// while it was staged. That archive is abandoned rather than installed.
pub(super) fn validate_checkpoint_barrier_snapshot(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    expected: &RelayCursor,
) -> Result<()> {
    ensure!(
        snapshot.operational.checkpoint_barrier.as_deref() == Some(command_id),
        "checkpoint barrier {command_id} is no longer active"
    );
    ensure!(
        snapshot.operational.checkpoint_ready.as_ref() == Some(expected),
        "checkpoint barrier {command_id} has a different ready cursor"
    );
    if snapshot
        .operational
        .last_harness_turn_started_ordinal
        .is_some_and(|ordinal| ordinal > expected.ordinal)
    {
        bail!(CheckpointDeferred::harness_turn_during_capture());
    }
    Ok(())
}

/// Validate the barrier cut and prove that a routine checkpoint still has no
/// provider-owned Kimi work. Close checkpoints intentionally use the more
/// permissive validator because close is allowed to interrupt/terminate work.
pub(super) fn validate_automatic_checkpoint_barrier_snapshot(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    expected: &RelayCursor,
    harness: HarnessKind,
) -> Result<()> {
    validate_checkpoint_barrier_snapshot(snapshot, command_id, expected)?;
    ensure!(
        snapshot.operational.safe_for_checkpoint(harness),
        CheckpointDeferred::background_snapshot(&snapshot.operational, harness)
    );
    Ok(())
}

pub(super) fn remove_uninstalled_checkpoint(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match std::fs::remove_file(path) {
        Ok(()) => error,
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => error,
        Err(remove_error) => error.context(format!(
            "also failed to remove uninstalled checkpoint {}: {remove_error}",
            path.display()
        )),
    }
}
