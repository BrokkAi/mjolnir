//! One recoverable stop/restore operation, independent of its initiating viewer.

mod destination;
mod handoff;
#[cfg(test)]
mod tests;
mod transfer;

use anyhow::{Context, Result, bail, ensure};
use mj_core::hex::lower_hex;
use sha2::{Digest, Sha256};

use super::lifecycle::SourceTargetDisposition;
use super::{Controller, SessionResumeOptions, now};

/// Execution and retained queue admission are phases of one ownership record.
#[derive(Clone, Copy)]
enum MoveOwnership {
    Executing,
    ExecutingQueue,
    PendingQueue,
}

pub(in crate::controller) enum SubagentMutationDrain {
    Drained,
    UnsupportedWorkerProtocol(u32),
}

fn move_ownership() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, MoveOwnership>>
{
    static OWNER: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, MoveOwnership>>,
    > = std::sync::OnceLock::new();
    OWNER.get_or_init(Default::default)
}

pub fn move_owns_session(session_id: &str) -> bool {
    move_ownership()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(session_id)
}

pub fn move_has_pending_queue(session_id: &str) -> bool {
    matches!(
        move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id),
        Some(MoveOwnership::ExecutingQueue | MoveOwnership::PendingQueue)
    )
}

pub fn release_move_queue_hold(session_id: &str) {
    set_move_queue_hold(session_id, false);
}

fn set_move_queue_hold(session_id: &str, pending: bool) {
    let mut owner = move_ownership()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = match (owner.get(session_id), pending) {
        (Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue), true) => {
            Some(MoveOwnership::ExecutingQueue)
        }
        (Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue), false) => {
            Some(MoveOwnership::Executing)
        }
        (_, true) => Some(MoveOwnership::PendingQueue),
        (_, false) => None,
    };
    if let Some(next) = next {
        owner.insert(session_id.to_owned(), next);
    } else {
        owner.remove(session_id);
    }
}

pub fn restore_move_queue_hold(operation: &MoveOperation) {
    set_move_queue_hold(
        &operation.selection.session_id,
        operation.queue_admission_started && !operation.queue_admission_finished,
    );
}

/// What a recovered, interrupted source stop tells the person.
///
/// Recovery keeps the environment promised by an in-place move.
fn interrupted_source_stop_message(recovered: &str, in_place: bool) -> String {
    if in_place {
        format!("{recovered}; the in-place swap was interrupted; the environment was retained")
    } else {
        recovered.to_owned()
    }
}

/// Whether the persisted record shows a source that has finished stopping with
/// a verified checkpoint to restore.
///
/// Only `Stopped` proves the source stop finished. Resume also admits recovery
/// states, but those do not establish that the source is already stopped.
fn source_stopped_with_verified_checkpoint(record: &mj_core::state::SessionRecord) -> bool {
    record.state == SessionState::Stopped && record.checkpoint.is_some()
}

/// The recovery guidance for a failed Move whose source is already stopped with
/// a verified checkpoint.
///
/// Resume restores the retained checkpoint on the destination profile and
/// target the Move selected.
fn stopped_source_recovery(
    session_id: &str,
    destination_profile: Option<&str>,
    destination_target: Option<&str>,
) -> String {
    let flag = |name: &str, value: Option<&str>| {
        value
            .filter(|value| !value.is_empty())
            .map(|value| format!(" --{name} {value}"))
            .unwrap_or_default()
    };
    format!(
        "Source is stopped with a verified checkpoint. Bring it back with \
         `mj resume --session {session_id}{}{} --queue start`.",
        flag("profile", destination_profile),
        flag("target", destination_target),
    )
}

/// The recovery guidance a failed or cancelled Move shows, from its persisted
/// state.
///
/// What is retained is read from the Move record alone
/// ([`MoveOperation::checkpoint_retained`] and
/// [`MoveOperation::holds_source_environment`]), the same facts the API's
/// `move_recovery` and the web page read, so the text never promises a retry
/// the record cannot support.
fn failed_move_recovery(operation: &MoveOperation, state: &mj_core::state::State) -> String {
    let record = state.sessions.get(&operation.selection.session_id);
    if let Some(destination) = &operation.prepared_destination
        && matches!(
            destination.state,
            PreparedDestinationState::CleanupPending { .. }
        )
    {
        return format!(
            "Source and recovery data retained. Automatic EC2 cleanup is pending for {}; charges may continue until termination is confirmed.",
            destination
                .instance_id()
                .unwrap_or("the recorded launch token")
        );
    }
    if operation.prepared_destination.is_some()
        && record.is_some_and(|r| {
            matches!(r.state, SessionState::Running | SessionState::Disconnected)
                && r.target == operation.source_target
        })
    {
        return "Source retained and still running. EC2 destination cleaned up; prepare Move again when ready.".into();
    }
    if operation.queue_admission_started {
        return "Destination is live; retry queue admission on this same destination. Already accepted work may have effects.".to_owned();
    }
    let source_live = record.is_some_and(|record| {
        matches!(
            record.state,
            SessionState::Running | SessionState::Disconnected
        )
    });
    let environment_held = operation.holds_source_environment()
        || (operation.in_place
            && !source_live
            && record.is_some_and(|record| record.target.is_some()));
    if environment_held && !operation.checkpoint_retained() {
        return missing_move_checkpoint_recovery(state, &operation.selection.session_id);
    }
    if operation.in_place && record.is_some_and(|record| record.target.is_some()) {
        return if source_live {
            "Source retained and still running. Retry Move when ready.".to_owned()
        } else {
            "Environment and checkpoint retained. Retry Move on the same target; the checkout will not be recreated.".to_owned()
        };
    }
    match record {
        Some(record) if source_stopped_with_verified_checkpoint(record) => {
            stopped_source_recovery(
                &operation.selection.session_id,
                operation.selection.profile_id.as_deref(),
                operation.selection.target_template_id.as_deref(),
            )
        }
        _ => "Source or partial destination is retained. Retry move after resolving the reported error.".to_owned(),
    }
}

/// The guidance for a Move that holds its source environment but has lost the
/// checkpoint a retry would restore. Neither a retry nor Resume can bring the
/// conversation back, and Destroy removes the checkout, so the text says where
/// the files are first.
fn missing_move_checkpoint_recovery(state: &mj_core::state::State, session_id: &str) -> String {
    let checkout_path = state
        .checkout(session_id)
        .ok()
        .and_then(|checkout| match &checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                Some(worktree.worktree_root.clone())
            }
            mj_core::state::Checkout::Attached { path } => Some((*path).to_path_buf()),
            mj_core::state::Checkout::Borrowed { .. } => {
                checkout.project_directory().map(|path| path.to_path_buf())
            }
            mj_core::state::Checkout::ManagedWorkspace => None,
        });
    let checkout = checkout_path
        .map(|path| format!(" in {}", path.display()))
        .unwrap_or_default();
    format!(
        "The Move's checkpoint archive is missing, so the Move cannot be retried and Resume cannot restore the session. \
         The environment and checkout{checkout} are retained; copy out anything you need, then destroy the session."
    )
}

/// The published message of a Move that did not finish. `phase` is where it
/// stopped, when that is known.
fn failed_move_message(
    phase: Option<&str>,
    cancelled: bool,
    recovery: &str,
    operation_id: &str,
) -> String {
    format!(
        "{}{}{}. {recovery} The daemon log records the reason under reference {operation_id}",
        mj_core::state::MOVE_FAILURE_PREFIX,
        phase
            .map(|phase| format!(" while {phase}"))
            .unwrap_or_default(),
        if cancelled { " (cancelled)" } else { "" },
    )
}

/// Drop every archive reference whose file is gone from this Move record, and
/// say whether anything changed.
///
/// A Move that took a handoff restores only from it: its `checkpoint` is the
/// source's older full checkpoint, and restoring that would silently lose the
/// conversation since. So a missing handoff drops both references, and the
/// record then says plainly that nothing is retained to restore.
pub(crate) fn forget_missing_move_archives(operation: &mut MoveOperation) -> bool {
    let missing = |checkpoint: &Option<mj_core::state::CheckpointMetadata>| {
        checkpoint.as_ref().is_some_and(|checkpoint| {
            matches!(
                std::fs::symlink_metadata(&checkpoint.archive_path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            )
        })
    };
    let lost = if missing(&operation.handoff) {
        [operation.handoff.take(), operation.checkpoint.take()]
    } else if operation.handoff.is_none() && missing(&operation.checkpoint) {
        [None, operation.checkpoint.take()]
    } else {
        return false;
    };
    for checkpoint in lost.iter().flatten() {
        tracing::warn!(
            session_id = %operation.selection.session_id,
            reference = %operation.operation_id,
            path = %checkpoint.archive_path.display(),
            "a Move's checkpoint archive is missing; recording that the Move cannot restore it"
        );
    }
    true
}

/// Record, at daemon startup, every Move whose archive is missing. An active
/// Move is then recovered from the corrected record; a finished one gets its
/// published guidance rewritten from it, since nothing else publishes it again.
pub(crate) fn record_missing_move_archives(
    state: &mj_core::state::State,
    operations: Vec<MoveOperation>,
) -> Result<()> {
    for mut operation in operations {
        if !operation.retains_checkpoint() || !forget_missing_move_archives(&mut operation) {
            continue;
        }
        let record = state.sessions.get(&operation.selection.session_id);
        let published = record.and_then(|record| record.last_error.as_deref());
        if operation.is_active()
            || !published
                .is_some_and(|error| error.starts_with(mj_core::state::MOVE_FAILURE_PREFIX))
        {
            crate::database::save_move_operation(&operation)?;
            continue;
        }
        record_finished_move_recovery(state, &mut operation)?;
    }
    Ok(())
}

/// Refresh recovery guidance when a durable background cleanup changes it.
pub(crate) fn record_finished_move_recovery(
    state: &mj_core::state::State,
    operation: &mut MoveOperation,
) -> Result<()> {
    let record = state.sessions.get(&operation.selection.session_id);
    operation.updated_at = now();
    if !operation.is_active()
        && record
            .and_then(|record| record.last_error.as_deref())
            .is_some_and(|message| message.starts_with(mj_core::state::MOVE_FAILURE_PREFIX))
    {
        let message = failed_move_message(
            None,
            operation.phase == MovePhase::Cancelled,
            &failed_move_recovery(operation, state),
            &operation.operation_id,
        );
        crate::database::save_move_outcome(operation, Some(&message))
    } else {
        crate::database::save_move_operation(operation)
    }
}

/// Why a retried sealed Move's selection is refused: each part that differs
/// from the selection the Move was sealed with, and what to send to match it.
fn sealed_selection_difference(retained: &MoveSelection, requested: &MoveSelection) -> String {
    let mut parts = Vec::new();
    for (name, flag, retained, requested) in [
        (
            "profile",
            "--profile",
            &retained.profile_id,
            &requested.profile_id,
        ),
        (
            "target",
            "--target",
            &retained.target_template_id,
            &requested.target_template_id,
        ),
    ] {
        if retained != requested {
            parts.push(format!(
                "{name} (sealed with {0}; pass {flag} {0})",
                retained.as_deref().unwrap_or_default()
            ));
        }
    }
    if retained.workspace.acknowledge_large_transfer
        != requested.workspace.acknowledge_large_transfer
    {
        parts.push(if retained.workspace.acknowledge_large_transfer {
            "large-transfer acknowledgement (the Move was sealed with it; pass --allow-large-transfer)".to_owned()
        } else {
            "large-transfer acknowledgement (the Move was sealed without it; omit --allow-large-transfer)".to_owned()
        });
    }
    if retained.workspace.exclusions != requested.workspace.exclusions {
        let excluded = retained
            .workspace
            .exclusions
            .iter()
            .map(|path| format!("{}:{}", path.repository, path.path.display()))
            .collect::<Vec<_>>();
        parts.push(if excluded.is_empty() {
            "excluded files (the Move was sealed excluding none)".to_owned()
        } else {
            format!(
                "excluded files (the Move was sealed excluding exactly {})",
                excluded.join(", ")
            )
        });
    }
    if retained.additional_mounts != requested.additional_mounts {
        parts.push("attached directories (send the ones the Move was sealed with)".to_owned());
    }
    if retained.resource_allocation != requested.resource_allocation
        || retained.clear_resource_allocation != requested.clear_resource_allocation
    {
        parts.push("resource allocation (send the one the Move was sealed with)".to_owned());
    }
    if retained.subagents != requested.subagents {
        parts.push("sub-agent policy (send the one the Move was sealed with)".to_owned());
    }
    if retained.session_id != requested.session_id || parts.is_empty() {
        parts.push("the session".to_owned());
    }
    format!(
        "a sealed Move must be retried with the selection it was sealed with; this request differs in its {}",
        parts.join("; ")
    )
}

pub struct MoveMutationGuard(String);

impl MoveMutationGuard {
    pub fn reserve(session_id: &str) -> Result<Self> {
        let mut owner = move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            !matches!(
                owner.get(session_id),
                Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue)
            ),
            "session already has a move owner"
        );
        let next = if owner.contains_key(session_id) {
            MoveOwnership::ExecutingQueue
        } else {
            MoveOwnership::Executing
        };
        owner.insert(session_id.to_owned(), next);
        Ok(Self(session_id.to_owned()))
    }
}

