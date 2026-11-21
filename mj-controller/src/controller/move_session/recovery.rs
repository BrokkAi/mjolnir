//! Ownership classification and startup recovery for current (rev 148+) and
//! legacy (pre-148) `MoveOperation` records, plus EC2 grant and SSH key
//! reconciliation.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use mj_core::state::{
    GrantedMoveKey, LegacyMoveOperation, LegacyMovePhase, MoveOperation, SessionRecord,
    SessionState,
};

use super::transport::ec2_grant::{
    AwsCliEc2Client, Ec2GrantConfig, Ec2GrantManager, Ec2GrantStore, Ec2SecurityGroupClient,
    ReconcileReport, SqliteEc2GrantStore,
};

/// Durable ownership classification for an in-flight Move operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOwnership {
    /// Source still owns the session: tear down any partial destination first,
    /// then restart the source agent in place.
    Source,
    /// Destination durably owns the session: finish forward (retire source if
    /// not already gone, complete exactly-once queue admission, settle).
    Destination,
}

/// Classify ownership of a rev-148+ cross-target `MoveOperation`.
///
/// The single commit point is `MovePhase::DestinationOwns`.
pub fn classify_move_ownership(operation: &MoveOperation) -> MoveOwnership {
    if operation.phase.destination_owns() || operation.queue_admission_started {
        MoveOwnership::Destination
    } else {
        MoveOwnership::Source
    }
}

/// Classify ownership of a pre-148 `LegacyMoveOperation` across an upgrade.
///
/// Preserves the existing classifier:
/// - `StartingQueue` or `queue_admission_started` -> Destination owns.
/// - Crash window: `ResumingDestination` while `session.state == Running` on
///   destination -> Destination owns.
/// - Otherwise -> Source still owns (roll back to source and notify user to
///   rerun the move).
pub fn classify_legacy_move_ownership(
    operation: &LegacyMoveOperation,
    session: Option<&SessionRecord>,
) -> MoveOwnership {
    if operation.queue_admission_started
        || matches!(operation.phase, LegacyMovePhase::StartingQueue)
    {
        return MoveOwnership::Destination;
    }
    if operation.phase == LegacyMovePhase::ResumingDestination
        && let Some(record) = session
        && record.state == SessionState::Running
        && operation
            .destination_target
            .as_ref()
            .is_some_and(|dest| record.target.as_deref() == Some(dest))
    {
        return MoveOwnership::Destination;
    }
    MoveOwnership::Source
}

/// Notice emitted when a legacy pre-148 source-owned Move is rolled back on
/// startup after upgrading to rev 148+.
pub fn legacy_source_rollback_notice(operation_id: &str) -> String {
    format!(
        "Move {operation_id} was interrupted before the destination took ownership and \
         was rolled back to the source after upgrading. Run `mj move` again."
    )
}

/// Best-effort local cleanup of ephemeral Move private key and known_hosts
/// files recorded in `MoveOperation::granted_keys`.
pub fn cleanup_local_move_key_files(keys: &[GrantedMoveKey]) {
    for key in keys {
        if let Some(path) = &key.private_key_path {
            remove_if_exists(path);
        }
        if let Some(path) = &key.known_hosts_path {
            remove_if_exists(path);
        }
    }
}

fn remove_if_exists(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "failed to remove ephemeral Move key/known_hosts file"
        );
    }
}

/// Reconcile EC2 Move security-group grants at daemon startup against the set
/// of still-active Move operation IDs.
pub fn reconcile_ec2_grants_startup<S: Ec2GrantStore, C: Ec2SecurityGroupClient>(
    store: S,
    client: C,
    daemon_instance_id: &str,
    active_operation_ids: &BTreeSet<String>,
) -> Result<ReconcileReport> {
    let security_groups: BTreeSet<String> = store.with_transaction(|records| {
        Ok(records
            .keys()
            .map(|k| k.security_group_id.clone())
            .collect())
    })?;
    let manager = Ec2GrantManager::new(
        store,
        client,
        daemon_instance_id,
        Ec2GrantConfig::default(),
    );
    let mut total = ReconcileReport::default();
    for sg in security_groups {
        let report =
            manager.reconcile_group_startup(&sg, |op_id| active_operation_ids.contains(op_id))?;
        total.revoked_rules += report.revoked_rules;
        total.dropped_records += report.dropped_records;
        total.resolved_installed += report.resolved_installed;
        total.remaining_pending += report.remaining_pending;
    }
    Ok(total)
}

