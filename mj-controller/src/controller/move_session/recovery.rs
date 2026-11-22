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

/// Prune dead pre-crash Move users from `ec2_move_grants` in one local SQLite
/// transaction before live Move admission opens.
///
/// Any `External` grant left with zero users is dropped immediately; any
/// `Installed`/`Installing`/`Removing` grant left with zero users remains in
/// the table with an empty user set so [`reconcile_ec2_grants_live`] can revoke
/// it safely alongside live Move admissions.
pub fn prune_stale_ec2_grant_users<S: Ec2GrantStore>(
    store: &S,
    active_operation_ids: &BTreeSet<String>,
) -> Result<usize> {
    store.with_transaction(|records| {
        let keys: Vec<_> = records.keys().cloned().collect();
        let mut pending = 0_usize;
        for key in keys {
            let remove = if let Some(record) = records.get_mut(&key) {
                record.users.retain(|op| active_operation_ids.contains(op));
                if record.users.is_empty()
                    && record.state
                        == super::transport::ec2_grant::Ec2GrantState::External
                {
                    true
                } else {
                    if record.users.is_empty()
                        || matches!(
                            record.state,
                            super::transport::ec2_grant::Ec2GrantState::Installing
                                | super::transport::ec2_grant::Ec2GrantState::Removing
                        )
                    {
                        pending += 1;
                    }
                    false
                }
            } else {
                false
            };
            if remove {
                records.remove(&key);
            }
        }
        Ok(pending)
    })
}

/// Live-safe EC2 grant reconciliation across every recorded security group.
///
/// Uses [`Ec2GrantManager::reconcile_group`], which never prunes active users
/// against an external snapshot, so concurrent Moves admitted after startup
/// cannot have their active grants revoked.
pub fn reconcile_ec2_grants_live<S: Ec2GrantStore, C: Ec2SecurityGroupClient>(
    store: S,
    client: C,
    daemon_instance_id: &str,
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
        let report = manager.reconcile_group(&sg)?;
        total.revoked_rules += report.revoked_rules;
        total.dropped_records += report.dropped_records;
        total.resolved_installed += report.resolved_installed;
        total.remaining_pending += report.remaining_pending;
    }
    Ok(total)
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

