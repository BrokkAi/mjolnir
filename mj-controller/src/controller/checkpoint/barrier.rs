use super::*;

pub(super) async fn connect_checkpoint_relay(
    session_id: &str,
    manager: Option<&SessionManagerControl>,
    reconnect: &targets::CommandSpec,
    project_memory: Option<crate::session_manager::ProjectMemorySyncTarget>,
) -> Result<ControllerRelayLease> {
    if let Some(manager) = manager {
        let handle = manager
            .wait_for_session(session_id, Duration::from_secs(5))
            .await?;
        let mut lease = handle.lease_connection().await?;
        lease
            .connection_mut()
            .set_project_memory_target(project_memory);
        Ok(ControllerRelayLease::Managed {
            handle,
            lease: Some(lease),
        })
    } else {
        let target = crate::session_manager::RelaySessionTarget {
            session_id: session_id.to_owned(),
            spec: reconnect.clone(),
            worker_recovery: None,
            project_memory,
        };
        Ok(ControllerRelayLease::Standalone(
            StandaloneSession::connect(&target).await?,
        ))
    }
}

pub(super) async fn adopt_restarted_checkpoint_relay(
    session_id: &str,
    manager: Option<&SessionManagerControl>,
    connection: StandaloneSession,
) -> Result<ControllerRelayLease> {
    let Some(manager) = manager else {
        return Ok(ControllerRelayLease::Standalone(connection));
    };
    let handle = manager
        .wait_for_session(session_id, Duration::from_secs(5))
        .await?;
    match handle.lease_connection().await {
        Ok(mut lease) => {
            lease.replace_connection(connection);
            Ok(ControllerRelayLease::Managed {
                handle,
                lease: Some(lease),
            })
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                "session actor could not lease after worker restart; using the restarted proxy: {error:#}"
            );
            Ok(ControllerRelayLease::Standalone(connection))
        }
    }
}

/// What waiting for a barrier does while the session is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarrierBusyPolicy {
    /// Give up as soon as the session is seen working. A checkpoint that can
    /// run again later has nothing to gain from holding a barrier behind a
    /// prompt or a turn the harness started on its own: the wait would only
    /// end at the deadline, and the deadline means "wedged", which restarts
    /// the worker and kills the work in flight.
    DeferWhileRunning,
    /// Request non-steering cancellation and wait for the turn to settle.
    /// Close may interrupt work, but only an unresponsive or incompatible
    /// worker needs restart recovery.
    InterruptWhileRunning,
}

impl BarrierBusyPolicy {
    pub(super) fn of(exclusivity: LatchExclusivity) -> Self {
        match exclusivity {
            LatchExclusivity::ReleaseAfterLatch => Self::DeferWhileRunning,
            LatchExclusivity::HoldThroughClose => Self::InterruptWhileRunning,
        }
    }
}