impl Drop for MoveMutationGuard {
    fn drop(&mut self) {
        let mut owner = move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match owner.get(&self.0) {
            Some(MoveOwnership::ExecutingQueue) => {
                owner.insert(self.0.clone(), MoveOwnership::PendingQueue);
            }
            Some(MoveOwnership::Executing) => {
                owner.remove(&self.0);
            }
            _ => {}
        }
    }
}

pub(crate) fn move_refuses_command(session_id: &str, command: &RelayCommand) -> bool {
    move_owns_session(session_id)
        && matches!(
            command,
            RelayCommand::Prompt { .. }
                | RelayCommand::SetConfig { .. }
                | RelayCommand::SetSessionMode { .. }
                | RelayCommand::RestoreExecutionMode
                | RelayCommand::RunUserShell { .. }
                | RelayCommand::CancelUserShell { .. }
                | RelayCommand::Cancel
                | RelayCommand::RemoveQueuedPrompt { .. }
                | RelayCommand::ClearQueuedPrompts
        )
}
use crate::session_manager::{SessionManagerControl, StandaloneSession, new_command_id};
use mj_checkpoint::archive::{CanonicalQueuedCommandKind, verify_archive_streaming};
use mj_core::state::{
    DestinationChecks, MoveOperation, MovePhase, PreparedDestinationState, ResumeQueueDisposition,
    SessionState,
};

pub use mj_core::state::{MoveOutcome, MovePreparation, MoveSelection, MoveSessionRequest};

use crate::targets::{CommandExecutor, ProvisionStage, ProvisionStageGuard};
use mj_core::relay::RelayCommand;

/// Refresh source state without turning a dead source harness into a Move prerequisite.
pub async fn refresh_move_source(
    manager: &SessionManagerControl,
    id: &str,
) -> Result<Option<mj_core::state::ManagedSessionSnapshot>> {
    Ok(MoveSourceRelay::lease(manager, id).await?.snapshot())
}

/// A Move's hold on its source's relay connection, from the confirmation
/// until the checkpoint that seals the source takes it over. While it is
/// held, the session actor admits nothing to the relay, so the state the Move
/// was confirmed against is the state it interrupts. Dropping it hands the
/// connection back to the actor.
#[derive(Default)]
pub(in crate::controller) struct MoveSourceRelay(
    Option<super::checkpoint::ControllerRelayLease>,
    Option<crate::worker_lifecycle::WorkerPermit>,
);

impl MoveSourceRelay {
    pub(in crate::controller) fn owner(&self) -> Option<crate::worker_lifecycle::WorkerPermit> {
        self.1.clone()
    }

    /// Take the source's connection from its session actor, which syncs it
    /// before handing it over. Holds nothing when the source's worker cannot
    /// be reached; the Move then recovers it without its harness. Leasing
    /// preserves typed transport failures, unlike the UI's string-valued sync.
    pub(in crate::controller) async fn lease(
        manager: &SessionManagerControl,
        id: &str,
    ) -> Result<Self> {
        let owner = crate::worker_lifecycle::WorkerPermit::acquire(
            id,
            "Move source relay",
            &crate::targets::ProcessExecutor,
        )
        .await?;
        let handle = manager
            .wait_for_session(id, std::time::Duration::from_secs(5))
            .await?;
        match handle.lease_connection().await {
            Ok(lease) => Ok(Self(
                Some(super::checkpoint::ControllerRelayLease::Managed {
                    handle,
                    lease: Some(lease),
                }),
                Some(owner),
            )),
            Err(error) if crate::worker_client::RelayTransportDead::marks(&error) => {
                tracing::warn!(session_id = id, error = %error, "Move will recover the unavailable source without its harness");
                Ok(Self(None, Some(owner)))
            }
            Err(error) => Err(error),
        }
    }

    async fn set_subagent_admission_via_manager(
        manager: &SessionManagerControl,
        id: &str,
        open: bool,
    ) -> Result<bool> {
        let handle = manager
            .wait_for_session(id, std::time::Duration::from_secs(5))
            .await?;
        let mut lease = handle.lease_connection().await?;
        if !(mj_core::relay::RelayRequest::SetSubagentAdmission { open })
            .supported_at(lease.connection_mut().protocol_version())
        {
            lease.release();
            return Ok(false);
        }
        let result = lease.connection_mut().set_subagent_admission(open).await;
        lease.release();
        result.map(|()| true)
    }

    /// The source's state as of its last sync, or `None` when it is unreachable.
    pub(in crate::controller) fn snapshot(
        &mut self,
    ) -> Option<mj_core::state::ManagedSessionSnapshot> {
        self.0
            .as_mut()
            .map(|relay| relay.connection_mut().snapshot())
    }

