//! One recoverable stop/restore operation, independent of its initiating viewer.

mod destination;
pub(in crate::controller) use destination::destroy_prepared_environment;
pub mod in_place;
pub mod recovery;
#[doc(hidden)]
pub mod test_fixtures;
#[cfg(test)]
mod tests;
pub mod transport;
pub use mj_session_types::ViewerMoveRecovery;

use anyhow::{Context, Result, bail, ensure};
use mj_core::hex::lower_hex;
use sha2::{Digest, Sha256};

use super::lifecycle::SourceTargetDisposition;
use super::{Controller, now};
use crate::session_manager::move_ownership::set_move_queue_hold;
pub use crate::session_manager::move_ownership::{
    MoveMutationGuard, move_has_pending_queue, move_owns_session, move_refuses_command,
    release_move_queue_hold, worker_subagent_queue_enabled,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum InPlaceReloadProgress {
    BeforeCommit,
    Unknown,
    Started,
    FailedAfterCommit,
}

struct CommittedReloadFailure {
    message: String,
    aws_profile: Option<String>,
}

/// Static Move safety refusals that may be written to the worker's quota journal.
pub const QUOTA_MOVE_SAFETY_REFUSAL: &str = "quota_move_safety";

pub(in crate::controller) enum SubagentMutationDrain {
    Drained,
    UnsupportedWorkerProtocol(u32),
}

/// Preserve a restored destination's readiness when close replaces Running
/// with Closing. The caller saves this intent and the session in one transaction.
pub fn move_destination_before_close(
    session: &mj_core::state::SessionRecord,
) -> Result<Option<MoveOperation>> {
    if session.state != SessionState::Running {
        return Ok(None);
    }
    let Some(mut operation) = crate::database::load_move_operation(&session.id)? else {
        return Ok(None);
    };
    if operation.phase != MovePhase::Cancelled
        || operation.is_resolved()
        || operation.queue_admission_started
        || operation.recovery_session.is_none()
    {
        return Ok(None);
    }
    operation.destination_target = session.target.clone().map(|target| *target);
    operation.destination_native_session_id = session.native_session_id.clone();
    operation.updated_at = now();
    Ok(Some(operation))
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
                && r.target.as_deref() == operation.source_target.as_ref()
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
    let lost = if missing(&operation.checkpoint) {
        operation.checkpoint.take()
    } else {
        return false;
    };
    if let Some(checkpoint) = lost {
        tracing::warn!(
            session_id = %operation.selection.session_id,
            reference = %operation.operation_id,
            path = %checkpoint.archive_path.display(),
            "a Move's checkpoint archive is missing; recording that the Move cannot restore it"
        );
    }
    true
}

/// Release the restore archives of a Move that ended without ever sealing its
/// source, once that source is live again.
///
/// The Move recorded its handoff before asking the worker to seal. When the
/// seal then failed or was refused, the source went on running and journalling,
/// so the archive no longer describes it. Keeping the reference would make the
/// next Move a "retry" of that archive. The Move writes `recovery_session`
/// only once the source is sealed, so its absence marks a Move that never got
/// that far. A source that is not live is another case: it was sealed or
/// stopped, and the archive is what its retry restores.
///
/// Returns `None` when nothing changed. Otherwise the Move-owned handoff files
/// to delete once the record is saved. The Move's `checkpoint` belongs to the
/// session record and is only dropped from the Move.
pub(crate) fn forget_unsealed_move_archives(
    operation: &mut MoveOperation,
    source_live: bool,
) -> Option<Vec<std::path::PathBuf>> {
    if !source_live
        || !matches!(operation.phase, MovePhase::Failed | MovePhase::Cancelled)
        || operation.is_resolved()
        || operation.queue_admission_started
        || operation.recovery_session.is_some()
        || (operation.checkpoint.is_none() && operation.source_snapshot.is_none())
    {
        return None;
    }
    tracing::info!(
        session_id = %operation.selection.session_id,
        reference = %operation.operation_id,
        "a Move ended before sealing its source, which is running again; releasing its stale restore archive"
    );
    operation.checkpoint = None;
    operation.source_snapshot = None;
    Some(Vec::new())
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
pub fn record_finished_move_recovery(
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
        crate::database::save_move_outcome(operation, Some(&message), false)
    } else {
        crate::database::save_move_operation(operation)
    }
}

/// Before a destination owns any work, its delegation policy can be changed
/// independently of the captured files and the agreed placement.
fn matches_move_retry_selection(operation: &MoveOperation, requested: &MoveSelection) -> bool {
    if operation.selection == *requested {
        return true;
    }
    failed_move_has_only_its_source(operation)
        && operation.selection
            == (MoveSelection {
                subagents: operation.selection.subagents.clone(),
                ..requested.clone()
            })
}

fn retry_move_operation(
    operation: &MoveOperation,
    source: &mj_core::state::SessionRecord,
    requested: &MoveSelection,
) -> bool {
    !move_needs_fresh_capture(operation, source)
        && matches_move_retry_selection(operation, requested)
        && operation.checkpoint_retained()
}

fn failed_move_has_only_its_source(operation: &MoveOperation) -> bool {
    !operation.in_place
        && operation.holds_source_environment()
        && matches!(operation.phase, MovePhase::Failed | MovePhase::Cancelled)
        && operation.destination_target.is_none()
        && operation
            .prepared_destination
            .as_ref()
            .is_none_or(|destination| !destination.owns_resource())
}

/// A reopened source can have newer files, prompts and journal entries.
/// Confirm a fresh Move from that source even if the old snapshot still exists.
fn move_needs_fresh_capture(
    operation: &MoveOperation,
    source: &mj_core::state::SessionRecord,
) -> bool {
    operation.source_snapshot.is_some()
        && failed_move_has_only_its_source(operation)
        && source.state == SessionState::Running
        && source.last_profile == operation.source_profile_id
        && source
            .target
            .as_deref()
            .zip(operation.source_target.as_ref())
            .is_some_and(|(current, original)| current.same_resource(original))
}

fn retained_move_snapshot_is_missing(
    operation: &MoveOperation,
    config: &mj_core::config::Config,
    executor: &(impl CommandExecutor + Sync),
) -> Result<bool> {
    if !failed_move_has_only_its_source(operation) {
        return Ok(false);
    }
    let Some(snapshot) = &operation.source_snapshot else {
        return Ok(false);
    };
    let source = operation
        .recovery_session
        .as_ref()
        .expect("retained source");
    let target = source
        .target
        .as_deref()
        .context("Move source has no target")?;
    if matches!(target, mj_core::state::TargetLocator::LocalBare { .. }) {
        return match std::fs::metadata(&snapshot.archive_path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error).context("check the retained Move snapshot"),
        };
    }
    let backend = super::backend_locator(target, source, config)?;
    // The marker comes from the target shell; a transport or engine
    // failure cannot be interpreted as proof that an archive is gone.
    let command = crate::targets::command_on_locator(
        crate::targets::SshTrafficClass::Control,
        &backend,
        &source.id,
        vec![
            "sh".into(),
            "-c".into(),
            "if [ -f \"$1\" ]; then printf 'present\\n'; else printf 'missing\\n'; fi".into(),
            "mj-move-snapshot".into(),
            snapshot.archive_path.to_string_lossy().into_owned(),
        ],
        "check the retained Move snapshot",
    )?;
    let output = executor.execute(&command)?;
    match (output.status, output.stdout.as_slice()) {
        (0, b"present\n") => Ok(false),
        (0, b"missing\n") => Ok(true),
        _ => bail!(
            "could not check the retained Move snapshot: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
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
    if retained.skip_keep_paths != requested.skip_keep_paths {
        let skipped = retained
            .skip_keep_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        parts.push(if skipped.is_empty() {
            "skipped .mj/keep/ paths (the Move was sealed skipping none)".to_owned()
        } else {
            format!(
                "skipped .mj/keep/ paths (the Move was sealed skipping exactly {})",
                skipped.join(", ")
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
    if retained.oracle != requested.oracle || retained.oracle_choice != requested.oracle_choice {
        parts.push("oracle choice (send the one the Move was sealed with)".to_owned());
    }
    if retained.session_id != requested.session_id || parts.is_empty() {
        parts.push("the session".to_owned());
    }
    format!(
        "a sealed Move must be retried with the selection it was sealed with; this request differs in its {}",
        parts.join("; ")
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

/// Bind automatic Move confirmation to the turn that stopped for quota.
pub fn quota_move_fingerprint(
    session_id: &str,
    base: &str,
    operational: &mj_core::relay::RelayOperationalState,
) -> Result<String> {
    in_place::ensure_worker_supports_in_place_move(
        session_id,
        operational.relay_protocol_version.unwrap_or_default(),
        true,
    )
    .map_err(|error| {
        mj_core::refusal::Refusal::precondition(error.to_string())
            .with_code(QUOTA_MOVE_SAFETY_REFUSAL)
    })?;
    let continuation = &operational.continuation;
    ensure!(
        !continuation.quota_suppressed
            && continuation.user_command_id.is_some()
            && operational.assessment.as_ref().is_some_and(|assessment| {
                assessment.current()
                    && assessment.action == Some(mj_core::assessment::Action::RecoverQuota)
                    && continuation.completed_command_id.as_ref() == Some(&assessment.turn_id)
                    && !continuation
                        .quota_recovery
                        .as_ref()
                        .is_some_and(|recovery| {
                            recovery.submitted
                                && recovery.completed_command_id == assessment.turn_id
                        })
            }),
        "quota Move assessment was superseded"
    );
    digest(&(
        base,
        &operational.continuation.user_command_id,
        &operational.continuation.completed_command_id,
    ))
}

pub(in crate::controller) fn quota_move_recovery(
    operational: &mj_core::relay::RelayOperationalState,
) -> Result<&mj_core::continuation::QuotaRecovery> {
    let continuation = &operational.continuation;
    let recovery = continuation
        .quota_recovery
        .as_ref()
        .context("quota Move recovery was superseded")?;
    ensure!(
        !continuation.quota_suppressed
            && !recovery.submitted
            && continuation.user_command_id.as_deref() == Some(recovery.user_command_id.as_str())
            && continuation.completed_command_id.as_deref()
                == Some(recovery.completed_command_id.as_str()),
        "quota Move recovery was superseded"
    );
    Ok(recovery)
}

/// Observe preparation state without taking the worker's mutation permit.
/// Confirmed execution leases and refreshes the source before interruption.
///
/// A session without a relay actor has no worker to observe: a Move that is
/// restoring it on its destination, or a worker that never came up, leaves
/// the record `Disconnected` with no actor. That is the answer (`None`, an
/// unavailable source), not something to wait for: preparing the retry of a
/// Move that is still running must reach the Move that owns the session.
pub async fn refresh_move_source(
    manager: &SessionManagerControl,
    id: &str,
) -> Result<Option<mj_core::state::ManagedSessionSnapshot>> {
    Ok(manager
        .find_session(id)
        .await?
        .and_then(|handle| handle.view().snapshot))
}

/// A Move's hold on its source's relay connection, from the confirmation
/// until the checkpoint that seals the source takes it over. While it is
/// held, the session actor admits nothing to the relay, so the state the Move
/// was confirmed against is the state it interrupts. Dropping it hands the
/// connection back to the actor.
#[derive(Default)]
pub struct MoveSourceRelay(
    Option<super::checkpoint::ControllerRelayLease>,
    Option<crate::worker_lifecycle::WorkerPermit>,
    Option<crate::recovery_gate::SessionAdmission>,
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
        admission: &crate::recovery_gate::SessionAdmission,
        manager: &SessionManagerControl,
        id: &str,
    ) -> Result<Self> {
        let owner = crate::worker_lifecycle::WorkerPermit::acquire(id, admission)?;
        let handle = manager
            .wait_for_session(id, std::time::Duration::from_secs(5))
            .await?;
        match handle.lease_connection(admission).await {
            Ok(lease) => Ok(Self(
                Some(super::checkpoint::ControllerRelayLease::Managed {
                    handle,
                    lease: Some(lease),
                }),
                Some(owner),
                Some(admission.clone()),
            )),
            Err(error) if crate::worker_client::RelayTransportDead::marks(&error) => {
                tracing::warn!(session_id = id, error = %error, "Move will recover the unavailable source without its harness");
                Ok(Self(None, Some(owner), Some(admission.clone())))
            }
            Err(error) => Err(error),
        }
    }

    async fn set_subagent_admission_via_manager(
        admission: &crate::recovery_gate::SessionAdmission,
        manager: &SessionManagerControl,
        id: &str,
        open: bool,
    ) -> Result<bool> {
        let handle = manager
            .wait_for_session(id, std::time::Duration::from_secs(5))
            .await?;
        let mut lease = handle.lease_connection(admission).await?;
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
        if !worker_drains_subagent_requests(protocol_version) {
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
        let delivery_drain = self
            .2
            .as_ref()
            .context("Move source admission missing")?
            .drain_delegation()?;

        let drain = async {
            loop {
                ensure!(
                    !executor.cancellation_requested(),
                    "Move cancelled while waiting for sub-agent requests"
                );
                let snapshot = match self.sync(id).await {
                    Err(error) if crate::session_manager::RelayJobDeferred::marks(&error) => {
                        executor.notify_notice("Waiting for subagent requests to finish");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    result => result?.context(
                        "source worker became unavailable while draining sub-agent requests",
                    )?,
                };
                if let Some(error) = &snapshot.subagent_queue_error {
                    executor.notify_notice(&format!(
                        "Waiting for subagent requests to synchronize: {error}"
                    ));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                let parent = id.to_owned();
                let effects_pending = mj_core::runtime::spawn_blocking(move || {
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

        drop(delivery_drain);
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
            *lease = Some(
                handle
                    .lease_connection(self.2.as_ref().context("Move source admission missing")?)
                    .await?,
            );
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
    fn apply_in_place_destination(&mut self, operation: &MoveOperation) -> Result<()> {
        let profile_id = operation
            .selection
            .profile_id
            .as_ref()
            .unwrap_or(&operation.source_profile_id);
        let profile = self.config.named_profile(profile_id)?;
        let session = self
            .state
            .sessions
            .get_mut(&operation.selection.session_id)
            .context("Move session missing")?;
        session.last_profile = profile_id.clone();
        session.harness_kind = profile.kind;
        session.target_template_id = operation
            .selection
            .target_template_id
            .clone()
            .unwrap_or_else(|| operation.source_target_template_id.clone());
        if let Some(policy) = &operation.selection.subagents {
            session.subagents = Some(policy.clone());
        }
        if let Some(oracle) = operation.selection.oracle {
            session.oracle = oracle;
        }
        if let Some(choice) = &operation.selection.oracle_choice {
            session.oracle_choice = Some(Box::new(choice.clone()));
        }
        session.native_session_id = operation.destination_native_session_id.clone();
        session.updated_at = now();
        Ok(())
    }

    fn settle_in_place_reload_failure(
        &mut self,
        operation: &mut MoveOperation,
        failure: CommittedReloadFailure,
    ) -> Result<()> {
        let previous = self
            .state
            .sessions
            .get(&operation.selection.session_id)
            .context("Move session missing")?
            .clone();
        self.apply_in_place_destination(operation)?;
        let session = self
            .state
            .sessions
            .get_mut(&operation.selection.session_id)
            .context("Move session missing")?;
        session.state = SessionState::Error;
        session.last_error = Some(failure.message.clone());
        session.staged_aws_profile = failure.aws_profile;
        if let Err(error) = crate::database::save_resumed_session(session, None) {
            self.state
                .sessions
                .insert(operation.selection.session_id.clone(), previous);
            return Err(error);
        }
        // Unfence only after the row names the same account as committed disk.
        operation.in_place_expected_store_id = None;
        operation.in_place_held_command_ids.clear();
        operation.phase = MovePhase::Failed;
        operation.error = Some(failure.message);
        operation.updated_at = now();
        crate::database::save_move_operation(operation)
    }

    fn committed_in_place_launch(
        &self,
        operation: &MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<mj_core::worker_launch::WorkerLaunchConfig>> {
        use super::lifecycle::origin_delta::read_wire;
        use mj_core::worker_launch::{
            PENDING_RELOAD_FILE, PendingInPlaceReload, RELOAD_CANDIDATES_DIR, WorkerLaunchConfig,
        };
        let id = &operation.selection.session_id;
        let session = self
            .state
            .sessions
            .get(id)
            .context("Move session missing")?;
        let backend = super::backend_locator(
            session.target.as_deref().context("Move target missing")?,
            session,
            &self.config,
        )?;
        let root = crate::targets::worker_root(&backend, id)?;
        let pending: Option<PendingInPlaceReload> =
            read_wire(&backend, id, &root, PENDING_RELOAD_FILE, executor)?;
        if let Some(pending) = pending.filter(|p| p.operation_id == operation.operation_id) {
            mj_core::worker_launch::validate_candidate_id(&pending.candidate_id)?;
            return read_wire(
                &backend,
                id,
                &root,
                &format!(
                    "{RELOAD_CANDIDATES_DIR}/{}/launch.json",
                    pending.candidate_id
                ),
                executor,
            );
        }
        let launch: Option<WorkerLaunchConfig> =
            read_wire(&backend, id, &root, "launch.json", executor)?;
        Ok(launch.filter(|launch| {
            launch.accepted_reload_operation_id.as_deref() == Some(operation.operation_id.as_str())
        }))
    }

    fn committed_in_place_failure(
        &self,
        operation: &MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<CommittedReloadFailure>> {
        use super::lifecycle::origin_delta::read_wire;
        let id = &operation.selection.session_id;
        let session = self
            .state
            .sessions
            .get(id)
            .context("Move session missing")?;
        let locator = session.target.as_deref().context("Move target missing")?;
        let backend = super::backend_locator(locator, session, &self.config)?;
        let root = crate::targets::worker_root(&backend, id)?;
        let exit: Option<super::worker_binary::WorkerExitRecord> = read_wire(
            &backend,
            id,
            &root,
            mj_core::relay::WORKER_EXIT_FILE,
            executor,
        )?;
        let Some(failure) = exit.and_then(|exit| exit.harness_startup_failure) else {
            return Ok(None);
        };
        let liveness = super::worker_binary::probe_worker_liveness(executor, &backend, id, &root)?;
        ensure!(
            liveness.status == 0,
            "cannot determine failed reload worker liveness"
        );
        if String::from_utf8_lossy(&liveness.stdout).trim() != "dead" {
            return Ok(None);
        }
        Ok(self
            .committed_in_place_launch(operation, executor)?
            .map(|launch| CommittedReloadFailure {
                message: failure.message,
                aws_profile: launch.environment.get("AWS_PROFILE").cloned(),
            }))
    }

    /// Close takes over only after the Move owner has actually stopped. A
    /// placement published during cloning is not a destination worker.
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
                super::environment::retire_owned_environment(
                    environment,
                    &self.config,
                    executor,
                    super::BranchDisposition::Keep,
                    super::CheckoutDisposition::Remove,
                )?;
                crate::database::forget_retained_move_source(operation_id)
            },
        )
    }

    pub async fn settle_superseded_move_for_close(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        before_close: Option<super::lifecycle::BeforeClose>,
        settlement: super::lifecycle::SuspendSettlement,
    ) -> Result<bool> {
        let Some(mut operation) = crate::database::load_move_operation(session_id)? else {
            return Ok(false);
        };
        if operation.phase != MovePhase::Cancelled
            || operation.is_resolved()
            || operation.queue_admission_started
        {
            return Ok(false);
        }
        let Some(source) = operation.recovery_session.clone() else {
            // An archive written before sealing cannot authorize teardown;
            // even a Closed source might contain work newer than its checkpoint.
            return Err(super::lifecycle::LifecycleFailureReason::UnsealedMoveSource.into());
        };
        let current = self
            .state
            .sessions
            .get(session_id)
            .context("Move session disappeared")?;
        // Restore publishes a boot owner before launching the harness. It
        // may already accept work before Running or StartingQueue is saved.
        if current.state == SessionState::Running
            || (current.target.is_some()
                && current.target.as_deref() == operation.destination_target.as_ref())
            || crate::database::load_worker_restart(session_id)?
                .is_some_and(|intent| current.target.as_deref() == Some(&intent.target))
        {
            return Ok(false);
        }
        let retained_source = operation.retains_source_environment() && source.target.is_some();
        let clone_prerequisite = if !retained_source {
            let verified = operation
                .checkpoint
                .as_ref()
                .context("superseded Move has no full source checkpoint")?;
            super::checkpoint::verify_installed_checkpoint_gate(session_id, verified)?;
            super::worktree::clone_retirement_prerequisite_for_checkpoint(verified)
        } else {
            super::worktree::CloneRetirementPrerequisite::None
        };
        if let Some(before_close) = before_close {
            before_close.await?;
        }
        // Created/CleanupPending destinations may not have a target locator
        // yet. Their durable owner must still retire them before resolution.
        self.cleanup_prepared_move_destination(&mut operation, executor)
            .context(super::lifecycle::LifecycleFailureReason::Cleanup)?;
        if retained_source {
            if operation.in_place {
                let reason = anyhow::Error::new(
                    super::lifecycle::LifecycleFailureReason::RetainedMoveSource,
                );
                self.retain_failed_in_place_move_with_persist(
                    session_id,
                    &source,
                    reason,
                    |record| {
                        crate::database::settle_superseded_move_source(
                            record,
                            None,
                            &operation.operation_id,
                            settlement == super::lifecycle::SuspendSettlement::Suspend,
                        )
                    },
                )?;
            } else {
                let source_environment =
                    crate::database::move_source_environment(&operation.operation_id)?
                        .context("superseded Move has no durable source environment")?;
                let source_target = source
                    .target
                    .as_deref()
                    .context("Move source has no target")?;
                ensure!(
                    source_environment.owner_session_id == session_id
                        && source_environment.phase == crate::database::EnvironmentPhase::Active
                        && source_environment
                            .placement
                            .target
                            .as_ref()
                            .is_some_and(|target| target.same_resource(source_target)),
                    "superseded Move no longer owns its retained source"
                );
                let current = &self.state.sessions[session_id];
                let current_destination = current
                    .target
                    .as_deref()
                    .filter(|target| !target.same_resource(source_target));
                // A crash after creating a destination checkout (even before
                // runtime provisioning sets `current.target`) leaves the
                // session bound to a distinct provisional environment in the
                // ledger. Retire it before rebinding the session to the
                // retained source.
                let provisional_destination =
                    match crate::database::session_environment(session_id)? {
                        Some(destination) if destination.id != source_environment.id => {
                            ensure!(
                                destination.owner_session_id == session_id,
                                "superseded Move no longer owns its provisional destination"
                            );
                            if let Some(current_target) = current_destination {
                                ensure!(
                                    destination
                                        .placement
                                        .target
                                        .as_ref()
                                        .is_some_and(|target| target.same_resource(current_target)),
                                    "superseded Move no longer owns its provisional destination"
                                );
                            } else {
                                ensure!(
                                    destination
                                        .placement
                                        .target
                                        .as_ref()
                                        .is_none_or(|target| !target.same_resource(source_target)),
                                    "provisional Move destination still names the retained source"
                                );
                            }
                            Some(destination)
                        }
                        _ => {
                            ensure!(
                                current_destination.is_none(),
                                "superseded Move has no durable destination environment"
                            );
                            None
                        }
                    };
                if let Some(destination) = provisional_destination {
                    if let (Some(source), Some(destination)) = (
                        &source_environment.placement.managed_worktree,
                        &destination.placement.managed_worktree,
                    ) {
                        ensure!(
                            source.worktree_root != destination.worktree_root
                                || !source.target.same_location(&destination.target),
                            "provisional Move destination still names the source checkout"
                        );
                    }
                    super::environment::retire_owned_environment(
                        destination,
                        &self.config,
                        executor,
                        super::BranchDisposition::Keep,
                        super::CheckoutDisposition::Remove,
                    )
                    .context(super::lifecycle::LifecycleFailureReason::Cleanup)?;
                }
                let backend = super::backend_locator(source_target, &source, &self.config)?;
                // Stop any surviving or sealed source worker before resolving
                // the Move so a failed stop keeps the operation retryable and
                // Resume cannot reattach to a sealed relay.
                crate::targets::stop_worker_plan(
                    &backend,
                    session_id,
                    "stop retained Move source worker before retry",
                )
                .and_then(|plan| {
                    plan.execute(
                        &crate::targets::CancellableProcessExecutor::with_timeout(
                            std::time::Duration::from_secs(15),
                        ),
                        &super::ControlWorker,
                    )
                })
                .context("stop retained worker before retry")?;
                // Clear any OriginDelta quiescence wire files left by the
                // abandoned Move so a later Suspend on the resumed source
                // cannot adopt the Move's old request or receipt.
                super::lifecycle::origin_delta::clear_suspend_records_at_target(
                    &backend, session_id, executor,
                )
                .context("clear retained Move source suspend records before retry")?;
                let mut retained = source;
                retained
                    .resource_allocation
                    .clone_from(&current.resource_allocation);
                retained
                    .additional_mounts
                    .clone_from(&current.additional_mounts);
                retained.state = SessionState::Error;
                retained.last_error =
                    Some(super::lifecycle::LifecycleFailureReason::RetainedMoveSource.to_string());
                retained.updated_at = now();
                crate::database::settle_superseded_move_source(
                    &retained,
                    Some(&source_environment.id),
                    &operation.operation_id,
                    settlement == super::lifecycle::SuspendSettlement::Suspend,
                )?;
                self.state.sessions.insert(session_id.to_owned(), retained);
            }
        } else {
            // The environment ledger owns both identities, even when the
            // failed provisioning left only a partial destination checkout.
            let source_environment =
                crate::database::move_source_environment(&operation.operation_id)?;
            let current = self.state.sessions[session_id].clone();
            let has_provisional_destination = crate::database::session_environment(session_id)?
                .is_some_and(|environment| {
                    source_environment
                        .as_ref()
                        .is_none_or(|source_env| environment.id != source_env.id)
                });
            if current.target.is_some() || has_provisional_destination {
                let prerequisite = if has_provisional_destination {
                    super::worktree::CloneRetirementPrerequisite::None
                } else {
                    clone_prerequisite
                };
                super::environment::retire_session_environment_with_clone_prerequisite(
                    &current,
                    &self.config,
                    executor,
                    super::BranchDisposition::Keep,
                    prerequisite,
                )
                .context(super::lifecycle::LifecycleFailureReason::Cleanup)?;
            }
            if source.target.is_some()
                && let Some(environment) = &source_environment
            {
                super::environment::retire_owned_environment_with_clone_prerequisite(
                    environment.clone(),
                    &self.config,
                    executor,
                    super::BranchDisposition::Keep,
                    super::CheckoutDisposition::Remove,
                    clone_prerequisite,
                )
                .context(super::lifecycle::LifecycleFailureReason::Cleanup)?;
            }
            let mut stopped = source;
            stopped.state = SessionState::Stopped;
            stopped.target = None;
            stopped.checkpoint = operation.checkpoint.clone().map(Box::new);
            stopped.last_error = None;
            stopped.updated_at = now();
            crate::database::settle_superseded_move_source(
                &stopped,
                source_environment
                    .as_ref()
                    .map(|environment| environment.id.as_str()),
                &operation.operation_id,
                settlement == super::lifecycle::SuspendSettlement::Suspend,
            )?;
            self.state.sessions.insert(session_id.to_owned(), stopped);
        }
        // Source ownership and the Move's resolution became durable together.
        release_move_queue_hold(session_id);
        if retained_source {
            return Err(super::lifecycle::LifecycleFailureReason::RetainedMoveSource.into());
        }
        Ok(true)
    }
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
                return Ok(Some(super::worktree::plan_raw_to_workspace_for_session(
                    source,
                    &checkout,
                    &self.config,
                    executor,
                )?));
            }
            ResumePlan::WorkspaceToRaw => {
                self.plan_workspace_to_raw(source, target_id, false, executor)?;
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
            let captured_prompts = if let Some(snapshot) = operation.source_snapshot.as_deref() {
                snapshot.queued_prompts.clone()
            } else {
                let checkpoint = operation
                    .restore_artifact()
                    .context("retained queue checkpoint is missing")?;
                let verified = verify_archive_streaming(&checkpoint.archive_path)?;
                ensure!(
                    verified.archive_sha256 == checkpoint.sha256
                        && verified.manifest.session.id == source.id,
                    "retained move checkpoint verification failed"
                );
                verified.canonical_session.queued_prompts
            };
            queued = captured_prompts
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
        ensure!(
            self.config.profiles.contains_key(profile_id),
            "move profile no longer exists"
        );
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        // A destination that cannot continue the native conversation starts a
        // fresh one on its own default model, so there is no model to keep.
        if profile_id == source.last_profile
            || !self
                .config
                .native_continuity(source_harness, &source.last_profile, profile_id)?
        {
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
        catalog: &std::sync::Arc<crate::profile_catalog::ProfileCatalog>,
        mut selection: MoveSelection,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MovePreparation> {
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown session")?;
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
            selection.oracle = selection.oracle.or(retained.selection.oracle);
            selection.oracle_choice = selection
                .oracle_choice
                .or(retained.selection.oracle_choice.clone());
            if retained.selection.oracle.is_none() && selection.oracle == Some(source.oracle) {
                selection.oracle = None;
            }
            if retained.selection.oracle_choice.is_none()
                && selection.oracle_choice.as_ref() == source.oracle_choice.as_deref()
            {
                selection.oracle_choice = None;
            }
            // Legacy selections omitted an unchanged policy. A viewer may
            // send that same effective policy explicitly; it changes nothing.
            if retained.selection.subagents.is_none()
                && selection.subagents.as_ref()
                    == Some(&source.subagents.clone().unwrap_or_default())
            {
                selection.subagents = None;
            }
        }
        let previous = previous.filter(|operation| !move_needs_fresh_capture(operation, source));
        // A Move must change something about the environment. A size alone is
        // a change: it resizes the destination container on the target the
        // session already names.
        ensure!(
            selection.profile_id.is_some()
                || selection.target_template_id.is_some()
                || selection.resource_allocation.is_some()
                || selection.additional_mounts.is_some()
                || selection.clear_resource_allocation,
            "move requires a target, a profile, or container settings"
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
        let retry = previous.as_ref().is_some_and(|op| op.checkpoint_retained());
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
        ensure!(
            !selection.clear_resource_allocation || selection.resource_allocation.is_none(),
            "select resource allocation or explicitly clear it, not both"
        );
        if let Some(retained) = previous.as_ref().filter(|op| op.holds_source_environment()) {
            // An in-place Move transfers no files, so its file selection is
            // not part of what a retry has to repeat. Every surface (the web
            // retry has no large-transfer flag at all) retries it the same way.
            if retained.in_place {
                selection.skip_keep_paths = retained.selection.skip_keep_paths.clone();
            }
            // A field the request omits means the one the Move was sealed
            // with, so a surface that cannot express it (the CLI has no flag
            // for attached directories) can still retry a Move another
            // surface sealed. Only the destination's delegation policy can
            // change on a failed pre-destination retry.
            let sealed = &retained.selection;
            if selection.additional_mounts.is_none() {
                selection.additional_mounts = sealed.additional_mounts.clone();
            }
            // The accepted preparation is the sealed record of this decision,
            // so a retry compares against what the Move decided rather than
            // against host state other sessions keep changing.
            reconcile_retry_resource_selection(
                sealed,
                retained
                    .accepted_preparation
                    .as_ref()
                    .is_some_and(|prepared| prepared.sized_by_destination_default),
                &mut selection,
            );
            if selection.subagents.is_none() {
                selection.subagents = sealed.subagents.clone();
            }
            if selection.oracle.is_none() && selection.oracle_choice.is_none() {
                selection.oracle = sealed.oracle;
                selection.oracle_choice = sealed.oracle_choice.clone();
            }
            ensure!(
                matches_move_retry_selection(retained, &selection),
                "{}",
                sealed_selection_difference(&retained.selection, &selection)
            );
            ensure!(
                !retained.in_place
                    || (retained.source_target.is_some()
                        && source.target.as_deref() == retained.source_target.as_ref()),
                "retained Move target is missing or changed; refusing to recreate it"
            );
        }
        // Owned, because the resource decision below rewrites the selection
        // they name and every later step still reads both identifiers.
        let profile_id = selection.profile_id.clone().unwrap();
        let target_id = selection.target_template_id.clone().unwrap();
        let yolo_target = self
            .config
            .targets
            .get(&target_id)
            .is_some_and(mj_core::config::TargetTemplate::is_yolo);
        let profile = self.config.named_profile(&profile_id)?;
        let requested_oracle = selection.oracle.unwrap_or(source.oracle);
        let explicit_oracle = selection.oracle == Some(true) || selection.oracle_choice.is_some();
        if requested_oracle
            && explicit_oracle
            && selection
                .oracle_choice
                .as_ref()
                .or(source.oracle_choice.as_deref())
                .is_none_or(|choice| choice.model.trim().is_empty())
            && !self.config.subagents.oracle_configured()
        {
            selection.oracle = Some(false);
            selection.oracle_choice = None;
            executor.notify_notice(mj_core::state::ORACLE_UNAVAILABLE_NOTICE);
        }
        let policy = move_subagent_policy(source, &selection);
        // An unchanged delegation policy on the same target mode has already
        // been admitted. Changing credentials for the same harness does not
        // change its eligible models, but a YOLO destination reads a different
        // set of choices and must be checked.
        let source_yolo = source.is_yolo_target(&self.config);
        if profile.kind != source.harness_kind
            || policy != source.subagents.clone().unwrap_or_default()
            || yolo_target != source_yolo
            || previous.as_ref().is_some_and(|operation| {
                operation.holds_source_environment()
                    && operation.selection.subagents != selection.subagents
            })
        {
            super::profile_config::validate_session_subagent_policy(
                catalog,
                &self.config,
                &profile_id,
                &policy,
                &target_id,
            )
            .await?;
        }
        self.config
            .targets
            .get(&target_id)
            .context("unknown destination target")?;
        let target = self.config.ensure_target_enabled(&target_id)?;
        self.validate_muse_resume_destination(source, profile.kind, &target_id)?;
        let checkout = self.state.checkout(&source.id)?;
        super::worktree::resume_compatibility_with_checkout(
            source,
            &checkout,
            &self.config,
            &target_id,
        )
        .map_err(anyhow::Error::msg)?;
        // The environment decision and the destination size are one decision,
        // so one function makes both and the sealed selection is what it
        // decided from.
        let (in_place, resolved_by_default) = resolve_move_size_for_environment(
            source,
            &mut selection,
            &self.state,
            &self.config.targets,
            target,
            self.state.subagents.contains_key(&source.id),
            previous.as_ref(),
        )?;
        // The resolved selection is what every later reader decides from --
        // the sealed Move, capacity admission inside execution and the
        // destination record -- so it is never put back to the unresolved
        // request.
        let sized_by_destination_default =
            move_size_is_destination_default(resolved_by_default, previous.as_ref());
        super::backend::validate_resource_allocation(
            target,
            move_requested_allocation(source, &selection),
        )?;
        // Preparation is where a Move is decided, and only an accepted Move
        // logs its decision, so a refused preparation would otherwise say
        // nothing about which part of the environment blocked keeping it.
        tracing::info!(
            session_id = %source.id,
            in_place,
            source_state = ?source.state,
            retained_checkpoint = retry,
            obstacle = move_environment_obstacle(
                source,
                &selection,
                &self.config.targets,
                self.state.subagents.contains_key(&source.id),
            ),
            "move environment decision at preparation"
        );
        let mounts = move_requested_mounts(source, &selection);
        ensure!(
            profile.kind != mj_core::config::HarnessKind::Muse || mounts.is_empty(),
            "{} ACP supports one workspace root; attached directories are unsupported",
            profile.kind.display_name()
        );
        ensure!(
            mounts.is_empty() || mj_core::config::mount_history_host(target).is_some(),
            "attached resources are unsupported for this target; select compatible resources explicitly"
        );
        crate::targets::validate_additional_mounts(mounts)?;
        let mut container_upgrade = None;
        if !retry && target_id == source.target_template_id && !in_place {
            let settings = mj_core::state::ContainerSettingsChange {
                resource_allocation: move_requested_allocation(source, &selection).cloned(),
                mounts: mounts.to_vec(),
                mount_history: None,
            };
            if let Some(plan) = self
                .plan_container_settings_replacement(&source.id, "preview", &settings, executor)?
            {
                ensure!(move_keeps_profile_and_policy(source, &selection), mj_core::refusal::Refusal::precondition(
                    "Apply the container size and mounts first, then Move to change the profile or policy."
                ).with_code("container-settings-with-profile-move"));
                container_upgrade = Some(Box::new(plan.settings_preview(&source.id, settings)));
            }
        }
        let keeps_workspace = in_place || container_upgrade.is_some();
        let planned_conversion = if keeps_workspace {
            None
        } else {
            for mount in mounts {
                self.validate_mount_source(&target_id, &mount.source, executor)?;
            }
            self.validate_move_destination_paths(source, &target_id, executor)?
        };
        ensure!(
            profile.home.is_dir(),
            "destination profile home is unavailable; configure the profile before moving"
        );
        let source_record = previous
            .as_ref()
            .filter(|operation| !operation.queue_admission_started && !operation.is_resolved())
            .and_then(|operation| operation.recovery_session.as_ref())
            .unwrap_or(source);
        let cross_harness = !self.config.native_continuity(
            source_record.harness_kind,
            &source_record.last_profile,
            &profile_id,
        )?;
        // A compatible in-place Move already has its worker, mounts and
        // runtime. Only reconstruction needs provisioning prerequisites.
        if !keeps_workspace || cross_harness || retry {
            let worker_target = super::backend::session_target(
                &self.config,
                &source_record.bundle_id,
                &target_id,
                executor,
            )?;
            super::worker_binary::preflight_worker_binary(&worker_target, executor)?;
            super::backend::preflight_target(
                target,
                executor,
                super::backend::TargetCheck::Launch,
            )?;
        }
        // What moving a local checkout into a target really does, computed
        // before anything is stopped so a person can confirm it.
        let conversion = planned_conversion
            .map(|conversion| {
                super::worktree::raw_conversion_preview_with_checkout(source, &conversion, executor)
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
                matches_move_retry_selection(operation, &selection)
                    && !operation.is_resolved()
                    && operation.checkpoint_retained()
            })
            .map(|operation| operation.operation_id.clone())
            .unwrap_or(new_command_id("move")?);
        let destination_has_parent_role =
            parent_tools_enabled(&move_subagent_policy(source, &selection), profile.kind);
        if in_place
            && !destination_has_parent_role
            && let Some(error) = roleless_move_children_error(&self.state, &source.id)
        {
            bail!("{error}; close or finish those children before moving to this harness");
        }
        let route_preview = if in_place {
            None
        } else {
            source.target.as_deref().and_then(|src| {
                let target_id = selection.target_template_id.as_deref()?;
                self.config.targets.get(target_id).map(|dest| {
                    if matches!(
                        src,
                        mj_core::state::TargetLocator::LocalBare { .. }
                            | mj_core::state::TargetLocator::LocalPodman { .. }
                            | mj_core::state::TargetLocator::LocalDocker { .. }
                            | mj_core::state::TargetLocator::AppleContainer { .. }
                    ) && matches!(
                        dest,
                        mj_core::config::TargetTemplate::LocalBare
                            | mj_core::config::TargetTemplate::LocalPodman { .. }
                            | mj_core::config::TargetTemplate::LocalDocker { .. }
                            | mj_core::config::TargetTemplate::AppleContainer { .. }
                    ) {
                        mj_core::state::RoutePreview::SameHost
                    } else {
                        mj_core::state::RoutePreview::DirectOrDaemonStream
                    }
                })
            })
        };
        let (suspend_preflight, skip_keep_sizes) = if !keeps_workspace
            && matches!(
                source.state,
                SessionState::Running | SessionState::Disconnected
            )
            && source.target.is_some()
        {
            match self
                .move_keep_selection(source, &selection, executor)
                .and_then(|keep| {
                    self.checkpoint_preflight(&source.id, true, false, Some(&keep), executor)
                }) {
                Ok((preflight, skip_keep_sizes)) => (Some(Box::new(preflight)), skip_keep_sizes),
                Err(err) => {
                    tracing::debug!(
                        session_id = %source.id,
                        error = %err,
                        "Move prepare could not collect suspend preflight; continuing without it"
                    );
                    (None, Vec::new())
                }
            }
        } else {
            (None, Vec::new())
        };
        let locked_queue = previous
            .as_ref()
            .filter(|op| retry_move_operation(op, source, &selection) && op.queue_admission_started)
            .map(|op| op.queue);
        Ok(MovePreparation {
            container_upgrade,
            destination_checks: DestinationChecks::Checked,
            route_preview,
            suspend_preflight,
            skip_keep_sizes,
            source_unavailable: false,
            in_place,
            // Recreating the environment can stop native children too; retain
            // that warning even when no managed child is recorded yet.
            stops_subagents: !in_place
                || (!destination_has_parent_role
                    && !self.state.active_borrowers(&source.id).is_empty()),
            conversion,
            selection,
            sized_by_destination_default,
            source_profile_id: source.last_profile.clone(),
            source_target_template_id: source.target_template_id.clone(),
            cross_harness,
            active,
            queued_commands,
            locked_queue,
            fingerprint,
            operation_id,
        })
    }

    /// Publish acceptance only after the existing Move intent is committed.
    #[allow(clippy::too_many_arguments)] // Session ownership is an explicit capability.
    pub async fn move_session_managed_controlled_with_admission(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        catalog: &std::sync::Arc<crate::profile_catalog::ProfileCatalog>,
        request: MoveSessionRequest,
        quota_move: Option<(String, mj_core::state::QuotaMoveContinuation)>,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        on_accepted: impl FnOnce() + Send,
    ) -> Result<MoveOutcome> {
        crate::worker_lifecycle::run_admitted(admission, &request.preparation.selection.session_id.clone(), async {
        let prepared = &request.preparation;
        let id = prepared.selection.session_id.clone();
        let started = std::time::Instant::now();
        executor.notify_notice("Checking destination");
        let mut source_relay = MoveSourceRelay::default();
        let mut checked = {
            let _checking_destination =
                ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            let mut checked = {
                let _timing = MovePhaseTimer::new(&id, "preflight destination checks");
                let mut checked = self
                    .prepare_move_session_controlled(
                        catalog,
                        prepared.selection.clone(),
                        executor,
                    )
                    .await?;
                // This re-prepares a selection preparation already resolved,
                // so it sees an explicit size and cannot tell that clearing
                // produced it. The confirmed preparation owns that fact; keep
                // it, or a retry of the same clear request stops matching the
                // Move this acceptance seals (#1585).
                checked.sized_by_destination_default =
                    prepared.sized_by_destination_default;
                checked
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
                    MoveSourceRelay::lease(admission, manager, &id).await?
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
                        let purpose = if quota_move.is_some() && checked.in_place && !checked.cross_harness {
                            mj_core::activity::MovePurpose::QuotaRetry
                        } else {
                            mj_core::activity::MovePurpose::Manual
                        };
                        !operational.safe_to_move(source_harness, purpose)
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
                checked.fingerprint = if quota_move.is_some() {
                    let operational = &snapshot.as_ref().context("quota Move source unavailable")?.operational;
                    quota_move_fingerprint(&id, &fingerprint, operational)?
                } else { fingerprint };
            }
            checked
        };
        ensure!(
            checked.container_upgrade.is_none(),
            mj_core::refusal::Refusal::precondition("Prepare Move again and confirm the container settings replacement.")
        );
        ensure!(
            checked.fingerprint == prepared.fingerprint,
            "session, pending work, or destination configuration changed; prepare and confirm Move again"
        );
        // Preserve the warning the person accepted, including old workers
        // whose child admission cannot be drained before interruption.
        if checked.stops_subagents && !prepared.stops_subagents {
            return Err(mj_core::refusal::Refusal::precondition(
                "sub-agents appeared after preparation; prepare and confirm Move again before stopping them"
            ).with_code(QUOTA_MOVE_SAFETY_REFUSAL).into());
        }
        checked.stops_subagents = prepared.stops_subagents;
        let source = self.state.sessions[&id].clone();
        let retry = crate::database::load_move_operation(&id)?
            .filter(|op| retry_move_operation(op, &source, &checked.selection));
        let locked_queue = retry
            .as_ref()
            .filter(|op| op.queue_admission_started)
            .map(|op| op.queue);
        let queue = request
            .queue
            .or(locked_queue)
            .unwrap_or(ResumeQueueDisposition::Discard);
        let updates_retry_policy = retry
            .as_ref()
            .is_some_and(|op| op.selection.subagents != checked.selection.subagents);
        if retry.is_none() && move_changes_nothing(&source, &checked.selection) {
            crate::database::accept_unchanged_move(&checked.selection)?;
            let latest = crate::database::load_session_record(&id)?.context("Move session disappeared")?;
            let record = self.state.sessions.get_mut(&id).unwrap();
            record.resource_allocation = latest.resource_allocation;
            record.additional_mounts = latest.additional_mounts;
            on_accepted();
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
            checked.queued_commands.is_empty() || request.queue.is_some() || locked_queue.is_some(),
            "pending work requires an explicit queue choice: discard or start"
        );
        let timestamp = now();
        let mut operation = match retry {
            Some(mut op) => {
                // A retry decides what is restored, never whether the
                // environment is touched.
                ensure!(
                    op.in_place == checked.in_place,
                    "the Move's environment decision changed after it was sealed; prepare and confirm the Move again"
                );
                ensure!(
                    !op.queue_admission_started || op.queue == queue,
                    "queued work may already have run; retry with the original queue choice on the same destination"
                );
                crate::database::clear_move_cancellation_for_retry(&id)?;
                op.configuration_fingerprint =
                    self.move_configuration_fingerprint(&checked.selection)?;
                op.selection.subagents = checked.selection.subagents.clone();
                op.queue = queue;
                op.cancellation_requested = false;
                op.error = None;
                op
            }
            None => {
                // Preserve any still-unrevoked ephemeral SSH keys from a prior
                // finished Move on this session so accepting a new Move cannot
                // drop pending remote credential revocation.
                let carried_keys = crate::database::load_move_operation(&id)?
                    .filter(|prev| !prev.is_active())
                    .map(|prev| prev.granted_keys)
                    .unwrap_or_default();
                MoveOperation {
                quota_stop_reason: None,
                quota_continuation: None,
                prepared_destination: None,
                accepted_preparation: None,
                acknowledge_interruption: false,
                transfer_route: None,
                consent: checked.selection.consent.clone(),
                skip_keep_paths: checked.selection.skip_keep_paths.clone(),
                granted_keys: carried_keys,
                granted_ec2_rules: Vec::new(),
                in_place_expected_store_id: None,
                in_place_continuation_command_id: None,
                in_place_held_command_ids: Vec::new(),
                in_place_harness_restarted: false,
                deferred_environment_changes: Vec::new(),
                source_snapshot: None,
                source_checkpoint_only: false,
                in_place: checked.in_place,
                operation_id: prepared.operation_id.clone(),
                selection: checked.selection.clone(),
                source_profile_id: source.last_profile.clone(),
                source_target_template_id: source.target_template_id.clone(),
                source_target: source.target.clone().map(|v| *v),
                source_native_session_id: source.native_session_id.clone(),
                inherited_resource_allocation: None,
                inherited_additional_mounts: Vec::new(),
                source_additional_mounts: Vec::new(),
                source_resource_allocation: None,
                destination_target: None,
                destination_native_session_id: if checked.in_place && self.config.native_continuity(
                    source.harness_kind,
                    &source.last_profile,
                    checked.selection.profile_id.as_ref().unwrap_or(&source.last_profile),
                )? {
                    source.native_session_id.clone()
                } else {
                    None
                },
                destination_store_id: None,
                configuration_fingerprint: self
                    .move_configuration_fingerprint(&checked.selection)?,
                checkpoint: None,
                recovery_session: None,
                queue,
                phase: MovePhase::Capturing,
                queue_admission_started: false,
                queue_admission_finished: false,
                cancellation_requested: false,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                error: None,
            }
            }
        };
        if let Some((reason, continuation)) = quota_move {
            operation.quota_stop_reason = Some(reason);
            operation.quota_continuation = Some(continuation);
        }
        operation.capture_container_settings(&source);
        operation.accepted_preparation = Some(Box::new(checked.clone()));
        operation.acknowledge_interruption = request.acknowledge_interruption;
        crate::database::accept_move_operation(&operation)?;
        on_accepted();
        if updates_retry_policy {
            executor.notify_notice("Retrying the sealed Move with the updated destination sub-agent policy.");
        }
        tracing::info!(
            session_id = id,
            in_place = operation.in_place,
            reason = move_environment_change_for(&source, &checked.selection, &self.config.targets)
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
        let result = Box::pin(self.execute_move(admission,
            &mut operation,
            Some(&checked),
            executor,
            manager,
            source_relay,
        ))
        .await;
        self.finish_move_result_with_subagent_recovery(admission,
            &mut operation,
            result,
            executor,
            manager,
        )
        .await

        }).await
    }

    pub(super) fn move_keep_selection(
        &self,
        source: &mj_core::state::SessionRecord,
        selection: &MoveSelection,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<mj_core::session_keep::MoveKeep> {
        let locator = source
            .target
            .as_deref()
            .context("Move source target is missing")?;
        let backend = super::backend::backend_locator(locator, source, &self.config)?;
        let directory = self.move_source_keep_dir(source, &backend, executor)?;
        mj_core::session_keep::MoveKeep::new(directory, &selection.skip_keep_paths)
    }

    pub fn finish_move_result(
        &mut self,
        operation: &mut MoveOperation,
        result: Result<()>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MoveOutcome> {
        let id = &operation.selection.session_id;
        let latest =
            crate::database::load_session_record(id)?.context("Move session disappeared")?;
        let record = self
            .state
            .sessions
            .get_mut(id)
            .context("Move session missing from controller")?;
        record.resource_allocation = latest.resource_allocation;
        record.additional_mounts = latest.additional_mounts;
        if result.is_err()
            && ((!operation.in_place
                && (executor.shutdown_interrupted() || !crate::upgrade::gate().is_open()))
                || (operation.in_place
                    && operation.phase == MovePhase::Capturing
                    && (executor.shutdown_interrupted() || !crate::upgrade::gate().is_open())))
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
        if let Err(error) = &result
            && operation.in_place
            && operation.in_place_expected_store_id.is_some()
            && operation.phase != MovePhase::DispatchReleased
            && operation.phase != MovePhase::Failed
        {
            let message = format!("{error:#}");
            operation.updated_at = now();
            operation.error = Some(message.clone());
            crate::database::save_move_operation(operation)?;
            return Ok(outcome(
                &operation.operation_id,
                &operation.selection,
                "interrupted",
                Some(message),
                Some("Move is awaiting its worker's reload outcome; retry to reconnect".into()),
            ));
        }
        let settled_reload_error = operation.in_place
            && operation.phase == MovePhase::Failed
            && self.state.sessions[&operation.selection.session_id].state == SessionState::Error
            && operation.in_place_expected_store_id.is_none();
        let result = match result {
            Err(error)
                if !operation.queue_admission_started
                    && operation
                        .prepared_destination
                        .as_ref()
                        .is_some_and(|d| d.owns_resource()) =>
            {
                executor.begin_resumable_work()?;
                let cleaned = self.cleanup_prepared_move_destination(operation, executor);
                executor.end_resumable_work("Move control transition")?;
                Err(match cleaned {
                    Ok(()) => error,
                    Err(cleanup_error) => error.context(format!("EC2 destination cleanup is pending and will retry automatically: {cleanup_error:#}")),
                })
            }
            result => result,
        };
        let session_id = operation.selection.session_id.clone();
        let fail_closing_source = result.is_err()
            && !operation.in_place
            && operation.holds_source_environment()
            && self.state.sessions[&session_id].state == SessionState::Closing;
        let mut last_error = self
            .state
            .sessions
            .get(&session_id)
            .context("Move outcome session is missing")?
            .last_error
            .clone();
        let mut released_handoffs = Vec::new();
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
                    MovePhase::Capturing => "capturing the source snapshot",
                    MovePhase::Transferring => "transferring the snapshot",
                    MovePhase::Restoring => "restoring the destination",
                    MovePhase::DestinationStarting => "starting the destination worker",
                    MovePhase::DestinationOwns => "retiring the source",
                    MovePhase::SourceRetired => "admitting the destination queue",
                    MovePhase::QueueAdmitted => "settling the move",
                    MovePhase::DispatchReleased => "releasing in-place dispatch",
                    MovePhase::Completed | MovePhase::Failed | MovePhase::Cancelled => {
                        "recovering the move"
                    }
                };
                let cancelled = !settled_reload_error
                    && (executor.cancellation_requested() || operation.cancellation_requested);
                // `DispatchReleased` is post-commit for an in-place Move: keep
                // its durable phase when the worker is disconnected so later
                // recovery (and fresh worker attach) retries releasing the hold.
                if !(operation.in_place && operation.phase == MovePhase::DispatchReleased) {
                    operation.phase = if cancelled {
                        MovePhase::Cancelled
                    } else {
                        MovePhase::Failed
                    };
                }
                operation.cancellation_requested = cancelled;
                released_handoffs = forget_unsealed_move_archives(
                    operation,
                    matches!(
                        self.state
                            .sessions
                            .get(&session_id)
                            .map(|record| record.state),
                        Some(SessionState::Running | SessionState::Disconnected)
                    ),
                )
                .unwrap_or_default();
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
                if !settled_reload_error {
                    last_error = Some(failed_move_message(
                        Some(phase),
                        cancelled,
                        &recovery,
                        &operation.operation_id,
                    ));
                }
                operation.error = Some(error.clone());
                (
                    if cancelled { "cancelled" } else { "failed" },
                    Some(error),
                    Some(recovery),
                )
            }
        };
        operation.updated_at = now();
        crate::database::save_move_outcome(operation, last_error.as_deref(), fail_closing_source)?;
        if !operation.is_active() && !operation.granted_keys.is_empty() {
            recovery::spawn_move_key_cleanup_background();
        }
        // The record no longer names these files, so nothing can restore them.
        for path in released_handoffs {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    %session_id,
                    path = %path.display(),
                    %error,
                    "could not remove the stale handoff of a Move that never sealed its source"
                );
            }
        }
        let record = self
            .state
            .sessions
            .get_mut(&session_id)
            .expect("Move outcome session");
        record.last_error = last_error;
        record.updated_at = operation.updated_at.clone();
        if fail_closing_source {
            record.state = SessionState::Error;
        }
        Ok(outcome(
            &operation.operation_id,
            &operation.selection,
            status,
            error,
            recovery,
        ))
    }

    /// Whether the session's worker exposes the durable sub-agent request
    /// queue, which every top-level session does.
    pub fn source_has_request_queue(&self, id: &str) -> bool {
        self.state.sessions.get(id).is_some_and(|session| {
            worker_subagent_queue_enabled(
                &session.subagents.clone().unwrap_or_default(),
                session.harness_kind,
                self.state
                    .subagents
                    .get(id)
                    .map(|child| child.handback_tool),
                true,
            )
        })
    }

    async fn finish_move_result_with_subagent_recovery(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
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
        ) || !worker_subagent_queue_enabled(
            &session.subagents.clone().unwrap_or_default(),
            session.harness_kind,
            self.state
                .subagents
                .get(session_id)
                .map(|child| child.handback_tool),
            true,
        ) {
            return Ok(outcome);
        }

        // A failed reply is ambiguous: the worker may already have persisted
        // `open=true`. Retry through a fresh actor lease once so the retry uses
        // the next relay connection if the first one was abandoned.
        for attempt in 1..=2 {
            match MoveSourceRelay::set_subagent_admission_via_manager(
                admission, manager, session_id, true,
            )
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

    /// Revoke any ephemeral direct-transfer SSH credentials (`authorized_keys`
    /// entries and staged private-key/`known_hosts` files) still recorded on
    /// `operation.granted_keys`, and persist the updated key list when entries
    /// are cleared.
    pub fn cleanup_move_granted_keys(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) {
        if operation.granted_keys.is_empty() {
            return;
        }
        let id = &operation.selection.session_id;
        let session_target = self
            .state
            .sessions
            .get(id)
            .and_then(|s| s.target.as_deref());
        let mut candidates: Vec<&mj_core::state::TargetLocator> = Vec::new();
        if let Some(target) = operation.source_target.as_ref() {
            candidates.push(target);
        }
        if let Some(target) = operation.destination_target.as_ref() {
            candidates.push(target);
        }
        if let Some(target) = session_target {
            candidates.push(target);
        }
        let backend_targets: Vec<mj_core::targets::TargetLocator> = candidates
            .into_iter()
            .filter_map(|locator| {
                self.state
                    .sessions
                    .get(id)
                    .and_then(|session| super::backend_locator(locator, session, &self.config).ok())
            })
            .collect();
        let refs: Vec<&mj_core::targets::TargetLocator> = backend_targets.iter().collect();
        let remaining = transport::cleanup_interrupted_move_credentials(
            &operation.operation_id,
            &operation.granted_keys,
            &refs,
            executor,
        );
        if remaining.len() != operation.granted_keys.len() {
            let revoked: Vec<_> = operation
                .granted_keys
                .iter()
                .filter(|key| !remaining.contains(key))
                .cloned()
                .collect();
            operation.granted_keys = remaining;
            let timestamp = now();
            operation.updated_at = timestamp.clone();
            let _ = crate::database::remove_revoked_move_granted_keys(id, &revoked, &timestamp);
        }
    }

    pub async fn recover_move_managed_controlled(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        mut operation: MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        crate::worker_lifecycle::run_admitted(admission, &operation.selection.session_id.clone(), async {
        let id = operation.selection.session_id.clone();
        let result = async {
            let mut session = self.state.sessions.get(&id).context("move session is missing")?.clone();
            // Check in-place moves FIRST so `MovePhase::DispatchReleased`
            // (which also sets `queue_admission_started`) routes to the
            // idempotent release handler rather than cross-target
            // `admit_move_queue`.
            if operation.in_place {
                match self.committed_in_place_failure(&operation, executor) {
                    Ok(Some(failure)) => {
                        let message = failure.message.clone();
                        self.settle_in_place_reload_failure(&mut operation, failure)?;
                        bail!("{message}");
                    }
                    Ok(None) => (),
                    Err(error) => tracing::warn!(session_id = %id, %error, "could not probe committed in-place reload failure"),
                }

                if operation.phase == MovePhase::DispatchReleased {
                    let mut relay = MoveSourceRelay::lease(admission, manager, &id).await?;
                    let conn = relay
                        .0
                        .as_mut()
                        .context(
                            "in-place Move dispatch hold release requires a live worker connection",
                        )?
                        .connection_mut();
                    if let Some(expected_store_id) = operation.in_place_expected_store_id.clone()
                        && conn.snapshot().operational.store_id.as_deref()
                            == Some(expected_store_id.as_str())
                    {
                        conn.client
                            .release_in_place_move_dispatch(
                                &operation.operation_id,
                                &expected_store_id,
                            )
                            .await?;
                        if self.source_has_request_queue(&id) {
                            let _ = conn.set_subagent_admission(true).await;
                        }
                    }
                    operation.phase = MovePhase::Completed;
                    operation.in_place_expected_store_id = None;
                    operation.in_place_held_command_ids.clear();
                    operation.queue_admission_finished = true;
                    operation.updated_at = now();
                    crate::database::save_move_operation(&operation)?;
                    return Ok(());
                }
                if operation.cancellation_requested {
                    if let Some(expected_store_id) = operation.in_place_expected_store_id.clone() {
                        let mut relay = MoveSourceRelay::lease(admission, manager, &id).await?;
                        let conn = relay.0.as_mut().context("in-place Move cancellation requires a live worker connection")?.connection_mut();
                        let status = conn.client.status().await?;
                        ensure!(status.store_id.as_deref() == Some(expected_store_id.as_str()), "cannot cancel in-place Move: relay store changed; credential synchronization remains fenced");
                        let hold = status.in_place_move_hold.filter(|hold| hold.operation_id == operation.operation_id);
                        use mj_core::relay::InPlaceReloadPhase;
                        match hold.as_ref().and_then(|hold| hold.reload_phase.as_ref()) {
                            Some(InPlaceReloadPhase::FailedAfterCommit { message }) => {
                                let message = message.clone();
                                let aws_profile = self.committed_in_place_launch(&operation, executor)?.and_then(|launch| launch.environment.get("AWS_PROFILE").cloned());
                                self.settle_in_place_reload_failure(&mut operation, CommittedReloadFailure { message: message.clone(), aws_profile })?;
                                bail!("{message}");
                            }
                            Some(InPlaceReloadPhase::Committed) => bail!("in-place Move cancellation is waiting for the committed harness reload to start or fail"),
                            _ => (),
                        }
                        let started = hold.as_ref().is_some_and(|hold| hold.harness_restarted || matches!(hold.reload_phase, Some(InPlaceReloadPhase::Started)))
                            || (hold.is_none() && self.committed_in_place_launch(&operation, executor)?.is_some());
                        let reopen_admission = self.source_has_request_queue(&id);
                        if started {
                            let destination_id = operation.selection.profile_id.as_ref().unwrap_or(&operation.source_profile_id).clone();
                            let native_continuity = operation.destination_native_session_id.is_some();
                            self.apply_in_place_destination(&operation)?;
                            if let Some(launch) = self.committed_in_place_launch(&operation, executor)? {
                                self.state.sessions.get_mut(&id).unwrap().staged_aws_profile = launch.environment.get("AWS_PROFILE").cloned();
                            }
                            crate::database::save_resumed_session(&self.state.sessions[&id], None)?;
                            if !native_continuity && !operation.in_place_harness_restarted {
                                let materialized = crate::database::load_materialized_session(&id)?.context("cancelled cross-harness Move session projection is missing")?;
                                let snapshot = mj_transcript::projection::canonical_session_from_materialized_owned(materialized)?;
                                let budget = crate::handoff::profile_handoff_bytes(Some(self.config.named_profile(&destination_id)?));
                                let handoff = crate::compaction::render_recent_snapshot(&snapshot, budget);
                                conn.client.install_prompt_context(handoff).await?;
                            }
                            operation.in_place_harness_restarted = true;
                            operation.updated_at = now();
                            crate::database::save_move_operation(&operation)?;
                        }
                        // Cancellation preserves existing queued work. No new
                        // continuation is admitted and no queued prompt is discarded.
                        conn.client.release_in_place_move_dispatch_checked(&operation.operation_id, &expected_store_id, Some(started)).await?;
                        if reopen_admission { conn.set_subagent_admission(true).await?; }
                        operation.in_place_expected_store_id = None;
                        operation.in_place_held_command_ids.clear();
                        operation.queue_admission_finished = true;
                        operation.updated_at = now();
                        crate::database::save_move_operation(&operation)?;
                        if started {
                            bail!("in-place Move was cancelled after switching to destination profile; destination retained");
                        }
                    }
                    bail!("in-place Move was cancelled; dispatch hold released");
                }
                let relay = MoveSourceRelay::lease(admission, manager, &id).await?;
                return Box::pin(self.execute_move(
                    admission,
                    &mut operation,
                    None,
                    executor,
                    manager,
                    relay,
                ))
                .await;
            }
            if operation.queue_admission_started || operation.phase.destination_owns() {
                ensure!(!operation.cancellation_requested, "Move was cancelled; destination retained without further queue admission");
                self.retire_move_source_if_needed(&mut operation, executor)?;
                return self.admit_move_queue(&mut operation, executor).await;
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
                    Box::pin(self.recover_move_source_stop(admission, &mut operation, &cleanup, manager)).await?;
                }
                // A Move that keeps the environment restores only from the
                // handoff its seal recorded. Taking the source's older full
                // checkpoint here would make a lost handoff look retained.
                if !operation.retains_source_environment() {
                    operation.checkpoint = self.state.sessions[&id].checkpoint.clone().map(|v| *v);
                }
                if operation.retains_source_environment() && self.state.sessions[&id].state == SessionState::Closing {
                    let previous = self.state.sessions[&id].clone();
                    operation.recovery_session = Some(previous.clone());
                    // Startup also recovers a Move that already ended (failed
                    // or cancelled) when its source is still `Closing`; only
                    // an active Move restores again.
                    if operation.in_place && operation.is_active() && !operation.cancellation_requested {
                        return self.retry_interrupted_in_place_move(admission, &mut operation, &previous, executor, manager).await;
                    }
                    if !operation.in_place
                        && matches!(
                            self.restart_retained_move_source(
                                admission, &id, &previous, executor,
                            )
                            .await,
                            Ok(true)
                        )
                    {
                        operation.recovery_session = None;
                        operation.source_snapshot = None;
                        if operation.is_active()
                            && operation.phase == MovePhase::Capturing
                            && !operation.cancellation_requested
                        {
                            session = self.state.sessions[&id].clone();
                        } else {
                            bail!("Move source capture was interrupted; source agent restarted in place. Retry Move when ready");
                        }
                    } else {
                        let cause = anyhow::anyhow!("Move source sealed; environment retained for explicit retry");
                        return Err(self.retain_failed_in_place_move(&id, &previous, cause)?);
                    }
                } else {
                    if !operation.retains_source_environment()
                        && self.state.sessions[&id].state == SessionState::Stopped
                        && self.state.sessions[&id].target.is_some()
                    {
                        self.cleanup_stopped_target(&id, &cleanup)?;
                    }
                    let reopened = matches!(
                        self.state.sessions[&id].state,
                        SessionState::Running | SessionState::Disconnected
                    );
                    let can_continue_capturing = operation.is_active()
                        && operation.phase == MovePhase::Capturing
                        && !operation.cancellation_requested
                        && (reopened
                            || (!operation.in_place
                                && self.state.sessions[&id].state == SessionState::Stopped
                                && operation.checkpoint_retained()));
                    if !can_continue_capturing {
                        bail!(
                            "{}",
                            interrupted_source_stop_message(
                                "Move source stop recovered; no destination work was started. Retry Move or Resume with previous settings",
                                operation.in_place,
                            )
                        );
                    }
                    // The stop was interrupted before the seal (or finished sealing
                    // a cross-target Move): resume the Move in the arm below.
                    session = self.state.sessions[&id].clone();
                }
            }
            match operation.phase {
                MovePhase::Capturing
                    if operation.is_active()
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Disconnected
                        )
                        && !operation.cancellation_requested =>
                {
                    let relay = if operation.in_place {
                        MoveSourceRelay::lease(admission, manager, &id).await?
                    } else {
                        MoveSourceRelay::default()
                    };
                    return Box::pin(self.execute_move(
                        admission,
                        &mut operation,
                        None,
                        executor,
                        manager,
                        relay,
                    ))
                    .await;
                }
                MovePhase::Capturing
                    if operation.in_place
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Disconnected
                        )
                        && operation.cancellation_requested =>
                {
                    ensure!(self.state.sessions.contains_key(&id), "move source is missing");
                    if self.source_has_request_queue(&id) {
                        MoveSourceRelay::set_subagent_admission_via_manager(admission,
                            manager, &id, true,
                        )
                        .await?;
                    }
                    bail!("Move was cancelled before source interruption; source retained and sub-agent requests reopened")
                }
                MovePhase::DestinationOwns
                | MovePhase::SourceRetired
                | MovePhase::QueueAdmitted => {
                    self.cleanup_move_granted_keys(&mut operation, executor);
                    ensure!(!operation.cancellation_requested, "Move was cancelled; ready destination retained");
                    self.retire_move_source_if_needed(&mut operation, executor)?;
                    restore_move_queue_hold(&operation);
                    self.admit_move_queue(&mut operation, executor).await
                }
                MovePhase::Restoring | MovePhase::DestinationStarting
                    if session.state == SessionState::Running =>
                {
                    self.cleanup_move_granted_keys(&mut operation, executor);
                    ensure!(Some(&session.last_profile) == operation.selection.profile_id.as_ref()
                        && Some(&session.target_template_id) == operation.selection.target_template_id.as_ref(),
                        "ready destination does not match the move intent");
                    operation.destination_target = session.target.clone().map(|v| *v);
                    operation.destination_native_session_id = session.native_session_id.clone();
                    operation.queue_admission_started = true;
                    operation.phase = MovePhase::DestinationOwns;
                    crate::database::commit_move_destination_owns(&operation, &session)?;
                    self.retire_move_source_if_needed(&mut operation, executor)?;
                    restore_move_queue_hold(&operation);
                    ensure!(!operation.cancellation_requested, "Move was cancelled; ready destination retained");
                    self.admit_move_queue(&mut operation, executor).await
                }
                MovePhase::Transferring | MovePhase::Restoring | MovePhase::DestinationStarting => {
                    self.cleanup_move_granted_keys(&mut operation, executor);
                    let previous = operation.recovery_session.clone().context("move lacks its stopped recovery identity; retain resources for inspection")?;
                    if operation.in_place && !operation.cancellation_requested {
                        return self.retry_interrupted_in_place_move(admission, &mut operation, &previous, executor, manager).await;
                    }
                    let cause = anyhow::anyhow!("destination restoration was interrupted before DestinationOwns; source retained for retry");
                    let error = if operation.in_place {
                        self.retain_failed_in_place_move(&id, &previous, cause)?
                    } else {
                        // Before DestinationOwns, source owns the session:
                        // tear down any destination environment FIRST, and ONLY
                        // restart the source agent in place once destination
                        // teardown is confirmed so two workers can never run
                        // concurrently.
                        executor.begin_resumable_work()?;
                        let cleanup_result =
                            self.cleanup_prepared_move_destination(&mut operation, executor);
                        executor.end_resumable_work("record Move recovery rollback")?;
                        let prepared_cleaned = match cleanup_result {
                            Ok(()) => true,
                            Err(cleanup_err) => {
                                tracing::warn!(
                                    session_id = %id,
                                    operation_id = %operation.operation_id,
                                    error = %cleanup_err,
                                    "prepared destination cleanup failed during Move recovery; keeping source stopped"
                                );
                                false
                            }
                        };
                        let rollback_err = self.rollback_failed_resume(
                            &id,
                            &previous,
                            crate::database::move_source_environment(&operation.operation_id)?.as_ref(),
                            false,
                            cause,
                            executor,
                        )?;
                        let destination_teardown_confirmed = prepared_cleaned
                            && self.state.sessions.get(&id).is_some_and(|record| {
                                record.state != SessionState::Error
                                    && record.target.as_deref() == previous.target.as_deref()
                            });
                        if destination_teardown_confirmed
                            && !operation.cancellation_requested
                            && operation.is_active()
                            && operation.checkpoint_retained()
                        {
                            if let Some(record) = self.state.sessions.get_mut(&id) {
                                record.last_error = previous.last_error.clone();
                                record.updated_at = now();
                                let _ = self.persist_session_state(&id);
                            }
                            tracing::info!(
                                session_id = %id,
                                reference = %operation.operation_id,
                                phase = ?operation.phase,
                                "continuing interrupted cross-target Move after daemon restart"
                            );
                            return Box::pin(self.execute_move(
                                admission,
                                &mut operation,
                                None,
                                executor,
                                manager,
                                MoveSourceRelay::default(),
                            ))
                            .await;
                        }
                        if destination_teardown_confirmed {
                            match self
                                .restart_retained_move_source(
                                    admission, &id, &previous, executor,
                                )
                                .await
                            {
                                Ok(true) => {
                                    operation.recovery_session = None;
                                    operation.source_snapshot = None;
                                }
                                Ok(false) => {}
                                Err(restart_err) => {
                                    tracing::warn!(
                                        session_id = %id,
                                        operation_id = %operation.operation_id,
                                        error = %restart_err,
                                        "could not restart retained Move source after destination rollback"
                                    );
                                }
                            }
                        } else {
                            tracing::warn!(
                                session_id = %id,
                                operation_id = %operation.operation_id,
                                "skipping source restart because destination teardown was not confirmed"
                            );
                        }
                        rollback_err
                    };
                    Err(error)
                }
                MovePhase::Capturing => {
                    if matches!(session.state, SessionState::Closing | SessionState::Destroying) {
                        Box::pin(self.recover_move_source_stop(admission, &mut operation, executor, manager)).await?;
                    }
                    operation.checkpoint = self.state.sessions[&id].checkpoint.clone().map(|v| *v);
                    if !operation.retains_source_environment() && self.state.sessions[&id].state == SessionState::Stopped && self.state.sessions[&id].target.is_some() {
                        self.cleanup_stopped_target(&id, executor)?;
                    }
                    if !operation.in_place
                        && operation.is_active()
                        && !operation.cancellation_requested
                        && operation.checkpoint_retained()
                    {
                        return Box::pin(self.execute_move(
                            admission,
                            &mut operation,
                            None,
                            executor,
                            manager,
                            MoveSourceRelay::default(),
                        ))
                        .await;
                    }
                    bail!("{}", interrupted_source_stop_message(
                        "source stop was recovered; verified checkpoint retained. Retry move or Resume with previous settings",
                        operation.in_place,
                    ))
                }
                MovePhase::DispatchReleased => {
                    let mut relay = MoveSourceRelay::lease(admission, manager, &id).await?;
                    let conn = relay
                        .0
                        .as_mut()
                        .context(
                            "in-place Move dispatch hold release requires a live worker connection",
                        )?
                        .connection_mut();
                    if let Some(expected_store_id) = operation.in_place_expected_store_id.clone()
                        && conn.snapshot().operational.store_id.as_deref()
                            == Some(expected_store_id.as_str())
                    {
                        conn.client
                            .release_in_place_move_dispatch(
                                &operation.operation_id,
                                &expected_store_id,
                            )
                            .await?;
                    }
                    operation.phase = MovePhase::Completed;
                    operation.queue_admission_finished = true;
                    operation.updated_at = now();
                    crate::database::save_move_operation(&operation)?;
                    Ok(())
                }
                _ => bail!("Move requires an explicit retry after the daemon restarted"),
            }
        }.await;
        self.finish_move_result_with_subagent_recovery(admission,
            &mut operation,
            result,
            executor,
            manager,
        )
        .await

        }).await
    }

    /// Finish an in-place Move whose daemon stopped after the source was
    /// sealed: right after the seal, before the swap, or during it. First give
    /// the record the shape a failed attempt leaves (`Error`, source identity,
    /// worker stopped, environment retained), then run the same restore the
    /// manual retry runs from that shape. A failure of this attempt ends the
    /// Move as `Failed`, where Destroy and a manual retry still work, and a
    /// `Failed` Move is not recovered again, so each interruption gets one
    /// automatic attempt.
    async fn retry_interrupted_in_place_move(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        operation: &mut MoveOperation,
        previous: &mj_core::state::SessionRecord,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let id = operation.selection.session_id.clone();
        // Record the destination phase before the record leaves `Closing`, so
        // a daemon that stops during this retry leaves a state that startup
        // retries again: `ClosingSource` with an `Error` record does not.
        operation.phase = MovePhase::Restoring;
        operation.recovery_session = Some(previous.clone());
        operation.updated_at = now();
        crate::database::save_move_operation(operation)?;
        self.retain_failed_in_place_move(
            &id,
            previous,
            anyhow::anyhow!("the daemon stopped during this in-place Move; retrying it"),
        )?;
        tracing::info!(
            session_id = %id,
            reference = %operation.operation_id,
            phase = ?operation.phase,
            "retrying an in-place Move the daemon stopped after sealing its source"
        );
        Box::pin(self.execute_move(
            admission,
            operation,
            None,
            executor,
            manager,
            MoveSourceRelay::default(),
        ))
        .await
    }

    async fn recover_move_source_stop(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let id = operation.selection.session_id.clone();
        if self.state.sessions[&id].state == SessionState::Closing {
            self.prepare_move_source_checkpoint(
                admission,
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
            let mut lease = handle.lease_connection(admission).await?;
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
                        admission,
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
        self.recover_interrupted_close_managed(
            admission,
            &id,
            executor,
            manager,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            super::lifecycle::SuspendSettlement::TemporaryStop,
        )
        .await?;
        Ok(())
    }

    /// `source_relay` is the source connection the Move confirmed against,
    /// held until the checkpoint that seals the source takes it over.
    pub async fn execute_move(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        operation: &mut MoveOperation,
        preparation: Option<&MovePreparation>,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        mut source_relay: MoveSourceRelay,
    ) -> Result<()> {
        let retained = source_relay.owner();
        let admission_id = operation.selection.session_id.clone();
        crate::worker_lifecycle::run_with_owner(admission, &admission_id, "execute move", executor, retained, async {
        let id = operation.selection.session_id.clone();
        let preparation = preparation.cloned();
        ensure!(
            !executor.cancellation_requested(),
            "move cancelled before source interruption"
        );
        if !operation.queue_admission_started {
            let source_snapshot_missing =
                retained_move_snapshot_is_missing(operation, &self.config, executor)?;
            // A retry restores from the archive this record names. When the
            // file is gone, record that before touching the session, so the
            // failure publishes what is really retained and the session stays
            // in the state Destroy can act on.
            if !source_snapshot_missing && forget_missing_move_archives(operation) {
                operation.updated_at = now();
                crate::database::save_move_operation(operation)?;
                bail!("the Move's checkpoint archive is missing; nothing is left to restore");
            }
            if operation.in_place {
                ensure!(
                    operation.source_target.is_some()
                        && self.state.sessions[&id].target.as_deref() == operation.source_target.as_ref(),
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
                } else {
                    let failure =
                        self.rollback_failed_resume(&id, previous, crate::database::move_source_environment(&operation.operation_id)?.as_ref(), false, cause, executor)?;
                    ensure!(
                        matches!(self.state.sessions[&id].state, SessionState::Stopped | SessionState::Closing),
                        "{failure:#}"
                    );
                }
            }
            if source_snapshot_missing {
                operation.source_snapshot = None;
                operation.checkpoint = None;
                let previous = operation.recovery_session.clone().expect("retained source");
                if self.state.sessions[&id].state == SessionState::Closing
                    && self.state.sessions[&id].target == previous.target
                    && self.restart_retained_move_source(admission, &id, &previous, executor).await?
                {
                    operation.recovery_session = None;
                    bail!("The Move snapshot is missing. The source is running again; prepare a fresh Move to capture it.");
                }
                bail!("The Move snapshot is missing; the source environment was retained.");
            }
            let state = self.state.sessions[&id].state;
            if matches!(state, SessionState::Closing | SessionState::Destroying)
                && !(operation.retains_source_environment() && operation.recovery_session.is_some())
            {
                Box::pin(self.recover_move_source_stop(admission, operation, executor, manager)).await?;
                operation.checkpoint = self.state.sessions[&id].checkpoint.clone().map(|v| *v);
            }
            if operation.in_place {
                // Same-target Move uses staged reload (Relay Protocol 91): never seal the relay, never take a handoff
                // archive, and never stop the worker process tree.
                if !source_relay.is_held() {
                    source_relay = MoveSourceRelay::lease(admission, manager, &id).await?;
                }
                let protocol_version = source_relay
                    .0
                    .as_mut()
                    .context("in-place Move requires a live worker connection")?
                    .connection_mut()
                    .protocol_version();
                in_place::ensure_worker_supports_in_place_move(
                    &id,
                    protocol_version,
                    operation.quota_stop_reason.is_some(),
                )?;
                let dest_profile_id = operation
                    .selection
                    .profile_id
                    .clone()
                    .unwrap_or_else(|| operation.source_profile_id.clone());
                let dest_profile = self.config.named_profile(&dest_profile_id)?.clone();
                let source_has_request_queue = self.source_has_request_queue(&id);
                let destination_subagents = operation
                    .selection
                    .subagents
                    .clone()
                    .or_else(|| self.state.sessions.get(&id).and_then(|s| s.subagents.clone()))
                    .unwrap_or_default();
                let subagent = if self.state.subagents.contains_key(&id) {
                    crate::database::load_subagent(&id)?
                } else {
                    None
                };
                let destination_has_parent_role = destination_subagents
                    .for_launch(dest_profile.kind, subagent.as_ref())
                    .parent_role()
                    .is_some();

                let mut closed_subagent_admission = false;
                if !operation.in_place_harness_restarted {
                    operation.phase = MovePhase::Capturing;
                    operation.updated_at = now();
                    crate::database::save_move_operation(operation)?;
                    if source_has_request_queue {
                        // Capturing is durable: handoff cancels this wait and
                        // the next daemon resumes it before holding dispatch.
                        executor.begin_resumable_work()?;
                        let drained = source_relay.drain_subagent_mutations(&id, executor).await;
                        executor.end_resumable_work("Move control transition")?;
                        match drained? {
                            SubagentMutationDrain::Drained => {
                                closed_subagent_admission = true;
                            }
                            SubagentMutationDrain::UnsupportedWorkerProtocol(version) => {
                                bail!(
                                    "in-place Move requires a worker that can drain sub-agent requests; source relay protocol is {version}"
                                );
                            }
                        }
                    }
                    if should_stop_move_subagents(true, destination_has_parent_role) {
                        let current = crate::database::load_state()?;
                        let stopping_children = !current.active_borrowers(&id).is_empty();
                        let error = if stopping_children && operation.quota_stop_reason.is_some() {
                            Some("automatic quota Move would stop existing sub-agents".into())
                        } else if stopping_children
                            && operation
                                .accepted_preparation
                                .as_ref()
                                .is_some_and(|accepted| !accepted.stops_subagents)
                        {
                            Some(
                                "sub-agents appeared after preparation; prepare and confirm Move again before stopping them"
                                    .into(),
                            )
                        } else {
                            roleless_move_children_error(&current, &id)
                        };
                        if let Some(error) = error {
                            if closed_subagent_admission {
                                source_relay
                                    .reopen_subagent_mutations(&id)
                                    .await
                                    .context(
                                        "could not reopen sub-agent requests after refusing Move",
                                    )?;
                            }
                            bail!(
                                "{error}; close or finish those children before moving to this harness"
                            );
                        }
                        let stop_confirmed = operation.quota_stop_reason.is_none()
                            && operation
                                .accepted_preparation
                                .as_ref()
                                .is_none_or(|accepted| accepted.stops_subagents);
                        if stop_confirmed
                            && let Err(error) = executor.stop_borrowed_workers_before_quiesce().await
                        {
                            if closed_subagent_admission {
                                source_relay
                                    .reopen_subagent_mutations(&id)
                                    .await
                                    .context(
                                        "could not reopen sub-agent requests after stopping children failed",
                                    )?;
                            }
                            return Err(error);
                        }
                    }
                }

                let conn = source_relay
                    .0
                    .as_mut()
                    .context("in-place Move requires a live worker connection")?
                    .connection_mut();
                let live_store_id = conn
                    .snapshot()
                    .operational
                    .store_id
                    .clone()
                    .context("in-place Move worker does not expose a durable store_id")?;
                let had_prior_expected_store_id = operation.in_place_expected_store_id.is_some();
                let expected_store_id = match operation.in_place_expected_store_id.clone() {
                    Some(recorded) => {
                        if live_store_id != recorded {
                            let unapplied = match operation.queue {
                                ResumeQueueDisposition::Discard => {
                                    "discarding held pending commands"
                                }
                                ResumeQueueDisposition::Start => {
                                    "submitting continuation and resuming queue"
                                }
                            };
                            if closed_subagent_admission {
                                let _ = conn.set_subagent_admission(true).await;
                            }
                            bail!(
                                "in-place Move could not safely apply its queue decision ({unapplied}): \
                                 relay store changed from {recorded} to {live_store_id}"
                            );
                        }
                        recorded
                    }
                    None => {
                        operation.in_place_expected_store_id = Some(live_store_id.clone());
                        operation.updated_at = now();
                        crate::database::save_move_operation(operation)?;
                        live_store_id
                    }
                };
                let held_ids = match conn
                    .client
                    .hold_in_place_move_dispatch(&operation.operation_id, &expected_store_id)
                    .await
                {
                    Ok(ids) => ids,
                    Err(err) => {
                        if !had_prior_expected_store_id && !operation.in_place_harness_restarted {
                            operation.in_place_expected_store_id = None;
                            operation.in_place_held_command_ids.clear();
                            operation.updated_at = now();
                            let _ = crate::database::save_move_operation(operation);
                        }
                        if closed_subagent_admission {
                            let _ = conn.set_subagent_admission(true).await;
                        }
                        return Err(err);
                    }
                };
                let source_session = self.state.sessions[&id].clone();
                let mut candidate_id = None;
                let mut reload_progress = if operation.in_place_harness_restarted { InPlaceReloadProgress::Started } else if had_prior_expected_store_id { InPlaceReloadProgress::Unknown } else { InPlaceReloadProgress::BeforeCommit };
                let post_hold: Result<()> = async {
                    if operation.in_place_held_command_ids.is_empty() && !held_ids.is_empty() {
                        operation.in_place_held_command_ids = held_ids;
                        operation.updated_at = now();
                        crate::database::save_move_operation(operation)?;
                    }
                    ensure!(
                        !executor.cancellation_requested(),
                        "in-place Move cancelled after holding dispatch"
                    );
                    ensure!(
                        !executor.shutdown_interrupted(),
                        "in-place Move interrupted by daemon shutdown after holding dispatch"
                    );
                    if had_prior_expected_store_id && !operation.in_place_harness_restarted {
                        let status = conn.client.status().await?;
                        if let Some(hold) = status.in_place_move_hold.filter(|hold| hold.operation_id == operation.operation_id) {
                            use mj_core::relay::InPlaceReloadPhase;
                            match hold.reload_phase {
                                Some(InPlaceReloadPhase::Started) => reload_progress = InPlaceReloadProgress::Started,
                                Some(InPlaceReloadPhase::FailedAfterCommit { message }) => {
                                    let aws_profile = self.committed_in_place_launch(operation, executor)?.and_then(|launch| launch.environment.get("AWS_PROFILE").cloned());
                                    self.settle_in_place_reload_failure(operation, CommittedReloadFailure { message: message.clone(), aws_profile })?;
                                    reload_progress = InPlaceReloadProgress::FailedAfterCommit;
                                    bail!("{message}");
                                }
                                Some(InPlaceReloadPhase::Committed) => bail!("in-place harness reload already committed and is starting; retry to reconnect"),
                                None if hold.harness_restarted => reload_progress = InPlaceReloadProgress::Started,
                                None => reload_progress = InPlaceReloadProgress::BeforeCommit,
                            }
                        } else { bail!("in-place Move worker lost its reload hold; keeping credential synchronization fenced"); }
                    }
                    if !operation.in_place_harness_restarted {
                        if reload_progress != InPlaceReloadProgress::Started {
                        let source_harness = self
                            .state
                            .sessions
                            .get(&id)
                            .map(|session| session.harness_kind)
                            .unwrap_or(dest_profile.kind);
                        let mut operational = conn.client.status().await?;
                        if operation.quota_stop_reason.is_some() {
                            ensure!(
                                operational.safe_to_move(
                                    source_harness,
                                    mj_core::activity::MovePurpose::QuotaRetry,
                                ),
                                "automatic quota Move aborted because the session is working"
                            );
                        } else if !operation.acknowledge_interruption {
                            let mut check = operational.clone();
                            check.queued_prompts.clear();
                            check.checkpoint_barrier = None;
                            ensure!(
                                check.safe_to_move(
                                    source_harness,
                                    mj_core::activity::MovePurpose::Manual,
                                ) && !in_place_restart_blocked_by_work(&operational),
                                "active work started after preparation; confirm Move again with interruption acknowledgement"
                            );
                        } else if in_place_restart_blocked_by_work(&operational) {
                            executor.begin_resumable_work()?;
                            let quiesce_result: Result<()> = async {
                                let deadline = tokio::time::Instant::now()
                                    + super::checkpoint::CHECKPOINT_CANCEL_TIMEOUT;
                                let mut cancel_submitted = false;
                                loop {
                                    if !in_place_restart_blocked_by_work(&operational) {
                                        return Ok(());
                                    }
                                    ensure!(
                                        !executor.cancellation_requested(),
                                        "in-place Move cancelled while waiting for active work to stop"
                                    );
                                    ensure!(
                                        !executor.shutdown_interrupted(),
                                        "in-place Move interrupted by daemon shutdown while waiting for active work to stop"
                                    );
                                    if !cancel_submitted
                                        && in_place_restart_has_cancellable_turn(&operational)
                                    {
                                        let cancel_id = crate::session_manager::new_command_id(
                                            "in-place-move-cancel",
                                        )?;
                                        match conn
                                            .client
                                            .submit(
                                                cancel_id,
                                                mj_core::relay::RelayCommand::CancelTurn,
                                            )
                                            .await
                                        {
                                            Ok(_) => {
                                                cancel_submitted = true;
                                            }
                                            Err(err) => {
                                                let fresh = conn.client.status().await?;
                                                if !in_place_restart_blocked_by_work(&fresh) {
                                                    return Ok(());
                                                }
                                                return Err(err.context(
                                                    "cancel active turn before in-place Move",
                                                ));
                                            }
                                        }
                                    }
                                    if tokio::time::Instant::now() >= deadline {
                                        bail!(
                                            "active work did not stop in time for in-place Move; retry once it settles"
                                        );
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    operational = conn.client.status().await?;
                                }
                            }
                            .await;
                            if let Err(resume_err) =
                                executor.end_resumable_work("stage in-place Move profile")
                            {
                                return Err(quiesce_result.err().unwrap_or(resume_err));
                            }
                            quiesce_result?;
                        }
                        }
                        let previous_session = self
                            .state
                            .sessions
                            .get(&id)
                            .context("move session missing")?
                            .clone();
                        let source_locator = previous_session
                            .target
                            .as_deref()
                            .context("in-place Move session has no target locator")?;
                        let source_backend =
                            super::backend_locator(source_locator, &previous_session, &self.config)?;
                        let source_worker_root = crate::targets::worker_root(&source_backend, &id)?;
                        // Pull any refreshed copied Codex login from the source
                        // staged home into the source profile's canonical store
                        // before overwriting the staged home with destination
                        // profile files (#1638). Skip on retries where destination
                        // files may already be staged on disk.
                        if !had_prior_expected_store_id {
                            self.pull_staged_codex_login(
                                &id,
                                &previous_session,
                                &source_backend,
                                executor,
                            )
                            .await
                            .context("pull source Codex login before in-place Move")?;
                        }
                        let source_harness_kind = self
                            .config
                            .named_profile(&operation.source_profile_id)
                            .map(|profile| profile.kind)
                            .unwrap_or(previous_session.harness_kind);
                        let native_continuity = operation.destination_native_session_id.is_some();
                        let utility_handoff = if !native_continuity {
                            let materialized = crate::database::load_materialized_session(&id)?
                                .context(
                                    "cross-harness in-place Move session projection is missing",
                                )?;
                            let canonical = mj_transcript::projection::canonical_session_from_materialized_owned(
                                materialized,
                            )?;
                            let canonical = std::sync::Arc::new(canonical);
                            let native_prior = if !had_prior_expected_store_id {
                                super::resume::native_prior_from_target_while_cancellable(
                                    &id,
                                    &source_backend,
                                    source_harness_kind,
                                    previous_session.native_session_id.as_deref(),
                                    &canonical,
                                    executor,
                                )
                                .await?
                            } else {
                                None
                            };
                            let context_bytes =
                                crate::handoff::profile_handoff_bytes(Some(&dest_profile));
                            Some(
                                super::resume::utility_handoff_while_cancellable(
                                    &id,
                                    &self.config,
                                    &canonical,
                                    native_prior.as_ref(),
                                    context_bytes,
                                    executor,
                                    tokio_util::sync::CancellationToken::new(),
                                )
                                .await
                                .context("prepare cross-harness in-place handoff")?,
                            )
                        } else {
                            None
                        };
                        self.apply_in_place_destination(operation)?;
                        if reload_progress == InPlaceReloadProgress::Started {
                            if let Some(launch) = self.committed_in_place_launch(operation, executor)? {
                                self.state.sessions.get_mut(&id).unwrap().staged_aws_profile = launch.environment.get("AWS_PROFILE").cloned();
                            }
                        } else {
                            let candidate = mj_core::relay::new_command_id("reload-candidate")?;
                            candidate_id = Some(candidate.clone());
                            let staged_launch: Result<mj_core::worker_launch::WorkerLaunchConfig> = (|| {
                                let staged = self.prepare_worker_files_for(
                                    &id, &source_backend, &source_worker_root, executor,
                                    super::worker_binary::launch::WorkerFilePreparation::StageCandidate {
                                        operation_id: operation.operation_id.clone(), expected_store_id: expected_store_id.clone(), candidate_id: candidate.clone(),
                                    }, super::worker_binary::launch::LaunchHistory::Stored,
                                )?;
                                self.state.sessions.get_mut(&id).unwrap().staged_aws_profile = staged.aws_profile;
                                self.current_worker_launch_config(&id, &source_backend)
                            })();
                            let launch_config = match staged_launch {
                                Ok(launch) => launch,
                                Err(error) => { self.state.sessions.insert(id.clone(), previous_session); return Err(error); }
                            };
                            match conn.client.restart_harness_in_place(&operation.operation_id, &expected_store_id, Box::new(launch_config), Some(candidate)).await {
                                Ok(()) => reload_progress = InPlaceReloadProgress::Started,
                                Err(error) => {
                                    use mj_core::relay::{RelayErrorCode, RelayErrorDetail};
                                    let rejection = error.downcast_ref::<crate::worker_client::RelayRejected>();
                                    let committed_failure = rejection.and_then(|rejected| match &rejected.0.detail {
                                        Some(RelayErrorDetail::ReloadFailedAfterCommit { operation_id }) if operation_id == &operation.operation_id => Some(rejected.0.message.clone()),
                                        _ => None,
                                    });
                                    if let Some(message) = committed_failure {
                                        // A failed database write cannot turn a worker commit
                                        // into a pre-commit refusal or lift its store fence.
                                        reload_progress = InPlaceReloadProgress::Unknown;
                                        let aws_profile = self.state.sessions[&id].staged_aws_profile.clone();
                                        self.settle_in_place_reload_failure(operation, CommittedReloadFailure { message, aws_profile })?;
                                        reload_progress = InPlaceReloadProgress::FailedAfterCommit;
                                    } else if rejection.is_some_and(|rejected| rejected.0.code == RelayErrorCode::InvalidState && rejected.0.detail.is_none()) {
                                        reload_progress = InPlaceReloadProgress::BeforeCommit;
                                        self.state.sessions.insert(id.clone(), previous_session);
                                    } else if rejection.is_some_and(|rejected| matches!(&rejected.0.detail, Some(RelayErrorDetail::ReloadInProgress { operation_id }) if operation_id == &operation.operation_id)) {
                                        reload_progress = InPlaceReloadProgress::Unknown;
                                        self.state.sessions.insert(id.clone(), previous_session);
                                    } else {
                                        reload_progress = InPlaceReloadProgress::Unknown;
                                        match self.committed_in_place_failure(operation, executor) {
                                            Ok(Some(failure)) => {
                                                self.settle_in_place_reload_failure(operation, failure)?;
                                                reload_progress = InPlaceReloadProgress::FailedAfterCommit;
                                            }
                                            Ok(None) => { self.state.sessions.insert(id.clone(), previous_session); }
                                            Err(probe_error) => {
                                                tracing::warn!(session_id = %id, %probe_error, "could not prove the in-place reload outcome");
                                                self.state.sessions.insert(id.clone(), previous_session);
                                            }
                                        }
                                    }
                                    return Err(error);
                                }
                            }
                        }
                        crate::database::save_resumed_session(&self.state.sessions[&id], None)?;
                        // Install cross-harness handoff context BEFORE marking
                        // `in_place_harness_restarted = true`: if context
                        // installation fails or crashes, retry sees
                        // `AlreadyRestarted` from the worker and completes the
                        // context installation cleanly.
                        if let Some(handoff) = utility_handoff {
                            conn.client.install_prompt_context(handoff).await?;
                        }
                        operation.in_place_harness_restarted = true;
                        operation.updated_at = now();
                        crate::database::save_move_operation(operation)?;
                    }
                    match operation.queue {
                        ResumeQueueDisposition::Discard => {
                            if !operation.in_place_held_command_ids.is_empty() {
                                conn.client
                                    .discard_held_move_commands(
                                        &operation.operation_id,
                                        &expected_store_id,
                                        operation.in_place_held_command_ids.clone(),
                                    )
                                    .await?;
                            }
                        }
                        ResumeQueueDisposition::Start => {
                            if operation.quota_stop_reason.is_some() {
                                use mj_core::state::QuotaMoveContinuation;
                                let continuation = operation
                                    .quota_continuation
                                    .context("quota Move has no saved continuation action")?;
                                let cmd_id = operation
                                    .in_place_continuation_command_id
                                    .clone()
                                    .unwrap_or_else(|| {
                                        mj_core::continuation::GeneratedPrompt::QuotaRetry
                                            .command_id(&operation.operation_id)
                                    });
                                let cmd = match continuation {
                                    QuotaMoveContinuation::ResumeGoal => {
                                        mj_core::relay::RelayCommand::GoalControl {
                                            action: mj_core::goal::GoalControlAction::Resume,
                                        }
                                    }
                                    QuotaMoveContinuation::Prompt => {
                                        mj_core::relay::RelayCommand::Prompt {
                                            prompt: mj_core::continuation::prompt_blocks(),
                                        }
                                    }
                                };
                                conn.client
                                    .submit_front(
                                        &operation.operation_id,
                                        &expected_store_id,
                                        cmd_id,
                                        cmd,
                                    )
                                    .await?;
                            }
                        }
                    }
                    operation.phase = MovePhase::DispatchReleased;
                    operation.queue_admission_started = true;
                    operation.updated_at = now();
                    crate::database::commit_in_place_dispatch_released(operation)?;
                    conn.client
                        .release_in_place_move_dispatch(&operation.operation_id, &expected_store_id)
                        .await?;
                    if closed_subagent_admission {
                        conn.set_subagent_admission(true).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(err) = post_hold {
                    if matches!(reload_progress, InPlaceReloadProgress::BeforeCommit | InPlaceReloadProgress::FailedAfterCommit)
                        || (reload_progress == InPlaceReloadProgress::Started && operation.in_place_harness_restarted)
                    {
                        if reload_progress == InPlaceReloadProgress::BeforeCommit {
                            self.state.sessions.insert(id.clone(), source_session.clone());
                            if let Some(candidate) = &candidate_id {
                                let cleanup = (|| -> Result<()> {
                                    let backend = super::backend_locator(source_session.target.as_deref().context("Move target missing")?, &source_session, &self.config)?;
                                    let root = crate::targets::worker_root(&backend, &id)?;
                                    super::worker_binary::remove_reload_candidate(executor, &backend, &root, candidate)
                                })();
                                if let Err(error) = cleanup {
                                    tracing::warn!(session_id = %id, %error, "could not remove refused reload candidate");
                                }
                            }
                        }
                        let expected_restarted = match reload_progress {
                            InPlaceReloadProgress::BeforeCommit => Some(false),
                            InPlaceReloadProgress::Started => Some(true),
                            InPlaceReloadProgress::FailedAfterCommit => None,
                            InPlaceReloadProgress::Unknown => unreachable!(),
                        };
                        let hold_released = match conn.client.release_in_place_move_dispatch_checked(&operation.operation_id, &expected_store_id, expected_restarted).await {
                            Ok(()) => true,
                            Err(error) => {
                                tracing::warn!(session_id = %id, %error, "could not release in-place Move hold after settlement");
                                false
                            }
                        };
                        if closed_subagent_admission { let _ = conn.set_subagent_admission(true).await; }
                        if reload_progress == InPlaceReloadProgress::FailedAfterCommit
                            || (hold_released && (reload_progress == InPlaceReloadProgress::BeforeCommit
                                || (reload_progress == InPlaceReloadProgress::Started && operation.in_place_harness_restarted)))
                        {
                            operation.in_place_expected_store_id = None;
                            operation.in_place_held_command_ids.clear();
                        }
                        operation.updated_at = now();
                        crate::database::save_move_operation(operation)?;
                    }
                    return Err(err);
                }
                operation.in_place_expected_store_id = None;
                operation.in_place_held_command_ids.clear();
                operation.phase = MovePhase::Completed;
                operation.queue_admission_finished = true;
                operation.updated_at = now();
                crate::database::save_move_operation(operation)?;
                for change in &operation.deferred_environment_changes {
                    executor.notify_notice(&in_place::deferred_environment_change_notice(change));
                }
                return Ok(());
            }
            if matches!(
                self.state.sessions[&id].state,
                SessionState::Running | SessionState::Disconnected
            ) && operation.destination_target.is_none()
            {
                // Cross-target Move stops child sessions before source capture.
                executor.begin_resumable_work()?;
                let stopped_children = executor.stop_borrowed_workers_before_quiesce().await;
                executor.end_resumable_work("start Move source capture")?;
                stopped_children?;
                ensure!(
                    !executor.cancellation_requested(),
                    "Move cancelled before source interruption"
                );
                executor.notify_notice("Stopping source");
                let _timing = MovePhaseTimer::new(&id, "checkpoint and source stop");
                operation.phase = MovePhase::Capturing;
                operation.updated_at = now();
                crate::database::save_move_operation(operation)?;
                // Cross-target Move keeps the source environment intact until
                // AFTER `DestinationOwns` commits, so any failure before
                // destination readiness can tear down the destination and
                // restart the source agent in place.
                let disposition = SourceTargetDisposition::RetainForInPlaceSwap;
                let suspended = Box::pin(self.suspend_session_for_move(
                    admission,
                    &id,
                    executor,
                    manager,
                    operation,
                    preparation.as_ref(),
                    disposition,
                    std::mem::take(&mut source_relay),
                ))
                .await;
                suspended?;
            }
            // Past the source stop, nothing else needs the source's connection.
            drop(source_relay);
            if operation.source_snapshot.is_none() {
                operation.checkpoint = operation
                    .checkpoint
                    .clone()
                    .or_else(|| self.state.sessions[&id].checkpoint.clone().map(|v| *v));
            }
            ensure!(
                operation.checkpoint_retained(),
                "move has no verified snapshot"
            );
            if operation.recovery_session.is_none() {
                operation.recovery_session = Some(self.state.sessions[&id].clone());
            }
            ensure!(
                !executor.cancellation_requested(),
                "move cancelled after source capture; snapshot and source environment retained"
            );
            operation.phase = MovePhase::Transferring;
            operation.updated_at = now();
            crate::database::save_move_operation(operation)?;
            executor.reserve_move_destination();
            executor.notify_notice("Preparing destination");
            // Provision from this attempt's admitted choices, not settings
            // edited while the source was checkpointing or rolling back.
            let session = self.state.sessions.get_mut(&id).unwrap();
            session.resource_allocation = if operation.selection.clear_resource_allocation {
                None
            } else {
                operation
                    .selection
                    .resource_allocation
                    .clone()
                    .or_else(|| operation.inherited_resource_allocation.clone())
            };
            session.additional_mounts = operation
                .selection
                .additional_mounts
                .clone()
                .unwrap_or_else(|| operation.inherited_additional_mounts.clone());
            // The destination worker reads its delegation policy from the
            // record at launch. `recovery_session` above keeps the old one
            // for a rollback.
            if operation.selection.subagents.is_some()
                || operation.selection.oracle.is_some()
                || operation.selection.oracle_choice.is_some()
            {
                let session = self.state.sessions.get_mut(&id).unwrap();
                if let Some(policy) = &operation.selection.subagents {
                    session.subagents = Some(policy.clone());
                }
                if let Some(oracle) = operation.selection.oracle {
                    session.oracle = oracle;
                }
                if let Some(choice) = &operation.selection.oracle_choice {
                    session.oracle_choice = Some(Box::new(choice.clone()));
                }
                crate::database::save_resumed_session(session, None)?;
            }
            executor.begin_resumable_work()?;
            let restore_result =
                Box::pin(self.resume_session_for_cross_target_move(operation, executor)).await;
            executor.end_resumable_work("commit Move destination ownership")?;
            if let Err(err) = restore_result {
                // Before DestinationOwns, the source still owns the session.
                // Tear down any prepared destination too, and ONLY restart the
                // source agent in place once destination teardown is confirmed
                // so two workers can never run concurrently.
                executor.begin_resumable_work()?;
                let cleanup_result = self.cleanup_prepared_move_destination(operation, executor);
                executor.end_resumable_work("record Move rollback outcome")?;
                let prepared_cleaned = match cleanup_result {
                    Ok(()) => true,
                    Err(cleanup_err) => {
                        tracing::warn!(
                            session_id = %id,
                            operation_id = %operation.operation_id,
                            error = %cleanup_err,
                            "prepared destination cleanup failed after cross-target Move error; keeping source stopped"
                        );
                        false
                    }
                };
                if let Some(previous) = operation.recovery_session.clone() {
                    let destination_teardown_confirmed = prepared_cleaned
                        && self.state.sessions.get(&id).is_some_and(|record| {
                            record.state != SessionState::Error
                                && record.target.as_deref() == previous.target.as_deref()
                        });
                    if destination_teardown_confirmed {
                        match self
                            .restart_retained_move_source(
                                admission, &id, &previous, executor,
                            )
                            .await
                        {
                            Ok(true) => {
                                operation.recovery_session = None;
                                operation.source_snapshot = None;
                            }
                            Ok(false) => {}
                            Err(restart_err) => {
                                tracing::warn!(
                                    session_id = %id,
                                    operation_id = %operation.operation_id,
                                    error = %restart_err,
                                    "could not restart source agent in place after failed cross-target Move"
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            session_id = %id,
                            operation_id = %operation.operation_id,
                            "skipping source restart because destination teardown was not confirmed"
                        );
                    }
                }
                return Err(err);
            }
            let destination = self.state.sessions[&id].clone();
            operation.destination_target = destination.target.clone().map(|v| *v);
            operation.destination_native_session_id = destination.native_session_id.clone();
            // Single-transaction commit point: DestinationOwns + session placement.
            operation.phase = MovePhase::DestinationOwns;
            operation.queue_admission_started = true;
            operation.updated_at = now();
            crate::database::commit_move_destination_owns(operation, &destination)?;

            // Retire the source environment ONLY AFTER DestinationOwns has committed.
            self.retire_move_source_if_needed(operation, executor)?;
        }
        restore_move_queue_hold(operation);
        self.admit_move_queue(operation, executor).await?;
        operation.phase = MovePhase::QueueAdmitted;
        operation.updated_at = now();
        crate::database::save_move_operation(operation)?;
        Ok(())

        }).await
    }

    /// Retire the source environment for a Move that has crossed `DestinationOwns`,
    /// persisting `MovePhase::SourceRetired` once done. Idempotent across recovery.
    pub(crate) fn retire_move_source_if_needed(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if matches!(
            operation.phase,
            MovePhase::SourceRetired | MovePhase::QueueAdmitted | MovePhase::Completed
        ) {
            return Ok(());
        }
        let id = &operation.selection.session_id;
        if let Some(source_env) = crate::database::move_source_environment(&operation.operation_id)?
        {
            executor.notify_notice("Retiring source environment");
            let _timing = MovePhaseTimer::new(id, "source environment retirement");
            executor.begin_resumable_work()?;
            let retired = super::environment::retire_owned_environment(
                source_env,
                &self.config,
                executor,
                super::BranchDisposition::Keep,
                super::CheckoutDisposition::Remove,
            );
            executor.end_resumable_work("record Move source retirement")?;
            retired?;
        }
        operation.phase = MovePhase::SourceRetired;
        operation.updated_at = now();
        crate::database::save_move_operation(operation)?;
        Ok(())
    }

    /// Recover a pre-149 `LegacyMoveOperation` BEFORE deleting its durable row:
    /// - `MoveOwnership::Source`: tear down any prepared destination environment,
    ///   recover any `Closing`/`Destroying` source session (reopening only when
    ///   the worker is live, otherwise marking `Error`/`Stopped`), and return a
    ///   rerun notice.
    /// - `MoveOwnership::Destination`: retire any remaining source environment,
    ///   complete pending queue admission when `!queue_admission_finished`, and
    ///   return a finish-forward notice.
    pub async fn recover_legacy_move_operation(
        &mut self,
        session_id: &str,
        legacy: &mj_core::state::LegacyMoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<String> {
        let session = self.state.sessions.get(session_id).cloned();
        match recovery::classify_legacy_move_ownership(legacy, session.as_ref()) {
            recovery::MoveOwnership::Source => {
                if let Some(prepared) = &legacy.prepared_destination
                    && prepared.owns_resource()
                {
                    let mut shim = legacy_shim_move_operation(session_id, legacy, session.as_ref());
                    self.cleanup_prepared_move_destination(&mut shim, executor)?;
                }
                if let Some(record) = self.state.sessions.get(session_id).cloned()
                    && matches!(
                        record.state,
                        SessionState::Closing | SessionState::Destroying
                    )
                {
                    let worker_alive = record.target.as_deref().is_some_and(|locator| {
                        super::backend_locator(locator, &record, &self.config)
                            .and_then(|backend| {
                                let root = crate::targets::worker_root(&backend, session_id)?;
                                super::worker_binary::probe_worker_liveness(
                                    executor, &backend, session_id, &root,
                                )
                            })
                            .is_ok_and(|out| {
                                out.status == 0
                                    && matches!(out.stdout.as_slice(), b"alive\n" | b"starting\n")
                            })
                    });
                    let notice = recovery::legacy_source_rollback_notice(&legacy.operation_id);
                    let mut updated = record;
                    if worker_alive {
                        updated.state = SessionState::Running;
                        updated.last_error = None;
                    } else if updated.target.is_some() {
                        updated.state = SessionState::Error;
                        updated.last_error = Some(notice.clone());
                    } else if updated.checkpoint.is_some() {
                        updated.state = SessionState::Stopped;
                        updated.last_error = Some(notice.clone());
                    } else {
                        updated.state = SessionState::Lost;
                        updated.last_error = Some(notice.clone());
                    }
                    updated.updated_at = now();
                    crate::database::save_resumed_session(&updated, None)?;
                    self.state.sessions.insert(session_id.to_owned(), updated);
                }
                Ok(recovery::legacy_source_rollback_notice(
                    &legacy.operation_id,
                ))
            }
            recovery::MoveOwnership::Destination => {
                if let Some(source_env) =
                    crate::database::move_source_environment(&legacy.operation_id)?
                {
                    super::environment::retire_owned_environment(
                        source_env,
                        &self.config,
                        executor,
                        super::BranchDisposition::Keep,
                        super::CheckoutDisposition::Remove,
                    )?;
                }
                if !legacy.queue_admission_finished
                    && let Some(record) = self.state.sessions.get(session_id)
                    && record.state == SessionState::Running
                {
                    let mut forward = legacy_shim_move_operation(session_id, legacy, Some(record));
                    forward.phase = MovePhase::SourceRetired;
                    forward.queue_admission_started = true;
                    forward.queue_admission_finished = false;
                    forward.destination_target = legacy
                        .destination_target
                        .clone()
                        .or_else(|| record.target.clone().map(|t| *t));
                    forward.destination_native_session_id = legacy
                        .destination_native_session_id
                        .clone()
                        .or_else(|| record.native_session_id.clone());
                    if forward.checkpoint.is_some()
                        || forward.queue == ResumeQueueDisposition::Discard
                    {
                        self.admit_move_queue(&mut forward, executor).await?;
                    }
                }
                Ok(format!(
                    "Finished forward recovery for upgraded Move {} on destination.",
                    legacy.operation_id
                ))
            }
        }
    }

    async fn admit_move_queue(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let timing_id = operation.selection.session_id.clone();
        let _timing = MovePhaseTimer::new(&timing_id, "queue admission");
        let id = &timing_id;
        let mut relay = {
            let _checking_destination =
                ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            let destination = &self.state.sessions[id];
            ensure!(
                destination.state == SessionState::Running
                    && destination.target.as_deref() == operation.destination_target.as_ref()
                    && destination.native_session_id == operation.destination_native_session_id,
                "cannot prove the same ready destination; refusing to replay potentially executed work"
            );
            let spec = self.reconnect_command(id)?;
            let pin_store = |operation: &mut MoveOperation,
                             store_id: Option<String>|
             -> Result<()> {
                let store_id = store_id.context("destination worker does not expose its durable store identity; upgrade the worker before admitting queued work")?;
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
                Ok(())
            };
            // Prove the store before the session connection syncs the
            // projection: a replaced store's older frontier is refused by
            // projection repair (ba25f8468), which would hide this cause.
            let mut probe = crate::worker_client::RelayClient::connect(&spec, id).await?;
            let probed = probe.status().await;
            probe.detach().await?;
            pin_store(operation, probed?.store_id)?;
            let relay = Box::pin(StandaloneSession::connect_command(&spec, id)).await?;
            pin_store(operation, relay.snapshot().operational.store_id.clone())?;
            ensure!(
                relay.snapshot().operational.native_session_id
                    == operation.destination_native_session_id,
                "destination relay native identity changed; refusing queue replay"
            );
            relay
        };
        // The destination's restore left the archived source's pending mail
        // for this point, after the ownership commit: until then a failed Move
        // hands the session back to a source that still holds it.
        let captured = if operation.queue_admission_finished {
            None
        } else if let Some(snapshot) = operation.source_snapshot.as_deref() {
            Some((snapshot.queued_prompts.clone(), snapshot.mailbox.clone()))
        } else if let Some(checkpoint) = operation.restore_artifact() {
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            ensure!(
                verified.archive_sha256 == checkpoint.sha256 && verified.manifest.session.id == *id,
                "move queue checkpoint verification failed"
            );
            Some((
                verified.canonical_session.queued_prompts,
                verified.canonical_session.mailbox,
            ))
        } else if operation.queue == ResumeQueueDisposition::Discard {
            Some((Vec::new(), None))
        } else {
            bail!("move queue snapshot is missing");
        };
        if let Some((queued_prompts, _)) = captured.as_ref()
            && operation.queue == ResumeQueueDisposition::Start
        {
            let _starting_queue = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
            executor.notify_notice("Starting queued work");
            relay
                .update_command_retention(
                    Some(operation.operation_id.clone()),
                    operation.queue_command_ids_from_prompts(queued_prompts),
                    false,
                )
                .await?;
            if operation.quota_stop_reason.is_some() && !operation.queue_admission_finished {
                ensure!(
                    !executor.cancellation_requested(),
                    "move cancelled during queue admission; destination retained"
                );
                use mj_core::state::QuotaMoveContinuation;
                let continuation = operation
                    .quota_continuation
                    .context("quota Move has no saved continuation action")?;
                let command = match continuation {
                    // Move owns the lifecycle, so use ordinary Resume semantics.
                    QuotaMoveContinuation::ResumeGoal => RelayCommand::GoalControl {
                        action: mj_core::goal::GoalControlAction::Resume,
                    },
                    QuotaMoveContinuation::Prompt => RelayCommand::Prompt {
                        prompt: mj_core::continuation::prompt_blocks(),
                    },
                };
                // The destination store is pinned above before any submission.
                // Its durable ledger deduplicates this operation across retries,
                // including a crash after acceptance but before our next save.
                relay
                    .submit_accepted(
                        mj_core::continuation::GeneratedPrompt::QuotaRetry
                            .command_id(&operation.operation_id),
                        command,
                    )
                    .await?;
            }
            for queued in queued_prompts {
                ensure!(
                    !executor.cancellation_requested(),
                    "move cancelled during queue admission; destination retained"
                );
                let command = match &queued.kind {
                    CanonicalQueuedCommandKind::Prompt => RelayCommand::Prompt {
                        prompt: queued
                            .content
                            .iter()
                            .cloned()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?,
                    },
                    CanonicalQueuedCommandKind::SetConfig { key, value } => {
                        RelayCommand::SetConfig {
                            key: key.clone(),
                            value: value.clone(),
                        }
                    }
                };
                relay
                    .submit_accepted(queued.command_id.clone(), command)
                    .await?;
            }
        }
        // Mail joins behind the admitted queue, whichever way the queue went:
        // discarding queued prompts never discards messages sent to the session.
        if let Some((_, mailbox)) = &captured {
            for (command_id, command) in
                super::resume::deferred_mail_commands_from_mailbox(mailbox.as_ref())
            {
                ensure!(
                    !executor.cancellation_requested(),
                    "move cancelled during queue admission; destination retained"
                );
                relay.submit_accepted(command_id, command).await?;
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
        let text = if let Some(reason) = &operation.quota_stop_reason {
            format!(
                "Switched from {source_profile} to {destination_profile} in place because {reason}; the workspace and environment were kept. Continuing the interrupted work. {queue_sentence}"
            )
        } else if operation.in_place {
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
        // Queue completion is already durable. Pin release is cleanup, retried
        // by ordinary attachment from this operation's finished record.
        if let Err(error) = relay
            .update_command_retention(
                Some(operation.operation_id.clone()),
                Default::default(),
                false,
            )
            .await
        {
            tracing::warn!(operation_id = operation.operation_id, session_id = operation.selection.session_id,
                %error, "Move completed; receipt-pin cleanup will retry on attachment");
        }
        Ok(())
    }

    /// A captured Move still clones the recorded origin before unpacking the
    /// transfer. Check it while the source barrier can still be released.
    fn validate_move_bundle_sources(
        &self,
        operation: &MoveOperation,
        manifest: &mj_checkpoint::archive::ArchiveManifest,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let target = operation
            .selection
            .target_template_id
            .as_ref()
            .and_then(|id| self.config.targets.get(id));
        if !target.is_some_and(|target| target.container().is_some()) {
            return Ok(());
        }
        let Some(origin) = manifest
            .repositories
            .iter()
            .map(|repository| repository.metadata.origin.as_str())
            .find(|origin| {
                !origin.is_empty() && mj_core::remote_git::is_host_only_git_source(origin)
            })
        else {
            return Ok(());
        };
        use mj_core::targets::storage::{LOCAL_STORAGE_HOST, session_storage_paths};
        let host = operation.source_target.as_ref().map(|source| {
            session_storage_paths(source, &operation.selection.session_id, None).host
        });
        let host = match host.as_deref() {
            Some(host) if host != LOCAL_STORAGE_HOST => host.to_owned(),
            Some(_) => {
                // Read the name of the engine host, outside the source container.
                let command =
                    crate::targets::CommandSpec::new("hostname", std::iter::empty::<&str>())
                        .purpose("identify the source container host");
                executor
                    .execute(&command)
                    .ok()
                    .filter(|output| output.status == 0)
                    .and_then(|output| String::from_utf8(output.stdout).ok())
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "the local container host".into())
            }
            None => "the source host".into(),
        };
        Err(mj_core::refusal::Refusal::precondition(format!(
            "Git source {} is reachable only from {host}, and Move clones from the source. Keep the existing environment on {host} for an in-place Move, or set the checkout's origin to a URL the destination can reach and retry.",
            mj_core::remote_git::display_url(origin)
        )).into())
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
        let bundle_move = !operation.in_place;
        let mut skip_daemon_checkpoint_preflight = false;
        if bundle_move || preparation.is_some() {
            if let Some(snapshot) = operation.source_snapshot.as_deref() {
                if bundle_move && let Some(manifest) = snapshot.manifest.as_ref() {
                    current.validate_move_bundle_sources(operation, manifest, executor)?;
                    skip_daemon_checkpoint_preflight = manifest.repositories.iter().all(|repo| {
                        repo.metadata.origin_delta.is_some() || repo.metadata.remote_workspace
                    });
                } else {
                    skip_daemon_checkpoint_preflight = true;
                }
                if let Some(prepared) = preparation {
                    let actual: Vec<_> = snapshot
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
            } else {
                let checkpoint = operation
                    .restore_artifact()
                    .or(self.state.sessions[id].checkpoint.as_deref())
                    .context("no move checkpoint")?;
                let verified = verify_archive_streaming(&checkpoint.archive_path)?;
                if bundle_move {
                    current.validate_move_bundle_sources(
                        operation,
                        &verified.manifest,
                        executor,
                    )?;
                }
                if let Some(prepared) = preparation {
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
            }
        }
        current.validate_move_destination_paths(
            &self.state.sessions[id],
            operation.selection.target_template_id.as_deref().unwrap(),
            executor,
        )?;
        if !operation.in_place
            && !skip_daemon_checkpoint_preflight
            && self.state.sessions[id].checkpoint.is_some()
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
        Ok(())
    }
}

/// Stream a cross-target Move archive and optional `.mj/keep/<session-id>/`
/// directory (excluding `operation.selection.skip_keep_paths`) straight from
/// `source` to `destination` via `transport::transfer_move_stream_with_direct`
/// (`SameHost` -> `DirectPush` -> `DirectPull` -> `DaemonStream`), persisting
/// `operation.transfer_route`, reporting the chosen route (plus keep bytes on
/// `DaemonStream`), and cleaning up ephemeral key files on every exit path.
#[allow(clippy::too_many_arguments)]
pub(super) fn transfer_move_snapshot_to_destination(
    operation: &mut MoveOperation,
    source: &mj_core::targets::TargetLocator,
    source_worker_root: &str,
    destination: &mj_core::targets::TargetLocator,
    destination_worker_root: &str,
    destination_archive: &std::path::Path,
    source_keep_dir: Option<&std::path::Path>,
    destination_keep_dir: Option<&std::path::Path>,
    executor: &(impl CommandExecutor + Sync),
) -> Result<transport::TransferReceipt> {
    let snapshot = operation
        .source_snapshot
        .as_deref()
        .context("cross-target Move has no source snapshot to transfer")?;
    let source_archive = snapshot.archive_path.clone();
    let sha256 = snapshot.sha256.clone();

    let keep = transport::KeepTransferOptions {
        keep_dir: source_keep_dir.map(std::path::Path::to_path_buf),
        keep_dest: destination_keep_dir.map(std::path::Path::to_path_buf),
        skip: operation.selection.skip_keep_paths.clone(),
    };
    let stream_cfg = transport::WorkerStreamConfig {
        source_worker_bin: format!("{source_worker_root}/hel"),
        destination_worker_bin: format!("{destination_worker_root}/hel"),
        keep: keep.clone(),
    };

    let direct_peer = if transport::targets_share_host(source, destination) {
        None
    } else {
        try_build_direct_peer_config(
            &operation.operation_id,
            source,
            source_worker_root,
            destination,
            destination_worker_root,
            keep,
            executor,
            &transport::ec2_grant::AwsCliEc2Client::default(),
            &mj_core::ec2_records::lookup_ec2_host_for_locator,
        )
    };
    // Keys appended below belong to this attempt; anything before them was
    // carried over from an earlier interrupted attempt.
    let attempt_keys_start = operation.granted_keys.len();
    if let Some(peer) = direct_peer.as_ref() {
        // Persist ephemeral key ownership BEFORE installing keys on either host,
        // preserving any carried-over unrevoked keys from earlier moves.
        let comment = format!("mj-move-{}", peer.operation_id);
        let src_ssh = direct_ssh_endpoint(source, source_worker_root).map(|(ssh, _)| ssh.clone());
        let dst_ssh =
            direct_ssh_endpoint(destination, destination_worker_root).map(|(ssh, _)| ssh.clone());
        operation.granted_keys.extend([
            mj_core::state::GrantedMoveKey {
                host: peer.source_address.clone(),
                comment: comment.clone(),
                private_key_path: Some(
                    peer.source_stage_dir
                        .join(format!("mj-move-{}.key", peer.operation_id)),
                ),
                known_hosts_path: Some(
                    peer.source_stage_dir
                        .join(format!("mj-move-{}.known_hosts", peer.operation_id)),
                ),
                authorized_keys_path: Some(peer.source_authorized_keys.clone()),
                worker_bin: Some(peer.source_host_worker_bin.clone()),
                ssh_target: src_ssh,
            },
            mj_core::state::GrantedMoveKey {
                host: peer.destination_address.clone(),
                comment,
                private_key_path: Some(
                    peer.destination_stage_dir
                        .join(format!("mj-move-{}.pull.key", peer.operation_id)),
                ),
                known_hosts_path: Some(
                    peer.destination_stage_dir
                        .join(format!("mj-move-{}.pull.known_hosts", peer.operation_id)),
                ),
                authorized_keys_path: Some(peer.destination_authorized_keys.clone()),
                worker_bin: Some(peer.destination_host_worker_bin.clone()),
                ssh_target: dst_ssh,
            },
        ]);
        operation.updated_at = now();
        crate::database::save_move_operation(operation)?;
    }

    let ec2_manager = direct_peer
        .as_ref()
        .filter(|peer| peer.destination_ec2_grant.is_some() || peer.source_ec2_grant.is_some())
        .and_then(|_| crate::database::daemon_instance_id().ok())
        .map(|instance_id| {
            transport::ec2_grant::Ec2GrantManager::new(
                transport::ec2_grant::SqliteEc2GrantStore,
                transport::ec2_grant::AwsCliEc2Client::default(),
                instance_id,
                transport::ec2_grant::Ec2GrantConfig::default(),
            )
        });

    let result = transport::transfer_move_stream_with_direct(
        source,
        &source_archive,
        destination,
        destination_archive,
        &sha256,
        executor,
        Some(&stream_cfg),
        direct_peer.as_ref(),
        ec2_manager.as_ref(),
    );
    let attempt_removed = drop_credentials_removed_by_transfer(
        &mut operation.granted_keys,
        attempt_keys_start,
        direct_peer.as_ref(),
    );
    if attempt_removed || !operation.granted_keys.is_empty() {
        if !operation.granted_keys.is_empty() {
            operation.granted_keys = transport::cleanup_interrupted_move_credentials(
                &operation.operation_id,
                &operation.granted_keys,
                &[source, destination],
                executor,
            );
        }
        operation.updated_at = now();
        let _ = crate::database::save_move_operation(operation);
        if !operation.granted_keys.is_empty() {
            // Host was transiently unreachable; schedule non-admitted
            // background retry so revocation completes even after the Move
            // settles as Completed or Failed.
            recovery::spawn_move_key_cleanup_background();
        }
    }
    let receipt = result?;
    operation.transfer_route = Some(receipt.route);
    operation.updated_at = now();
    crate::database::save_move_operation(operation)?;
    let notice = if receipt.route == mj_core::state::TransferRoute::DaemonStream {
        format!(
            "Transferred snapshot via {} (.mj/keep/: {})",
            receipt.route.label(),
            format_transfer_bytes(receipt.keep_bytes),
        )
    } else {
        format!("Transferred snapshot via {}", receipt.route.label())
    };
    executor.notify_notice(&notice);
    Ok(receipt)
}

/// Forget the keys this attempt recorded (those from `attempt_keys_start`) when
/// the transfer's own cleanup reports it removed them all, which it also does
/// after a failed transfer. Running the recovery cleanup over them would only
/// repeat its SSH calls. Keys carried over from earlier attempts stay.
fn drop_credentials_removed_by_transfer(
    granted_keys: &mut Vec<mj_core::state::GrantedMoveKey>,
    attempt_keys_start: usize,
    peer: Option<&transport::DirectPeerConfig>,
) -> bool {
    let removed =
        peer.is_some_and(|peer| peer.cleanup.is_clean()) && granted_keys.len() > attempt_keys_start;
    if removed {
        granted_keys.truncate(attempt_keys_start);
    }
    removed
}

fn format_transfer_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

/// Probe verified peer SSH host keys and paths over the daemon's already-trusted
/// SSH connections to build a `DirectPeerConfig` for `DirectPush` / `DirectPull`.
/// Returns `None` when either endpoint is local, is a Docker or Apple container
/// target, or lacks a runnable host-side worker binary for `authorized-keys`,
/// falling back cleanly to framed `DaemonStream`.
///
/// EC2 instances are identified only through `ec2_lookup` (the local `mj ec2`
/// records); nothing on a remote host is asked who it is. Two instances in one
/// VPC and region talk over private IPs, otherwise over public IPs, and each
/// side's security group gets the grant its peer needs.
#[allow(clippy::too_many_arguments)]
fn try_build_direct_peer_config(
    operation_id: &str,
    source: &mj_core::targets::TargetLocator,
    source_worker_root: &str,
    destination: &mj_core::targets::TargetLocator,
    destination_worker_root: &str,
    keep: transport::KeepTransferOptions,
    executor: &(impl CommandExecutor + Sync),
    ec2: &impl transport::ec2_grant::Ec2SecurityGroupClient,
    ec2_lookup: &dyn Fn(
        &mj_core::targets::TargetLocator,
    ) -> Option<mj_core::ec2_records::Ec2InstanceRef>,
) -> Option<transport::DirectPeerConfig> {
    let (src_ssh, src_host_worker) = direct_ssh_endpoint(source, source_worker_root)?;
    let (dst_ssh, dst_host_worker) = direct_ssh_endpoint(destination, destination_worker_root)?;
    let host_locator =
        |ssh: &mj_core::targets::SshTarget| mj_core::targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: String::new(),
            worker_id: None,
        };
    let src_probe = probe_ssh_host_identity(&host_locator(src_ssh), &src_host_worker, executor)?;
    let dst_probe = probe_ssh_host_identity(&host_locator(dst_ssh), &dst_host_worker, executor)?;
    let source_ec2 = resolve_ec2_endpoint(source, ec2_lookup, ec2);
    let destination_ec2 = resolve_ec2_endpoint(destination, ec2_lookup, ec2);
    let route = plan_direct_ec2_route(
        ssh_host_of(src_ssh),
        ssh_host_of(dst_ssh),
        source_ec2.as_ref(),
        destination_ec2.as_ref(),
    );
    let rewrite_destination = |ssh: &mj_core::targets::SshTarget, address: &Option<String>| match (
        address,
        ssh.destination.rsplit_once('@'),
    ) {
        (Some(address), Some((user, _))) => format!("{user}@{address}"),
        (Some(address), None) => address.clone(),
        (None, _) => ssh.destination.clone(),
    };
    Some(transport::DirectPeerConfig {
        operation_id: operation_id.to_owned(),
        source_address: route
            .source_address
            .clone()
            .unwrap_or_else(|| ssh_host_of(src_ssh)),
        destination_address: route
            .destination_address
            .clone()
            .unwrap_or_else(|| ssh_host_of(dst_ssh)),
        destination_ssh_destination: rewrite_destination(dst_ssh, &route.destination_ssh_address),
        source_ssh_destination: rewrite_destination(src_ssh, &route.source_ssh_address),
        destination_host_key: dst_probe.host_key,
        source_host_key: src_probe.host_key,
        source_stage_dir: std::path::PathBuf::from(&src_probe.home)
            .join(".cache/mjolnir/move-stage"),
        destination_stage_dir: std::path::PathBuf::from(&dst_probe.home)
            .join(".cache/mjolnir/move-stage"),
        destination_authorized_keys: std::path::PathBuf::from(&dst_probe.home)
            .join(".ssh/authorized_keys"),
        source_authorized_keys: std::path::PathBuf::from(&src_probe.home)
            .join(".ssh/authorized_keys"),
        source_worker_bin: format!("{source_worker_root}/hel"),
        destination_worker_bin: format!("{destination_worker_root}/hel"),
        source_host_worker_bin: src_probe.host_worker_bin,
        destination_host_worker_bin: dst_probe.host_worker_bin,
        destination_ec2_grant: route.destination_grant,
        source_ec2_grant: route.source_grant,
        keep,
        cleanup: Default::default(),
    })
}

fn ssh_host_of(ssh: &mj_core::targets::SshTarget) -> String {
    ssh.destination
        .rsplit_once('@')
        .map_or(ssh.destination.as_str(), |(_, host)| host)
        .to_owned()
}

/// An EC2 endpoint's identity from local records and its network placement.
struct Ec2Endpoint {
    instance: mj_core::ec2_records::Ec2InstanceRef,
    network: transport::ec2_grant::Ec2InstanceNetwork,
}

impl Ec2Endpoint {
    fn security_group(&self) -> Option<&str> {
        self.network.security_group_ids.first().map(String::as_str)
    }

    fn grant(&self, peer_ip: &str) -> Option<transport::ec2_grant::Ec2GrantTarget> {
        Some(transport::ec2_grant::Ec2GrantTarget::new(
            transport::ec2_grant::Ec2GrantKey::ssh(self.security_group()?, format!("{peer_ip}/32")),
            transport::ec2_grant::Ec2GrantRoute::new(
                Some(self.instance.region.clone()),
                self.instance.aws_profile.clone(),
            ),
        ))
    }
}

/// `None` when the locator is not a recorded EC2 instance or AWS cannot
/// describe it; that endpoint then simply gets no security-group grant.
fn resolve_ec2_endpoint(
    locator: &mj_core::targets::TargetLocator,
    lookup: &dyn Fn(
        &mj_core::targets::TargetLocator,
    ) -> Option<mj_core::ec2_records::Ec2InstanceRef>,
    ec2: &impl transport::ec2_grant::Ec2SecurityGroupClient,
) -> Option<Ec2Endpoint> {
    let instance = lookup(locator)?;
    match ec2.describe_instance(
        &instance.instance_id,
        Some(&instance.region),
        instance.aws_profile.as_deref(),
    ) {
        Ok(network) => Some(Ec2Endpoint { instance, network }),
        Err(err) => {
            tracing::warn!(
                instance_id = %instance.instance_id,
                error = %err,
                "could not describe EC2 instance for direct Move; no security-group grant for it"
            );
            None
        }
    }
}

/// Addresses and security-group grants a direct Move uses between two hosts.
#[derive(Debug, Default)]
struct DirectEc2Route {
    /// Address of the source as the destination sees it (`authorized_keys` `from=`).
    source_address: Option<String>,
    destination_address: Option<String>,
    /// Host the other side's SSH connects to when it replaces the configured one.
    source_ssh_address: Option<String>,
    destination_ssh_address: Option<String>,
    destination_grant: Option<transport::ec2_grant::Ec2GrantTarget>,
    source_grant: Option<transport::ec2_grant::Ec2GrantTarget>,
}

fn plan_direct_ec2_route(
    source_host: String,
    destination_host: String,
    source: Option<&Ec2Endpoint>,
    destination: Option<&Ec2Endpoint>,
) -> DirectEc2Route {
    let mut route = DirectEc2Route::default();
    match (source, destination) {
        (Some(src), Some(dst)) => {
            let same_vpc = src.network.vpc_id.is_some()
                && src.network.vpc_id == dst.network.vpc_id
                && src.instance.region == dst.instance.region;
            if same_vpc
                && let (Some(src_ip), Some(dst_ip)) =
                    (&src.network.private_ip, &dst.network.private_ip)
                && src.security_group().is_some()
                && dst.security_group().is_some()
            {
                route.source_address = Some(src_ip.clone());
                route.destination_address = Some(dst_ip.clone());
                route.source_ssh_address = Some(src_ip.clone());
                route.destination_ssh_address = Some(dst_ip.clone());
                route.destination_grant = dst.grant(src_ip);
                route.source_grant = src.grant(dst_ip);
            } else if let (Some(src_ip), Some(dst_ip)) =
                (&src.network.public_ip, &dst.network.public_ip)
            {
                route.source_address = Some(src_ip.clone());
                route.destination_address = Some(dst_ip.clone());
                route.destination_grant = dst.grant(src_ip);
                route.source_grant = src.grant(dst_ip);
            }
        }
        (Some(src), None) => {
            if destination_host.parse::<std::net::Ipv4Addr>().is_ok() {
                route.source_grant = src.grant(&destination_host);
            }
        }
        (None, Some(dst)) => {
            if source_host.parse::<std::net::Ipv4Addr>().is_ok() {
                route.destination_grant = dst.grant(&source_host);
            }
        }
        (None, None) => {}
    }
    route
}

/// How the host-side worker binary (the one that edits `authorized_keys`) is
/// found. A bare or EC2 host runs the session's own `{worker_root}/hel`; a
/// container host has no such binary on the host, so the probe looks for one
/// that runs there.
enum HostWorker {
    Known(String),
    FindOnPodmanHost,
    FindOnNitroHost,
}

fn direct_ssh_endpoint<'a>(
    locator: &'a mj_core::targets::TargetLocator,
    worker_root: &str,
) -> Option<(&'a mj_core::targets::SshTarget, HostWorker)> {
    use mj_core::targets::TargetLocator;
    match locator {
        TargetLocator::SshBare { ssh, .. } | TargetLocator::AwsEc2 { ssh, .. } => {
            Some((ssh, HostWorker::Known(format!("{worker_root}/hel"))))
        }
        TargetLocator::SshPodman { ssh, .. } => Some((ssh, HostWorker::FindOnPodmanHost)),
        TargetLocator::SshNitro { ssh, .. } => Some((ssh, HostWorker::FindOnNitroHost)),
        // Docker and Apple container hosts keep the framed daemon stream.
        _ => None,
    }
}

struct SshHostProbe {
    home: String,
    host_key: String,
    host_worker_bin: String,
}

/// One SSH round trip: the login home, the trusted SSH host key and, for
/// container hosts, a host worker binary that runs here and knows
/// `authorized-keys`. `None` when any of them is missing.
fn probe_ssh_host_identity(
    host_locator: &mj_core::targets::TargetLocator,
    host_worker: &HostWorker,
    executor: &(impl CommandExecutor + Sync),
) -> Option<SshHostProbe> {
    let find_worker = |candidates: &str| {
        format!(
            "{{ {candidates}ls -t -- \"$HOME\"/.local/share/hel/workers/worker-binaries/*/hel 2>/dev/null; }} | \
             while IFS= read -r cand; do \
               if [ -x \"$cand\" ] && \"$cand\" worker authorized-keys --help >/dev/null 2>&1; then \
                 printf '%s\\n' \"$cand\"; break; fi; done"
        )
    };
    let worker_line = match host_worker {
        HostWorker::Known(_) => String::new(),
        HostWorker::FindOnPodmanHost => find_worker(""),
        HostWorker::FindOnNitroHost => {
            find_worker("printf '%s\\n' \"$HOME/.local/share/hel/microvm/hel\"; ")
        }
    };
    let mut script = String::from(
        "printf '%s\\n' \"$HOME\"\n\
         key=$( (cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null || cat /etc/ssh/ssh_host_rsa_key.pub 2>/dev/null) | head -n 1)\n\
         printf '%s\\n' \"$key\"\n",
    );
    if !worker_line.is_empty() {
        script.push_str(&worker_line);
        script.push('\n');
    }
    let spec = mj_core::targets::locator_command(
        mj_core::targets::SshTrafficClass::Control,
        host_locator,
        vec!["sh".into(), "-c".into(), script],
    )
    .purpose("probe trusted SSH host identity for direct Move");
    let out = executor.execute(&spec).ok()?;
    if out.status != 0 {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let home = lines.next()?.trim().to_owned();
    let key_line = lines.next()?.trim();
    let mut parts = key_line.split_whitespace();
    let key_type = parts.next()?;
    let key_b64 = parts.next()?;
    if home.is_empty() || key_type.is_empty() || key_b64.is_empty() {
        return None;
    }
    let host_worker_bin = match host_worker {
        HostWorker::Known(bin) => bin.clone(),
        HostWorker::FindOnPodmanHost | HostWorker::FindOnNitroHost => {
            lines.next()?.trim().to_owned()
        }
    };
    if host_worker_bin.is_empty() {
        return None;
    }
    Some(SshHostProbe {
        home,
        host_key: format!("{key_type} {key_b64}"),
        host_worker_bin,
    })
}

fn legacy_shim_move_operation(
    session_id: &str,
    legacy: &mj_core::state::LegacyMoveOperation,
    session: Option<&mj_core::state::SessionRecord>,
) -> MoveOperation {
    MoveOperation {
        quota_stop_reason: legacy.quota_stop_reason.clone(),
        quota_continuation: legacy.quota_continuation,
        prepared_destination: legacy.prepared_destination.clone(),
        accepted_preparation: None,
        acknowledge_interruption: legacy.acknowledge_interruption,
        transfer_route: None,
        consent: Default::default(),
        skip_keep_paths: Vec::new(),
        granted_keys: Vec::new(),
        granted_ec2_rules: Vec::new(),
        in_place_expected_store_id: None,
        in_place_continuation_command_id: None,
        in_place_held_command_ids: Vec::new(),
        in_place_harness_restarted: false,
        deferred_environment_changes: Vec::new(),
        source_snapshot: None,
        source_checkpoint_only: legacy.source_checkpoint_only,
        in_place: legacy.in_place,
        operation_id: legacy.operation_id.clone(),
        selection: MoveSelection {
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
            clear_resource_allocation: false,
            session_id: session_id.to_owned(),
            profile_id: session.map(|s| s.last_profile.clone()),
            target_template_id: session.map(|s| s.target_template_id.clone()),
            additional_mounts: None,
            resource_allocation: None,
            subagents: None,
            oracle: None,
            oracle_choice: None,
        },
        source_profile_id: legacy.source_profile_id.clone(),
        source_target_template_id: legacy.source_target_template_id.clone(),
        source_target: legacy.source_target.clone(),
        source_native_session_id: legacy.source_native_session_id.clone(),
        inherited_resource_allocation: legacy.inherited_resource_allocation.clone(),
        inherited_additional_mounts: legacy.inherited_additional_mounts.clone(),
        source_additional_mounts: legacy.source_additional_mounts.clone(),
        source_resource_allocation: legacy.source_resource_allocation.clone(),
        destination_target: legacy.destination_target.clone(),
        destination_native_session_id: legacy.destination_native_session_id.clone(),
        destination_store_id: legacy.destination_store_id.clone(),
        configuration_fingerprint: legacy.configuration_fingerprint.clone(),
        checkpoint: legacy.handoff.clone().or_else(|| legacy.checkpoint.clone()),
        recovery_session: legacy.recovery_session.clone(),
        queue: legacy.queue,
        phase: MovePhase::Capturing,
        queue_admission_started: legacy.queue_admission_started,
        queue_admission_finished: legacy.queue_admission_finished,
        cancellation_requested: legacy.cancellation_requested,
        created_at: legacy.created_at.clone(),
        updated_at: legacy.updated_at.clone(),
        error: legacy.error.clone(),
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

/// Whether a Move would leave a running session exactly as it is: the same
/// profile, the same environment, and the same delegation policy. The worker
/// reads its delegation policy at launch, so a policy change alone still needs
/// the restart a Move performs.
fn move_changes_nothing(source: &mj_core::state::SessionRecord, selection: &MoveSelection) -> bool {
    source.state == SessionState::Running
        && move_environment_change(source, selection, false).is_none()
        && move_keeps_profile_and_policy(source, selection)
}

fn move_keeps_profile_and_policy(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
) -> bool {
    selection.profile_id.as_deref() == Some(source.last_profile.as_str())
        && move_subagent_policy(source, selection) == source.subagents.clone().unwrap_or_default()
        && selection
            .oracle
            .is_none_or(|oracle| oracle == source.oracle)
        && selection
            .oracle_choice
            .as_ref()
            .is_none_or(|choice| Some(choice) == source.oracle_choice.as_deref())
}

pub(crate) fn parent_tools_enabled(
    policy: &mj_core::subagent::SubagentPolicy,
    harness: mj_core::config::HarnessKind,
) -> bool {
    policy.for_launch(harness, None).parent_role().is_some()
}

pub(in crate::controller) fn move_children(
    state: &mj_core::state::State,
    parent_session_id: &str,
) -> Vec<mj_core::subagent::InPlaceSubagent> {
    state
        .active_borrowers(parent_session_id)
        .into_iter()
        .filter_map(|child_id| state.subagents.get(&child_id))
        // These children are named to the primary after an in-place swap, and
        // the oracle is never named to it. A Move stops the oracle through the
        // ordinary stop path, so it needs no listing.
        .filter(|relation| relation.kind != mj_core::subagent::SubagentKind::Oracle)
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
                name: child.message_name(),
                state,
            })
        })
        .collect()
}

fn roleless_move_children_error(
    state: &mj_core::state::State,
    parent_session_id: &str,
) -> Option<String> {
    let children: Vec<_> = move_children(state, parent_session_id)
        .into_iter()
        .filter(|child| child.state == mj_core::subagent::InPlaceSubagentState::Running)
        .map(|child| {
            format!(
                "{} (child_session_id {})",
                child.name, child.child_session_id
            )
        })
        .collect();
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

/// Whether a source worker at this relay protocol can close sub-agent
/// admission, which an in-place Move needs to keep the parent's children.
pub fn worker_drains_subagent_requests(protocol_version: u32) -> bool {
    mj_core::relay::RelayRequest::SetSubagentAdmission { open: false }
        .supported_at(protocol_version)
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

/// Whether this move replaces only the harness inside the source target.
///
/// That is a fact about the environments alone: the same target template (or
/// another one that names the same bare machine), the same attached mounts and
/// the same resource allocation. A previous Move's retry state never decides
/// it, only what a retry restores. A sub-agent never owns its own target.
///
/// A live source takes this decision fresh. A source that is not live can only
/// continue an in-place Move that sealed it and still holds its restore
/// archive; if the environment no longer matches what that Move kept, the
/// Move is refused, never turned into a transfer of the checkout.
pub fn move_in_place_for_record(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
    is_subagent: bool,
    previous: Option<&MoveOperation>,
) -> Result<bool> {
    // A Move that began admitting queued work has already carried out its
    // environment decision: the session now runs on the destination, and a
    // retry only finishes that admission. Comparing the destination with
    // itself would call every such Move in place.
    if let Some(operation) = previous.filter(|operation| {
        operation.queue_admission_started && !operation.queue_admission_finished
    }) {
        return Ok(operation.in_place);
    }
    let obstacle = move_environment_obstacle(source, selection, targets, is_subagent);
    if matches!(
        source.state,
        SessionState::Running | SessionState::Disconnected
    ) {
        return Ok(obstacle.is_none());
    }
    if previous.is_some_and(|operation| operation.in_place && operation.checkpoint_retained()) {
        if let Some(obstacle) = obstacle {
            bail!(
                "the retained in-place Move kept this session's environment, but {obstacle} now; \
                 destroy the session or restore the earlier settings"
            );
        }
        return Ok(true);
    }
    Ok(false)
}

/// Why the source's environment cannot be kept for this destination, if it
/// cannot. The decision and its logs read this one answer.
fn move_environment_obstacle(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
    is_subagent: bool,
) -> Option<&'static str> {
    if is_subagent {
        Some("a sub-agent never owns its environment")
    } else if source.target.is_none() {
        Some("the session has no environment")
    } else {
        move_environment_change_for(source, selection, targets)
    }
}

/// What about the requested destination differs from the source's environment,
/// if anything. The one place that pairs the comparison with the bare-target
/// sharing rule.
fn move_environment_change_for(
    source: &mj_core::state::SessionRecord,
    selection: &MoveSelection,
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
) -> Option<&'static str> {
    move_environment_change(
        source,
        selection,
        bare_targets_share_environment(
            targets,
            &source.target_template_id,
            selection.target_template_id.as_deref().unwrap_or_default(),
        ),
    )
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

fn move_requested_allocation<'a>(
    source: &'a mj_core::state::SessionRecord,
    selection: &'a MoveSelection,
) -> Option<&'a mj_core::state::SessionResourceAllocation> {
    if selection.clear_resource_allocation {
        None
    } else {
        selection
            .resource_allocation
            .as_ref()
            .or(source.resource_allocation.as_ref())
    }
}

/// The size a Move gives a destination that has none of its own: the host's
/// remembered size, configured target size or baseline. Capacity admission
/// checks the result against current host totals.
fn move_default_allocation(
    state: &mj_core::state::State,
    target: &mj_core::config::TargetTemplate,
) -> Option<mj_core::state::SessionResourceAllocation> {
    let host = mj_core::config::container_size_host(target)?;
    let size = mj_core::state::resolve_default_container_size(
        target
            .container()
            .and_then(|c| c.default_host_container_size()),
        state.container_sizes.get(host).copied(),
        None,
    );
    Some(mj_core::state::SessionResourceAllocation::Container {
        cpus: size.cpus,
        memory_bytes: size.memory_bytes,
    })
}

/// Decide a Move's environment and, when it provisions, the size it will
/// provision with.
///
/// One function makes both decisions so the sealed selection is always the
/// selection the decision was made from: execution re-prepares that selection
/// and capacity admission re-decides `in_place` from it, and a Move whose
/// sealed selection decided something else would be refused as having changed
/// its mind (#1585).
///
/// A profile-only Move of a container that records no size changes nothing
/// about the environment, so it stays in place and is not given a size here.
/// Inventing one would turn it into a rebuild, and the automatic quota switch,
/// which only ever sends a profile-only selection, refuses a Move that changes
/// the environment.
///
/// Returns whether the environment is kept, and whether the applied size is
/// the destination's default.
fn resolve_move_size_for_environment(
    source: &mj_core::state::SessionRecord,
    selection: &mut MoveSelection,
    state: &mj_core::state::State,
    targets: &std::collections::BTreeMap<String, mj_core::config::TargetTemplate>,
    target: &mj_core::config::TargetTemplate,
    is_subagent: bool,
    previous: Option<&MoveOperation>,
) -> Result<(bool, bool)> {
    let sealed = previous.is_some_and(MoveOperation::holds_source_environment);
    // An explicit clear means "the destination's default" and is always
    // resolved: the person asked for a size, so an unknown current size is no
    // reason to keep an unbounded container. Otherwise a size is resolved
    // before the decision wherever comparing sizes means something -- the
    // environment's own size is known -- so a request that resolves to the
    // size the container already has is not a change. A profile-only Move of a
    // container whose size is unknown resolves nothing here, and keeps its
    // environment.
    let comparable = mj_core::state::applied_container_settings(source)
        .resource_allocation
        .is_some();
    let mut sized_by_destination_default = !sealed
        && (selection.clear_resource_allocation || comparable)
        && resolve_move_destination_size(source, selection, state, target);
    let mut in_place = move_in_place_for_record(source, selection, targets, is_subagent, previous)?;
    // A Move that provisions needs a size to create the container with. It
    // takes the inherited one, else the destination's default. Sizing it can
    // also settle the decision: a container whose environment the resolution
    // made identical to its own is kept, not rebuilt.
    if !in_place && !sealed && selection.resource_allocation.is_none() {
        sized_by_destination_default |=
            resolve_move_destination_size(source, selection, state, target);
        in_place = move_in_place_for_record(source, selection, targets, is_subagent, previous)?;
    }
    Ok((in_place, sized_by_destination_default))
}

/// What a Move applies where its selection names no size, or where it clears
/// the inherited one: the inherited size, or the destination's default.
/// Returns whether the applied size is that default.
///
/// A destination without a default -- a fixed host -- has none to give, so
/// there the request keeps its own meaning, and the clear request is what
/// allows a sized session onto that host at all. Where a default exists the
/// sealed selection names the size that will be applied, so no later reader
/// has to work it out from the source record again.
///
/// The caller decides when this applies: never to a retained Move, whose
/// sealed selection is its own record of what was decided and is re-prepared
/// and compared by execution.
fn resolve_move_destination_size(
    source: &mj_core::state::SessionRecord,
    selection: &mut MoveSelection,
    state: &mj_core::state::State,
    target: &mj_core::config::TargetTemplate,
) -> bool {
    let Some(default_size) = move_default_allocation(state, target) else {
        return false;
    };
    if let Some(inherited) = move_requested_allocation(source, selection) {
        if selection.resource_allocation.is_none() {
            selection.resource_allocation = Some(inherited.clone());
        }
        return false;
    }
    selection.resource_allocation = Some(default_size);
    selection.clear_resource_allocation = false;
    true
}

/// Whether the destination's size is the default this Move resolved.
///
/// A preparation resolves it for a request that names none. A retry reaches
/// preparation with the sealed size already in its selection, so it cannot
/// resolve anything; the sealed Move's own record is what keeps the fact that
/// this size came from clearing, and with it the match between the repeated
/// command and the Move it retries (#1585).
fn move_size_is_destination_default(resolved_now: bool, previous: Option<&MoveOperation>) -> bool {
    resolved_now
        || previous.is_some_and(|operation| {
            operation.holds_source_environment()
                && operation
                    .accepted_preparation
                    .as_ref()
                    .is_some_and(|prepared| prepared.sized_by_destination_default)
        })
}

/// Reconcile a retry's resource selection with the one its Move was sealed
/// with.
///
/// A request that omits the size keeps the sealed one, so a surface without
/// the flag can retry a Move another surface sealed. A request that repeats a
/// clear instruction is the same request as one whose sealed Move holds the
/// size that instruction resolved to, because preparation replaces it with
/// that size (#1585). The sealed Move owns that fact, so the comparison does
/// not consult the host's remembered size, which other sessions change.
/// Anything else keeps its own value and is then refused as a different
/// selection.
fn reconcile_retry_resource_selection(
    sealed: &MoveSelection,
    sealed_by_default: bool,
    request: &mut MoveSelection,
) {
    if request.resource_allocation.is_some() {
        return;
    }
    let repeats_a_resolved_clear = request.clear_resource_allocation && sealed_by_default;
    if !request.clear_resource_allocation || repeats_a_resolved_clear {
        request.resource_allocation = sealed.resource_allocation.clone();
        request.clear_resource_allocation = sealed.clear_resource_allocation;
    }
}

fn move_requested_mounts<'a>(
    source: &'a mj_core::state::SessionRecord,
    selection: &'a MoveSelection,
) -> &'a [crate::targets::AdditionalMount] {
    selection
        .additional_mounts
        .as_deref()
        .unwrap_or(&source.additional_mounts)
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
    } else if mj_core::state::applied_container_settings(source).additional_mounts
        != move_requested_mounts(source, selection)
    {
        Some("attached mounts changed")
    } else if mj_core::state::applied_container_settings(source).resource_allocation
        != move_requested_allocation(source, selection)
    {
        Some("resource allocation changed")
    } else {
        None
    }
}

fn in_place_restart_has_cancellable_turn(
    operational: &mj_core::relay::RelayOperationalState,
) -> bool {
    operational.active_prompt.is_some()
        || operational.harness_turn.is_some()
        || operational.execution == mj_core::relay::RelayExecutionState::Running
        || !operational.tools_in_flight.is_empty()
}

fn in_place_restart_blocked_by_work(operational: &mj_core::relay::RelayOperationalState) -> bool {
    in_place_restart_has_cancellable_turn(operational)
        || operational.clear_context_started_at_ms.is_some()
        || !operational.active_user_shells.is_empty()
}
