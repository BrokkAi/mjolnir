//! Same-target in-place Move executor built on Slice 5A (Relay Protocol 81).
//!
//! A same-target Move never seals the relay, never takes a handoff archive,
//! never runs `WorkerRootReset::InPlace`, and never stops the worker process
//! tree. Instead it holds dispatch, restarts only the harness under the new
//! launch configuration, applies the user's queue disposition (`Discard` or
//! `SubmitFront` continuation), durably records `MovePhase::DispatchReleased`,
//! and then releases the hold.

use anyhow::{Context, Result, bail, ensure};
use mj_core::relay::{RELAY_IN_PLACE_MOVE_PROTOCOL, RelayCommand, RelayRequest};
use mj_core::state::{MoveOperation, MovePhase, QuotaMoveContinuation, ResumeQueueDisposition};
use mj_core::worker_launch::WorkerLaunchConfig;

use super::now;

/// User-facing refusal when a manual same-target Move targets a worker that
/// predates Relay Protocol 81 (`RELAY_IN_PLACE_MOVE_PROTOCOL`).
pub fn pre_release_worker_manual_refusal(session_id: &str) -> String {
    format!(
        "this session's worker predates in-place restarts. Restart the agent first; \
         that restarts its worker and stops every process it is running. Then move. \
         (Run `mj restart --session {session_id}` or choose Restart agent in the UI.)"
    )
}

/// Notice for automatic quota Move when the worker predates Relay Protocol 81.
pub fn pre_release_worker_quota_notice(session_id: &str) -> String {
    format!(
        "automatic quota Move skipped for {session_id}: this session's worker predates \
         in-place restarts (relay protocol {RELAY_IN_PLACE_MOVE_PROTOCOL} required). \
         Waiting for quota reset, or run `mj restart --session {session_id}` first."
    )
}

/// Format a deferred environment change notice for a same-target Move.
pub fn deferred_environment_change_notice(change: &str) -> String {
    format!("{change} takes effect when this session's environment is next recreated")
}

/// Validate that a worker supports Slice 5A in-place moves.
pub fn ensure_worker_supports_in_place_move(
    session_id: &str,
    worker_protocol: u32,
    is_automatic_quota: bool,
) -> Result<()> {
    if worker_protocol >= RELAY_IN_PLACE_MOVE_PROTOCOL {
        return Ok(());
    }
    if is_automatic_quota {
        bail!("{}", pre_release_worker_quota_notice(session_id));
    }
    bail!("{}", pre_release_worker_manual_refusal(session_id));
}

/// Abstract relay transport for Slice 5A in-place Move commands so both live
/// worker connections and unit tests exercise the exact same state machine.
pub trait InPlaceMoveRelay {
    fn protocol_version(&self) -> u32;
    fn store_id(&self) -> &str;
    fn send_hold(&mut self, operation_id: &str, expected_store_id: &str) -> Result<Vec<String>>;
    fn send_restart_harness(
        &mut self,
        operation_id: &str,
        expected_store_id: &str,
        launch_config: Box<WorkerLaunchConfig>,
    ) -> Result<()>;
    fn send_discard_held(
        &mut self,
        operation_id: &str,
        expected_store_id: &str,
        command_ids: Vec<String>,
    ) -> Result<()>;
    fn send_submit_front(
        &mut self,
        operation_id: &str,
        expected_store_id: &str,
        command_id: String,
        command: RelayCommand,
    ) -> Result<()>;
    fn send_release(&mut self, operation_id: &str, expected_store_id: &str) -> Result<()>;
}

/// Build the Slice 5A `RelayRequest` sequence helpers.
pub fn build_hold_request(operation_id: &str, expected_store_id: &str) -> RelayRequest {
    RelayRequest::HoldInPlaceMoveDispatch {
        operation_id: operation_id.to_owned(),
        expected_store_id: expected_store_id.to_owned(),
    }
}

pub fn build_release_request(operation_id: &str, expected_store_id: &str) -> RelayRequest {
    RelayRequest::ReleaseInPlaceMoveDispatch {
        operation_id: operation_id.to_owned(),
        expected_store_id: expected_store_id.to_owned(),
    }
}

