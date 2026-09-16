use super::*;

#[derive(Debug, Clone)]
pub struct CheckpointArtifact {
    pub metadata: CheckpointMetadata,
    pub native_session_id: String,
    /// Digest paired with `metadata.event_frontier` at the relay barrier.
    pub event_frontier_digest: String,
}

/// The relay connection one lifecycle operation talks to.
///
/// A managed operation borrows the session actor's own connection instead of
/// opening a competing one. Exclusivity is only needed while a checkpoint
/// latches its projection at the barrier's ready cursor; `end_latch` hands the
/// connection back so the dashboard keeps syncing and submitting while the
/// archive exports and transfers.
pub(in crate::controller) enum ControllerRelayLease {
    Managed {
        handle: ManagedSessionHandle,
        lease: Option<ManagedSessionLease>,
    },
    Standalone(StandaloneSession),
}

impl ControllerRelayLease {
    /// The exclusively held connection. Only a latch phase, or an operation
    /// that deliberately holds its lease to the end, may use this.
    pub(in crate::controller) fn connection_mut(&mut self) -> &mut StandaloneSession {
        match self {
            Self::Managed { lease, .. } => lease
                .as_mut()
                .expect("checkpoint latch has already returned its connection")
                .connection_mut(),
            Self::Standalone(connection) => connection,
        }
    }

    pub(super) async fn submit(
        &mut self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<u64> {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.connection_mut().submit(command_id, command).await,
            Self::Managed { handle, .. } => handle.submit(command_id, command).await,
            Self::Standalone(connection) => connection.submit(command_id, command).await,
        }
    }

    pub(super) async fn sync_snapshot(&mut self) -> Result<ManagedSessionSnapshot> {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.connection_mut().sync().await,
            Self::Managed { handle, .. } => {
                handle.sync_now().await?;
                handle
                    .view()
                    .snapshot
                    .context("managed session has no snapshot")
            }
            Self::Standalone(connection) => connection.sync().await,
        }
    }

    /// Swap the proxy after the worker process behind it was restarted.
    pub(super) fn replace_connection(&mut self, connection: StandaloneSession) {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.replace_connection(connection),
            Self::Standalone(existing) => *existing = connection,
            Self::Managed { lease: None, .. } => {
                *self = Self::Standalone(connection);
            }
        }
    }

    /// Return the connection to its session actor now that the projection is
    /// latched. Releasing keeps the connection alive, so the relay barrier it
    /// opened stays open. Idempotent.
    pub(super) fn end_latch(&mut self) {
        if let Self::Managed { lease, .. } = self
            && let Some(lease) = lease.take()
        {
            lease.release();
        }
    }

    /// Abandon a checkpoint barrier this controller can no longer complete.
    ///
    /// A relay barrier belongs to the connection that opened it and only a
    /// disconnect cancels it (`cancel_checkpoint_barrier_on_disconnect`).
    /// Completing it instead would advance the relay's recovery floor past
    /// history that no verified checkpoint covers, so reclaim the connection
    /// and drop it: the worker cancels the barrier and resumes dispatch.
    pub(super) async fn cancel_abandoned_barrier(&mut self) -> Result<()> {
        let Self::Managed { handle, lease } = self else {
            // A standalone connection is dropped with this value, which the
            // worker sees as the same disconnect.
            return Ok(());
        };
        match lease.take() {
            Some(lease) => drop(lease),
            None => drop(handle.lease_connection().await?),
        }
        Ok(())
    }

    pub(in crate::controller) fn release(self) {
        if let Self::Managed {
            lease: Some(lease), ..
        } = self
        {
            lease.release();
        }
    }
}

/// Whether a checkpoint keeps its exclusive connection after latching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller) enum LatchExclusivity {
    /// Ordinary and recovery checkpoints only need exclusivity to latch the
    /// projection at the barrier's ready cursor. Everything after that runs
    /// through the session actor, so prompts keep flowing while the archive
    /// exports and transfers.
    ReleaseAfterLatch,
    /// Close seals the relay at the exact latched cursor, so nothing else may
    /// reach the relay between the barrier and its Close command.
    HoldThroughClose,
}

/// Whether a latched checkpoint must export a fresh archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller) enum CheckpointExportPolicy {
    /// Always export, transfer, and install a new archive.
    Always,
    /// Keep the installed archive when the latched projection holds the same
    /// session content. Relay bookkeeping (the checkpoint commands themselves)
    /// always moves the event frontier, so only content can decide this.
    ReuseUnchangedArchive,
}

/// How a latched checkpoint ends the barrier it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller) enum CheckpointCompletion {
    /// The barrier is still open. Completing it resumes ACP dispatch and
    /// advances the relay's recovery floor in one durable step; abandoning it
    /// cancels the barrier and leaves the floor alone.
    HeldBarrier,
    /// The worker already resumed dispatch when target capture finished. All that
    /// is left for a durably installed archive is the recovery floor move.
    ReleasedAfterCapture,
}

pub(in crate::controller) struct LatchedCheckpoint {
    pub(in crate::controller) artifact: CheckpointArtifact,
    pub(in crate::controller) relay: ControllerRelayLease,
    pub(in crate::controller) barrier_command_id: String,
    pub(in crate::controller) cursor: RelayCursor,
    pub(in crate::controller) completion: CheckpointCompletion,
}

/// A latched checkpoint owns an open relay barrier, and that barrier freezes
/// ACP dispatch until something ends it. Every path out of one must therefore
/// either [`LatchedCheckpoint::complete`] it or [`LatchedCheckpoint::abandon`]
/// it; both consume the value so a new exit cannot quietly skip the choice.
/// Close is the exception: it holds its lease to the end, so dropping that
/// lease is what ends its barrier.
impl LatchedCheckpoint {
    /// Let the relay release the history that this installed archive covers.
    pub(super) async fn complete(mut self) -> Result<()> {
        let (prefix, command) = match self.completion {
            CheckpointCompletion::HeldBarrier => (
                "checkpoint-complete",
                RelayCommand::CompleteCheckpoint {
                    barrier_command_id: self.barrier_command_id.clone(),
                },
            ),
            // The worker that accepted the early release also understands the
            // floor move; they were added together.
            CheckpointCompletion::ReleasedAfterCapture => (
                "checkpoint-floor",
                RelayCommand::AdvanceRecoveryFloor {
                    through: self.cursor.clone(),
                },
            ),
        };
        let command_id = new_command_id(prefix)?;
        self.relay.submit(command_id, command).await.map(|_| ())
    }

    /// Cancel the barrier of a checkpoint the caller could not install.
    ///
    /// The latch is already back with the session actor, whose connection can
    /// stay healthy for the rest of the session, so nothing else would ever
    /// end this barrier.
    pub(super) async fn abandon(mut self, session_id: &str) {
        if self.completion == CheckpointCompletion::ReleasedAfterCapture {
            // Dispatch resumed when target capture finished, so there is no barrier
            // left to cancel, and the recovery floor must stay behind an
            // archive that was never installed. Doing nothing is the exit.
            return;
        }
        if let Err(error) = self.relay.cancel_abandoned_barrier().await {
            tracing::warn!(
                session_id,
                "abandoned checkpoint could not cancel its relay barrier: {error:#}"
            );
        }
    }
}
