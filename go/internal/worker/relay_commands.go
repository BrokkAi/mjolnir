// Port of mj-worker/src/relay/commands.rs.
//
// The relay's durable command queue: acceptance, the claim scheduler,
// terminal receipts, mailbox delivery and the lifecycle barriers.

package worker

import (
	"fmt"
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func validateIdentifier(value, name string) error {
	valid := len(value) >= 8 && len(value) <= 128
	for _, character := range value {
		if !valid {
			break
		}
		valid = character < 0x80 && (character >= 'a' && character <= 'z' ||
			character >= 'A' && character <= 'Z' ||
			character >= '0' && character <= '9' ||
			character == '-' || character == '_')
	}
	if !valid {
		return fmt.Errorf("invalid %s", name)
	}
	return nil
}

func (r *DurableRelay) returnBoundarySteerAfterTurnEnd(activePromptID string) error {
	panic("unported: mj-worker/src/relay/commands.rs return_boundary_steer_after_turn_end")
}

func (r *DurableRelay) turnControls() core.TurnControls {
	panic("unported: mj-worker/src/relay/commands.rs turn_controls")
}

// validateTurnControl returns the refusal text as an error (Rust's
// Result<(), String>).
func (r *DurableRelay) validateTurnControl(command core.RelayCommand) error {
	panic("unported: mj-worker/src/relay/commands.rs validate_turn_control")
}

func (r *DurableRelay) DeliverAgentMessage(source, key, text string) error {
	panic("unported: mj-worker/src/relay/commands.rs deliver_agent_message")
}

// submitCommand accepts a command. The protocol error is the command's
// refusal (Rust's inner Err); err is a failure to apply it (the outer Err).
func (r *DurableRelay) submitCommand(commandID string, command core.RelayCommand) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	panic("unported: mj-worker/src/relay/commands.rs submit_command")
}

// recoveryFloorRejection is the refusal of an AdvanceRecoveryFloor to
// through, if any.
func (r *DurableRelay) recoveryFloorRejection(through *core.RelayCursor) (string, bool, error) {
	panic("unported: mj-worker/src/relay/commands.rs recovery_floor_rejection")
}

func (r *DurableRelay) isQueuedRecoveryCancel(commandID string) bool {
	if r.snapshot.ActivePrompt != nil || r.snapshot.CancellingPromptID == nil ||
		!core.IsRestartContinuation(*r.snapshot.CancellingPromptID) {
		return false
	}
	dispatch, ok := r.snapshot.Dispatches[commandID]
	if !ok {
		return false
	}
	_, cancel := dispatch.Command.(core.RelayCommandCancelTurn)
	return cancel
}

func (r *DurableRelay) finishRelayLocalCommand(commandID string) error {
	panic("unported: mj-worker/src/relay/commands.rs finish_relay_local_command")
}

func (r *DurableRelay) ClaimPendingCommands(acpSessionConfigured bool) ([]core.ClaimedRelayCommand, error) {
	return r.ClaimPendingCommandsUpTo(acpSessionConfigured, math.MaxInt)
}

func (r *DurableRelay) ClaimPendingCommandsUpTo(acpSessionConfigured bool, maximum int) ([]core.ClaimedRelayCommand, error) {
	panic("unported: mj-worker/src/relay/commands.rs claim_pending_commands_up_to")
}

func (r *DurableRelay) submitAutomaticSteer() error {
	panic("unported: mj-worker/src/relay/commands.rs submit_automatic_steer")
}

// automaticSteerTarget is the (active prompt, queued prompt) pair the relay
// would steer on its own.
func (r *DurableRelay) automaticSteerTarget() (activePromptID, queuedPromptID string, ok bool) {
	if _, active := r.automaticSteerActivePrompt(); !active {
		return "", "", false
	}
	primary := r.turnControls().Primary
	if primary == nil {
		return "", "", false
	}
	steer, isSteer := primary.Command.(core.RelayCommandSteer)
	if !isSteer {
		return "", "", false
	}
	return steer.ActivePromptID, steer.QueuedPromptID, true
}

