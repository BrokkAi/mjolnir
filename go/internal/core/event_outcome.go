// Port of mj-core/src/event_outcome.rs.
//
// Shared semantic outcomes. Diagnostic prose and command IDs never classify
// a result.

package core

// OutcomeReason is why a turn or command ended as it did. The Rust default
// is LegacyUnclassified.
type OutcomeReason string

const (
	OutcomeReasonControllerDisconnected OutcomeReason = "controller_disconnected"
	OutcomeReasonOwnerLostOnRestart     OutcomeReason = "owner_lost_on_restart"
	OutcomeReasonWorkerRestarted        OutcomeReason = "worker_restarted"
	OutcomeReasonRuntimeStopped         OutcomeReason = "runtime_stopped"
	OutcomeReasonRuntimeFailure         OutcomeReason = "runtime_failure"
	OutcomeReasonRequestedCancellation  OutcomeReason = "requested_cancellation"
	OutcomeReasonAdmissionRejected      OutcomeReason = "admission_rejected"
	OutcomeReasonCommandFailed          OutcomeReason = "command_failed"
	OutcomeReasonProviderFailure        OutcomeReason = "provider_failure"
	OutcomeReasonQuotaLimit             OutcomeReason = "quota_limit"
	OutcomeReasonUnrecognizedStopReason OutcomeReason = "unrecognized_stop_reason"
	OutcomeReasonNonzeroExit            OutcomeReason = "nonzero_exit"
	OutcomeReasonSignaled               OutcomeReason = "signaled"
	OutcomeReasonTimedOut               OutcomeReason = "timed_out"
	OutcomeReasonStartupFailed          OutcomeReason = "startup_failed"
	OutcomeReasonLifecycleFailed        OutcomeReason = "lifecycle_failed"
	OutcomeReasonRuntimeUnavailable     OutcomeReason = "runtime_unavailable"
	OutcomeReasonCheckpointFailed       OutcomeReason = "checkpoint_failed"
	OutcomeReasonCheckpointDeferred     OutcomeReason = "checkpoint_deferred"
	OutcomeReasonControllerRestarted    OutcomeReason = "controller_restarted"
	OutcomeReasonLegacyUnclassified     OutcomeReason = "legacy_unclassified"
)

func (r OutcomeReason) ExpectedCancellation() bool {
	switch r {
	case OutcomeReasonControllerDisconnected, OutcomeReasonOwnerLostOnRestart, OutcomeReasonRequestedCancellation:
		return true
	}
	return false
}

type TurnResultKind string

const (
	TurnResultKindCompleted     TurnResultKind = "completed"
	TurnResultKindInputRequired TurnResultKind = "input_required"
	TurnResultKindCancelled     TurnResultKind = "cancelled"
	TurnResultKindRejected      TurnResultKind = "rejected"
	TurnResultKindInterrupted   TurnResultKind = "interrupted"
	TurnResultKindFailed        TurnResultKind = "failed"
)

type TurnResult struct {
	Kind       TurnResultKind `json:"kind"`
	Reason     *OutcomeReason `json:"reason,omitempty"`
	Message    *string        `json:"message,omitempty"`
	StopReason *string        `json:"stop_reason,omitempty"`
}

// turnOutcomeKindResult is TurnOutcomeKind::result.
func turnOutcomeKindResult(kind TurnOutcomeKind) TurnResult {
	reasonOf := func(reason OutcomeReason) *OutcomeReason { return &reason }
	switch v := kind.(type) {
	case TurnOutcomeKindCompleted:
		var result TurnResult
		switch ClassifyPromptCompletion(v.StopReason) {
		case PromptCompletionFinished:
			result.Kind = TurnResultKindCompleted
		case PromptCompletionInputRequired:
			result.Kind = TurnResultKindInputRequired
		case PromptCompletionCancelled:
			result.Kind, result.Reason = TurnResultKindCancelled, reasonOf(OutcomeReasonRequestedCancellation)
		case PromptCompletionQuotaLimit:
			result.Kind, result.Reason = TurnResultKindFailed, reasonOf(OutcomeReasonQuotaLimit)
		default:
			result.Kind, result.Reason = TurnResultKindFailed, reasonOf(OutcomeReasonUnrecognizedStopReason)
		}
		stopReason := v.StopReason
		if result.Kind == TurnResultKindFailed {
			message := stopReason
			result.Message = &message
		}
		result.StopReason = &stopReason
		return result
	case TurnOutcomeKindRejected:
		result := TurnResult{Kind: TurnResultKindFailed, Reason: reasonOf(OutcomeReasonLegacyUnclassified)}
		if v.Reason == nil || *v.Reason == OutcomeReasonAdmissionRejected {
			result.Kind = TurnResultKindRejected
		}
		if v.Reason != nil {
			result.Reason = reasonOf(*v.Reason)
		}
		message := v.Message
		result.Message = &message
		return result
	case TurnOutcomeKindInterrupted:
		result := TurnResult{Kind: TurnResultKindInterrupted, Reason: reasonOf(OutcomeReasonLegacyUnclassified)}
		if v.Reason != nil {
			if v.Reason.ExpectedCancellation() {
				result.Kind = TurnResultKindCancelled
			}
			result.Reason = reasonOf(*v.Reason)
		}
		message := v.Message
		result.Message = &message
		return result
	}
	panic("turnOutcomeKindResult: unknown TurnOutcomeKind")
}