pub(super) async fn wait_for_checkpoint_barrier(
    relay: &mut StandaloneSession,
    session_id: &str,
    command_id: &str,
    timeout: Duration,
    busy: BarrierBusyPolicy,
    harness: HarnessKind,
) -> Result<ManagedSessionSnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut cancel_submitted = false;
    let mut cancel_deadline = None;
    let mut cancel_started_at: Option<Instant> = None;
    loop {
        let snapshot = relay.sync().await?;
        if busy == BarrierBusyPolicy::DeferWhileRunning {
            if !snapshot.operational.safe_for_checkpoint(harness) {
                // The native task level can change after the controller's
                // initial idle sync and before the queued BeginCheckpoint is
                // processed. Defer from the barrier wait rather than allowing
                // its timeout to classify the worker as wedged and restart it.
                return Err(
                    CheckpointDeferred::background_snapshot(&snapshot.operational, harness).into(),
                );
            }
            if snapshot.operational.has_work_in_flight() {
                // A foreground tool, a turn the execution flag has not caught
                // up with, or queued work can all appear after the initial
                // sync. Defer rather than let the deadline restart the worker
                // underneath it.
                return Err(CheckpointDeferred::harness_busy().into());
            }
        }
        if checkpoint_barrier_is_ready(&snapshot, command_id) {
            if let Some(started_at) = cancel_started_at {
                tracing::info!(
                    session_id,
                    barrier_command_id = command_id,
                    cancellation_ms = started_at.elapsed().as_millis() as u64,
                    "active turn cancellation settled before checkpoint barrier"
                );
            }
            return Ok(snapshot);
        }
        if busy == BarrierBusyPolicy::InterruptWhileRunning
            && snapshot.operational.execution == RelayExecutionState::Running
            && !cancel_submitted
        {
            let cancel_command_id = new_command_id("checkpoint-cancel-turn")?;
            match relay
                .submit(cancel_command_id, RelayCommand::CancelTurn)
                .await
            {
                Ok(_) => {
                    cancel_submitted = true;
                    cancel_started_at = Some(Instant::now());
                    cancel_deadline = Some(tokio::time::Instant::now() + CHECKPOINT_CANCEL_TIMEOUT);
                    tracing::info!(
                        session_id,
                        barrier_command_id = command_id,
                        "requested active turn cancellation before checkpoint barrier"
                    );
                }
                Err(error) if checkpoint_cancel_turn_needs_worker_restart(&error) => {
                    return Err(error.context(
                        CheckpointBarrierUnreachable::cancel_turn_unavailable(
                            command_id,
                            relay.protocol_version(),
                        ),
                    ));
                }
                Err(error) if worker_connect_needs_restart(&error) => {
                    return Err(error.context(
                        CheckpointBarrierUnreachable::cancel_turn_unreachable(command_id),
                    ));
                }
                Err(error) => {
                    // The turn can finish between the status sync and this
                    // submit. If the barrier won that race, continue from its
                    // durable ready state; otherwise preserve the rejection.
                    if let Ok(snapshot) = relay.sync().await
                        && checkpoint_barrier_is_ready(&snapshot, command_id)
                    {
                        tracing::info!(
                            session_id,
                            barrier_command_id = command_id,
                            "active turn settled while submitting checkpoint cancellation"
                        );
                        return Ok(snapshot);
                    }
                    return Err(error.context("cancel active ACP turn before checkpoint barrier"));
                }
            }
            continue;
        }
        let out_of_time = tokio::time::Instant::now() >= cancel_deadline.unwrap_or(deadline);
        if let Some(error) = checkpoint_barrier_wait_ended(
            &snapshot,
            command_id,
            busy,
            out_of_time,
            cancel_submitted,
        ) {
            return Err(error);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Why one sync of a barrier that is not ready yet ends the wait, or `None` to
/// keep waiting.
///
/// The deadline means "wedged": it restarts the worker only after a close has
/// already requested cancellation and the turn still has not settled. A
/// checkpoint that can try again later defers as soon as it sees work. "Work"
/// is the shared predicate, not the bare execution flag: a stale projection
/// can report `Idle` while a harness turn or a foreground tool is still live,
/// and treating that as wedged would restart the worker under it.
pub(super) fn checkpoint_barrier_wait_ended(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    busy: BarrierBusyPolicy,
    out_of_time: bool,
    cancel_submitted: bool,
) -> Option<anyhow::Error> {
    if snapshot.operational.execution == RelayExecutionState::Closed {
        return Some(CheckpointBarrierUnreachable::runtime_stopped().into());
    }
    if busy == BarrierBusyPolicy::DeferWhileRunning {
        if snapshot.operational.has_work_in_flight() {
            return Some(CheckpointDeferred::harness_busy().into());
        }
        return out_of_time.then(|| CheckpointBarrierUnreachable::not_admitted(command_id).into());
    }
    // Close already asked to interrupt the active turn, so only a turn that
    // never settles after cancellation reaches the restart path.
    if snapshot.operational.execution == RelayExecutionState::Running {
        return (out_of_time && cancel_submitted)
            .then(|| CheckpointBarrierUnreachable::cancel_timed_out(command_id).into());
    }
    out_of_time.then(|| CheckpointBarrierUnreachable::not_admitted(command_id).into())
}

/// The ACP runtime never admitted a checkpoint barrier: it stopped first, or it
/// never reached the barrier before the deadline.
///
/// [`wait_for_checkpoint_barrier`] is the only producer, and the retry decision
/// downcasts for this marker rather than reading the message, so rewording a
/// diagnostic cannot silently disable the restart-and-retry path.
#[derive(Debug)]
pub(super) struct CheckpointBarrierUnreachable(pub(super) String);

impl CheckpointBarrierUnreachable {
    pub(super) fn runtime_stopped() -> Self {
        Self("ACP runtime stopped before reaching the checkpoint barrier".to_owned())
    }

    pub(super) fn not_admitted(command_id: &str) -> Self {
        Self(format!(
            "ACP relay did not reach checkpoint barrier {command_id}"
        ))
    }

    pub(super) fn cancel_timed_out(command_id: &str) -> Self {
        Self(format!(
            "active ACP turn did not settle after cancellation before checkpoint barrier {command_id}"
        ))
    }

    pub(super) fn cancel_turn_unavailable(command_id: &str, protocol_version: u32) -> Self {
        Self(format!(
            "worker protocol {protocol_version} cannot cancel the active ACP turn before checkpoint barrier {command_id} (requires protocol {})",
            RelayCommand::CancelTurn.minimum_protocol(),
        ))
    }

    pub(super) fn cancel_turn_unreachable(command_id: &str) -> Self {
        Self(format!(
            "worker transport became unavailable while cancelling the active ACP turn before checkpoint barrier {command_id}"
        ))
    }
}

impl std::fmt::Display for CheckpointBarrierUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointBarrierUnreachable {}

pub(super) fn checkpoint_barrier_needs_worker_restart(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CheckpointBarrierUnreachable>()
        .is_some()
}

/// A worker that cannot decode `CancelTurn` needs to be replaced before the
/// close can retry the checkpoint with cancellation available. The relay client
/// refuses the command for an older worker with the same code the worker uses.
pub(super) fn checkpoint_cancel_turn_needs_worker_restart(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let Some(rejected) = cause.downcast_ref::<RelayRejected>() else {
            return false;
        };
        rejected.0.code == mj_core::relay::RelayErrorCode::IncompatibleProtocol
    })
}