/// Execute or recover a durable Slice 5A same-target in-place Move.
///
/// `persist_op` persists intermediate `MoveOperation` updates;
/// `commit_dispatch_released` runs the single SQLite transaction recording
/// `MovePhase::DispatchReleased` before `ReleaseInPlaceMoveDispatch` is sent.
pub fn run_in_place_move_state_machine<R, P, C>(
    operation: &mut MoveOperation,
    relay: &mut R,
    launch_config: Option<Box<WorkerLaunchConfig>>,
    continuation: Option<(String, RelayCommand)>,
    mut persist_op: P,
    mut commit_dispatch_released: C,
) -> Result<Vec<String>>
where
    R: InPlaceMoveRelay,
    P: FnMut(&MoveOperation) -> Result<()>,
    C: FnMut(&MoveOperation) -> Result<()>,
{
    let session_id = operation.selection.session_id.clone();
    let is_quota = operation.quota_stop_reason.is_some();
    ensure_worker_supports_in_place_move(&session_id, relay.protocol_version(), is_quota)?;

    let mut notices: Vec<String> = operation
        .deferred_environment_changes
        .iter()
        .map(|change| deferred_environment_change_notice(change))
        .collect();

    // Recovery at `DispatchReleased`: never reapply queue decisions or resubmit
    // work; only idempotently ensure the hold is gone and settle.
    if operation.phase == MovePhase::DispatchReleased {
        if let Some(expected_store_id) = operation.in_place_expected_store_id.clone()
            && relay.store_id() == expected_store_id
        {
            relay.send_release(&operation.operation_id, &expected_store_id)?;
        }
        operation.phase = MovePhase::Completed;
        operation.queue_admission_finished = true;
        operation.updated_at = now();
        persist_op(operation)?;
        return Ok(notices);
    }

    // 1. Persist Move intent with the relay's current `store_id` before holding.
    let expected_store_id = match operation.in_place_expected_store_id.clone() {
        Some(recorded) => {
            if relay.store_id() != recorded {
                let unapplied = match operation.queue {
                    ResumeQueueDisposition::Discard => "discarding held pending commands",
                    ResumeQueueDisposition::Start => "submitting continuation and resuming queue",
                };
                let msg = format!(
                    "in-place Move could not safely apply its queue decision ({unapplied}): \
                     relay store changed from {recorded} to {}",
                    relay.store_id()
                );
                operation.phase = MovePhase::Failed;
                operation.error = Some(msg.clone());
                operation.updated_at = now();
                persist_op(operation)?;
                notices.push(msg.clone());
                bail!("{msg}");
            }
            recorded
        }
        None => {
            let current = relay.store_id().to_owned();
            operation.in_place_expected_store_id = Some(current.clone());
            if let Some((ref cmd_id, _)) = continuation {
                operation.in_place_continuation_command_id = Some(cmd_id.clone());
            }
            operation.updated_at = now();
            persist_op(operation)?;
            current
        }
    };

    // 2. Idempotently hold dispatch and record the pending command IDs.
    let held_ids = relay.send_hold(&operation.operation_id, &expected_store_id)?;

    // 3-5. Run post-hold steps (including persisting held command IDs); on any
    // error, best-effort release the hold so the worker is never stranded.
    let post_hold: Result<()> = (|| {
        if operation.in_place_held_command_ids.is_empty() && !held_ids.is_empty() {
            operation.in_place_held_command_ids = held_ids;
            operation.updated_at = now();
            persist_op(operation)?;
        }
        // 3. Restart only the harness under the new launch configuration.
        if !operation.in_place_harness_restarted {
            let launch_config = launch_config
                .context("in-place Move requires a destination WorkerLaunchConfig")?;
            relay.send_restart_harness(&operation.operation_id, &expected_store_id, launch_config)?;
            operation.in_place_harness_restarted = true;
            operation.updated_at = now();
            persist_op(operation)?;
        }

        // 4. Apply the queue choice while dispatch is still held.
        match operation.queue {
            ResumeQueueDisposition::Discard => {
                if !operation.in_place_held_command_ids.is_empty() {
                    relay.send_discard_held(
                        &operation.operation_id,
                        &expected_store_id,
                        operation.in_place_held_command_ids.clone(),
                    )?;
                }
            }
            ResumeQueueDisposition::Start => {
                if let Some((cmd_id, command)) = continuation {
                    ensure!(
                        operation
                            .in_place_continuation_command_id
                            .as_deref()
                            .is_none_or(|recorded| recorded == cmd_id),
                        "in-place Move continuation command ID mismatch"
                    );
                    relay.send_submit_front(
                        &operation.operation_id,
                        &expected_store_id,
                        cmd_id,
                        command,
                    )?;
                }
            }
        }

        // 5. Durable transition `DispatchReleased` in one SQLite transaction, THEN
        //    release the hold, THEN settle `Completed`.
        operation.phase = MovePhase::DispatchReleased;
        operation.queue_admission_started = true;
        operation.updated_at = now();
        commit_dispatch_released(operation)?;

        relay.send_release(&operation.operation_id, &expected_store_id)?;
        Ok(())
    })();

    if let Err(err) = post_hold {
        let _ = relay.send_release(&operation.operation_id, &expected_store_id);
        return Err(err);
    }

    operation.phase = MovePhase::Completed;
    operation.queue_admission_finished = true;
    operation.updated_at = now();
    persist_op(operation)?;

    if matches!(
        operation.quota_continuation,
        Some(QuotaMoveContinuation::ResumeGoal | QuotaMoveContinuation::Prompt)
    ) {
        notices.push("Resumed work in place without stopping worker processes.".to_owned());
    }

    Ok(notices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::RelayCommand;
    use mj_core::state::{MoveSelection, ResumeQueueDisposition};

    struct FakeInPlaceRelay {
        protocol: u32,
        store_id: String,
        held: Vec<String>,
        restarted: usize,
        discarded: Vec<String>,
        submitted_front: Vec<String>,
        released: usize,
    }

    impl FakeInPlaceRelay {
        fn new(protocol: u32, store_id: &str, pending: &[&str]) -> Self {
            Self {
                protocol,
                store_id: store_id.to_owned(),
                held: pending.iter().map(|s| (*s).to_owned()).collect(),
                restarted: 0,
                discarded: Vec::new(),
                submitted_front: Vec::new(),
                released: 0,
            }
        }
    }

    impl InPlaceMoveRelay for FakeInPlaceRelay {
        fn protocol_version(&self) -> u32 {
            self.protocol
        }

        fn store_id(&self) -> &str {
            &self.store_id
        }

        fn send_hold(&mut self, _op_id: &str, expected_store_id: &str) -> Result<Vec<String>> {
            ensure!(self.store_id == expected_store_id, "store_id mismatch");
            Ok(self.held.clone())
        }

        fn send_restart_harness(
            &mut self,
            _op_id: &str,
            expected_store_id: &str,
            _launch_config: Box<WorkerLaunchConfig>,
        ) -> Result<()> {
            ensure!(self.store_id == expected_store_id, "store_id mismatch");
            self.restarted += 1;
            Ok(())
        }

        fn send_discard_held(
            &mut self,
            _op_id: &str,
            expected_store_id: &str,
            command_ids: Vec<String>,
        ) -> Result<()> {
            ensure!(self.store_id == expected_store_id, "store_id mismatch");
            self.discarded.extend(command_ids);
            Ok(())
        }

        fn send_submit_front(
            &mut self,
            _op_id: &str,
            expected_store_id: &str,
            command_id: String,
            _command: RelayCommand,
        ) -> Result<()> {
            ensure!(self.store_id == expected_store_id, "store_id mismatch");
            self.submitted_front.push(command_id);
            Ok(())
        }

        fn send_release(&mut self, _op_id: &str, expected_store_id: &str) -> Result<()> {
            ensure!(self.store_id == expected_store_id, "store_id mismatch");
            self.released += 1;
            Ok(())
        }
    }

    fn sample_in_place_op(queue: ResumeQueueDisposition) -> MoveOperation {
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
            in_place: true,
            operation_id: "op-in-place-1".to_owned(),
            selection: MoveSelection {
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: false,
                session_id: "sess-1".to_owned(),
                profile_id: Some("p2".to_owned()),
                target_template_id: Some("local".to_owned()),
                additional_mounts: None,
                resource_allocation: None,
                subagents: None,
                oracle: None,
                oracle_choice: None,
            },
            source_profile_id: "p1".to_owned(),
            source_target_template_id: "local".to_owned(),
            source_target: None,
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: String::new(),
            checkpoint: None,
            recovery_session: None,
            queue,
            phase: MovePhase::Capturing,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-11-21T00:00:00Z".to_owned(),
            updated_at: "2026-11-21T00:00:00Z".to_owned(),
            error: None,
        }
    }

    #[test]
    fn pre_release_worker_refuses_manual_move_naming_restart_command() {
        let err = ensure_worker_supports_in_place_move("sess-42", 80, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mj restart --session sess-42"), "{msg}");
        assert!(msg.contains("Restart agent"), "{msg}");
    }

    #[test]
    fn pre_release_worker_skips_automatic_quota_move_with_notice() {
        let err = ensure_worker_supports_in_place_move("sess-42", 80, true).unwrap_err();
        assert!(err.to_string().contains("Waiting for quota reset"));
    }

    #[test]
    fn in_place_move_discard_and_deferred_mount_notice() {
        let mut op = sample_in_place_op(ResumeQueueDisposition::Discard);
        op.in_place_harness_restarted = true;
        op.deferred_environment_changes.push("Additional mounts".to_owned());
        let mut relay = FakeInPlaceRelay::new(81, "store-A", &["cmd-1", "cmd-2"]);
        let mut committed_release = false;

        let notices = run_in_place_move_state_machine(
            &mut op,
            &mut relay,
            None,
            None,
            |_| Ok(()),
            |saved| {
                assert_eq!(saved.phase, MovePhase::DispatchReleased);
                committed_release = true;
                Ok(())
            },
        )
        .unwrap();

        assert!(committed_release);
        assert_eq!(op.phase, MovePhase::Completed);
        assert_eq!(relay.discarded, vec!["cmd-1", "cmd-2"]);
        assert_eq!(relay.released, 1);
        assert!(notices
            .iter()
            .any(|n| n == "Additional mounts takes effect when this session's environment is next recreated"));
    }

    #[test]
    fn in_place_move_submit_front_and_dispatch_released_recovery() {
        let mut op = sample_in_place_op(ResumeQueueDisposition::Start);
        op.in_place_harness_restarted = true;
        let mut relay = FakeInPlaceRelay::new(81, "store-A", &["cmd-existing"]);

        run_in_place_move_state_machine(
            &mut op,
            &mut relay,
            None,
            Some((
                "cont-1".to_owned(),
                RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                        "continue",
                    )],
                },
            )),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(relay.submitted_front, vec!["cont-1"]);
        assert_eq!(relay.released, 1);

        // Simulate recovery at DispatchReleased: must NOT resubmit or discard,
        // only idempotently release and settle.
        op.phase = MovePhase::DispatchReleased;
        run_in_place_move_state_machine(
            &mut op,
            &mut relay,
            None,
            Some((
                "cont-1".to_owned(),
                RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                        "continue",
                    )],
                },
            )),
            |_| Ok(()),
            |_| panic!("must not re-commit DispatchReleased"),
        )
        .unwrap();
        assert_eq!(relay.submitted_front.len(), 1);
        assert_eq!(relay.released, 2);
        assert_eq!(op.phase, MovePhase::Completed);
    }

    #[test]
    fn in_place_move_store_id_mismatch_refuses_without_mutating_replacement_store() {
        let mut op = sample_in_place_op(ResumeQueueDisposition::Discard);
        op.in_place_expected_store_id = Some("store-OLD".to_owned());
        op.in_place_held_command_ids = vec!["cmd-1".to_owned()];
        op.in_place_harness_restarted = true;
        let mut relay = FakeInPlaceRelay::new(81, "store-NEW", &["cmd-99"]);

        let err = run_in_place_move_state_machine(
            &mut op,
            &mut relay,
            None,
            None,
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("store-OLD"));
        assert!(err.to_string().contains("store-NEW"));
        assert_eq!(op.phase, MovePhase::Failed);
        assert!(relay.discarded.is_empty());
        assert_eq!(relay.released, 0);
    }

    #[test]
    fn in_place_move_failure_after_hold_releases_dispatch_hold() {
        let mut op = sample_in_place_op(ResumeQueueDisposition::Discard);
        // `in_place_harness_restarted == false` with `launch_config == None`
        // forces a failure AFTER `send_hold` succeeds.
        let mut relay = FakeInPlaceRelay::new(81, "store-A", &["cmd-1"]);

        let err = run_in_place_move_state_machine(
            &mut op,
            &mut relay,
            None,
            None,
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("WorkerLaunchConfig"), "{err}");
        assert_eq!(
            relay.released, 1,
            "dispatch hold must be released on post-hold failure so the worker is never stranded"
        );

        // Failing while persisting `in_place_held_command_ids` right after
        // `send_hold` must also release the dispatch hold.
        let mut op2 = sample_in_place_op(ResumeQueueDisposition::Discard);
        let mut relay2 = FakeInPlaceRelay::new(81, "store-A", &["cmd-1"]);
        let mut persist_calls = 0_usize;
        let err2 = run_in_place_move_state_machine(
            &mut op2,
            &mut relay2,
            None,
            None,
            |_| {
                persist_calls += 1;
                if persist_calls == 2 {
                    anyhow::bail!("simulated persist held ids failure");
                }
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(
            err2.to_string().contains("simulated persist held ids failure"),
            "{err2}"
        );
        assert_eq!(
            relay2.released, 1,
            "dispatch hold must be released when persisting held command IDs fails"
        );
    }
}