/// Spawn a background exponential-backoff retry task for any unresolved
/// `Installing` / `Removing` EC2 Move grants left after startup reconciliation.
///
/// Per `AGENTS.md`, this task does NOT hold daemon handoff admission: if the
/// daemon upgrades or restarts, the next daemon resumes reconciliation from
/// `ec2_move_grants`.
pub fn spawn_ec2_grant_reconcile_background(active_operation_ids: BTreeSet<String>) {
    let Ok(daemon_instance_id) = crate::database::daemon_instance_id() else {
        tracing::warn!("could not load daemon_instance_id for EC2 Move grant reconciliation");
        return;
    };
    mj_core::runtime::spawn(async move {
        let mut backoff = Duration::from_secs(5);
        let max_backoff = Duration::from_secs(300);
        loop {
            let active = active_operation_ids.clone();
            let instance_id = daemon_instance_id.clone();
            let outcome = mj_core::runtime::spawn_blocking(move || {
                reconcile_ec2_grants_startup(
                    SqliteEc2GrantStore,
                    AwsCliEc2Client::default(),
                    &instance_id,
                    &active,
                )
            })
            .await;
            match outcome {
                Ok(Ok(report)) if report.remaining_pending == 0 => {
                    tracing::debug!(
                        revoked = report.revoked_rules,
                        dropped = report.dropped_records,
                        "EC2 Move grant reconciliation settled"
                    );
                    break;
                }
                Ok(Ok(report)) => {
                    tracing::info!(
                        remaining_pending = report.remaining_pending,
                        backoff_secs = backoff.as_secs(),
                        "EC2 Move grant reconciliation has unresolved grants; retrying"
                    );
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        error = %err,
                        backoff_secs = backoff.as_secs(),
                        "EC2 Move grant reconciliation error; retrying"
                    );
                }
                Err(err) => {
                    tracing::warn!(error = %err, "EC2 Move grant reconciliation task panicked");
                    break;
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{
        MovePhase, MoveSelection, ResumeQueueDisposition, StoredMoveOperation, TargetLocator,
    };

    fn sample_target(name: &str) -> TargetLocator {
        TargetLocator::LocalBare {
            worker_root: std::path::PathBuf::from(format!("/tmp/{name}")),
        }
    }

    fn sample_legacy_op(phase: LegacyMovePhase, queue_started: bool) -> LegacyMoveOperation {
        LegacyMoveOperation {
            selection: None,
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: None,
            acknowledge_interruption: false,
            handoff: None,
            source_checkpoint_only: false,
            in_place: false,
            operation_id: "legacy-op-1".to_owned(),
            source_profile_id: "p1".to_owned(),
            source_target_template_id: "local".to_owned(),
            source_target: Some(sample_target("src")),
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: Some(sample_target("dst")),
            destination_native_session_id: None,
            destination_store_id: Some("store-dst".to_owned()),
            configuration_fingerprint: String::new(),
            checkpoint: None,
            recovery_session: None,
            queue: ResumeQueueDisposition::Start,
            phase,
            queue_admission_started: queue_started,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-11-21T00:00:00Z".to_owned(),
            updated_at: "2026-11-21T00:00:00Z".to_owned(),
            error: None,
        }
    }

    fn sample_current_op(phase: MovePhase) -> MoveOperation {
        MoveOperation {
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: None,
            accepted_preparation: None,
            acknowledge_interruption: false,
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
            source_checkpoint_only: false,
            in_place: false,
            operation_id: "op-new-1".to_owned(),
            selection: MoveSelection {
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: false,
                session_id: "sess-1".to_owned(),
                profile_id: Some("p2".to_owned()),
                target_template_id: Some("remote".to_owned()),
                additional_mounts: None,
                resource_allocation: None,
                subagents: None,
                oracle: None,
                oracle_choice: None,
            },
            source_profile_id: "p1".to_owned(),
            source_target_template_id: "local".to_owned(),
            source_target: Some(sample_target("src")),
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: Some(sample_target("dst")),
            destination_native_session_id: None,
            destination_store_id: Some("store-dst".to_owned()),
            configuration_fingerprint: String::new(),
            checkpoint: None,
            recovery_session: None,
            queue: ResumeQueueDisposition::Start,
            phase,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-11-21T00:00:00Z".to_owned(),
            updated_at: "2026-11-21T00:00:00Z".to_owned(),
            error: None,
        }
    }

    #[test]
    fn current_move_crash_before_and_after_destination_owns_commit_point() {
        for phase in [
            MovePhase::Capturing,
            MovePhase::Transferring,
            MovePhase::Restoring,
            MovePhase::DestinationStarting,
        ] {
            let op = sample_current_op(phase);
            assert_eq!(classify_move_ownership(&op), MoveOwnership::Source);
        }
        for phase in [
            MovePhase::DestinationOwns,
            MovePhase::SourceRetired,
            MovePhase::QueueAdmitted,
        ] {
            let op = sample_current_op(phase);
            assert_eq!(classify_move_ownership(&op), MoveOwnership::Destination);
        }
    }

    #[test]
    fn legacy_move_ownership_source_rollback_and_destination_finish_forward() {
        // Source-owned legacy phases roll back to source.
        for phase in [
            LegacyMovePhase::Preparing,
            LegacyMovePhase::ClosingSource,
            LegacyMovePhase::ResumingDestination,
        ] {
            let op = sample_legacy_op(phase, false);
            assert_eq!(
                classify_legacy_move_ownership(&op, None),
                MoveOwnership::Source
            );
        }

        // StartingQueue is destination-owned.
        let starting = sample_legacy_op(LegacyMovePhase::StartingQueue, true);
        assert_eq!(
            classify_legacy_move_ownership(&starting, None),
            MoveOwnership::Destination
        );

        // Legacy JSON decodes via StoredMoveOperation::Legacy.
        let json = serde_json::to_string(&starting).unwrap();
        assert!(matches!(
            StoredMoveOperation::decode_json(&json).unwrap(),
            StoredMoveOperation::Legacy(_)
        ));
    }
}