func (r *DurableRelay) automaticSteerActivePrompt() (string, bool) {
	panic("unported: mj-worker/src/relay/commands.rs automatic_steer_active_prompt")
}

func (r *DurableRelay) DispatchCheckpointOnly() error {
	if !r.checkpointOnly {
		return fmt.Errorf("worker is not in checkpoint-only mode")
	}
	return r.dispatchRelayLocalLifecycle()
}

func (r *DurableRelay) DispatchPreparationLifecycle() error {
	if r.checkpointOnly {
		return fmt.Errorf("checkpoint-only workers use dispatch_checkpoint_only")
	}
	return r.dispatchRelayLocalLifecycle()
}

func (r *DurableRelay) dispatchRelayLocalLifecycle() error {
	panic("unported: mj-worker/src/relay/commands.rs dispatch_relay_local_lifecycle")
}

func (r *DurableRelay) ClaimPendingUserShellCommandsUpTo(maximumRuns int) ([]core.ClaimedRelayCommand, error) {
	panic("unported: mj-worker/src/relay/commands.rs claim_pending_user_shell_commands_up_to")
}

func (r *DurableRelay) startQueuedControls(commandIDs []string) error {
	for _, commandID := range commandIDs {
		id := commandID
		if _, err := r.appendRelayEvent(&id, core.RelayObservationCommandStarted{
			CommandID:   commandID,
			StartedAtMs: core.EpochMillis(),
		}); err != nil {
			return err
		}
	}
	return nil
}

func (r *DurableRelay) queuedControlsBefore(beforeOrdinal uint64) []string {
	panic("unported: mj-worker/src/relay/commands.rs queued_controls_before")
}

func (r *DurableRelay) effectfulCommandInProgress() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		if (core.RelayCommandIsEffectfulACP(dispatch.Command) || core.RelayCommandIsEffectfulUserShell(dispatch.Command)) &&
			(dispatch.State == core.RelayDispatchStatePending || dispatch.State == core.RelayDispatchStateInFlight) {
			return true
		}
	}
	return false
}

// nextQueuedCheckpoint is the earliest accepted queued checkpoint barrier
// and its accepted ordinal.
func (r *DurableRelay) nextQueuedCheckpoint() (string, uint64, bool) {
	panic("unported: mj-worker/src/relay/commands.rs next_queued_checkpoint")
}

func (r *DurableRelay) quotaRecoveryAdmissible(user, completed string) bool {
	panic("unported: mj-worker/src/relay/commands.rs quota_recovery_admissible")
}

// authenticationRetryIntent is the accepted ordinal and the rejected login
// of a retry the worker still owes.
func (r *DurableRelay) authenticationRetryIntent() (uint64, string, bool) {
	panic("unported: mj-worker/src/relay/commands.rs authentication_retry_intent")
}

func (r *DurableRelay) authenticationRetryGateOpen() bool {
	facts := r.ActivityFacts()
	return core.CanSubmit(&facts) && !core.DriverPresent(&facts) && !r.pendingCheckpointBarrier()
}

func (r *DurableRelay) submitAuthenticationRetry(expected uint64, login string) (bool, error) {
	panic("unported: mj-worker/src/relay/commands.rs submit_authentication_retry")
}

func (r *DurableRelay) CapacityRetryDeadline() (int64, bool) {
	retry := r.snapshot.CapacityRetry
	if retry == nil || retry.Submitted {
		return 0, false
	}
	return retry.RetryAtMs, true
}

func (r *DurableRelay) SubmitDueCapacityRetry(nowMs int64) (bool, error) {
	panic("unported: mj-worker/src/relay/commands.rs submit_due_capacity_retry")
}

func (r *DurableRelay) RecordCommandCompleted(commandID string, outcome core.RelayCommandOutcome) (uint64, error) {
	panic("unported: mj-worker/src/relay/commands.rs record_command_completed")
}