// RequiresInput interprets a completed reply using the worker's assessment
// of that turn. Harness questions remain authoritative when no usable
// assessment exists.
func (o *MaterializedTurnOutcome) RequiresInput(assessment *TurnAssessment) bool {
	panic("unported: mj-core/src/event_outcome.rs MaterializedTurnOutcome::requires_input")
}

func (o *MaterializedTurnOutcome) Result() TurnResult {
	result := o.Outcome.Result()
	if o.Diagnostic != nil {
		message := o.Diagnostic.Message
		result.Message = &message
		if result.Kind == TurnResultKindFailed && (result.Reason == nil || *result.Reason != OutcomeReasonQuotaLimit) {
			reason := OutcomeReasonProviderFailure
			result.Reason = &reason
		}
	}
	return result
}

type CommandOwner string

const (
	CommandOwnerWorker CommandOwner = "worker"
	CommandOwnerDaemon CommandOwner = "daemon"
)

type CommandResultKind string

const (
	CommandResultKindSucceeded CommandResultKind = "succeeded"
	CommandResultKindCancelled CommandResultKind = "cancelled"
	CommandResultKindRejected  CommandResultKind = "rejected"
	CommandResultKindFailed    CommandResultKind = "failed"
)

type CommandResult struct {
	Owner             CommandOwner      `json:"owner"`
	CommandID         string            `json:"command_id"`
	CommandKind       string            `json:"command_kind"`
	Outcome           CommandResultKind `json:"outcome"`
	Reason            *OutcomeReason    `json:"reason,omitempty"`
	Message           *string           `json:"message,omitempty"`
	RelatedCommandIDs []string          `json:"related_command_ids,omitempty"`
}

// CommandResultWorker is CommandResult::worker.
func CommandResultWorker(commandID string, command RelayCommandKind, outcome CommandResultKind, reason *OutcomeReason, message *string) CommandResult {
	return CommandResult{
		Owner:       CommandOwnerWorker,
		CommandID:   commandID,
		CommandKind: command.AsStr(),
		Outcome:     outcome,
		Reason:      reason,
		Message:     message,
	}
}

// CommandResultCompleted is CommandResult::completed.
func CommandResultCompleted(commandID string, command RelayCommandKind, outcome RelayCommandOutcome) CommandResult {
	panic("unported: mj-core/src/event_outcome.rs CommandResult::completed")
}

// ApiTurnOutcome: the public turn result keeps raw provider evidence but
// gives outcome semantic meaning.
type ApiTurnOutcome struct {
	CommandID         string          `json:"command_id"`
	AcceptedOrdinal   *uint64         `json:"accepted_ordinal,omitempty"`
	TurnStartPosition *uint64         `json:"turn_start_position,omitempty"`
	CompletedOrdinal  uint64          `json:"completed_ordinal"`
	CompletedAtMs     int64           `json:"completed_at_ms"`
	Outcome           TurnResult      `json:"outcome"`
	Diagnostic        *TurnDiagnostic `json:"diagnostic,omitempty"`
	Usage             *TokenUsage     `json:"usage,omitempty"`
}

// ApiTurnOutcomeFrom is From<&MaterializedTurnOutcome> for ApiTurnOutcome.
func ApiTurnOutcomeFrom(turn *MaterializedTurnOutcome) ApiTurnOutcome {
	return ApiTurnOutcome{
		CommandID:         turn.CommandID,
		AcceptedOrdinal:   turn.AcceptedOrdinal,
		TurnStartPosition: turn.TurnStartPosition,
		CompletedOrdinal:  turn.CompletedOrdinal,
		CompletedAtMs:     turn.CompletedAtMs,
		Outcome:           turn.Result(),
		Diagnostic:        turn.Diagnostic,
		Usage:             turn.Usage,
	}
}