/// Synchronously prune stale pre-crash Move users from `ec2_move_grants` (pure
/// local SQLite, before live Move admission opens), then spawn a background
/// exponential-backoff retry task running live-safe reconciliation
/// (`reconcile_ec2_grants_live`) for any unresolved grants.
///
/// Per `AGENTS.md`, the background task does NOT hold daemon handoff admission
/// and never prunes users against a startup snapshot, so new Moves admitted
/// concurrently keep their grants intact.
pub fn spawn_ec2_grant_reconcile_background(active_operation_ids: BTreeSet<String>) {
    if let Err(err) = prune_stale_ec2_grant_users(&SqliteEc2GrantStore, &active_operation_ids) {
        tracing::warn!(
            error = %err,
            "could not prune stale EC2 Move grant users at startup"
        );
    }
    let Ok(daemon_instance_id) = crate::database::daemon_instance_id() else {
        tracing::warn!("could not load daemon_instance_id for EC2 Move grant reconciliation");
        return;
    };
    mj_core::runtime::spawn(async move {
        let mut backoff = Duration::from_secs(5);
        let max_backoff = Duration::from_secs(300);
        loop {
            let instance_id = daemon_instance_id.clone();
            let outcome = mj_core::runtime::spawn_blocking(move || {
                reconcile_ec2_grants_live(
                    SqliteEc2GrantStore,
                    AwsCliEc2Client::default(),
                    &instance_id,
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

static MOVE_KEY_CLEANUP_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct MoveKeyCleanupGuard;
impl Drop for MoveKeyCleanupGuard {
    fn drop(&mut self) {
        MOVE_KEY_CLEANUP_RUNNING.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Spawn a non-admitted background exponential-backoff retry task that revokes
/// any ephemeral Move SSH credentials (`granted_keys`) left behind on finished
/// or failed moves when a remote host was transiently unreachable.
///
/// Only non-active (`Completed | Failed | Cancelled`) moves are touched so an
/// in-flight transfer's keys are never revoked mid-stream. At most one loop
/// runs per daemon; it exits once no non-active move has remaining keys.
pub fn spawn_move_key_cleanup_background() {
    if MOVE_KEY_CLEANUP_RUNNING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    mj_core::runtime::spawn(async move {
        let _guard = MoveKeyCleanupGuard;
        let mut backoff = Duration::from_secs(5);
        let max_backoff = Duration::from_secs(300);
        loop {
            let outcome = mj_core::runtime::spawn_blocking(move || -> Result<usize> {
                let operations = crate::database::load_move_operations()?;
                let pending_ops: Vec<_> = operations
                    .into_iter()
                    .filter(|op| !op.is_active() && !op.granted_keys.is_empty())
                    .collect();
                if pending_ops.is_empty() {
                    return Ok(0);
                }
                let controller = crate::controller::Controller::load()?;
                let executor = mj_core::targets::ProcessExecutor;
                let mut remaining_count = 0_usize;
                for mut op in pending_ops {
                    controller.cleanup_move_granted_keys(&mut op, &executor);
                    remaining_count += op.granted_keys.len();
                }
                Ok(remaining_count)
            })
            .await;
            match outcome {
                Ok(Ok(0)) => {
                    tracing::debug!("ephemeral Move SSH credential cleanup settled");
                    break;
                }
                Ok(Ok(remaining)) => {
                    tracing::info!(
                        remaining_keys = remaining,
                        backoff_secs = backoff.as_secs(),
                        "ephemeral Move SSH credential cleanup has unreachable hosts; retrying"
                    );
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        error = %err,
                        backoff_secs = backoff.as_secs(),
                        "ephemeral Move SSH credential cleanup error; retrying"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "ephemeral Move SSH credential cleanup task panicked"
                    );
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
                supervisors: None,
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

    #[test]
    fn startup_prune_and_live_reconcile_preserve_concurrent_new_moves() {
        use super::super::transport::ec2_grant::{
            Ec2ApiError, Ec2GrantKey, Ec2GrantRecord, Ec2GrantState, Ec2IngressRule,
            Ec2SecurityGroupClient, InMemoryGrantStore,
        };
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct SharedStore(Arc<InMemoryGrantStore>);
        impl Ec2GrantStore for SharedStore {
            fn with_transaction<R, F>(&self, f: F) -> Result<R>
            where
                F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
            {
                self.0.with_transaction(f)
            }
        }

        #[derive(Clone, Default)]
        struct StubEc2 {
            rules: Arc<Mutex<Vec<(String, Ec2IngressRule)>>>,
        }
        impl Ec2SecurityGroupClient for StubEc2 {
            fn describe_ingress_rules(
                &self,
                group_id: &str,
            ) -> std::result::Result<Vec<Ec2IngressRule>, Ec2ApiError> {
                Ok(self
                    .rules
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(sg, _)| sg == group_id)
                    .map(|(_, r)| r.clone())
                    .collect())
            }
            fn authorize_ingress(
                &self,
                group_id: &str,
                port: u16,
                cidr_ip: &str,
                description: &str,
            ) -> std::result::Result<(), Ec2ApiError> {
                self.rules.lock().unwrap().push((
                    group_id.to_owned(),
                    Ec2IngressRule {
                        cidr_ip: cidr_ip.to_owned(),
                        port,
                        description: Some(description.to_owned()),
                    },
                ));
                Ok(())
            }
            fn revoke_ingress(
                &self,
                group_id: &str,
                port: u16,
                cidr_ip: &str,
            ) -> std::result::Result<(), Ec2ApiError> {
                self.rules.lock().unwrap().retain(|(sg, r)| {
                    !(sg == group_id && r.cidr_ip == cidr_ip && r.port == port)
                });
                Ok(())
            }
        }

        let store = SharedStore(Arc::new(InMemoryGrantStore::new()));
        let client = StubEc2::default();
        let dead_key = Ec2GrantKey::ssh("sg-1", "10.0.0.1/32");
        let live_key = Ec2GrantKey::ssh("sg-1", "10.0.0.2/32");

        // Pre-crash grant for an operation no longer active at startup.
        store
            .with_transaction(|map| {
                map.insert(
                    dead_key.clone(),
                    Ec2GrantRecord {
                        key: dead_key.clone(),
                        state: Ec2GrantState::Installed,
                        users: BTreeSet::from(["op-dead".to_owned()]),
                        absent_checks: 0,
                        attempt_id: 1,
                    },
                );
                Ok(())
            })
            .unwrap();
        client
            .authorize_ingress("sg-1", 22, "10.0.0.1/32", "mj-move-grant:daemon-1")
            .unwrap();

        // 1. Startup prunes dead pre-crash users synchronously (before live admission).
        let pending = prune_stale_ec2_grant_users(&store, &BTreeSet::new()).unwrap();
        assert_eq!(pending, 1);

        // 2. A new Move is admitted concurrently after startup and acquires `live_key`.
        store
            .with_transaction(|map| {
                map.insert(
                    live_key.clone(),
                    Ec2GrantRecord {
                        key: live_key.clone(),
                        state: Ec2GrantState::Installed,
                        users: BTreeSet::from(["op-new-concurrent".to_owned()]),
                        absent_checks: 0,
                        attempt_id: 2,
                    },
                );
                Ok(())
            })
            .unwrap();
        client
            .authorize_ingress("sg-1", 22, "10.0.0.2/32", "mj-move-grant:daemon-1")
            .unwrap();

        // 3. Background live reconciliation revokes the dead pre-crash rule
        //    WITHOUT touching the concurrent new Move's grant.
        let report = reconcile_ec2_grants_live(store.clone(), client.clone(), "daemon-1").unwrap();
        assert_eq!(report.revoked_rules, 1);
        assert_eq!(report.remaining_pending, 0);
        let remaining_rules = client.describe_ingress_rules("sg-1").unwrap();
        assert_eq!(remaining_rules.len(), 1);
        assert_eq!(remaining_rules[0].cidr_ip, "10.0.0.2/32");
        let snapshot = store.0.snapshot();
        assert!(!snapshot.contains_key(&dead_key));
        assert!(
            snapshot
                .get(&live_key)
                .is_some_and(|r| r.users.contains("op-new-concurrent"))
        );
    }
}