func (r *DurableRelay) RecordCommandRejected(commandID string, reason core.OutcomeReason, message string) (uint64, error) {
	panic("unported: mj-worker/src/relay/commands.rs record_command_rejected")
}

func (r *DurableRelay) RecordCommandInterrupted(commandID string, reason core.OutcomeReason, message string) (uint64, error) {
	panic("unported: mj-worker/src/relay/commands.rs record_command_interrupted")
}

func (r *DurableRelay) restartContinuationFor(commandID string) (string, bool) {
	dispatch := r.snapshot.Dispatches[commandID]
	if core.RelayCommandKindOf(dispatch.Command) != core.RelayCommandKindPrompt ||
		dispatch.State != core.RelayDispatchStateInFlight ||
		(r.snapshot.CancellingPromptID != nil && *r.snapshot.CancellingPromptID == commandID) {
		return "", false
	}
	return r.snapshot.RestartContinuationID(commandID), true
}

func (r *DurableRelay) RecordCheckpointReady(commandID string) (uint64, error) {
	panic("unported: mj-worker/src/relay/commands.rs record_checkpoint_ready")
}

// CancelCheckpointBarrierOnDisconnect returns the ordinal of the event that
// cancelled the barrier, if one was recorded.
func (r *DurableRelay) CancelCheckpointBarrierOnDisconnect(commandID string) (uint64, bool, error) {
	panic("unported: mj-worker/src/relay/commands.rs cancel_checkpoint_barrier_on_disconnect")
}

func (r *DurableRelay) requireDispatch(commandID string) error {
	if _, ok := r.snapshot.Dispatches[commandID]; !ok {
		return fmt.Errorf("unknown relay command %s", commandID)
	}
	return nil
}

func (r *DurableRelay) requireInFlight(commandID string) error {
	dispatch, ok := r.snapshot.Dispatches[commandID]
	if !ok {
		return fmt.Errorf("unknown relay command %s", commandID)
	}
	if dispatch.State != core.RelayDispatchStateInFlight {
		return fmt.Errorf("relay command %s is not in flight", commandID)
	}
	return nil
}

func (r *DurableRelay) closeRequested() bool {
	_, ok := r.pendingCloseBarrierID()
	return ok
}

func (r *DurableRelay) ClosePending() bool {
	return r.closeRequested()
}

func (r *DurableRelay) SealedByClose() bool {
	return r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed
}

func (r *DurableRelay) UndeliveredCancellationOrClosePending() bool {
	if r.closeRequested() {
		return true
	}
	for _, dispatch := range r.snapshot.Dispatches {
		if isCancellationCommand(dispatch.Command) &&
			(dispatch.State == core.RelayDispatchStateQueued || dispatch.State == core.RelayDispatchStatePending) {
			return true
		}
	}
	return false
}

// isCancellationCommand is the set of commands Rust matches as Cancel,
// CancelTurn, CancelTurnFor and ApplyQueuedFor.
func isCancellationCommand(command core.RelayCommand) bool {
	switch command.(type) {
	case core.RelayCommandCancel, core.RelayCommandCancelTurn, core.RelayCommandCancelTurnFor, core.RelayCommandApplyQueuedFor:
		return true
	}
	return false
}

func (r *DurableRelay) pendingCloseBarrierID() (string, bool) {
	panic("unported: mj-worker/src/relay/commands.rs pending_close_barrier_id")
}

// promoteNextQueuedCommand returns the ordinal of the promotion it
// journaled, if any.
func (r *DurableRelay) promoteNextQueuedCommand() (uint64, bool, error) {
	panic("unported: mj-worker/src/relay/commands.rs promote_next_queued_command")
}

func (r *DurableRelay) mailboxWakeIsAllowed() bool {
	wake := false
	for _, event := range r.snapshot.PendingMailboxEvents {
		if event.Wake {
			wake = true
			break
		}
	}
	if !wake {
		return false
	}
	facts := r.ActivityFacts()
	return core.CanSubmit(&facts) && !r.turnCancellationPending()
}