/// The session was working, so this checkpoint did not run. Nothing is wrong
/// with the session, the target, or the last archive.
///
/// A busy session is the normal state of a session someone is using, including
/// one working through a turn the harness started on its own after a
/// background command. Treating that as a checkpoint failure would restart the
/// worker, record a failure against the session, and back the next attempt off
/// for hours. Callers that can try again later defer instead; the same work is
/// copied at the next idle observation.
#[derive(Debug)]
pub struct CheckpointDeferred(String);

impl CheckpointDeferred {
    pub fn harness_busy() -> Self {
        Self("the agent is working; try again when it is idle".to_owned())
    }

    pub(super) fn background_work() -> Self {
        Self("Kimi background-agent state could not be synchronized; checkpoint requires a synchronized empty task list".into())
    }

    pub(super) fn background_snapshot(
        state: &mj_core::relay::RelayOperationalState,
        harness: HarnessKind,
    ) -> Self {
        Self(
            state
                .checkpoint_background_blocker(harness)
                .unwrap_or("background state changed during checkpoint")
                .into(),
        )
    }

    pub(super) fn frontier_moved() -> Self {
        Self(
            "the session moved past the checkpoint-ready cursor before the barrier latched, so this checkpoint was deferred"
                .to_owned(),
        )
    }

    pub(super) fn harness_turn_during_capture() -> Self {
        Self(
            "the agent started a turn of its own while target state was captured, so this checkpoint was deferred"
                .to_owned(),
        )
    }
}

impl std::fmt::Display for CheckpointDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointDeferred {}

/// Whether a failed checkpoint only means the session was busy.
///
/// The marker is carried by the error, not by its text. It may be the root
/// error or attached with `context`, and callers wrap checkpoint errors in
/// further context. `anyhow`'s own downcast walks every context layer;
/// `chain()` does not expose a context value, so it must not be used here.
pub fn checkpoint_was_deferred(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CheckpointDeferred>().is_some()
}