    /// Sync the held connection again. A source whose transport died since it
    /// was leased is let go and reported unreachable.
    pub(in crate::controller) async fn sync(
        &mut self,
        id: &str,
    ) -> Result<Option<mj_core::state::ManagedSessionSnapshot>> {
        let Some(relay) = self.0.as_mut() else {
            return Ok(None);
        };
        match relay.sync_snapshot().await {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(error) if crate::worker_client::RelayTransportDead::marks(&error) => {
                tracing::warn!(session_id = id, error = %error, "Move will recover the unavailable source without its harness");
                self.0 = None;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Close mutation admission at the worker, let its actor continue polling
    /// accepted requests until they finish, then take the relay back for the
    /// checkpoint. The worker queue lock is the admission/drain boundary.
    pub(in crate::controller) async fn drain_subagent_mutations(
        &mut self,
        id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<SubagentMutationDrain> {
        ensure!(
            self.0.is_some(),
            "cannot safely drain sub-agent requests because the source worker is unavailable"
        );
        let protocol_version = self
            .0
            .as_mut()
            .expect("held source relay checked")
            .connection_mut()
            .protocol_version();
        if !(mj_core::relay::RelayRequest::SetSubagentAdmission { open: false })
            .supported_at(protocol_version)
        {
            return Ok(SubagentMutationDrain::UnsupportedWorkerProtocol(
                protocol_version,
            ));
        }
        if let Err(error) = self
            .0
            .as_mut()
            .expect("held source relay checked")
            .connection_mut()
            .set_subagent_admission(false)
            .await
        {
            // The worker may have persisted the close before its reply was
            // lost. Drop this connection and make a best-effort idempotent
            // reopen on the actor's next relay connection.
            self.release_managed_connection();
            if let Err(reopen) = self.reopen_subagent_mutations_with_retry(id).await {
                return Err(error.context(format!(
                    "could not reopen sub-agent requests after an ambiguous admission close: {reopen:#}"
                )));
            }
            return Err(error);
        }
        self.release_managed_connection();

        let drain = async {
            loop {
                ensure!(
                    !executor.cancellation_requested(),
                    "Move cancelled while waiting for sub-agent requests"
                );
                let snapshot = self.sync(id).await?.context(
                    "source worker became unavailable while draining sub-agent requests",
                )?;
                let parent = id.to_owned();
                let effects_pending = tokio::task::spawn_blocking(move || {
                    crate::database::has_pending_mutating_delegations(&parent)
                })
                .await??;
                if !subagent_mutations_pending(&snapshot.subagent_requests, effects_pending) {
                    break;
                }
                executor.notify_notice("Waiting for subagent requests to finish");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(error) = drain {
            if let Err(reopen) = self.reopen_subagent_mutations_with_retry(id).await {
                return Err(error.context(format!(
                    "could not reopen sub-agent requests after Move stopped waiting: {reopen:#}"
                )));
            }
            return Err(error);
        }
        self.reacquire_managed_connection().await?;
        if executor.cancellation_requested() {
            self.reopen_subagent_mutations_with_retry(id).await?;
            bail!("Move cancelled before source interruption; sub-agent requests reopened");
        }
        Ok(SubagentMutationDrain::Drained)
    }

    pub(in crate::controller) async fn reopen_subagent_mutations(
        &mut self,
        id: &str,
    ) -> Result<()> {
        self.reopen_subagent_mutations_with_retry(id).await
    }

    async fn open_subagent_mutations(&mut self) -> Result<()> {
        self.reacquire_managed_connection().await?;
        let result = self
            .0
            .as_mut()
            .context("source worker is unavailable")?
            .connection_mut()
            .set_subagent_admission(true)
            .await;
        self.release_managed_connection();
        result
    }

    async fn reopen_subagent_mutations_with_retry(&mut self, id: &str) -> Result<()> {
        let first_error = match self.open_subagent_mutations().await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        tracing::warn!(
            session_id = id,
            error = %first_error,
            "sub-agent admission reopen failed; retrying on a fresh relay connection"
        );
        tokio::task::yield_now().await;
        match self.open_subagent_mutations().await {
            Ok(()) => {
                tracing::info!(
                    session_id = id,
                    "reopened sub-agent requests on a replacement relay connection"
                );
                Ok(())
            }
            Err(retry_error) => {
                tracing::warn!(
                    session_id = id,
                    error = %retry_error,
                    "could not reopen sub-agent requests on the replacement relay connection"
                );
                Err(first_error.context(format!("relay reconnect retry failed: {retry_error:#}")))
            }
        }
    }

    async fn reacquire_managed_connection(&mut self) -> Result<()> {
        if let Some(super::checkpoint::ControllerRelayLease::Managed { handle, lease }) =
            self.0.as_mut()
            && lease.is_none()
        {
            *lease = Some(handle.lease_connection().await?);
        }
        Ok(())
    }

    fn release_managed_connection(&mut self) {
        if let Some(super::checkpoint::ControllerRelayLease::Managed { lease, .. }) =
            self.0.as_mut()
            && let Some(lease) = lease.take()
        {
            lease.release();
        }
    }

    pub(in crate::controller) fn is_held(&self) -> bool {
        self.0.is_some()
    }

    /// Hand the connection to the checkpoint that seals the source.
    pub(in crate::controller) fn take(
        &mut self,
    ) -> Option<super::checkpoint::ControllerRelayLease> {
        self.0.take()
    }

    /// Keep holding the source across a worker restart, on the new worker's
    /// connection.
    pub(in crate::controller) fn replace_connection(
        &mut self,
        connection: crate::session_manager::StandaloneSession,
    ) {
        if let Some(relay) = self.0.as_mut() {
            relay.replace_connection(connection);
        }
    }
}

impl Drop for MoveSourceRelay {
    fn drop(&mut self) {
        if let Some(relay) = self.0.take() {
            relay.release();
        }
        self.1.take();
    }
}

fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(lower_hex(Sha256::digest(serde_json::to_vec(value)?)))
}

struct MovePhaseTimer<'a> {
    session_id: &'a str,
    phase: &'static str,
    started: std::time::Instant,
}

impl<'a> MovePhaseTimer<'a> {
    fn new(session_id: &'a str, phase: &'static str) -> Self {
        Self {
            session_id,
            phase,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for MovePhaseTimer<'_> {
    fn drop(&mut self) {
        tracing::info!(
            session_id = self.session_id,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "move phase finished"
        );
    }
}

/// Confirmation is an inspector, not transport for attachment bytes: every
/// surface that shows a move preparation sees a placeholder for each queued
/// image. Replay always reads the verified archive after destination readiness.
fn replace_queued_images_with_placeholders(
    queued_commands: &mut [mj_core::state::MaterializedQueuedPrompt],
) {
    for block in queued_commands
        .iter_mut()
        .flat_map(|command| command.content.iter_mut())
    {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("image") {
            continue;
        }
        let mime = block
            .get("mimeType")
            .or_else(|| block.get("mime_type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("image");
        *block = serde_json::json!({"type": "text", "text": format!("[Image attachment: {mime}]")});
    }
}

impl Controller {
    /// Returns the planned conversion when this move turns a local checkout
    /// into an isolated workspace, so the caller can describe it without
    /// reading Git a second time.
    fn validate_move_destination_paths(
        &self,
        source: &mj_core::state::SessionRecord,
        target_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<super::worktree::RawToWorkspaceConversion>> {
        use super::worktree::ResumePlan;
        let checkout = self.state.checkout(&source.id)?;
        match super::worktree::resume_compatibility_with_checkout(
            source,
            &checkout,
            &self.config,
            target_id,
        )
        .map_err(anyhow::Error::msg)?
        {
            ResumePlan::RawToWorkspace => {
                return Ok(Some(super::worktree::plan_raw_to_workspace_with_checkout(
                    &checkout, executor,
                )?));
            }
            ResumePlan::WorkspaceToRaw => {
                self.plan_workspace_to_raw(source, target_id, executor)?;
            }
            ResumePlan::InPlace
                if matches!(checkout, mj_core::state::Checkout::Attached { .. }) =>
            {
                if let Some(path) = checkout.project_directory() {
                    self.validate_project_directory(target_id, path, executor)?;
                }
            }
            ResumePlan::InPlace => {}
        }
        Ok(None)
    }
    fn move_confirmation(
        &self,
        selection: &MoveSelection,
        conversion: Option<&mj_core::state::RawConversionPreview>,
    ) -> Result<(bool, Vec<mj_core::state::MaterializedQueuedPrompt>, String)> {
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        let (mut active, mut queued) = crate::database::move_pending_work(&source.id)?;
        if let Some(operation) = crate::database::load_move_operation(&source.id)?
            && operation.queue_admission_started
            && !operation.queue_admission_finished
        {
            ensure!(
                operation.selection == *selection,
                "queue admission is incomplete on the live destination; retry that move before selecting another destination"
            );
            let checkpoint = operation
                .restore_artifact()
                .context("retained queue checkpoint is missing")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            ensure!(
                verified.archive_sha256 == checkpoint.sha256
                    && verified.manifest.session.id == source.id,
                "retained move checkpoint verification failed"
            );
            queued = verified
                .canonical_session
                .queued_prompts
                .into_iter()
                .map(|entry| mj_core::state::MaterializedQueuedPrompt {
                    accepted_ordinal: None,
                    command_id: entry.command_id,
                    kind: match entry.kind {
                        CanonicalQueuedCommandKind::Prompt => {
                            mj_core::state::QueuedCommandKind::Prompt
                        }
                        CanonicalQueuedCommandKind::SetConfig { key, value } => {
                            mj_core::state::QueuedCommandKind::SetConfig { key, value }
                        }
                    },
                    content: entry.content,
                    queued_at_ms: entry.queued_at_ms,
                })
                .collect();
            active = false;
        }
        let fingerprint = digest(&(
            &source.last_profile,
            &source.target_template_id,
            &source.target,
            &source.target_runtime,
            &source.native_session_id,
            &source.resource_allocation,
            &source.additional_mounts,
            &source.container_cpus,
            &source.container_memory,
            selection,
            self.move_configuration_fingerprint(selection)?,
            &queued,
            // Only what the destination is built from. The dirty counts move
            // with every keystroke of a live agent, and hashing them would
            // invalidate the confirmation the person is reading.
            conversion.map(|preview| {
                (
                    &preview.fetch_url,
                    &preview.push_urls,
                    &preview.branch,
                    &preview.destination,
                )
            }),
        ))?;
        Ok((active, queued, fingerprint))
    }
    fn verify_current_move_configuration(&self, operation: &MoveOperation) -> Result<()> {
        let current = Controller {
            config: mj_core::config::Config::load()?,
            state: self.state.clone(),
        };
        ensure!(
            current.move_configuration_fingerprint(&operation.selection)?
                == operation.configuration_fingerprint,
            "destination configuration changed during Move preparation; source retained, prepare and confirm Move again"
        );
        Ok(())
    }

    pub(super) fn move_configuration_fingerprint(
        &self,
        selection: &MoveSelection,
    ) -> Result<String> {
        let profile = self
            .config
            .profiles
            .get(
                selection
                    .profile_id
                    .as_deref()
                    .context("move profile is unresolved")?,
            )
            .context("move profile no longer exists")?;
        let target = self
            .config
            .targets
            .get(
                selection
                    .target_template_id
                    .as_deref()
                    .context("move target is unresolved")?,
            )
            .context("move target no longer exists")?;
        // Only a digest crosses IPC or enters the move record. Configuration
        // may contain credential-bearing values and must never be copied there.
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        digest(&(
            profile,
            target,
            &self.config.bundles,
            self.config.targets.get(&source.target_template_id),
            self.config.profiles.get(&source.last_profile),
        ))
    }

    async fn validate_move_destination_configuration(
        &self,
        selection: &MoveSelection,
        source_harness: mj_core::config::HarnessKind,
        operational: &mj_core::relay::RelayOperationalState,
    ) -> Result<()> {
        let profile_id = selection
            .profile_id
            .as_deref()
            .context("move profile is unresolved")?;
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .context("move profile no longer exists")?;
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        if profile.kind != source_harness || profile_id == source.last_profile {
            return Ok(());
        }
        let accepted = mj_core::acp::AcceptedSessionConfig::from_configuration(
            &operational.config,
            &operational.config_options,
        );
        if accepted.model.is_none() && accepted.effort.is_none() {
            return Ok(());
        }
        // A fresh probe prevents a stale local catalogue from approving a
        // move that the destination profile will immediately reset.
        let choices =
            super::profile_config::discover(profile_id.to_owned(), accepted.model.clone(), true)
                .await
                .with_context(|| {
                    format!("discover destination profile {profile_id:?} configuration")
                })?;
        validate_preserved_configuration(profile_id, &accepted, &choices)
    }

    pub async fn prepare_move_session_controlled(
        &self,
        mut selection: MoveSelection,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MovePreparation> {
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown session")?;
        let source_checkout = self.state.checkout(&source.id)?;
        ensure!(
            !self.state.subagents.contains_key(&source.id),
            "sub-agent sessions cannot move independently of their parent"
        );
        let previous = crate::database::load_move_operation(&source.id)?;
        // The sealed selection owns retry defaults. Reconstructing them from
        // the source can change the destination or lose an explicit policy.
        if let Some(retained) = previous.as_ref().filter(|op| op.holds_source_environment()) {
            selection.profile_id = selection
                .profile_id
                .or(retained.selection.profile_id.clone());
            selection.target_template_id = selection
                .target_template_id
                .or(retained.selection.target_template_id.clone());
            selection.subagents = selection.subagents.or(retained.selection.subagents.clone());
            // Legacy selections omitted an unchanged policy. A viewer may
            // send that same effective policy explicitly; it changes nothing.
            if retained.selection.subagents.is_none()
                && selection.subagents.as_ref()
                    == Some(&source.subagents.clone().unwrap_or_default())
            {
                selection.subagents = None;
            }
        }
        ensure!(
            selection.profile_id.is_some() || selection.target_template_id.is_some(),
            "move requires a target or profile selection"
        );
        ensure!(
            previous
                .as_ref()
                .and_then(|op| op.prepared_destination.as_ref())
                .is_none_or(|d| !matches!(
                    d.state,
                    PreparedDestinationState::CleanupPending { .. }
                )),
            "EC2 destination cleanup is pending; source retained. Automatic cleanup will retry before another Move"
        );

        if previous
            .as_ref()
            .is_some_and(|op| op.holds_source_environment() && !op.checkpoint_retained())
        {
            bail!(
                "this Move cannot be retried. {}",
                missing_move_checkpoint_recovery(&self.state, &source.id)
            );
        }
        let retry = previous.as_ref().is_some_and(|op| {
            !matches!(op.phase, MovePhase::Completed)
                && (op.restore_artifact().is_some() || source.checkpoint.is_some())
        });
        ensure!(
            matches!(
                source.state,
                SessionState::Running | SessionState::Disconnected
            ) || retry,
            "only active sessions can move; run `mj resume` (or POST /api/v1/sessions/{}/resume) for a stopped or lost session",
            source.id
        );
        selection
            .profile_id
            .get_or_insert_with(|| source.last_profile.clone());
        selection
            .target_template_id
            .get_or_insert_with(|| source.target_template_id.clone());
        selection
            .additional_mounts
            .get_or_insert_with(|| source.additional_mounts.clone());
        ensure!(
            !selection.clear_resource_allocation || selection.resource_allocation.is_none(),
            "select resource allocation or explicitly clear it, not both"
        );
        if selection.resource_allocation.is_none()
            && matches!(
                self.config
                    .targets
                    .get(selection.target_template_id.as_deref().unwrap()),
                Some(mj_core::config::TargetTemplate::AwsEc2 { .. })
            )
            && (matches!(
                source.resource_allocation,
                Some(mj_core::state::SessionResourceAllocation::Container { .. })
            ) || source.container_cpus.is_some()
                || source.container_memory.is_some())
        {
            selection.clear_resource_allocation = true;
        }
        if !selection.clear_resource_allocation {
            selection.resource_allocation = selection
                .resource_allocation
                .or_else(|| source.resource_allocation.clone());
        }
        if selection.clear_resource_allocation
            && source.resource_allocation.is_none()
            && source.container_cpus.is_none()
            && source.container_memory.is_none()
        {
            selection.clear_resource_allocation = false;
        }
        if let Some(retained) = previous.as_ref().filter(|op| op.holds_source_environment()) {
            // An in-place Move transfers no files, so its file selection is
            // not part of what a retry has to repeat. Every surface (the web
            // retry has no large-transfer flag at all) retries it the same way.
            if retained.in_place {
                selection.workspace = retained.selection.workspace.clone();
            }
            ensure!(
                retained.selection == selection,
                "{}",
                sealed_selection_difference(&retained.selection, &selection)
            );
            ensure!(
                !retained.in_place
                    || (retained.source_target.is_some()
                        && source.target == retained.source_target),
                "retained Move target is missing or changed; refusing to recreate it"
            );
        }
        let profile_id = selection.profile_id.as_deref().unwrap();
        let target_id = selection.target_template_id.as_deref().unwrap();
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .context("unknown destination profile")?;
        ensure!(
            profile.enabled,
            "destination profile {profile_id:?} is disabled"
        );
        if let Some(policy) = &selection.subagents
            && policy != &source.subagents.clone().unwrap_or_default()
        {
            super::profile_config::validate_session_subagent_policy(
                &self.config,
                profile_id,
                policy,
            )
            .await?;
        }
        let target = self
            .config
            .targets
            .get(target_id)
            .context("unknown destination target")?;
        self.validate_muse_resume_destination(source, profile.kind, target_id)?;
        let checkout = self.state.checkout(&source.id)?;
        super::worktree::resume_compatibility_with_checkout(
            source,
            &checkout,
            &self.config,
            target_id,
        )
        .map_err(anyhow::Error::msg)?;
        super::backend::validate_resource_allocation(
            target,
            selection.resource_allocation.as_ref(),
        )?;
        let mounts = selection.additional_mounts.as_deref().unwrap_or_default();
        ensure!(
            profile.kind != mj_core::config::HarnessKind::Muse || mounts.is_empty(),
            "Muse Code ACP supports one workspace root; attached directories are unsupported"
        );
        ensure!(
            mounts.is_empty() || mj_core::config::mount_history_host(target).is_some(),
            "attached resources are unsupported for this target; select compatible resources explicitly"
        );
        crate::targets::validate_additional_mounts(mounts)?;
        for mount in mounts {
            self.validate_mount_source(target_id, &mount.source, executor)?;
        }
        let planned_conversion =
            self.validate_move_destination_paths(source, target_id, executor)?;
        ensure!(
            profile.home.is_dir(),
            "destination profile home is unavailable; configure the profile before moving"
        );
        super::worker_binary::preflight_worker_binary(target, executor)?;
        super::backend::preflight_target(target, executor, super::backend::TargetCheck::Launch)?;
        let source_harness = previous
            .as_ref()
            .filter(|operation| {
                !operation.queue_admission_started && operation.phase != MovePhase::Completed
            })
            .and_then(|operation| operation.recovery_session.as_ref())
            .map_or(source.harness_kind, |record| record.harness_kind);
        let cross_harness = profile.kind != source_harness;
        if cross_harness {
            let cancel = tokio_util::sync::CancellationToken::new();
            let resolve =
                crate::utility_llm::UtilityLlmRuntime::shared().resolve(&self.config, &cancel);
            tokio::pin!(resolve);
            loop {
                tokio::select! {
                    result = &mut resolve => { result.context("cross-harness move needs an available utility model")?; break; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        if executor.cancellation_requested() { cancel.cancel(); bail!("move preparation cancelled"); }
                    }
                }
            }
        }
        // What moving a local checkout into a target really does, computed
        // before anything is stopped so a person can confirm it.
        let conversion = planned_conversion
            .map(|conversion| {
                super::worktree::raw_conversion_preview_with_checkout(
                    source,
                    &source_checkout,
                    &conversion,
                    executor,
                )
                .context("describe the move of this checkout into the target")
            })
            .transpose()?
            .map(Box::new);
        let (active, mut queued_commands, fingerprint) =
            self.move_confirmation(&selection, conversion.as_deref())?;
        replace_queued_images_with_placeholders(&mut queued_commands);
        let operation_id = previous
            .as_ref()
            .filter(|operation| {
                operation.selection == selection
                    && operation.phase != MovePhase::Completed
                    && operation.restore_artifact().is_some()
            })
            .map(|operation| operation.operation_id.clone())
            .unwrap_or(new_command_id("move")?);
        let in_place = previous
            .as_ref()
            .is_some_and(|op| retry && op.in_place && op.selection == selection)
            || in_place_move_eligible(
                source,
                &selection,
                &self.config.targets,
                self.state.subagents.contains_key(&source.id),
                retry,
            );
        let destination_has_parent_role =
            parent_tools_enabled(&move_subagent_policy(source, &selection), profile.kind);
        if in_place
            && !destination_has_parent_role
            && let Some(error) =
                roleless_move_children_error(&live_move_children(&self.state, &source.id))
        {
            bail!("{error}; close or finish those children before moving to this harness");
        }
        let workspace = if in_place {
            None
        } else {
            Some(self.assess_move_workspace(&selection, executor)?)
        };
        Ok(MovePreparation {
            destination_checks: if !in_place
                && matches!(target, mj_core::config::TargetTemplate::AwsEc2 { .. })
            {
                DestinationChecks::AfterProvisioning
            } else {
                DestinationChecks::Checked
            },
            workspace,
            source_unavailable: false,
            in_place,
            conversion,
            selection,
            source_profile_id: source.last_profile.clone(),
            source_target_template_id: source.target_template_id.clone(),
            cross_harness,
            active,
            queued_commands,
            fingerprint,
            operation_id,
        })
    }

    /// Called with one daemon lifecycle and recovery reservation already held.
    pub async fn move_session_managed_controlled(
        &mut self,
        request: MoveSessionRequest,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        crate::worker_lifecycle::run(&request.preparation.selection.session_id.clone(), "move session managed controlled", executor, async {
        let prepared = &request.preparation;
        let id = prepared.selection.session_id.clone();
        let started = std::time::Instant::now();
        executor.notify_notice("Checking destination");
        let mut source_relay = MoveSourceRelay::default();
        let checked = {
            let _checking_destination =
                ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            let mut checked = {
                let _timing = MovePhaseTimer::new(&id, "preflight destination checks");
                self.prepare_move_session_controlled(prepared.selection.clone(), executor)
                    .await?
            };
            // Destination checks can outlast a turn. Confirm against the relay
            // immediately before interruption, and hold its connection from
            // here until the checkpoint seals the source, so nothing reaches
            // the relay between this confirmation and the interruption.
            if matches!(
                self.state.sessions[&id].state,
                SessionState::Running | SessionState::Disconnected
            ) {
                let source_harness = self.state.sessions[&id].harness_kind;
                source_relay = {
                    let _timing = MovePhaseTimer::new(&id, "preflight source lease");
                    MoveSourceRelay::lease(manager, &id).await?
                };
                let snapshot = source_relay.snapshot();
                let (active, queue, fingerprint) =
                    self.move_confirmation(&checked.selection, checked.conversion.as_deref())?;
                checked.source_unavailable = snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.operational.native_session_is_ready());
                checked.active = active
                    || checked.source_unavailable
                    || snapshot.as_ref().is_some_and(|snapshot| {
                        let mut operational = snapshot.operational.clone();
                        operational.queued_prompts.clear();
                        operational.checkpoint_barrier = None;
                        !operational.safe_to_replace(source_harness)
                    });
                if let Some(snapshot) = &snapshot {
                    let _timing = MovePhaseTimer::new(&id, "preflight destination configuration");
                    self.validate_move_destination_configuration(
                        &checked.selection,
                        source_harness,
                        &snapshot.operational,
                    )
                    .await?;
                }
                checked.queued_commands = queue;
                checked.fingerprint = fingerprint;
            }
            checked
        };
        ensure!(
            checked.fingerprint == prepared.fingerprint,
            "session, pending work, or destination configuration changed; prepare and confirm Move again"
        );
        let queue = request.queue.unwrap_or(ResumeQueueDisposition::Discard);
        let source = self.state.sessions[&id].clone();
        if let Some(assessment) = &checked.workspace {
            checked.selection.workspace.validate(assessment)?;
        }
        let old_operation = crate::database::load_move_operation(&id)?;
        let retry = old_operation.filter(|op| {
            op.selection == prepared.selection
                && op.phase != MovePhase::Completed
                && op.restore_artifact().is_some()
        });
        if retry.is_none()
            && source.state == SessionState::Running
            && source.last_profile == checked.selection.profile_id.as_deref().unwrap()
            && move_environment_change(&source, &checked.selection, false).is_none()
        {
            return Ok(outcome(
                &prepared.operation_id,
                &prepared.selection,
                "unchanged",
                None,
                None,
            ));
        }
        ensure!(
            !checked.active || request.acknowledge_interruption,
            "active work will be interrupted; confirm Move again with interruption acknowledgement"
        );
        ensure!(
            checked.queued_commands.is_empty() || request.queue.is_some(),
            "pending work requires an explicit queue choice: discard or start"
        );
        let timestamp = now();
        let mut operation = match retry {
            Some(mut op) => {
                ensure!(
                    !op.queue_admission_started || op.queue == queue,
                    "queued work may already have run; retry with the original queue choice on the same destination"
                );
                crate::database::clear_move_cancellation_for_retry(&id)?;
                op.configuration_fingerprint =
                    self.move_configuration_fingerprint(&checked.selection)?;
                op.queue = queue;
                op.cancellation_requested = false;
                op.error = None;
                op
            }
            None => MoveOperation {
                prepared_destination: None,
                accepted_preparation: None,
                acknowledge_interruption: false,
                workspace_transfer: if checked.in_place {
                    None
                } else {
                    Some(
                        self.new_workspace_transfer(
                            &id,
                            &prepared.operation_id,
                            checked
                                .workspace
                                .clone()
                                .context("Move workspace assessment missing")?,
                            executor,
                        )?,
                    )
                },
                handoff: None,
                source_checkpoint_only: false,
                in_place: in_place_move_eligible(
                    &source,
                    &checked.selection,
                    &self.config.targets,
                    self.state.subagents.contains_key(&id),
                    false,
                ),
                operation_id: prepared.operation_id.clone(),
                selection: checked.selection.clone(),
                source_profile_id: source.last_profile.clone(),
                source_target_template_id: source.target_template_id.clone(),
                source_target: source.target.clone(),
                source_native_session_id: source.native_session_id.clone(),
                source_additional_mounts: source.additional_mounts.clone(),
                source_resource_allocation: source.resource_allocation.clone(),
                destination_target: None,
                destination_native_session_id: None,
                destination_store_id: None,
                configuration_fingerprint: self
                    .move_configuration_fingerprint(&checked.selection)?,
                checkpoint: None,
                recovery_session: None,
                queue,
                phase: MovePhase::Preparing,
                queue_admission_started: false,
                queue_admission_finished: false,
                cancellation_requested: false,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                error: None,
            },
        };
        operation.accepted_preparation = Some(Box::new(checked.clone()));
        operation.acknowledge_interruption = request.acknowledge_interruption;
        crate::database::save_move_operation(&operation)?;
        tracing::info!(
            session_id = id,
            in_place = operation.in_place,
            reason = move_environment_change(
                &source,
                &checked.selection,
                bare_targets_share_environment(
                    &self.config.targets,
                    &source.target_template_id,
                    checked
                        .selection
                        .target_template_id
                        .as_deref()
                        .unwrap_or_default(),
                ),
            )
            .unwrap_or(if operation.in_place {
                "environment unchanged"
            } else {
                "source environment unavailable or previously released"
            }),
            "move environment decision"
        );
        tracing::info!(
            session_id = id,
            phase = "preflight",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "move phase completed"
        );
        // Move nests checkpoint and restore state machines. Heap-own their
        // futures so dev builds fit the runtime's ordinary thread stacks.
        let result = Box::pin(self.execute_move(
            &mut operation,
            Some(&checked),
            executor,
            manager,
            source_relay,
        ))
        .await;
        self.finish_move_result_with_subagent_recovery(
            &mut operation,
            result,
            executor,
            manager,
        )
        .await

        }).await
    }

    fn finish_move_result(
        &mut self,
        operation: &mut MoveOperation,
        result: Result<()>,
        executor: &impl CommandExecutor,
    ) -> Result<MoveOutcome> {
        if result.is_err()
            && operation.workspace_transfer.is_some()
            && !crate::upgrade::gate().is_open()
        {
            // Handoff cancellation is not user cancellation. Leave the durable
            // phase active so the next daemon resumes the accepted Move.
            return Ok(outcome(
                &operation.operation_id,
                &operation.selection,
                "interrupted",
                None,
                Some("Move will continue after the daemon upgrade".into()),
            ));
        }
        if let Some(saved) = crate::database::load_move_operation(&operation.selection.session_id)?
            && saved.operation_id == operation.operation_id
        {
            operation.prepared_destination = saved.prepared_destination;
        }
        let result = match result {
            Err(error)
                if !operation.queue_admission_started
                    && operation
                        .prepared_destination
                        .as_ref()
                        .is_some_and(|d| d.owns_resource()) =>
            {
                executor.begin_resumable_move_work()?;
                let cleaned = self.cleanup_prepared_move_destination(operation, executor);
                executor.end_resumable_move_work()?;
                Err(match cleaned {
                    Ok(()) => error,
                    Err(cleanup_error) => error.context(format!("EC2 destination cleanup is pending and will retry automatically: {cleanup_error:#}")),
                })
            }
            result => result,
        };
        let session_id = operation.selection.session_id.clone();
        let mut last_error = self
            .state
            .sessions
            .get(&session_id)
            .context("Move outcome session is missing")?
            .last_error
            .clone();
        let (status, error, recovery) = match result {
            Ok(()) => {
                operation.phase = MovePhase::Completed;
                operation.error = None;
                if last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with(mj_core::state::MOVE_FAILURE_PREFIX))
                {
                    last_error = None;
                }
                ("completed", None, None)
            }
            Err(error) => {
                let phase = match operation.phase {
                    MovePhase::Preparing => "preparing the destination",
                    MovePhase::ClosingSource => "checkpointing and suspending the source",
                    MovePhase::ResumingDestination => "resuming the destination",
                    MovePhase::StartingQueue => "starting the destination queue",
                    MovePhase::Completed | MovePhase::Failed | MovePhase::Cancelled => {
                        "recovering the move"
                    }
                };
                let cancelled =
                    executor.cancellation_requested() || operation.cancellation_requested;
                operation.phase = if cancelled {
                    MovePhase::Cancelled
                } else {
                    MovePhase::Failed
                };
                operation.cancellation_requested = cancelled;
                let recovery = failed_move_recovery(operation, &self.state);
                let error = format!("{error:#}");
                tracing::warn!(
                    %session_id,
                    reference = %operation.operation_id,
                    phase,
                    cancelled,
                    %error,
                    "session move did not finish"
                );
                last_error = Some(failed_move_message(
                    Some(phase),
                    cancelled,
                    &recovery,
                    &operation.operation_id,
                ));
                operation.error = Some(error.clone());
                (
                    if cancelled { "cancelled" } else { "failed" },
                    Some(error),
                    Some(recovery),
                )
            }
        };
        operation.updated_at = now();
        crate::database::save_move_outcome(operation, last_error.as_deref())?;
        let record = self
            .state
            .sessions
            .get_mut(&session_id)
            .expect("Move outcome session");
        record.last_error = last_error;
        record.updated_at = operation.updated_at.clone();
        Ok(outcome(
            &operation.operation_id,
            &operation.selection,
            status,
            error,
            recovery,
        ))
    }

    async fn finish_move_result_with_subagent_recovery(
        &mut self,
        operation: &mut MoveOperation,
        result: Result<()>,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        let outcome = self.finish_move_result(operation, result, executor)?;
        if !operation.in_place
            || !matches!(operation.phase, MovePhase::Failed | MovePhase::Cancelled)
        {
            return Ok(outcome);
        }
        let session_id = &operation.selection.session_id;
        let Some(session) = self.state.sessions.get(session_id) else {
            return Ok(outcome);
        };
        if !matches!(
            session.state,
            SessionState::Running | SessionState::Disconnected
        ) || !parent_tools_enabled(
            &session.subagents.clone().unwrap_or_default(),
            session.harness_kind,
        ) {
            return Ok(outcome);
        }

        // A failed reply is ambiguous: the worker may already have persisted
        // `open=true`. Retry through a fresh actor lease once so the retry uses
        // the next relay connection if the first one was abandoned.
        for attempt in 1..=2 {
            match MoveSourceRelay::set_subagent_admission_via_manager(manager, session_id, true)
                .await
            {
                Ok(true) => {
                    if attempt > 1 {
                        tracing::info!(
                            session_id,
                            "reopened sub-agent requests on a replacement relay connection"
                        );
                    }
                    break;
                }
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        attempt,
                        error = %error,
                        "could not reopen sub-agent requests after the in-place Move left its source running"
                    );
                    if attempt == 1 {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        Ok(outcome)
    }

    pub async fn recover_move_managed_controlled(
        &mut self,
        mut operation: MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        crate::worker_lifecycle::run(&operation.selection.session_id.clone(), "recover move managed controlled", executor, async {
        let id = operation.selection.session_id.clone();
        let result = async {
            let session = self.state.sessions.get(&id).context("move session is missing")?.clone();
            if operation.phase == MovePhase::Preparing
                && operation.accepted_preparation.is_some()
                && matches!(self.config.targets.get(operation.selection.target_template_id.as_deref().unwrap_or_default()),
                    Some(mj_core::config::TargetTemplate::AwsEc2 { .. }))
                && !operation.cancellation_requested {
                return Box::pin(self.execute_move(&mut operation, None, executor, manager, MoveSourceRelay::default())).await;
            }
            if operation.queue_admission_started {
                ensure!(!operation.cancellation_requested, "Move was cancelled; destination retained without further queue admission");
                self.finish_workspace_transfer(&mut operation, executor)?;
                return self.admit_move_queue(&mut operation, executor).await;
            }
            if operation.workspace_transfer.is_some() && operation.recovery_session.is_some() && !operation.cancellation_requested
                && !(operation.phase == MovePhase::ResumingDestination && session.state == SessionState::Running) {
                if session.state == SessionState::Provisioning || session.target != operation.source_target {
                    self.rollback_move_destination(&operation, anyhow::anyhow!("resume interrupted Move transfer"), executor)?;
                }
                return Box::pin(self.execute_move(&mut operation, None, executor, manager, MoveSourceRelay::default())).await;
            }
            if matches!(session.state, SessionState::Closing | SessionState::Destroying) {
                let cleanup = crate::targets::CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(15));
                // An in-place Move writes `recovery_session` only after its
                // relay reported Closed, so the seal is a durable fact of the
                // Move. Asking the sealed relay again cannot change it and
                // fails when its actor is gone, which left the record
                // `Closing` (suspending) with nothing to finish it.
                let sealed = operation.in_place && operation.recovery_session.is_some() && session.state == SessionState::Closing;
                if !sealed {
                    Box::pin(self.recover_move_source_stop(&mut operation, &cleanup, manager)).await?;
                }
                // A Move that keeps the environment restores only from the
                // handoff its seal recorded. Taking the source's older full
                // checkpoint here would make a lost handoff look retained.
                if !operation.retains_source_environment() {
                    operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
                }
                if operation.retains_source_environment() && self.state.sessions[&id].state == SessionState::Closing {
                    let previous = self.state.sessions[&id].clone();
                    operation.recovery_session = Some(previous.clone());
                    let cause = anyhow::anyhow!("Move source sealed; environment retained for explicit retry");
                    return Err(self.retain_failed_in_place_move(&id, &previous, cause)?);
                }
                if !operation.retains_source_environment() && self.state.sessions[&id].state == SessionState::Stopped && self.state.sessions[&id].target.is_some() {
                    self.cleanup_stopped_target(&id, &cleanup)?;
                }
                bail!("{}", interrupted_source_stop_message(
                    "Move source stop recovered; no destination work was started. Retry Move or Resume with previous settings",
                    operation.in_place,
                ));
            }
            match operation.phase {
                MovePhase::ClosingSource
                    if operation.in_place
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Disconnected
                        )
                        && !operation.cancellation_requested =>
                {
                    // The phase is durable before the worker admission gate
                    // closes. If the daemon died before the session reached
                    // Closing, resume the gate/drain/stop sequence on startup.
                    let relay = MoveSourceRelay::lease(manager, &id).await?;
                    return Box::pin(self.execute_move(
                        &mut operation,
                        None,
                        executor,
                        manager,
                        relay,
                    ))
                    .await;
                }
                MovePhase::ClosingSource
                    if operation.in_place
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Disconnected
                        )
                        && operation.cancellation_requested =>
                {
                    let source = self.state.sessions.get(&id).context("move source is missing")?;
                    if parent_tools_enabled(
                        &source.subagents.clone().unwrap_or_default(),
                        source.harness_kind,
                    ) {
                        MoveSourceRelay::set_subagent_admission_via_manager(
                            manager, &id, true,
                        )
                        .await?;
                    }
                    bail!("Move was cancelled before source interruption; source retained and sub-agent requests reopened")
                }
                MovePhase::Preparing => bail!("Move preparation was interrupted; source retained. Prepare Move again."),
                MovePhase::ResumingDestination if session.state == SessionState::Running => {
                    // Running is installed only after native readiness and the
                    // handoff. A crash before the next intent write is safe to
                    // advance, since restore started with an empty queue.
                    ensure!(Some(&session.last_profile) == operation.selection.profile_id.as_ref()
                        && Some(&session.target_template_id) == operation.selection.target_template_id.as_ref(),
                        "ready destination does not match the move intent");
                    operation.destination_target = session.target.clone();
                    operation.destination_native_session_id = session.native_session_id.clone();
                    operation.queue_admission_started = true;
                    operation.phase = MovePhase::StartingQueue;
                    crate::database::save_move_operation(&operation)?;
                    restore_move_queue_hold(&operation);
                    ensure!(!operation.cancellation_requested, "Move was cancelled; ready destination retained");
                    self.finish_workspace_transfer(&mut operation, executor)?;
                    self.admit_move_queue(&mut operation, executor).await
                }
                MovePhase::ResumingDestination => {
                    let previous = operation.recovery_session.as_ref().context("move lacks its stopped recovery identity; retain resources for inspection")?;
                    // The retained environment belongs to this Move, even
                    // when only part of its worker installation completed.
                    let cause = anyhow::anyhow!("destination restoration was interrupted; checkpoint retained for an explicit retry");
                    let error = if operation.in_place {
                        self.retain_failed_in_place_move(&id, previous, cause)?
                    } else if operation.workspace_transfer.is_some() {
                        self.rollback_move_destination(&operation, cause, executor)?
                    } else {
                        self.rollback_failed_resume(&id, previous, false, cause, executor)?
                    };
                    Err(error)
                }
                MovePhase::ClosingSource => {
                    if matches!(session.state, SessionState::Closing | SessionState::Destroying) {
                        Box::pin(self.recover_move_source_stop(&mut operation, executor, manager)).await?;
                    }
                    operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
                    if !operation.retains_source_environment() && self.state.sessions[&id].state == SessionState::Stopped && self.state.sessions[&id].target.is_some() {
                        self.cleanup_stopped_target(&id, executor)?;
                    }
                    bail!("{}", interrupted_source_stop_message(
                        "source stop was recovered; verified checkpoint retained. Retry move or Resume with previous settings",
                        operation.in_place,
                    ))
                }
                _ => bail!("Move requires an explicit retry after the daemon restarted"),
            }
        }.await;
        self.finish_move_result_with_subagent_recovery(
            &mut operation,
            result,
            executor,
            manager,
        )
        .await

        }).await
    }

    async fn recover_move_source_stop(
        &mut self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let id = operation.selection.session_id.clone();
        if self.state.sessions[&id].state == SessionState::Closing {
            self.prepare_move_source_checkpoint(
                &id,
                executor,
                manager,
                operation,
                &mut MoveSourceRelay::default(),
            )
            .await?;
            let handle = manager
                .wait_for_session(&id, std::time::Duration::from_secs(5))
                .await?;
            let mut lease = handle.lease_connection().await?;
            let execution = lease.connection_mut().sync().await?.operational.execution;
            if operation.retains_source_environment()
                && matches!(
                    execution,
                    mj_core::relay::RelayExecutionState::Closing
                        | mj_core::relay::RelayExecutionState::Closed
                )
            {
                super::checkpoint::wait_for_relay_closed(lease.connection_mut()).await?;
                lease.release();
                // The source is sealed whether or not its handoff survived.
                // A missing handoff is already recorded on the Move, whose
                // owner retains the environment and publishes the loss;
                // refusing here would leave the session suspending instead.
                return Ok(());
            }
            lease.release();
            if matches!(
                execution,
                mj_core::relay::RelayExecutionState::Idle
                    | mj_core::relay::RelayExecutionState::Running
            ) {
                if operation.cancellation_requested || operation.retains_source_environment() {
                    let record = self.state.sessions.get_mut(&id).unwrap();
                    record.state = SessionState::Running;
                    record.updated_at = now();
                    record.last_error = Some(
                        "Move was interrupted before the source was sealed; source retained".into(),
                    );
                    crate::database::save_lifecycle_session(record)?;
                } else {
                    // Fresh-environment moves finish their admitted source stop.
                    Box::pin(self.suspend_session_for_move(
                        &id,
                        executor,
                        manager,
                        operation,
                        None,
                        SourceTargetDisposition::Destroy,
                        MoveSourceRelay::default(),
                    ))
                    .await?;
                }
                return Ok(());
            }
        }
        ensure!(
            !operation.retains_source_environment(),
            "retained Move source cannot be proven; refusing target teardown"
        );
        // A move's source stop was admitted by the move itself.
        self.recover_interrupted_close_managed(&id, executor, manager, true, None)
            .await?;
        Ok(())
    }

    /// `source_relay` is the source connection the Move confirmed against,
    /// held until the checkpoint that seals the source takes it over.
    async fn execute_move(
        &mut self,
        operation: &mut MoveOperation,
        preparation: Option<&MovePreparation>,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        mut source_relay: MoveSourceRelay,
    ) -> Result<()> {
        let retained = source_relay.owner();
        let admission_id = operation.selection.session_id.clone();
        crate::worker_lifecycle::run_with_owner(&admission_id, "execute move", executor, retained, async {
        let id = operation.selection.session_id.clone();
        let mut preparation = preparation.cloned();
        if let Some(saved) = crate::database::load_move_operation(&id)?
            && saved.operation_id == operation.operation_id
        {
            operation.prepared_destination = saved.prepared_destination;
        }

        if !operation.in_place
            && !operation.queue_admission_started
            && matches!(
                self.config.targets.get(
                    operation
                        .selection
                        .target_template_id
                        .as_deref()
                        .unwrap_or_default()
                ),
                Some(mj_core::config::TargetTemplate::AwsEc2 { .. })
            )
        {
            drop(source_relay);
            source_relay = MoveSourceRelay::default();
            self.verify_current_move_configuration(operation)?;
            // Resolve the previous destination before acquiring another one. The
            // rollback must never interpret a newly prepared instance as the old
            // destination whose session record still needs restoring.
            if self.state.sessions[&id].state == SessionState::Error
                && operation.recovery_session.is_some()
            {
                ensure!(
                    !forget_missing_move_archives(operation),
                    "the Move's checkpoint archive is missing; nothing is left to restore"
                );
                self.rollback_move_destination(
                    operation,
                    anyhow::anyhow!("clean up the partial Move destination before retry"),
                    executor,
                )?;
                let record = self.state.sessions.get_mut(&id).unwrap();
                record.state = SessionState::Closing;
                crate::database::save_resumed_session(record, None)?;
                operation.prepared_destination = crate::database::load_move_operation(&id)?
                    .context("Move cleanup intent missing")?
                    .prepared_destination;
            }
            self.prepare_ec2_move_destination(operation, executor)?;
            self.verify_current_move_configuration(operation)?;
            if matches!(
                self.state.sessions[&id].state,
                SessionState::Running | SessionState::Disconnected
            ) && operation.recovery_session.is_none()
            {
                let accepted = operation
                    .accepted_preparation
                    .as_ref()
                    .context("EC2 Move confirmation missing")?;
                let mut checked = self
                    .prepare_move_session_controlled(operation.selection.clone(), executor)
                    .await?;
                let harness = self.state.sessions[&id].harness_kind;
                source_relay = MoveSourceRelay::lease(manager, &id).await?;
                let snapshot = source_relay.snapshot();
                let (active, queue, fingerprint) =
                    self.move_confirmation(&checked.selection, checked.conversion.as_deref())?;
                let unavailable = snapshot
                    .as_ref()
                    .is_none_or(|s| !s.operational.native_session_is_ready());
                let active = active
                    || unavailable
                    || snapshot.as_ref().is_some_and(|s| {
                        let mut state = s.operational.clone();
                        state.queued_prompts.clear();
                        state.checkpoint_barrier = None;
                        !state.safe_to_replace(harness)
                    });
                ensure!(
                    fingerprint == accepted.fingerprint,
                    "session, pending work, or destination configuration changed; source retained, prepare and confirm Move again"
                );
                ensure!(
                    !active || operation.acknowledge_interruption,
                    "active work will be interrupted; source retained, confirm Move again with interruption acknowledgement"
                );
                if let Some(snapshot) = snapshot {
                    self.validate_move_destination_configuration(
                        &checked.selection,
                        harness,
                        &snapshot.operational,
                    )
                    .await?;
                }
                checked.active = active;
                checked.queued_commands = queue;
                checked.source_unavailable = unavailable;
                let assessment = checked
                    .workspace
                    .as_mut()
                    .context("EC2 Move workspace missing")?;
                self.assess_prepared_destination(operation, assessment, executor)?;
                operation
                    .workspace_transfer
                    .as_mut()
                    .context("EC2 Move transfer missing")?
                    .assessment = assessment.clone();
                crate::database::save_move_operation(operation)?;
                preparation = Some(checked);
            } else {
                let mut assessment = operation
                    .workspace_transfer
                    .as_ref()
                    .context("EC2 Move transfer missing")?
                    .assessment
                    .clone();
                assessment
                    .storage
                    .retain(|s| !s.allocations.contains("destination"));
                self.assess_prepared_destination(operation, &mut assessment, executor)?;
            }
        }
        ensure!(
            !executor.cancellation_requested(),
            "move cancelled before source interruption"
        );
        if !operation.queue_admission_started {
            // A retry restores from the archive this record names. When the
            // file is gone, record that before touching the session, so the
            // failure publishes what is really retained and the session stays
            // in the state Destroy can act on.
            if forget_missing_move_archives(operation) {
                operation.updated_at = now();
                crate::database::save_move_operation(operation)?;
                bail!("the Move's checkpoint archive is missing; nothing is left to restore");
            }
            if operation.in_place {
                ensure!(
                    operation.source_target.is_some()
                        && self.state.sessions[&id].target == operation.source_target,
                    "retained Move target is missing or changed; refusing to recreate the environment"
                );
            }
            if self.state.sessions[&id].state == SessionState::Error
                && let Some(previous) = operation.recovery_session.as_ref()
            {
                // A prior failed teardown retains its exact partial checkout.
                // Restore source identity only after that owner is stopped.
                let cause = anyhow::anyhow!("clean up the partial Move destination before retry");
                if operation.in_place {
                    // The record stays `Error`: `restore_session_in_place`
                    // accepts a retained environment in that state, and every
                    // failure before the swap leaves it there for Destroy or
                    // another retry. Marking it `Closing` here left a failed
                    // retry suspending with nothing to finish it.
                    self.retain_failed_in_place_move(&id, previous, cause)?;
                } else if operation.workspace_transfer.is_some() {
                    self.rollback_move_destination(operation, cause, executor)?;
                    let record = self.state.sessions.get_mut(&id).unwrap();
                    record.state = SessionState::Closing;
                    crate::database::save_resumed_session(record, None)?;
                } else {
                    let failure =
                        self.rollback_failed_resume(&id, previous, false, cause, executor)?;
                    ensure!(
                        self.state.sessions[&id].state == SessionState::Stopped,
                        "{failure:#}"
                    );
                }
            }
            let state = self.state.sessions[&id].state;
            if matches!(state, SessionState::Closing | SessionState::Destroying)
                && !(operation.retains_source_environment() && operation.recovery_session.is_some())
            {
                Box::pin(self.recover_move_source_stop(operation, executor, manager)).await?;
                operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
            }
            if matches!(
                self.state.sessions[&id].state,
                SessionState::Running | SessionState::Disconnected
            ) && operation.destination_target.is_none()
            {
                let source = self.state.sessions[&id].clone();
                let source_has_parent_role = parent_tools_enabled(
                    &source.subagents.clone().unwrap_or_default(),
                    source.harness_kind,
                );
                let destination_has_parent_role = parent_tools_enabled(
                    &move_subagent_policy(&source, &operation.selection),
                    operation
                        .selection
                        .profile_id
                        .as_deref()
                        .and_then(|profile| self.config.profiles.get(profile))
                        .context("destination profile is missing")?
                        .kind,
                );
                if operation.in_place {
                    // Persist the phase before closing worker admission. A daemon
                    // crash from this point can reopen the relay, finish the
                    // drain, or observe that source sealing already began.
                    operation.phase = MovePhase::ClosingSource;
                    operation.updated_at = now();
                    crate::database::save_move_operation(operation)?;
                    let mut stopped_subagents_for_legacy_worker = false;
                    if source_has_parent_role {
                        if !source_relay.is_held() {
                            source_relay = MoveSourceRelay::lease(manager, &id).await?;
                        }
                        match source_relay.drain_subagent_mutations(&id, executor).await? {
                            SubagentMutationDrain::Drained => {}
                            SubagentMutationDrain::UnsupportedWorkerProtocol(version) => {
                                executor.notify_notice(&format!(
                                    "The source worker uses relay protocol {version}; protocol 32 is required to safely drain sub-agent requests, so stopping sub-agents before this in-place Move"
                                ));
                                executor.before_move_source_stop().await?;
                                stopped_subagents_for_legacy_worker = true;
                            }
                        }
                    }
                    if should_stop_move_subagents(true, destination_has_parent_role)
                        && !stopped_subagents_for_legacy_worker
                    {
                        let current = crate::database::load_state()?;
                        if let Some(error) =
                            roleless_move_children_error(&live_move_children(&current, &id))
                        {
                            if source_has_parent_role {
                                source_relay
                                    .reopen_subagent_mutations(&id)
                                    .await
                                    .context("could not reopen sub-agent requests after refusing Move")?;
                            }
                            bail!("{error}; close or finish those children before moving to this harness");
                        }
                        if let Err(error) = executor.before_move_source_stop().await {
                            if source_has_parent_role {
                                source_relay
                                    .reopen_subagent_mutations(&id)
                                    .await
                                    .context("could not reopen sub-agent requests after stopping children failed")?;
                            }
                            return Err(error);
                        }
                    }
                } else {
                    // Non-in-place Moves retain their existing child-stop
                    // behavior before the source checkpoint begins.
                    executor.before_move_source_stop().await?;
                }
                if executor.cancellation_requested() {
                    if operation.in_place && source_has_parent_role {
                        source_relay.reopen_subagent_mutations(&id).await?;
                    }
                    bail!("Move cancelled before source interruption");
                }
                executor.notify_notice("Stopping source");
                let _timing = MovePhaseTimer::new(&id, "checkpoint and source stop");
                if !operation.in_place {
                    operation.phase = MovePhase::ClosingSource;
                    operation.updated_at = now();
                    crate::database::save_move_operation(operation)?;
                }
                // An eligible move keeps its environment: the close stops at
                // the sealed relay and the record keeps its target for
                // `restore_session_in_place`.
                let disposition = if operation.in_place {
                    SourceTargetDisposition::RetainForInPlaceSwap
                } else {
                    SourceTargetDisposition::Destroy
                };
                let suspended = Box::pin(self.suspend_session_for_move(
                    &id,
                    executor,
                    manager,
                    operation,
                    preparation.as_ref(),
                    disposition,
                    std::mem::take(&mut source_relay),
                ))
                .await;
                if let Err(error) = suspended {
                    if operation.in_place
                        && source_has_parent_role
                        && matches!(
                            self.state.sessions[&id].state,
                            SessionState::Running | SessionState::Disconnected
                        )
                        && let Err(reopen) = MoveSourceRelay::set_subagent_admission_via_manager(
                            manager,
                            &id,
                            true,
                        )
                        .await
                    {
                        return Err(error.context(format!(
                            "could not reopen sub-agent requests after source checkpoint failed: {reopen:#}"
                        )));
                    }
                    return Err(error);
                }
            }
            // Past the source stop, nothing else needs the source's connection.
            drop(source_relay);
            // An in-place move never reaches `Stopped` with a target: its
            // source stays `Closing` in the environment the destination reuses.
            if !operation.in_place
                && operation.workspace_transfer.is_none()
                && self.state.sessions[&id].state == SessionState::Stopped
                && self.state.sessions[&id].target.is_some()
            {
                executor.notify_notice("Cleaning up source");
                let _timing = MovePhaseTimer::new(&id, "source storage cleanup");
                self.cleanup_stopped_target(&id, executor)?;
            }
            operation.checkpoint = operation
                .checkpoint
                .clone()
                .or_else(|| self.state.sessions[&id].checkpoint.clone());
            ensure!(
                operation.restore_artifact().is_some(),
                "move has no verified checkpoint"
            );
            ensure!(
                !executor.cancellation_requested(),
                "move cancelled after source sealing; checkpoint and remaining environment retained"
            );
            operation.phase = MovePhase::ResumingDestination;
            operation.recovery_session = Some(self.state.sessions[&id].clone());
            operation.updated_at = now();
            crate::database::save_move_operation(operation)?;
            executor.reserve_move_destination();
            executor.notify_notice("Preparing destination");
            // The destination worker reads its delegation policy from the
            // record at launch. `recovery_session` above keeps the old one
            // for a rollback.
            if let Some(policy) = &operation.selection.subagents {
                let session = self.state.sessions.get_mut(&id).unwrap();
                session.subagents = Some(policy.clone());
                crate::database::save_resumed_session(session, None)?;
            }
            if operation.in_place {
                // The environment is kept, so there is nothing to provision and
                // no allocation to clear: eligibility already required the
                // allocation to be unchanged.
                Box::pin(self.restore_session_in_place(
                    &id,
                    operation.selection.profile_id.as_deref().unwrap(),
                    operation.selection.target_template_id.as_deref().unwrap(),
                    executor,
                ))
                .await?;
            } else {
                if operation.selection.clear_resource_allocation {
                    let session = self.state.sessions.get_mut(&id).unwrap();
                    session.resource_allocation = None;
                    session.container_cpus = None;
                    session.container_memory = None;
                    crate::database::save_resumed_session(session, None)?;
                }
                if operation.workspace_transfer.is_some() {
                    self.capture_move_workspace(operation, executor)?;
                    Box::pin(self.resume_session_for_move(operation, executor)).await?;
                    let saved = crate::database::load_move_operation(&id)?
                        .context("Move intent disappeared during restore")?;
                    operation.workspace_transfer = saved.workspace_transfer;
                    operation.prepared_destination = saved.prepared_destination;
                } else {
                    Box::pin(self.resume_session_controlled(
                        &id,
                        operation.selection.profile_id.as_deref().unwrap(),
                        operation.selection.target_template_id.as_deref().unwrap(),
                        SessionResumeOptions {
                            additional_mounts: operation.selection.additional_mounts.clone(),
                            resource_allocation: operation.selection.resource_allocation.clone(),
                            discard_queue: true,
                        },
                        executor,
                    ))
                    .await?;
                }
            }
            let destination = &self.state.sessions[&id];
            operation.destination_target = destination.target.clone();
            operation.destination_native_session_id = destination.native_session_id.clone();
            // Persist readiness before submitting even the first queued command.
            operation.phase = MovePhase::StartingQueue;
            operation.queue_admission_started = true;
            operation.updated_at = now();
            crate::database::save_move_operation(operation)?;
        }
        restore_move_queue_hold(operation);
        self.finish_workspace_transfer(operation, executor)?;
        self.admit_move_queue(operation, executor).await

        }).await
    }

    async fn admit_move_queue(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let timing_id = operation.selection.session_id.clone();
        let _timing = MovePhaseTimer::new(&timing_id, "queue admission");
        let id = &operation.selection.session_id;
        let mut relay = {
            let _checking_destination =
                ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            let destination = &self.state.sessions[id];
            ensure!(
                destination.state == SessionState::Running
                    && destination.target == operation.destination_target
                    && destination.native_session_id == operation.destination_native_session_id,
                "cannot prove the same ready destination; refusing to replay potentially executed work"
            );
            let spec = self.reconnect_command(id)?;
            let relay = StandaloneSession::connect_command(&spec, id).await?;
            let store_id = relay.snapshot().operational.store_id.context("destination worker does not expose its durable store identity; upgrade the worker before admitting queued work")?;
            if let Some(expected) = &operation.destination_store_id {
                ensure!(
                    *expected == store_id,
                    "destination relay storage was replaced; refusing to replay potentially executed work"
                );
            } else {
                // No command can be admitted until this identity is durable.
                operation.destination_store_id = Some(store_id);
                crate::database::save_move_operation(operation)?;
            }
            ensure!(
                relay.snapshot().operational.native_session_id
                    == operation.destination_native_session_id,
                "destination relay native identity changed; refusing queue replay"
            );
            relay
        };
        if operation.queue == ResumeQueueDisposition::Start {
            let _starting_queue = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
            executor.notify_notice("Starting queued work");
            let checkpoint = operation
                .restore_artifact()
                .context("move queue archive is missing")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            ensure!(
                verified.archive_sha256 == checkpoint.sha256 && verified.manifest.session.id == *id,
                "move queue checkpoint verification failed"
            );
            for queued in verified.canonical_session.queued_prompts {
                if operation.queue_admission_finished {
                    continue;
                }
                ensure!(
                    !executor.cancellation_requested(),
                    "move cancelled during queue admission; destination retained"
                );
                let command = match queued.kind {
                    CanonicalQueuedCommandKind::Prompt => RelayCommand::Prompt {
                        prompt: queued
                            .content
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?,
                    },
                    CanonicalQueuedCommandKind::SetConfig { key, value } => {
                        RelayCommand::SetConfig { key, value }
                    }
                };
                relay.submit_accepted(queued.command_id, command).await?;
            }
        }
        operation.queue_admission_finished = true;
        crate::database::save_move_operation(operation)?;
        restore_move_queue_hold(operation);
        let queue_sentence = if operation.queue == ResumeQueueDisposition::Discard {
            "Queued work was discarded; ready and idle."
        } else {
            "Queued work was accepted."
        };
        let source_profile = &operation.source_profile_id;
        let source_target = &operation.source_target_template_id;
        let destination_profile = operation.selection.profile_id.as_deref().unwrap();
        let destination_target = operation.selection.target_template_id.as_deref().unwrap();
        // The two moves are different events for the person reading the
        // conversation: one rebuilt the environment, the other kept it.
        let text = if operation.in_place {
            format!(
                "Switched from {source_profile} / {source_target} to {destination_profile} / {destination_target} in place; the workspace and environment were kept. {queue_sentence} The interrupted prompt was not replayed."
            )
        } else {
            format!(
                "Moved from {source_profile} / {source_target} to {destination_profile} / {destination_target} in a fresh environment. {queue_sentence} The interrupted prompt was not replayed."
            )
        };
        relay
            .submit(
                format!("{}-notice", operation.operation_id),
                RelayCommand::RecordNotice { text },
            )
            .await?;
        Ok(())
    }

    pub(super) fn validate_move_checkpoint(
        &self,
        operation: &MoveOperation,
        preparation: Option<&MovePreparation>,
        github_token: Option<&str>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
        let current = Controller {
            config: mj_core::config::Config::load()?,
            state: self.state.clone(),
        };
        ensure!(
            current.move_configuration_fingerprint(&operation.selection)?
                == operation.configuration_fingerprint,
            "destination configuration changed during move"
        );
        let id = &operation.selection.session_id;
        current.validate_move_destination_paths(
            &self.state.sessions[id],
            operation.selection.target_template_id.as_deref().unwrap(),
            executor,
        )?;
        if !operation.in_place
            && operation.workspace_transfer.is_none()
            && let super::ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) = self
                .preflight_resume_repository_sources_with_token(
                    id,
                    operation.selection.target_template_id.as_deref().unwrap(),
                    github_token,
                    executor,
                )?
        {
            bail!(
                "destination repository source is missing checkpoint commit {}; source retained",
                mismatch.missing_commit
            );
        }
        if let Some(prepared) = preparation {
            let checkpoint = operation
                .restore_artifact()
                .or(self.state.sessions[id].checkpoint.as_ref())
                .context("no move checkpoint")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            let actual: Vec<_> = verified
                .canonical_session
                .queued_prompts
                .iter()
                .map(|p| p.command_id.as_str())
                .collect();
            let expected: Vec<_> = prepared
                .queued_commands
                .iter()
                .map(|p| p.command_id.as_str())
                .collect();
            ensure!(
                actual == expected,
                "pending queue changed before checkpoint capture; source retained, confirm Move again"
            );
        }
        Ok(())
    }
}

fn validate_preserved_configuration(
    profile_id: &str,
    accepted: &mj_core::acp::AcceptedSessionConfig,
    choices: &mj_core::worker_launch::ProfileConfig,
) -> Result<()> {
    for (key, value, offered) in [
        ("model", accepted.model.as_deref(), &choices.models),
        ("effort", accepted.effort.as_deref(), &choices.efforts),
    ] {
        let Some(value) = value else { continue };
        ensure!(
            offered.iter().any(|choice| choice.value == value),
            "destination profile {profile_id:?} does not offer the session's accepted {key} {value:?}; choices: {}",
            offered
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn move_subagent_policy(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
) -> mj_core::subagent::SubagentPolicy {
    selection
        .subagents
        .clone()
        .or_else(|| source.subagents.clone())
        .unwrap_or_default()
}

pub(crate) fn parent_tools_enabled(
    policy: &mj_core::subagent::SubagentPolicy,
    harness: mj_core::config::HarnessKind,
) -> bool {
    policy.for_launch(harness, false).parent_role().is_some()
}

pub(in crate::controller) fn move_children(
    state: &mj_core::state::State,
    parent_session_id: &str,
) -> Vec<mj_core::subagent::InPlaceSubagent> {
    state
        .subagents
        .values()
        .filter(|relation| relation.parent_session_id == parent_session_id)
        .filter_map(|relation| {
            let child = state.sessions.get(&relation.child_session_id)?;
            let state = if child.state.has_live_worker() {
                mj_core::subagent::InPlaceSubagentState::Running
            } else if child.state == SessionState::Parked {
                mj_core::subagent::InPlaceSubagentState::Parked
            } else {
                return None;
            };
            Some(mj_core::subagent::InPlaceSubagent {
                child_session_id: relation.child_session_id.clone(),
                task_name: relation.task_name.clone(),
                state,
            })
        })
        .collect()
}

fn live_move_children(state: &mj_core::state::State, parent_session_id: &str) -> Vec<String> {
    move_children(state, parent_session_id)
        .into_iter()
        .filter(|child| child.state == mj_core::subagent::InPlaceSubagentState::Running)
        .map(|child| {
            format!(
                "{} (child_session_id {})",
                child.task_name, child.child_session_id
            )
        })
        .collect()
}

fn roleless_move_children_error(children: &[String]) -> Option<String> {
    (!children.is_empty()).then(|| {
        format!(
            "cannot in-place Move to a harness without mj-agents parent tools while live sub-agents exist: {}",
            children.join(", ")
        )
    })
}

fn should_stop_move_subagents(in_place: bool, destination_has_parent_role: bool) -> bool {
    !in_place || !destination_has_parent_role
}

fn subagent_mutations_pending(
    requests: &[mj_core::subagent::SubagentToolRequest],
    durable_effect_pending: bool,
) -> bool {
    durable_effect_pending
        || requests
            .iter()
            .any(|request| request.action.mutates_child_state())
}

/// Whether this move can replace only the harness inside the source target.
///
/// The environment is kept only when nothing about it changes: the same target
/// template (or another one that names the same bare machine), the same
/// attached mounts, and the same resource allocation. A retry takes its retention decision from the durable Move intent instead;
/// a sub-agent never owns its own target.
pub(super) fn in_place_move_eligible(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
    is_subagent: bool,
    retry: bool,
) -> bool {
    !retry
        && !is_subagent
        && source.target.is_some()
        && matches!(
            source.state,
            SessionState::Running | SessionState::Disconnected
        )
        && move_environment_change(
            source,
            selection,
            bare_targets_share_environment(
                targets,
                &source.target_template_id,
                selection.target_template_id.as_deref().unwrap_or_default(),
            ),
        )
        .is_none()
}

/// Whether two target templates are bare targets on the same machine, so the
/// session's worker root and workspace are already where the destination wants
/// them. Two local bare targets qualify, and so do two SSH bare targets with
/// the same connection; only the template the session names changes.
fn bare_targets_share_environment(
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
    source_id: &str,
    destination_id: &str,
) -> bool {
    use mj_core::config::TargetTemplate::{LocalBare, SshBare};
    match (targets.get(source_id), targets.get(destination_id)) {
        (Some(LocalBare), Some(LocalBare)) => true,
        (
            Some(SshBare { ssh: source, .. }),
            Some(SshBare {
                ssh: destination, ..
            }),
        ) => {
            crate::targets::SshTarget::from(source) == crate::targets::SshTarget::from(destination)
        }
        _ => false,
    }
}
fn outcome(
    operation_id: &str,
    selection: &MoveSelection,
    status: &str,
    error: Option<String>,
    recovery: Option<String>,
) -> MoveOutcome {
    MoveOutcome {
        operation_id: operation_id.into(),
        session_id: selection.session_id.clone(),
        profile_id: selection.profile_id.clone().unwrap_or_default(),
        target_template_id: selection.target_template_id.clone().unwrap_or_default(),
        outcome: status.into(),
        error,
        recovery,
    }
}

/// One decision shared by no-op detection and environment retention. A
/// different target template counts as a change unless `same_environment` says
/// the two name the same bare machine.
fn move_environment_change(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
    same_environment: bool,
) -> Option<&'static str> {
    if Some(&source.target_template_id) != selection.target_template_id.as_ref()
        && !same_environment
    {
        Some("target changed")
    } else if Some(&source.additional_mounts) != selection.additional_mounts.as_ref() {
        Some("attached mounts changed")
    } else if source.resource_allocation != selection.resource_allocation
        || (selection.clear_resource_allocation
            && (source.container_cpus.is_some() || source.container_memory.is_some()))
    {
        Some("resource allocation changed")
    } else {
        None
    }
}