func (r *DurableRelay) turnCancellationPending() bool {
	if r.snapshot.CancellingPromptID != nil {
		return true
	}
	for _, dispatch := range r.snapshot.Dispatches {
		if isCancellationCommand(dispatch.Command) &&
			(dispatch.State == core.RelayDispatchStateQueued ||
				dispatch.State == core.RelayDispatchStatePending ||
				dispatch.State == core.RelayDispatchStateInFlight) {
			return true
		}
	}
	return false
}

func (r *DurableRelay) enqueueMailboxWake() error {
	panic("unported: mj-worker/src/relay/commands.rs enqueue_mailbox_wake")
}

// pendingHookSteering is the steering a mailbox hook for hookEvent would
// deliver, with the queued prompt's hidden context.
func (r *DurableRelay) pendingHookSteering(hookEvent string) (core.MailboxHookSteering, string, bool) {
	panic("unported: mj-worker/src/relay/commands.rs pending_hook_steering")
}

func (r *DurableRelay) DrainMailbox(hookEvent string) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/relay/commands.rs drain_mailbox")
}

func (r *DurableRelay) ackMailbox(leaseID string) (core.RelayResponsePayload, error) {
	panic("unported: mj-worker/src/relay/commands.rs ack_mailbox")
}

func (r *DurableRelay) expireMailboxHookLeaseAt(nowMs int64) error {
	if lease := r.snapshot.MailboxHookLease; lease != nil && lease.ExpiresAtMs <= nowMs {
		return r.returnMailboxHookLease(core.MailboxHookLeaseReturnReasonTimeout)
	}
	return nil
}

func (r *DurableRelay) MailboxHookLeaseDeadline() (int64, bool) {
	if lease := r.snapshot.MailboxHookLease; lease != nil {
		return lease.ExpiresAtMs, true
	}
	return 0, false
}

func (r *DurableRelay) ExpireMailboxHookLeaseAndPromote(nowMs int64) (bool, error) {
	if deadline, ok := r.MailboxHookLeaseDeadline(); !ok || deadline > nowMs {
		return false, nil
	}
	if err := r.expireMailboxHookLeaseAt(nowMs); err != nil {
		return false, err
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return false, err
	}
	return true, nil
}

func (r *DurableRelay) returnMailboxHookLease(reason core.MailboxHookLeaseReturnReason) error {
	lease := r.snapshot.MailboxHookLease
	if lease == nil {
		return nil
	}
	_, err := r.appendRelayEvent(nil, core.RelayObservationMailboxHookLeaseReturned{LeaseID: lease.LeaseID, Reason: reason})
	return err
}

func (r *DurableRelay) unfinishedDispatch() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		switch dispatch.State {
		case core.RelayDispatchStateQueued, core.RelayDispatchStatePending, core.RelayDispatchStateInFlight:
			return true
		}
	}
	return false
}

func (r *DurableRelay) configurationDispatchInProgress() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		if dispatch.State != core.RelayDispatchStatePending && dispatch.State != core.RelayDispatchStateInFlight {
			continue
		}
		switch dispatch.Command.(type) {
		case core.RelayCommandSetConfig, core.RelayCommandSetSessionMode, core.RelayCommandRestoreExecutionMode:
			return true
		}
	}
	return false
}

func (r *DurableRelay) pendingCheckpointBarrier() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		if _, checkpoint := dispatch.Command.(core.RelayCommandBeginCheckpoint); !checkpoint {
			continue
		}
		switch dispatch.State {
		case core.RelayDispatchStateQueued, core.RelayDispatchStatePending, core.RelayDispatchStateInFlight:
			return true
		}
	}
	return false
}

func mailboxPromptEligible(command core.RelayCommand) bool {
	prompt, ok := core.RelayCommandPromptBlocks(command)
	if !ok {
		return false
	}
	if _, _, isContext := core.ContextCommandOf(prompt); isContext {
		return false
	}
	return !core.PromptRequestsCompaction(prompt) && !core.PromptIsSlashCommand(prompt)
}
