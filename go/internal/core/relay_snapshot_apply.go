// Port of mj-core/src/relay/snapshot/apply.rs.
//
// Folding one relay event into the next snapshot.

package core

import (
	"cmp"
	"errors"
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"
)

// ObservationChangesState reports whether applying this observation moves
// durable relay state beyond the event frontier.
//
// Assistant text also updates bounded authorization evidence. Other
// transcript observations only advance the frontier. Every arm here mirrors
// an arm of ApplyRelayEvent;
// transcript_observations_move_nothing_but_the_frontier fails if the two
// ever disagree.
func ObservationChangesState(observation RelayObservation) bool {
	switch observation := observation.(type) {
	case RelayObservationQuestionTargetUpdated, RelayObservationQuestionLocatorFinished,
		RelayObservationManagedSubagentConfigured, RelayObservationWorkLivenessChanged,
		RelayObservationHandbackCheckStarted, RelayObservationTurnAssessmentUpdated,
		RelayObservationSteeringUnconfirmed, RelayObservationAgentInitialized, RelayObservationSessionOpened,
		RelayObservationSessionConfigured, RelayObservationSessionModesConfigured, RelayObservationCommandQueued,
		RelayObservationCommandStarted, RelayObservationCommandCompleted, RelayObservationRetryAssessmentStarted,
		RelayObservationRetryAssessmentResolved, RelayObservationCommandRejected,
		RelayObservationCommandInterrupted, RelayObservationMailboxHookLeaseCreated,
		RelayObservationMailboxHookLeaseReturned, RelayObservationMailboxEventsDelivered,
		RelayObservationConfigurationUpdated, RelayObservationCheckpointReady,
		// A restart ends any turn the harness started on its own, so it now
		// moves durable state instead of only the frontier.
		RelayObservationSessionRestarted, RelayObservationHarnessTurnStarted, RelayObservationHarnessTurnSettled,
		RelayObservationElicitationRequested, RelayObservationElicitationResolved,
		RelayObservationElicitationsCleared, RelayObservationClosing, RelayObservationClosed:
		return true
	case RelayObservationSessionUpdate:
		switch observation.Update.Kind {
		case acpschema.SessionUpdateKindAvailableCommandsUpdate, acpschema.SessionUpdateKindConfigOptionUpdate,
			acpschema.SessionUpdateKindCurrentModeUpdate, acpschema.SessionUpdateKindSessionInfoUpdate,
			acpschema.SessionUpdateKindAgentMessageChunk:
			return true
		}
		return false
	case RelayObservationNativeAgent:
		_, update := observation.Event.(NativeAgentEventUpdate)
		return !update
	case RelayObservationPermissionAutoApproved, RelayObservationSessionFault, RelayObservationWarning,
		RelayObservationUserShellOutput, RelayObservationTerminalOutput, RelayObservationNotice:
		return false
	}
	panic(fmt.Sprintf("unknown relay observation %T", observation))
}

// notificationIndex is the position of completedOrdinal in the stack's
// entries, or where it would be inserted (Rust's binary_search_by_key).
func notificationIndex(entries []NotificationStackEntry, completedOrdinal uint64) (int, bool) {
	return slices.BinarySearchFunc(entries, completedOrdinal, func(entry NotificationStackEntry, target uint64) int {
		return cmp.Compare(entry.CompletedOrdinal, target)
	})
}

func clearNotifications(snapshot *RelaySnapshot, ordinal uint64) {
	snapshot.PersonInputThrough = ordinal
	snapshot.Request = nil
	if len(snapshot.NotificationStack.Entries) > 0 {
		// A fresh slice: published snapshots may share the old one.
		snapshot.NotificationStack.Entries = []NotificationStackEntry{}
		snapshot.NotificationStack.Revision = ordinal
	}
}

func recordNotification(snapshot *RelaySnapshot, completedOrdinal uint64, completedAtMs int64, kind NotificationKind, message EntryMessage, ordinal uint64) {
	entry := NotificationStackEntry{
		CompletedAtMs:    completedAtMs,
		CompletedOrdinal: completedOrdinal,
		Kind:             kind,
		Message:          message,
	}
	index, found := notificationIndex(snapshot.NotificationStack.Entries, completedOrdinal)
	if found {
		current := snapshot.NotificationStack.Entries[index]
		if current == entry || (current.Kind == kind && entry.Message == EntryMessage(EntryMessageLocating{})) {
			return
		}
	}
	entries := slices.Clone(snapshot.NotificationStack.Entries)
	if found {
		entries[index] = entry
	} else {
		entries = slices.Insert(entries, index, entry)
	}
	snapshot.NotificationStack.Entries = entries
	snapshot.NotificationStack.Revision = ordinal
}

// settleQuestionTarget records the located question message, if any (target
// nil when none was found).
func settleQuestionTarget(snapshot *RelaySnapshot, completedOrdinal uint64, target *string, ordinal uint64) {
	if completedOrdinal <= snapshot.PersonInputThrough {
		return
	}
	index, found := notificationIndex(snapshot.NotificationStack.Entries, completedOrdinal)
	if found {
		entry := snapshot.NotificationStack.Entries[index]
		if entry.Kind != NotificationKindNeedsInput || entry.Message != EntryMessage(EntryMessageLocating{}) {
			return
		}
		var message EntryMessage = EntryMessageNone{}
		if target != nil {
			message = EntryMessageFound{Value: *target}
		}
		recordNotification(snapshot, completedOrdinal, entry.CompletedAtMs, NotificationKindNeedsInput, message, ordinal)
	}
	if snapshot.Request != nil && snapshot.Request.Question.CompletedOrdinal == completedOrdinal {
		request := *snapshot.Request
		request.QuestionMessage = cloneStringPtr(target)
		snapshot.Request = &request
	}
	if snapshot.Assessment != nil && snapshot.Assessment.CompletedOrdinal == completedOrdinal {
		assessment := *snapshot.Assessment
		assessment.QuestionMessage = cloneStringPtr(target)
		snapshot.Assessment = &assessment
	}
}

func cloneStringPtr(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

func claimWakeMailboxEvents(snapshot *RelaySnapshot, events []MailboxEvent, ordinal uint64) error {
	if !slices.ContainsFunc(events, func(event MailboxEvent) bool { return event.Wake }) {
		return errors.New("mailbox wake must carry at least one waking event")
	}
	uniqueKeys := map[string]bool{}
	for _, event := range events {
		uniqueKeys[event.Key] = true
	}
	if len(uniqueKeys) != len(events) {
		return errors.New("mailbox wake repeats an event key")
	}
	for _, event := range events {
		if !slices.ContainsFunc(snapshot.PendingMailboxEvents, func(pending MailboxEvent) bool {
			return reflect.DeepEqual(pending, event)
		}) {
			return errors.New("mailbox wake carries an event that is not pending")
		}
	}
	snapshot.PendingMailboxEvents = slices.DeleteFunc(slices.Clone(snapshot.PendingMailboxEvents), func(event MailboxEvent) bool {
		return uniqueKeys[event.Key]
	})
	retainDeliveredMailboxEventKeys(snapshot, slices.Sorted(maps.Keys(uniqueKeys)), ordinal)
	return nil
}

func retainDeliveredMailboxEventKeys(snapshot *RelaySnapshot, eventKeys []string, ordinal uint64) {
	delivered := maps.Clone(snapshot.DeliveredMailboxEventKeys)
	if delivered == nil {
		delivered = map[string]uint64{}
	}
	for _, key := range eventKeys {
		delivered[key] = ordinal
	}
	for len(delivered) > RelayRetryIDRetention {
		oldestKey, oldestOrdinal, first := "", uint64(0), true
		for key, keyOrdinal := range delivered {
			if first || keyOrdinal < oldestOrdinal || (keyOrdinal == oldestOrdinal && key < oldestKey) {
				oldestKey, oldestOrdinal, first = key, keyOrdinal, false
			}
		}
		delete(delivered, oldestKey)
	}
	snapshot.DeliveredMailboxEventKeys = delivered
}

func hookSteeringPrompt(snapshot *RelaySnapshot, steering *MailboxHookSteering) ([]acpschema.ContentBlock, error) {
	operation := snapshot.Steering
	if operation == nil {
		return nil, errors.New("hook steer names no active steering operation")
	}
	if operation.CommandID != steering.CommandID ||
		operation.ActivePromptID != steering.ActivePromptID ||
		operation.QueuedPromptID != steering.QueuedPromptID ||
		operation.Status != SteeringStatusPending ||
		snapshot.ActivePrompt == nil || snapshot.ActivePrompt.CommandID != steering.ActivePromptID {
		return nil, errors.New("hook steer no longer matches its active turn")
	}
	dispatch, ok := snapshot.Dispatches[steering.CommandID]
	if !ok {
		return nil, errors.New("hook steer names an unknown command")
	}
	steer, isSteer := dispatch.Command.(RelayCommandSteer)
	if dispatch.State != RelayDispatchStatePending || !isSteer ||
		steer.ActivePromptID != steering.ActivePromptID || steer.QueuedPromptID != steering.QueuedPromptID {
		return nil, errors.New("hook steer command is not waiting at the tool boundary")
	}
	if len(snapshot.QueuedPrompts) == 0 {
		return nil, errors.New("hook steer prompt is no longer queued")
	}
	queued := snapshot.QueuedPrompts[0]
	if queued.CommandID != steering.QueuedPromptID {
		return nil, errors.New("hook steer prompt is not at the queue head")
	}
	prompt, isPrompt := queued.Payload.(StoredQueuedRelayPayloadPrompt)
	if !isPrompt {
		return nil, errors.New("hook steer target is not a prompt")
	}
	if _, ok := SteeringTextContext(prompt.Prompt); !ok {
		return nil, errors.New("hook steer prompt cannot be represented as text context")
	}
	return slices.Clone(prompt.Prompt), nil
}

// setDispatchState updates one dispatch record, which a Go map holds by
// value; false when the command is unknown.
func setDispatchState(snapshot *RelaySnapshot, commandID string, state RelayDispatchState) bool {
	dispatch, ok := snapshot.Dispatches[commandID]
	if !ok {
		return false
	}
	dispatch.State = state
	snapshot.Dispatches[commandID] = dispatch
	return true
}

// updateHandled edits one ledger record in place; false when the command is
// not in the ledger.
func updateHandled(snapshot *RelaySnapshot, commandID string, edit func(*HandledRelayCommand)) bool {
	handled, ok := snapshot.HandledCommands[commandID]
	if !ok {
		return false
	}
	edit(&handled)
	snapshot.HandledCommands[commandID] = handled
	return true
}

func completeHookSteering(snapshot *RelaySnapshot, steering *MailboxHookSteering, ordinal uint64) error {
	if _, err := hookSteeringPrompt(snapshot, steering); err != nil {
		return err
	}
	if !setDispatchState(snapshot, steering.CommandID, RelayDispatchStateCompleted) {
		panic("validated hook steer command disappeared")
	}
	if !updateHandled(snapshot, steering.CommandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
		handled.Outcome = RelayCommandOutcomeSteered{QueuedCommandID: steering.QueuedPromptID}
	}) {
		return errors.New("hook steer is not in the command ledger")
	}
	operation := *snapshot.Steering
	operation.Status = SteeringStatusApplied
	operation.Message = nil
	snapshot.Steering = &operation

	queued := snapshot.QueuedPrompts[0]
	snapshot.QueuedPrompts = slices.Clone(snapshot.QueuedPrompts[1:])
	if !setDispatchState(snapshot, queued.CommandID, RelayDispatchStateCompleted) {
		return errors.New("hook steered an unknown queued prompt")
	}
	if !updateHandled(snapshot, queued.CommandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
	}) {
		return errors.New("hook steered prompt is not in the ledger")
	}
	if snapshot.PendingPromptContext != nil && snapshot.PendingPromptContext.AttachedCommandID != nil &&
		*snapshot.PendingPromptContext.AttachedCommandID == steering.QueuedPromptID {
		snapshot.PendingPromptContext = nil
	}
	snapshot.PendingUserShellContexts = slices.DeleteFunc(slices.Clone(snapshot.PendingUserShellContexts), func(context PendingUserShellContext) bool {
		return context.AttachedCommandID != nil && *context.AttachedCommandID == steering.QueuedPromptID
	})
	return nil
}

func ApplyRelayEvent(snapshot *RelaySnapshot, event *RelayEvent) error {
	if err := ValidateRelayEvent(snapshot.LatestOrdinal, snapshot.LatestDigest, event); err != nil {
		return err
	}
	if snapshot.HandledCommands == nil {
		snapshot.HandledCommands = map[string]HandledRelayCommand{}
	}
	if snapshot.Dispatches == nil {
		snapshot.Dispatches = map[string]RelayDispatchRecord{}
	}
	if snapshot.Config == nil {
		snapshot.Config = map[string]string{}
	}
	if err := applyObservation(snapshot, event); err != nil {
		return err
	}
	if steering := snapshot.Steering; steering != nil &&
		(steering.HoldsQueue() || steering.Status == SteeringStatusFailed) &&
		!slices.ContainsFunc(snapshot.QueuedPrompts, func(queued StoredQueuedRelayCommand) bool {
			return queued.CommandID == steering.QueuedPromptID
		}) {
		resolved := *steering
		resolved.Status = SteeringStatusResolved
		snapshot.Steering = &resolved
	}
	if id := snapshot.CancellingPromptID; id != nil {
		active := snapshot.ActivePrompt != nil && snapshot.ActivePrompt.CommandID == *id
		recoveryCancelPending := false
		if IsRestartContinuation(*id) {
			for _, dispatch := range snapshot.Dispatches {
				if _, cancelTurn := dispatch.Command.(RelayCommandCancelTurn); cancelTurn {
					switch dispatch.State {
					case RelayDispatchStateQueued, RelayDispatchStatePending, RelayDispatchStateInFlight:
						recoveryCancelPending = true
					}
				}
			}
		}
		if !active && !recoveryCancelPending {
			snapshot.CancellingPromptID = nil
		}
	}
	// The stop belongs to the autonomous turn it named; once that turn is
	// gone (settled, interrupted, or taken over by a prompt) nothing is left
	// to stop.
	if target := snapshot.CancellingHarnessTurn; target != nil {
		if turn := snapshot.AutonomousHarnessTurn(); turn == nil || turn.FirstOrdinal != target.FirstOrdinal {
			snapshot.CancellingHarnessTurn = nil
		}
	}
	applyAssessmentContext(snapshot, event)
	if _, err := snapshot.CompactTerminalCommands(); err != nil {
		return err
	}
	if completed, ok := event.Observation.(RelayObservationCommandCompleted); ok {
		switch completed.Outcome.(type) {
		case RelayCommandOutcomePrompt, RelayCommandOutcomeSteered:
			snapshot.NativeSessionUsed = true
			snapshot.RecordNativeUse(NativeSessionUseUsed)
		}
	}
	snapshot.LatestOrdinal = event.Ordinal
	snapshot.LatestDigest = event.Digest
	return nil
}

func applyObservation(snapshot *RelaySnapshot, event *RelayEvent) error {
	switch observation := event.Observation.(type) {
	case RelayObservationManagedSubagentConfigured:
		snapshot.ManagedSubagent = observation.Managed
	case RelayObservationWorkLivenessChanged:
		// A late recount for an older turn must not overwrite the current one.
		if snapshot.WorkLiveness != nil && snapshot.WorkLiveness.CompletedOrdinal == observation.Liveness.CompletedOrdinal {
			liveness := observation.Liveness
			snapshot.WorkLiveness = &liveness
		}
	case RelayObservationQuestionTargetUpdated:
		message := observation.QuestionMessage
		settleQuestionTarget(snapshot, observation.Question.CompletedOrdinal, &message, event.Ordinal)
	case RelayObservationQuestionLocatorFinished:
		settleQuestionTarget(snapshot, observation.CompletedOrdinal, observation.QuestionMessage, event.Ordinal)
	case RelayObservationHandbackCheckStarted:
		snapshot.HandbackChecks.Checked = insertSorted(snapshot.HandbackChecks.Checked, observation.AssignmentID)
	case RelayObservationTurnAssessmentUpdated:
		return applyTurnAssessmentUpdated(snapshot, event, &observation.Assessment)
	case RelayObservationSteeringUnconfirmed:
		if snapshot.Steering == nil {
			return errors.New("unknown steering operation")
		}
		if snapshot.Steering.CommandID != observation.CommandID {
			return errors.New("steering identity changed")
		}
		if snapshot.Steering.HoldsQueue() {
			steering := *snapshot.Steering
			steering.Status = SteeringStatusUnconfirmed
			message := observation.Message
			steering.Message = &message
			snapshot.Steering = &steering
		}
	case RelayObservationAgentInitialized:
		capabilities := observation.Capabilities
		snapshot.AgentCapabilities = &capabilities
		snapshot.AgentInfo = observation.AgentInfo
		snapshot.Runtime = nil
		if observation.Runtime != nil {
			snapshot.Runtime = &RuntimeReceipt{
				Identity:     *observation.Runtime,
				EventOrdinal: event.Ordinal,
				ObservedAtMs: event.RecordedAtMs,
			}
		}
	case RelayObservationSessionOpened:
		applySessionOpened(snapshot, event, &observation)
	case RelayObservationSessionConfigured:
		snapshot.ConfigOptions = relayNonNil(slices.Clone(observation.ConfigOptions))
		recordCatalogueMode(snapshot)
	case RelayObservationSessionModesConfigured:
		snapshot.Modes = observation.Modes
		mode := ConfigCurrentValue(snapshot.ConfigOptions, "mode")
		if mode == nil && observation.Modes != nil {
			current := string(observation.Modes.CurrentModeID)
			mode = &current
		}
		if mode != nil {
			// Initial negotiation prefers the ACP option over legacy modes,
			// as the live runtime does. Later CurrentModeUpdate is ordered.
			snapshot.Config["mode"] = *mode
		}
	case RelayObservationCommandQueued:
		return applyCommandQueued(snapshot, event, &observation)
	case RelayObservationCommandStarted:
		return applyCommandStarted(snapshot, &observation)
	case RelayObservationCommandCompleted:
		return applyCommandCompleted(snapshot, event, &observation)
	case RelayObservationRetryAssessmentStarted:
		handled, ok := snapshot.HandledCommands[observation.CommandID]
		if !ok {
			return fmt.Errorf("retry assessment for unknown command %s", observation.CommandID)
		}
		if !(handled.Command != nil && RelayCommandKindOf(handled.Command) == RelayCommandKindPrompt) &&
			!(handled.TerminalIdentity != nil && handled.TerminalIdentity.Prompt) {
			return errors.New("retry assessment requires a prompt command")
		}
		superseded := false
		for _, other := range snapshot.HandledCommands {
			if other.AcceptedOrdinal > handled.AcceptedOrdinal &&
				((other.Command != nil && cancelsCapacityRetry(other.Command)) ||
					(other.TerminalIdentity != nil && other.TerminalIdentity.CancelsCapacityRetry)) {
				superseded = true
			}
		}
		if !superseded {
			snapshot.RetryAssessment = &RetryAssessment{
				CommandID: observation.CommandID,
				Ordinal:   event.Ordinal,
				Evidence:  observation.Evidence,
			}
		}
	case RelayObservationRetryAssessmentResolved:
		if pending := snapshot.RetryAssessment; pending != nil &&
			pending.CommandID == observation.CommandID && pending.Ordinal == observation.AssessmentOrdinal {
			snapshot.RetryAssessment = nil
			if observation.Retryable {
				attempt := uint32(1)
				if retry := snapshot.CapacityRetry; retry != nil && retry.CommandID == observation.CommandID {
					attempt = saturatingAddU32(retry.Attempt, 1)
				}
				retry := NewCapacityRetry(attempt, event.Ordinal, event.RecordedAtMs)
				snapshot.CapacityRetry = &retry
			} else {
				snapshot.CapacityRetry = nil
			}
		}
	case RelayObservationCommandRejected:
		return applyCommandTerminated(snapshot, event, observation.CommandID, observation.Command, observation.Message, nil, nil, true)
	case RelayObservationCommandInterrupted:
		return applyCommandTerminated(snapshot, event, observation.CommandID, observation.Command, observation.Message,
			observation.ContinuationID, observation.Reason, false)
	case RelayObservationConfigurationUpdated:
		snapshot.Config[observation.Key] = observation.Value
		// Keep the accepted model/effort pair coherent however the change
		// arrived. A selector Hel applied for itself has no SetConfig command
		// to fold in afterwards, and a model change can retire the effort the
		// session had stored.
		AcceptedSessionConfigRecordCompleted(snapshot.Config, observation.Key, observation.Value, snapshot.ConfigOptions)
	case RelayObservationCheckpointReady:
		dispatch, ok := snapshot.Dispatches[observation.CommandID]
		if !ok {
			return fmt.Errorf("checkpoint ready for unknown command %s", observation.CommandID)
		}
		if _, barrier := dispatch.Command.(RelayCommandBeginCheckpoint); !barrier {
			return fmt.Errorf("checkpoint ready for non-barrier command %s", observation.CommandID)
		}
		if snapshot.CheckpointBarrier == nil || *snapshot.CheckpointBarrier != observation.CommandID {
			return errors.New("checkpoint ready does not match the active barrier")
		}
		if observation.Through != event.Ordinal {
			return errors.New("checkpoint ready frontier does not match its event ordinal")
		}
		through, digest := observation.Through, event.Digest
		snapshot.CheckpointReadyThrough = &through
		snapshot.CheckpointReadyDigest = &digest
	case RelayObservationHarnessTurnStarted:
		// The agent moved on by itself, so a pending resume is moot. The new
		// turn gets its own check when it ends, which schedules again if the
		// quota still blocks it.
		if recovery := snapshot.Continuation.QuotaRecovery; recovery != nil && !recovery.Submitted {
			snapshot.Continuation.QuotaRecovery = nil
		}
		// Codex also reports native starts inside our own prompts; only a
		// start with no prompt in flight is a turn of the harness's own.
		startedAtMs := observation.StartedAtMs
		snapshot.ActivityTurnStartedAtMs = &startedAtMs
		snapshot.HarnessTurn = &StoredHarnessTurn{
			Autonomous:   snapshot.ActivePrompt == nil,
			StartedAtMs:  observation.StartedAtMs,
			FirstOrdinal: event.Ordinal,
		}
		ordinal := event.Ordinal
		snapshot.LastHarnessTurnStartedOrdinal = &ordinal
		if snapshot.Execution == RelayExecutionStateIdle {
			snapshot.Execution = RelayExecutionStateRunning
		}
	case RelayObservationHarnessTurnSettled:
		applyHarnessTurnSettled(snapshot, event, &observation)
	// The control plane behind the session was replaced, so a turn the
	// harness had started on its own no longer exists. Both callers record
	// this with no prompt in flight.
	case RelayObservationSessionRestarted:
		snapshot.AssessmentQuestions = []string{}
		snapshot.PendingElicitations = nil
		snapshot.Goal.Restart()
		hadTurn := snapshot.HarnessTurn != nil
		snapshot.HarnessTurn = nil
		if hadTurn && snapshot.ActivePrompt == nil && snapshot.Execution == RelayExecutionStateRunning {
			snapshot.Execution = RelayExecutionStateIdle
		}
	case RelayObservationClosing:
		snapshot.RetryAssessment = nil
		snapshot.CapacityRetry = nil
		snapshot.HarnessTurn = nil
		snapshot.Execution = RelayExecutionStateClosing
	case RelayObservationClosed:
		snapshot.RetryAssessment = nil
		snapshot.CapacityRetry = nil
		snapshot.ActivityTurnStartedAtMs = nil
		snapshot.HarnessTurn = nil
		snapshot.Execution = RelayExecutionStateClosed
		snapshot.ActivePrompt = nil
	case RelayObservationSessionUpdate:
		update := observation.Update
		switch update.Kind {
		case acpschema.SessionUpdateKindSessionInfoUpdate:
			if _, err := snapshot.Goal.Apply(&update); err != nil {
				return err
			}
		case acpschema.SessionUpdateKindAvailableCommandsUpdate:
			snapshot.AvailableCommands = relayNonNil(slices.Clone(update.AvailableCommandsUpdate.AvailableCommands))
		case acpschema.SessionUpdateKindConfigOptionUpdate:
			snapshot.ConfigOptions = relayNonNil(slices.Clone(update.ConfigOptionUpdate.ConfigOptions))
			recordCatalogueMode(snapshot)
		case acpschema.SessionUpdateKindCurrentModeUpdate:
			if snapshot.Modes != nil {
				modes := *snapshot.Modes
				modes.CurrentModeID = update.CurrentModeUpdate.CurrentModeID
				snapshot.Modes = &modes
			}
			snapshot.Config["mode"] = string(update.CurrentModeUpdate.CurrentModeID)
		}
	case RelayObservationNativeAgent:
		applyNativeAgentEvent(snapshot, observation.Event)
	case RelayObservationPermissionAutoApproved, RelayObservationElicitationRequested,
		RelayObservationElicitationResolved, RelayObservationElicitationsCleared, RelayObservationSessionFault,
		RelayObservationWarning, RelayObservationUserShellOutput, RelayObservationTerminalOutput,
		RelayObservationNotice:
	case RelayObservationMailboxHookLeaseCreated:
		lease := observation.Lease
		if snapshot.MailboxHookLease != nil {
			return errors.New("a mailbox hook lease is already active")
		}
		if strings.TrimSpace(lease.LeaseID) == "" ||
			(lease.HookEvent != "PostToolUse" && lease.HookEvent != "PostToolBatch") ||
			(len(lease.Events) == 0 && lease.Steering == nil) {
			return errors.New("mailbox hook lease is missing its ID, event, or events")
		}
		if lease.Steering != nil && lease.HookEvent != "PostToolBatch" {
			return errors.New("hook steer requires the PostToolBatch boundary")
		}
		uniqueKeys := map[string]bool{}
		for _, leased := range lease.Events {
			uniqueKeys[leased.Key] = true
		}
		if len(uniqueKeys) != len(lease.Events) {
			return errors.New("mailbox hook lease repeats an event key")
		}
		if !mailboxEventsEqual(snapshot.PendingMailboxEvents, lease.Events) {
			return errors.New("mailbox hook lease does not claim all pending events in order")
		}
		if lease.Steering != nil {
			if _, err := hookSteeringPrompt(snapshot, lease.Steering); err != nil {
				return err
			}
		}
		snapshot.PendingMailboxEvents = nil
		snapshot.MailboxHookLease = &lease
	case RelayObservationMailboxHookLeaseReturned:
		lease := snapshot.MailboxHookLease
		if lease == nil {
			return errors.New("mailbox hook lease return names no active lease")
		}
		if lease.LeaseID != observation.LeaseID {
			return errors.New("mailbox hook lease return names a different lease")
		}
		snapshot.MailboxHookLease = nil
		pending := append(slices.Clone(lease.Events), snapshot.PendingMailboxEvents...)
		snapshot.PendingMailboxEvents = pending
	case RelayObservationMailboxEventsDelivered:
		return applyMailboxEventsDelivered(snapshot, event, &observation)
	default:
		return fmt.Errorf("unknown relay observation %T", observation)
	}
	return nil
}

// mailboxEventsEqual is Rust's Vec<MailboxEvent> equality.
func mailboxEventsEqual(left, right []MailboxEvent) bool {
	return len(left) == len(right) && (len(left) == 0 || reflect.DeepEqual(left, right))
}

// insertSorted adds value to a sorted, duplicate-free slice (a Rust BTreeSet).
func insertSorted(values []string, value string) []string {
	index, found := slices.BinarySearch(values, value)
	if found {
		return values
	}
	return slices.Insert(values, index, value)
}

// removeSorted removes value from a sorted, duplicate-free slice and reports
// whether it was present.
func removeSorted(values []string, value string) ([]string, bool) {
	index, found := slices.BinarySearch(values, value)
	if !found {
		return values, false
	}
	return slices.Delete(values, index, index+1), true
}

func saturatingAddU32(value, add uint32) uint32 {
	if value > ^uint32(0)-add {
		return ^uint32(0)
	}
	return value + add
}

func applyTurnAssessmentUpdated(snapshot *RelaySnapshot, event *RelayEvent, assessment *TurnAssessment) error {
	current := snapshot.Assessment
	if current == nil {
		return errors.New("assessment has no completed turn")
	}
	if current.TurnID != assessment.TurnID || current.CompletedOrdinal != assessment.CompletedOrdinal || !current.Current() {
		return errors.New("assessment completion changed")
	}
	completedAtMs := current.CompletedAtMs
	retryProvider := assessment.Action != nil && *assessment.Action == AssessmentActionRetryProvider
	if retryProvider && assessment.Status == StatusScheduled {
		attempt := uint32(1)
		if retry := snapshot.CapacityRetry; retry != nil && retry.Submitted {
			attempt = saturatingAddU32(retry.Attempt, 1)
		}
		retry := NewCapacityRetry(attempt, assessment.CompletedOrdinal, event.RecordedAtMs)
		snapshot.CapacityRetry = &retry
	} else if assessment.Action != nil && !retryProvider {
		// Only consecutive provider refusals share a retry backoff.
		snapshot.CapacityRetry = nil
	}
	if assessment.CompletedOrdinal > snapshot.PersonInputThrough {
		question := assessment.InferredQuestion()
		if !snapshot.ManagedSubagent && question != nil {
			completed := question.CompletedOrdinal
			if open := snapshot.OpenRequest(); open == nil || open.Question.CompletedOrdinal < completed {
				message := cloneStringPtr(assessment.QuestionMessage)
				if message == nil {
					message = cloneStringPtr(snapshot.CompletedReplyMessage)
				}
				snapshot.Request = &OpenRequest{Question: *question, QuestionMessage: message}
			}
			var message EntryMessage = EntryMessageLocating{}
			if assessment.QuestionMessage != nil {
				message = EntryMessageFound{Value: *assessment.QuestionMessage}
			}
			recordNotification(snapshot, completed, completedAtMs, NotificationKindNeedsInput, message, event.Ordinal)
		} else {
			completed := assessment.CompletedOrdinal
			if request := snapshot.Request; request != nil &&
				request.Question.CompletedOrdinal == completed && request.Question.TurnID == assessment.TurnID {
				snapshot.Request = nil
			}
			if assessment.Current() && assessment.Attention != nil && *assessment.Attention == AssessmentAttentionImportant &&
				completed > snapshot.NotificationsReadThrough {
				var message EntryMessage = EntryMessageNone{}
				if snapshot.CompletedReplyMessage != nil {
					message = EntryMessageFound{Value: *snapshot.CompletedReplyMessage}
				}
				recordNotification(snapshot, completed, completedAtMs, NotificationKindImportant, message, event.Ordinal)
			} else if index, found := notificationIndex(snapshot.NotificationStack.Entries, completed); found {
				snapshot.NotificationStack.Entries = slices.Delete(slices.Clone(snapshot.NotificationStack.Entries), index, index+1)
				snapshot.NotificationStack.Revision = event.Ordinal
			}
		}
	}
	snapshot.RetryAssessment = nil
	updated := *assessment
	updated.CompletedAtMs = completedAtMs
	snapshot.Assessment = &updated
	return nil
}

func applySessionOpened(snapshot *RelaySnapshot, event *RelayEvent, opened *RelayObservationSessionOpened) {
	// This bridge may have opened live forms before session/open completed.
	// Bridge exit/restart retires old responders, not open.
	snapshot.Continuation.Suppressed = true
	ordinal := event.Ordinal
	if opened.NativeContinuityLost {
		snapshot.Continuation.QuotaRecovery = nil
		snapshot.Continuation.QuotaSuppressed = true
		snapshot.Continuation.WithdrawnOrdinal = &ordinal
	}
	nativeSessionID := opened.NativeSessionID
	snapshot.NativeSessionID = &nativeSessionID
	snapshot.NativeSessionOpenedOrdinal = &ordinal
	snapshot.RestoredNativeSessionUnused = false
	// A normal open clears the flag; only the fallback sets it.
	snapshot.NativeContinuityLost = opened.NativeContinuityLost
	snapshot.ReplacedUnusedNativeSessionID = cloneStringPtr(opened.ReplacedUnusedNativeSessionID)
	// A resumed thread was not created here, so this journal cannot show
	// everything the thread contains. Once true this never clears: replacing
	// such a thread would discard native history.
	if opened.Resumed {
		snapshot.NativeSessionUsed = true
	}
	var usage NativeSessionUse
	switch evidence := snapshot.NativeUseEvidence; {
	case opened.Resumed:
		usage = NativeSessionUseUsed
	case evidence != nil && evidence.Version == 1 && evidence.NativeSessionID == opened.NativeSessionID:
		usage = evidence.Usage
	case snapshot.NativeSessionUsed:
		usage = NativeSessionUseUnknown
	default:
		usage = NativeSessionUseUnused
	}
	snapshot.RecordNativeUse(usage)
}

// promptText joins a prompt's text blocks with newlines.
func promptText(prompt []acpschema.ContentBlock) string {
	var texts []string
	for _, block := range prompt {
		if block.Kind == acpschema.ContentBlockKindText && block.Text != nil {
			texts = append(texts, block.Text.Text)
		}
	}
	return strings.Join(texts, "\n")
}

func isQueuedOrRunning(state RelayDispatchState) bool {
	return state == RelayDispatchStateQueued || state == RelayDispatchStatePending || state == RelayDispatchStateInFlight
}

func applyCommandQueued(snapshot *RelaySnapshot, event *RelayEvent, queued *RelayObservationCommandQueued) error {
	commandID, command := queued.CommandID, queued.Command
	ordinal := event.Ordinal
	if err := snapshot.ValidateNotificationCommand(command); err != nil {
		return err
	}
	prompt, isPrompt := RelayCommandPromptBlocks(command)
	if isPrompt {
		snapshot.LatestPromptAcceptedOrdinal = &ordinal
	}
	switch command := command.(type) {
	case RelayCommandDeliverMailboxEvent:
		mailboxEvent := command.Event
		if strings.TrimSpace(mailboxEvent.Key) == "" || strings.TrimSpace(mailboxEvent.Source) == "" ||
			!mailboxEvent.HasRenderableContent() {
			return errors.New("mailbox event key, source, and content are required")
		}
		_, delivered := snapshot.DeliveredMailboxEventKeys[mailboxEvent.Key]
		if delivered || slices.ContainsFunc(snapshot.PendingMailboxEvents, func(pending MailboxEvent) bool {
			return pending.Key == mailboxEvent.Key
		}) {
			return errors.New("mailbox event key was queued more than once")
		}
		snapshot.PendingMailboxEvents = append(snapshot.PendingMailboxEvents, mailboxEvent)
	case RelayCommandMailboxWake:
		if err := claimWakeMailboxEvents(snapshot, command.Events, ordinal); err != nil {
			return err
		}
	case RelayCommandSteer:
		snapshot.Steering = &SteeringOperation{
			CommandID:      commandID,
			ActivePromptID: command.ActivePromptID,
			QueuedPromptID: command.QueuedPromptID,
			Status:         SteeringStatusPending,
		}
	case RelayCommandCancelTurnFor:
		id := command.ActivePromptID
		snapshot.CancellingPromptID = &id
	case RelayCommandApplyQueuedFor:
		id := command.ActivePromptID
		snapshot.CancellingPromptID = &id
	case RelayCommandCancel, RelayCommandCancelTurn:
		snapshot.CancellingPromptID = nil
		if snapshot.ActivePrompt != nil {
			id := snapshot.ActivePrompt.CommandID
			snapshot.CancellingPromptID = &id
		} else if _, cancelTurn := command.(RelayCommandCancelTurn); cancelTurn {
			for _, queued := range snapshot.QueuedPrompts {
				if IsRestartContinuation(queued.CommandID) {
					id := queued.CommandID
					snapshot.CancellingPromptID = &id
					break
				}
			}
		}
		// Without a prompt or queued recovery target, the stop is aimed at
		// the harness's own turn. A second stop of the same turn never
		// downgrades an acknowledged first one.
		if snapshot.CancellingPromptID == nil {
			if turn := snapshot.AutonomousHarnessTurn(); turn != nil &&
				(snapshot.CancellingHarnessTurn == nil || snapshot.CancellingHarnessTurn.FirstOrdinal != turn.FirstOrdinal) {
				snapshot.CancellingHarnessTurn = &HarnessTurnCancel{FirstOrdinal: turn.FirstOrdinal}
			}
		}
	}
	withdraw := func() {
		snapshot.Continuation.Suppressed = true
		snapshot.Continuation.QuotaSuppressed = true
		snapshot.Continuation.WithdrawnOrdinal = &ordinal
	}
	// Another session's message delivered as a turn, by a mailbox wake or,
	// for workers without mailbox delivery, as a prompt. A wake is known by
	// its ID, as the projection knows it: restored from a checkpoint it is a
	// plain prompt under that ID.
	deliveredMessageTurn := func() bool {
		if CarriesMailboxWake(commandID) {
			return true
		}
		producer, ok := GeneratedPromptOf(commandID)
		return ok && producer == GeneratedPromptSessionMessageTurn
	}
	switch command := command.(type) {
	// Legacy protocol-39 journals still replay this command.
	case RelayCommandSetSubagentWait:
	case RelayCommandPrompt:
		id := commandID
		if IsUserAuthoredMessage(&id, promptText(command.Prompt)) {
			clearNotifications(snapshot, ordinal)
			_, _, context := ContextCommandOf(command.Prompt)
			withdrawn := snapshot.Continuation.WithdrawnOrdinal
			if context {
				withdrawn = &ordinal
			}
			snapshot.Continuation = ContinuationState{
				UserCommandID:    &id,
				Suppressed:       context,
				QuotaSuppressed:  context,
				WithdrawnOrdinal: withdrawn,
			}
		} else if deliveredMessageTurn() {
			beginDeliveredMessageTurn(&snapshot.Continuation, commandID)
		}
	case RelayCommandContinueAuthorizedWork:
		snapshot.Continuation.Attempts = command.Attempt
		snapshot.Continuation.CompletedCommandID = nil
		snapshot.Continuation.HarnessTurn = nil
	case RelayCommandMailboxWake:
		if deliveredMessageTurn() {
			beginDeliveredMessageTurn(&snapshot.Continuation, commandID)
		}
	case RelayCommandInstallPromptContext:
		switch context := snapshot.PendingPromptContext; {
		case context == nil:
			snapshot.PendingPromptContext = &PendingPromptContext{Text: command.Text}
		case context.Text != command.Text:
			updated := *context
			updated.Text += "\n\n" + command.Text
			snapshot.PendingPromptContext = &updated
		}
	case RelayCommandDismissQuestion:
		if open := snapshot.OpenRequest(); open == nil || open.Question != command.Question {
			return errors.New("dismissed question no longer matches the open request")
		}
		clearNotifications(snapshot, ordinal)
		if snapshot.AssessmentContext != nil {
			context := *snapshot.AssessmentContext
			context.SetAsideReplies(false)
			snapshot.AssessmentContext = &context
		}
	case RelayCommandClearNotifications:
		if snapshot.OpenRequest() != nil && snapshot.AssessmentContext != nil {
			context := *snapshot.AssessmentContext
			context.SetAsideReplies(false)
			snapshot.AssessmentContext = &context
		}
		clearNotifications(snapshot, ordinal)
	case RelayCommandNotificationsRead:
		through := command.Through
		if through > snapshot.NotificationsReadThrough {
			snapshot.NotificationsReadThrough = through
			important := func(entry NotificationStackEntry) bool {
				return entry.Kind == NotificationKindImportant && entry.CompletedOrdinal <= through
			}
			if slices.ContainsFunc(snapshot.NotificationStack.Entries, important) {
				snapshot.NotificationStack.Entries = slices.DeleteFunc(slices.Clone(snapshot.NotificationStack.Entries), important)
				snapshot.NotificationStack.Revision = ordinal
			}
		}
	case RelayCommandSeedAssessmentContext:
		context := command.Context
		snapshot.AssessmentContext = &context
		if a := snapshot.Assessment; a != nil && a.Current() &&
			!(a.Action != nil && (*a.Action == AssessmentActionRetryProvider || *a.Action == AssessmentActionRecoverQuota)) {
			updated := *a
			updated.ClassifyAgain(ordinal)
			updated.Failures = 0
			updated.RetryAtMs = nil
			updated.Reason = "authorization_evidence_changed"
			snapshot.Assessment = &updated
		}
	case RelayCommandSetQuotaRecovery:
		snapshot.Continuation.QuotaRecovery = nil
		if command.Recovery != nil {
			recovery := *command.Recovery
			snapshot.Continuation.QuotaRecovery = &recovery
		}
	case RelayCommandResumeAfterQuota:
		resumeAfterQuota(&snapshot.Continuation)
	case RelayCommandGoalControl:
		if command.Action == GoalControlActionResume && IsQuotaGoalResume(commandID) {
			resumeAfterQuota(&snapshot.Continuation)
		} else {
			withdraw()
		}
	case RelayCommandCancel, RelayCommandCancelTurn, RelayCommandCancelTurnFor, RelayCommandApplyQueuedFor,
		RelayCommandClearContext, RelayCommandRestoreExecutionMode, RelayCommandSetSessionMode:
		withdraw()
	}
	// A quota goal resume is automatic work, not the user taking over.
	userControl := !IsQuotaGoalResume(commandID)
	var lifecycle bool
	switch command.(type) {
	case RelayCommandCancelTurnFor, RelayCommandApplyQueuedFor, RelayCommandClearContext:
		lifecycle = true
	}
	_, promptCommand := command.(RelayCommandPrompt)
	// A checkpoint barrier is Mjolnir's own housekeeping (a routine copy, a
	// worker replacement, a file write), not the user taking over, so it
	// leaves a scheduled recovery alone (F18).
	if (cancelsCapacityRetry(command) && userControl) || lifecycle {
		snapshot.Continuation.QuotaRecovery = nil
		if !promptCommand {
			snapshot.Continuation.QuotaSuppressed = true
			snapshot.Continuation.WithdrawnOrdinal = &ordinal
		}
	}
	if cancelsCapacityRetry(command) || lifecycle {
		// A checkpoint barrier is housekeeping, so it leaves a pending retry
		// assessment alone, as it leaves a quota recovery (F24).
		snapshot.RetryAssessment = nil
	}
	if cancelsCapacityRetry(command) {
		if retry := snapshot.CapacityRetry; retry != nil && retry.CommandID == commandID && promptCommand {
			submitted := *retry
			submitted.Submitted = true
			snapshot.CapacityRetry = &submitted
		} else {
			snapshot.CapacityRetry = nil
		}
	}
	snapshot.HandledCommands[commandID] = HandledRelayCommand{Command: command, AcceptedOrdinal: ordinal}
	snapshot.Dispatches[commandID] = RelayDispatchRecord{Command: command, State: RelayDispatchStateQueued}
	// Prompts and configuration changes share one FIFO queue so they reach
	// the agent in the order the user submitted them.
	var payload StoredQueuedRelayPayload
	if isPrompt {
		payload = StoredQueuedRelayPayloadPrompt{Prompt: slices.Clone(prompt)}
	} else if setConfig, ok := command.(RelayCommandSetConfig); ok {
		payload = StoredQueuedRelayPayloadSetConfig{Key: setConfig.Key, Value: setConfig.Value}
	}
	if payload != nil {
		entry := StoredQueuedRelayCommand{
			CommandID:   commandID,
			Payload:     payload,
			CreatedAtMs: queued.CreatedAtMs,
		}
		if queued.InsertAtFront {
			snapshot.QueuedPrompts = slices.Insert(snapshot.QueuedPrompts, 0, entry)
		} else {
			snapshot.QueuedPrompts = append(snapshot.QueuedPrompts, entry)
		}
	}
	switch command := command.(type) {
	case RelayCommandRunUserShell:
		if snapshot.ActiveUserShells == nil {
			snapshot.ActiveUserShells = map[string]ActiveUserShell{}
		}
		snapshot.ActiveUserShells[commandID] = ActiveUserShell{
			CommandID:   commandID,
			Command:     command.Command,
			CreatedAtMs: queued.CreatedAtMs,
		}
	case RelayCommandClearContext:
		createdAtMs := queued.CreatedAtMs
		snapshot.ActivityTurnStartedAtMs = &createdAtMs
		snapshot.Execution = RelayExecutionStateRunning
	case RelayCommandClose:
		snapshot.Execution = RelayExecutionStateClosing
	}
	return nil
}

func resumeAfterQuota(continuation *ContinuationState) {
	if continuation.QuotaRecovery != nil {
		recovery := *continuation.QuotaRecovery
		recovery.Submitted = true
		continuation.QuotaRecovery = &recovery
	}
	continuation.CompletedCommandID = nil
	continuation.HarnessTurn = nil
	continuation.Suppressed = false
}

func isPromptCommand(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandPrompt, RelayCommandHandbackReminder, RelayCommandHoldFollowUp, RelayCommandMailboxWake,
		RelayCommandContinueAuthorizedWork, RelayCommandResumeAfterQuota:
		return true
	}
	return false
}

func queuedIndex(snapshot *RelaySnapshot, commandID string) int {
	return slices.IndexFunc(snapshot.QueuedPrompts, func(queued StoredQueuedRelayCommand) bool {
		return queued.CommandID == commandID
	})
}

func applyCommandStarted(snapshot *RelaySnapshot, started *RelayObservationCommandStarted) error {
	commandID := started.CommandID
	dispatch, ok := snapshot.Dispatches[commandID]
	if !ok {
		return fmt.Errorf("started unknown relay command %s", commandID)
	}
	dispatch.State = RelayDispatchStatePending
	snapshot.Dispatches[commandID] = dispatch
	switch dispatch.Command.(type) {
	case RelayCommandPrompt, RelayCommandHandbackReminder, RelayCommandHoldFollowUp, RelayCommandMailboxWake,
		RelayCommandContinueAuthorizedWork, RelayCommandResumeAfterQuota:
		index := queuedIndex(snapshot, commandID)
		if index < 0 {
			return fmt.Errorf("started prompt %s was not queued", commandID)
		}
		queued := snapshot.QueuedPrompts[index]
		snapshot.QueuedPrompts = slices.Delete(snapshot.QueuedPrompts, index, index+1)
		prompt, isPrompt := queued.Payload.(StoredQueuedRelayPayloadPrompt)
		if !isPrompt {
			return fmt.Errorf("queued command %s is not a prompt", commandID)
		}
		snapshot.Execution = RelayExecutionStateRunning
		// New input supersedes the finished turn the liveness describes once
		// it runs, not while it is only queued: a queued prompt that is
		// dropped leaves the child held.
		snapshot.WorkLiveness = nil
		startedAtMs := started.StartedAtMs
		snapshot.ActivityTurnStartedAtMs = &startedAtMs
		snapshot.ActivePrompt = &StoredActiveRelayPrompt{
			CommandID:   queued.CommandID,
			Prompt:      prompt.Prompt,
			CreatedAtMs: queued.CreatedAtMs,
			StartedAtMs: started.StartedAtMs,
		}
	// A configuration change leaves the queue when it starts, but the ACP
	// session stays idle: it applies between turns.
	case RelayCommandSetConfig:
		index := queuedIndex(snapshot, commandID)
		if index < 0 {
			return fmt.Errorf("started configuration change %s was not queued", commandID)
		}
		snapshot.QueuedPrompts = slices.Delete(snapshot.QueuedPrompts, index, index+1)
	case RelayCommandClose:
		snapshot.Execution = RelayExecutionStateClosing
	case RelayCommandRunUserShell:
		shell, ok := snapshot.ActiveUserShells[commandID]
		if !ok {
			return fmt.Errorf("started unknown user shell %s", commandID)
		}
		startedAtMs := started.StartedAtMs
		shell.StartedAtMs = &startedAtMs
		snapshot.ActiveUserShells[commandID] = shell
	case RelayCommandBeginCheckpoint:
		if snapshot.CheckpointBarrier != nil {
			return errors.New("checkpoint barrier started while another barrier was active")
		}
		id := commandID
		snapshot.CheckpointBarrier = &id
		snapshot.CheckpointReadyThrough = nil
		snapshot.CheckpointReadyDigest = nil
	}
	return nil
}

// detachPromptContexts drops the contexts attached to a prompt that has
// ended.
func detachPromptContexts(snapshot *RelaySnapshot, commandID string) {
	if context := snapshot.PendingPromptContext; context != nil && context.AttachedCommandID != nil &&
		*context.AttachedCommandID == commandID {
		snapshot.PendingPromptContext = nil
	}
	snapshot.PendingUserShellContexts = slices.DeleteFunc(snapshot.PendingUserShellContexts, func(context PendingUserShellContext) bool {
		return context.AttachedCommandID != nil && *context.AttachedCommandID == commandID
	})
}

func applyCommandCompleted(snapshot *RelaySnapshot, event *RelayEvent, completed *RelayObservationCommandCompleted) error {
	commandID, outcome := completed.CommandID, completed.Outcome
	ordinal := event.Ordinal
	dispatch, ok := snapshot.Dispatches[commandID]
	if !ok {
		return fmt.Errorf("completed unknown relay command %s", commandID)
	}
	command := dispatch.Command
	if completed.Command != nil && *completed.Command != RelayCommandKindOf(command) {
		return fmt.Errorf("completed command %s has the wrong command identity", commandID)
	}
	if completed.BarrierCommandID != nil {
		matches := false
		switch command := command.(type) {
		case RelayCommandCompleteCheckpoint:
			matches = command.BarrierCommandID == *completed.BarrierCommandID
		case RelayCommandReleaseCheckpoint:
			matches = command.BarrierCommandID == *completed.BarrierCommandID
		}
		if !matches {
			return fmt.Errorf("completed command %s has the wrong checkpoint identity", commandID)
		}
	}
	setDispatchState(snapshot, commandID, RelayDispatchStateCompleted)
	if !updateHandled(snapshot, commandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
		handled.Outcome = outcome
	}) {
		return fmt.Errorf("completed command %s is not in the ledger", commandID)
	}
	if prompt, ok := outcome.(RelayCommandOutcomePrompt); ok {
		snapshot.WorkLiveness = WorkLivenessFromTurnFacts(ordinal, prompt.WorkFacts)
		// Always KeepCurrent; see TurnCompletion for why the field is still
		// written.
		snapshot.TurnCompletion = &TurnCompletion{CommandID: commandID, CompletedOrdinal: ordinal, Decision: DecisionKeepCurrent}
		snapshot.Continuation.CompletedCommandID = nil
		if snapshot.Continuation.UserCommandID != nil {
			id := commandID
			snapshot.Continuation.CompletedCommandID = &id
		}
		snapshot.Continuation.HarnessTurn = nil
		if ClassifyPromptCompletion(prompt.StopReason) != PromptCompletionFinished {
			snapshot.Continuation.Suppressed = true
		}
		if (snapshot.RetryAssessment == nil || snapshot.RetryAssessment.CommandID != commandID) &&
			(snapshot.CapacityRetry == nil || snapshot.CapacityRetry.CommandID != commandID) {
			snapshot.CapacityRetry = nil
		}
	}
	incompatible := func() error {
		return fmt.Errorf("relay command %s has incompatible completion outcome %s",
			relayKindDebug(RelayCommandKindOf(command)), relayOutcomeDebug(outcome))
	}
	switch command := command.(type) {
	case RelayCommandPrompt, RelayCommandHandbackReminder, RelayCommandHoldFollowUp, RelayCommandMailboxWake,
		RelayCommandContinueAuthorizedWork, RelayCommandResumeAfterQuota:
		if _, ok := outcome.(RelayCommandOutcomePrompt); !ok {
			return incompatible()
		}
		if snapshot.ActivePrompt != nil && snapshot.ActivePrompt.CommandID == commandID {
			snapshot.ActivePrompt = nil
			if snapshot.CancellingPromptID != nil && *snapshot.CancellingPromptID == commandID {
				snapshot.CancellingPromptID = nil
			}
		}
		// ACP completion must not settle a later native goal turn.
		if !snapshot.Goal.Running() {
			snapshot.HarnessTurn = nil
		}
		if snapshot.Execution == RelayExecutionStateRunning && !snapshot.Goal.Running() {
			snapshot.Execution = RelayExecutionStateIdle
		}
		detachPromptContexts(snapshot, commandID)
		delete(snapshot.MailboxPromptContexts, commandID)
	case RelayCommandRunUserShell:
		shell, ok := outcome.(RelayCommandOutcomeUserShell)
		if !ok {
			return incompatible()
		}
		delete(snapshot.ActiveUserShells, commandID)
		handled, ok := snapshot.HandledCommands[commandID]
		if !ok {
			return errors.New("completed user shell is not in the ledger")
		}
		snapshot.PendingUserShellContexts = append(snapshot.PendingUserShellContexts, PendingUserShellContext{
			ShellCommandID:  commandID,
			AcceptedOrdinal: handled.AcceptedOrdinal,
			Text:            shell.Result.PromptContext(),
		})
	case RelayCommandCancelUserShell:
		if _, ok := outcome.(RelayCommandOutcomeUserShellCancelled); !ok {
			return incompatible()
		}
	case RelayCommandRemoveQueuedPrompt:
		changed, ok := outcome.(RelayCommandOutcomeQueueChanged)
		if !ok {
			return incompatible()
		}
		queued := queuedIndex(snapshot, command.QueuedCommandID) >= 0
		if !queued || !slices.Equal(changed.RemovedCommandIds, []string{command.QueuedCommandID}) {
			return errors.New("removed queue outcome does not match the durable queue")
		}
		return terminalizeRemovedPrompts(snapshot, changed.RemovedCommandIds, ordinal)
	case RelayCommandClearQueuedPrompts:
		changed, ok := outcome.(RelayCommandOutcomeQueueChanged)
		if !ok {
			return incompatible()
		}
		expected := make([]string, 0, len(snapshot.QueuedPrompts))
		for _, queued := range snapshot.QueuedPrompts {
			expected = append(expected, queued.CommandID)
		}
		if !slices.Equal(expected, changed.RemovedCommandIds) {
			return errors.New("cleared queue outcome does not match the durable queue")
		}
		return terminalizeRemovedPrompts(snapshot, changed.RemovedCommandIds, ordinal)
	case RelayCommandSetConfig:
		if _, ok := outcome.(RelayCommandOutcomeConfigured); !ok {
			return incompatible()
		}
		snapshot.Config[command.Key] = command.Value
		AcceptedSessionConfigRecordCompleted(snapshot.Config, command.Key, command.Value, snapshot.ConfigOptions)
	case RelayCommandSetSessionMode:
		if _, ok := outcome.(RelayCommandOutcomeSessionModeSet); !ok {
			return incompatible()
		}
		snapshot.Config["mode"] = command.ModeID
	case RelayCommandRestoreExecutionMode:
		if _, ok := outcome.(RelayCommandOutcomeConfigured); !ok {
			return incompatible()
		}
	case RelayCommandSeedAssessmentContext, RelayCommandDismissQuestion, RelayCommandClearNotifications,
		RelayCommandNotificationsRead, RelayCommandInstallPromptContext, RelayCommandRecordNotice,
		RelayCommandSetQuotaRecovery, RelayCommandSetSubagentWait, RelayCommandDeliverMailboxEvent:
		if _, ok := outcome.(RelayCommandOutcomeNoticeRecorded); !ok {
			return incompatible()
		}
	case RelayCommandGoalControl:
		if _, ok := outcome.(RelayCommandOutcomeGoalControlled); !ok {
			return incompatible()
		}
	case RelayCommandCancelTurnFor, RelayCommandApplyQueuedFor:
		// A cancel acknowledgement does not end its originating prompt, and
		// must never retarget cancellation to a later one.
		if _, ok := outcome.(RelayCommandOutcomeCancelled); !ok {
			return incompatible()
		}
	case RelayCommandResolveSteering:
		if _, ok := outcome.(RelayCommandOutcomeNoticeRecorded); !ok {
			return incompatible()
		}
		if snapshot.Steering != nil {
			if snapshot.Steering.CommandID != command.SteeringID {
				return errors.New("steering identity changed")
			}
			steering := *snapshot.Steering
			steering.Status = SteeringStatusResolved
			snapshot.Steering = &steering
		}
	case RelayCommandCancel, RelayCommandCancelTurn, RelayCommandSteer:
		_, isCancel := command.(RelayCommandCancel)
		_, isCancelTurn := command.(RelayCommandCancelTurn)
		switch outcome := outcome.(type) {
		case RelayCommandOutcomeCancelled:
			if !isCancel && !isCancelTurn {
				return incompatible()
			}
			if target := snapshot.CancellingHarnessTurn; target != nil {
				applied := *target
				applied.Applied = true
				snapshot.CancellingHarnessTurn = &applied
			}
		case RelayCommandOutcomeSteered:
			if isCancelTurn {
				return incompatible()
			}
			return applySteered(snapshot, commandID, outcome.QueuedCommandID, isCancel, ordinal)
		case RelayCommandOutcomeSteeringReturned:
			if isCancelTurn {
				return incompatible()
			}
			if isCancel {
				snapshot.CancellingPromptID = nil
			}
			// The turn it targeted had already ended, so nothing was
			// delivered and there is nothing for the user to decide: the
			// prompt runs next from the queue.
			if snapshot.Steering != nil && snapshot.Steering.CommandID == commandID {
				steering := *snapshot.Steering
				steering.Status = SteeringStatusResolved
				steering.Message = nil
				snapshot.Steering = &steering
			}
		default:
			return incompatible()
		}
	case RelayCommandClose:
		if _, ok := outcome.(RelayCommandOutcomeClosed); !ok {
			return incompatible()
		}
		snapshot.Execution = RelayExecutionStateClosed
		snapshot.ActivePrompt = nil
	case RelayCommandCompleteCheckpoint:
		if _, ok := outcome.(RelayCommandOutcomeCheckpointCompleted); !ok {
			return incompatible()
		}
		if snapshot.CheckpointBarrier == nil || *snapshot.CheckpointBarrier != command.BarrierCommandID {
			return errors.New("checkpoint completion does not match the active barrier")
		}
		if snapshot.CheckpointReadyThrough == nil {
			return errors.New("checkpoint barrier was not ready")
		}
		if snapshot.CheckpointReadyDigest == nil {
			return errors.New("checkpoint barrier ready digest is missing")
		}
		snapshot.RecoveryFloorOrdinal = *snapshot.CheckpointReadyThrough
		snapshot.RecoveryFloorDigest = *snapshot.CheckpointReadyDigest
		releaseCheckpointBarrier(snapshot, command.BarrierCommandID, ordinal)
	case RelayCommandReleaseCheckpoint:
		if _, ok := outcome.(RelayCommandOutcomeCheckpointReleased); !ok {
			return incompatible()
		}
		if snapshot.CheckpointBarrier == nil || *snapshot.CheckpointBarrier != command.BarrierCommandID {
			return errors.New("checkpoint release does not match the active barrier")
		}
		if snapshot.CheckpointReadyThrough == nil {
			return errors.New("checkpoint barrier was not ready")
		}
		// Dispatch resumes, but the recovery floor stays where the last
		// installed archive left it: nothing yet proves this archive reached
		// the controller's disk.
		releaseCheckpointBarrier(snapshot, command.BarrierCommandID, ordinal)
	case RelayCommandAdvanceRecoveryFloor:
		if _, ok := outcome.(RelayCommandOutcomeRecoveryFloorAdvanced); !ok {
			return incompatible()
		}
		if command.Through.Ordinal < snapshot.RecoveryFloorOrdinal {
			return errors.New("recovery floor cannot move back")
		}
		snapshot.RecoveryFloorOrdinal = command.Through.Ordinal
		snapshot.RecoveryFloorDigest = command.Through.Digest
	case RelayCommandClearContext:
		cleared, ok := outcome.(RelayCommandOutcomeContextCleared)
		if !ok {
			return incompatible()
		}
		applyContextCleared(snapshot, &cleared, ordinal)
	case RelayCommandBeginCheckpoint:
		return errors.New("checkpoint barriers complete through checkpoint-ready")
	default:
		return incompatible()
	}
	return nil
}

// relayKindDebug renders a command kind as Rust's {:?} does: the variant name.
func relayKindDebug(kind RelayCommandKind) string {
	var b strings.Builder
	for _, part := range strings.Split(string(kind), "_") {
		if part != "" {
			b.WriteString(strings.ToUpper(part[:1]) + part[1:])
		}
	}
	return b.String()
}

// relayOutcomeDebug names an outcome for an error message.
func relayOutcomeDebug(outcome RelayCommandOutcome) string {
	encoded, err := relayMarshalJSON(outcome)
	if err != nil {
		return fmt.Sprintf("%T", outcome)
	}
	return string(encoded)
}

func releaseCheckpointBarrier(snapshot *RelaySnapshot, barrierCommandID string, ordinal uint64) {
	snapshot.CheckpointBarrier = nil
	snapshot.CheckpointReadyThrough = nil
	snapshot.CheckpointReadyDigest = nil
	setDispatchState(snapshot, barrierCommandID, RelayDispatchStateCompleted)
	updateHandled(snapshot, barrierCommandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
	})
}

func applySteered(snapshot *RelaySnapshot, commandID, queuedCommandID string, isCancel bool, ordinal uint64) error {
	if snapshot.Steering != nil && snapshot.Steering.CommandID == commandID {
		steering := *snapshot.Steering
		steering.Status = SteeringStatusApplied
		steering.Message = nil
		snapshot.Steering = &steering
	}
	if len(snapshot.QueuedPrompts) == 0 {
		return errors.New("steered prompt is no longer queued")
	}
	queued := snapshot.QueuedPrompts[0]
	if _, isPrompt := queued.Payload.(StoredQueuedRelayPayloadPrompt); queued.CommandID != queuedCommandID || !isPrompt {
		return errors.New("steered prompt is not the queued prompt head")
	}
	target, ok := snapshot.Dispatches[queuedCommandID]
	if !ok {
		return errors.New("steered unknown queued prompt")
	}
	if _, prompt := RelayCommandPromptBlocks(target.Command); target.State != RelayDispatchStateQueued || !prompt {
		return errors.New("steered target is not a queued prompt")
	}
	setDispatchState(snapshot, queuedCommandID, RelayDispatchStateCompleted)
	if !updateHandled(snapshot, queuedCommandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
	}) {
		return errors.New("steered prompt is not in the ledger")
	}
	snapshot.QueuedPrompts = slices.Delete(snapshot.QueuedPrompts, 0, 1)
	detachPromptContexts(snapshot, queuedCommandID)
	if isCancel {
		snapshot.CancellingPromptID = nil
	}
	return nil
}

func applyContextCleared(snapshot *RelaySnapshot, cleared *RelayCommandOutcomeContextCleared, ordinal uint64) {
	snapshot.Continuation.Suppressed = true
	nativeSessionID := cleared.NativeSessionID
	snapshot.NativeSessionID = &nativeSessionID
	snapshot.NativeSessionOpenedOrdinal = &ordinal
	snapshot.NativeSessionUsed = false
	snapshot.RecordNativeUse(NativeSessionUseUnused)
	snapshot.NativeContinuityLost = false
	snapshot.ReplacedUnusedNativeSessionID = nil
	snapshot.PendingPromptContext = nil
	if cleared.Memory != nil && strings.TrimSpace(*cleared.Memory) != "" {
		snapshot.PendingPromptContext = &PendingPromptContext{Text: *cleared.Memory}
	}
	snapshot.PendingUserShellContexts = nil
	var discardedKeys []string
	for _, pending := range snapshot.PendingMailboxEvents {
		discardedKeys = append(discardedKeys, pending.Key)
	}
	snapshot.PendingMailboxEvents = nil
	if lease := snapshot.MailboxHookLease; lease != nil {
		for _, leased := range lease.Events {
			discardedKeys = append(discardedKeys, leased.Key)
		}
		snapshot.MailboxHookLease = nil
	}
	retainDeliveredMailboxEventKeys(snapshot, discardedKeys, ordinal)
	snapshot.MailboxPromptContexts = nil
	snapshot.ActivityTurnStartedAtMs = nil
	snapshot.HarnessTurn = nil
	snapshot.CapacityRetry = nil
	snapshot.NativeAgents = nil
	snapshot.Goal = GoalState{Capability: snapshot.Goal.Capability, Known: true}
	snapshot.Execution = RelayExecutionStateIdle
}

func applyCommandTerminated(snapshot *RelaySnapshot, event *RelayEvent, commandID string, observedCommand RelayCommandKind,
	message string, continuationID *string, reason *OutcomeReason, rejected bool) error {
	ordinal := event.Ordinal
	if pending := snapshot.RetryAssessment; pending != nil && pending.CommandID == commandID {
		snapshot.RetryAssessment = nil
	}
	state := RelayDispatchStateInterrupted
	if rejected {
		state = RelayDispatchStateRejected
	}
	dispatch, ok := snapshot.Dispatches[commandID]
	if !ok {
		return fmt.Errorf("terminated unknown relay command %s", commandID)
	}
	command := dispatch.Command
	// A prompt in flight when the worker restarted may have reached the
	// harness before its use was recorded. The interruption itself records
	// that evidence, so no separate snapshot write can be lost to a crash or
	// grow a full snapshot before this event shrinks it.
	workerRestarted := !rejected && reason != nil && *reason == OutcomeReasonWorkerRestarted
	mayHaveReachedAgent := workerRestarted && dispatch.PromptMayHaveReachedAgent()
	if RelayCommandKindOf(command) != observedCommand {
		return fmt.Errorf("terminated command %s has the wrong command identity", commandID)
	}
	if snapshot.Steering != nil && snapshot.Steering.CommandID == commandID {
		steering := *snapshot.Steering
		steering.Status = SteeringStatusUnconfirmed
		if rejected {
			steering.Status = SteeringStatusFailed
		}
		failure := message
		steering.Message = &failure
		snapshot.Steering = &steering
	}
	switch command := command.(type) {
	case RelayCommandCancelTurnFor:
		if snapshot.CancellingPromptID != nil && *snapshot.CancellingPromptID == command.ActivePromptID {
			snapshot.CancellingPromptID = nil
		}
	case RelayCommandApplyQueuedFor:
		if snapshot.CancellingPromptID != nil && *snapshot.CancellingPromptID == command.ActivePromptID {
			snapshot.CancellingPromptID = nil
		}
	case RelayCommandCancel, RelayCommandCancelTurn:
		snapshot.CancellingPromptID = nil
		// A refused stop leaves the turn running. Admission refuses a second
		// stop while one is pending, so an acknowledged stop is the only one
		// this can meet.
		if snapshot.CancellingHarnessTurn != nil && !snapshot.CancellingHarnessTurn.Applied {
			snapshot.CancellingHarnessTurn = nil
		}
	}
	setDispatchState(snapshot, commandID, state)
	if mayHaveReachedAgent {
		snapshot.NativeSessionUsed = true
		snapshot.RecordNativeUse(NativeSessionUseUsed)
	}
	if !updateHandled(snapshot, commandID, func(handled *HandledRelayCommand) {
		handled.TerminalOrdinal = &ordinal
		failure := message
		handled.Failure = &failure
	}) {
		return fmt.Errorf("terminated command %s is not in the ledger", commandID)
	}
	snapshot.QueuedPrompts = slices.DeleteFunc(snapshot.QueuedPrompts, func(queued StoredQueuedRelayCommand) bool {
		return queued.CommandID == commandID
	})
	mailboxContext, hasMailboxContext := snapshot.MailboxPromptContexts[commandID]
	delete(snapshot.MailboxPromptContexts, commandID)
	delete(snapshot.ActiveUserShells, commandID)
	if shell, ok := command.(RelayCommandRunUserShell); ok {
		status := UserShellStatusInterrupted
		if rejected {
			status = UserShellStatusFailed
		}
		failure := message
		result := UserShellResult{Command: shell.Command, Status: status, Error: &failure}
		snapshot.PendingUserShellContexts = append(snapshot.PendingUserShellContexts, PendingUserShellContext{
			ShellCommandID:  commandID,
			AcceptedOrdinal: snapshot.HandledCommands[commandID].AcceptedOrdinal,
			Text:            result.PromptContext(),
		})
	}
	if snapshot.ActivePrompt != nil && snapshot.ActivePrompt.CommandID == commandID {
		snapshot.ActivePrompt = nil
		if !snapshot.Goal.Running() {
			snapshot.HarnessTurn = nil
			snapshot.Execution = RelayExecutionStateIdle
		}
	}
	if context := snapshot.PendingPromptContext; context != nil && context.AttachedCommandID != nil &&
		*context.AttachedCommandID == commandID {
		detached := *context
		detached.AttachedCommandID = nil
		snapshot.PendingPromptContext = &detached
	}
	for i := range snapshot.PendingUserShellContexts {
		if attached := snapshot.PendingUserShellContexts[i].AttachedCommandID; attached != nil && *attached == commandID {
			snapshot.PendingUserShellContexts[i].AttachedCommandID = nil
		}
	}
	if _, barrier := command.(RelayCommandBeginCheckpoint); barrier &&
		snapshot.CheckpointBarrier != nil && *snapshot.CheckpointBarrier == commandID {
		snapshot.CheckpointBarrier = nil
		snapshot.CheckpointReadyThrough = nil
		snapshot.CheckpointReadyDigest = nil
	}
	if _, clear := command.(RelayCommandClearContext); clear {
		snapshot.ActivityTurnStartedAtMs = nil
		snapshot.Execution = RelayExecutionStateIdle
	}
	if _, closing := command.(RelayCommandClose); closing && snapshot.Execution == RelayExecutionStateClosing {
		snapshot.Execution = RelayExecutionStateIdle
	}
	if continuationID == nil {
		return nil
	}
	recoveryID := *continuationID
	if _, occupied := snapshot.HandledCommands[recoveryID]; !workerRestarted || observedCommand != RelayCommandKindPrompt ||
		recoveryID != snapshot.RestartContinuationID(commandID) || occupied {
		return fmt.Errorf("invalid interrupted prompt recovery %s", recoveryID)
	}
	acceptedOrdinal := snapshot.HandledCommands[commandID].AcceptedOrdinal
	// Release every stored copy of the old execution payload before
	// installing its replacement. No journal prefix or snapshot can contain
	// two accepted copies, even at the snapshot byte limit.
	if _, err := snapshot.CompactTerminalCommands(); err != nil {
		return err
	}
	prompt, ok := RelayCommandPromptBlocks(command)
	if !ok {
		return errors.New("recovery command is not a prompt")
	}
	snapshot.HandledCommands[recoveryID] = HandledRelayCommand{Command: command, AcceptedOrdinal: acceptedOrdinal}
	snapshot.Dispatches[recoveryID] = RelayDispatchRecord{Command: command, State: RelayDispatchStateQueued}
	snapshot.QueuedPrompts = slices.Insert(snapshot.QueuedPrompts, 0, StoredQueuedRelayCommand{
		CommandID:   recoveryID,
		Payload:     StoredQueuedRelayPayloadPrompt{Prompt: slices.Clone(prompt)},
		CreatedAtMs: event.RecordedAtMs,
	})
	if hasMailboxContext {
		if snapshot.MailboxPromptContexts == nil {
			snapshot.MailboxPromptContexts = map[string]string{}
		}
		snapshot.MailboxPromptContexts[recoveryID] = mailboxContext
	}
	// This is the same accepted turn, not a fresh mailbox claim. Keep its
	// acceptance and the actual latest input unchanged: input already queued
	// after this turn still supersedes it. A stale retry intent must not
	// also resume it.
	snapshot.CapacityRetry = nil
	snapshot.RetryAssessment = nil
	snapshot.Continuation.QuotaRecovery = nil
	return nil
}

func applyHarnessTurnSettled(snapshot *RelaySnapshot, event *RelayEvent, settled *RelayObservationHarnessTurnSettled) {
	ordinal := event.Ordinal
	if turn := settled.Autonomous; turn != nil {
		// The same rule as a prompted turn's outcome: whatever started the
		// turn, its end decides what the worker watches.
		snapshot.WorkLiveness = WorkLivenessFromTurnFacts(ordinal, turn.WorkFacts)
		snapshot.HarnessTurnWork = nil
		if turn.WorkFacts != nil {
			snapshot.HarnessTurnWork = &HarnessTurnWork{CompletedOrdinal: ordinal, Facts: *turn.WorkFacts}
		}
		snapshot.TurnCompletion = &TurnCompletion{
			CommandID:        HarnessTurnID(turn.StartOrdinal),
			CompletedOrdinal: ordinal,
			Decision:         DecisionKeepCurrent,
		}
	}
	autonomousTurn := snapshot.AutonomousHarnessTurn()
	if autonomousTurn != nil && !settled.PromptInFlight {
		snapshot.CompletedReplyMessage = completedReplyMessage(snapshot)
		// Physical completion does not depend on a retained user command.
		// Missing authorization gates nudges, never provider assessment.
		assessment := TurnAssessmentPending(
			HarnessTurnID(autonomousTurn.FirstOrdinal),
			ordinal,
			event.RecordedAtMs,
			CompletionEvidenceBounded("harness_turn_settled", nil),
		)
		snapshot.Assessment = &assessment
	}
	if autonomousTurn != nil && !settled.PromptInFlight && snapshot.Continuation.UserCommandID != nil {
		id := HarnessTurnID(autonomousTurn.FirstOrdinal)
		snapshot.Continuation.CompletedCommandID = &id
		snapshot.Continuation.HarnessTurn = &HarnessCompletion{
			ID:             id,
			StartPosition:  autonomousTurn.FirstOrdinal,
			SettledOrdinal: ordinal,
			SettledAtMs:    event.RecordedAtMs,
		}
	}
	snapshot.HarnessTurn = nil
	if snapshot.ActivePrompt == nil && snapshot.Execution == RelayExecutionStateRunning {
		snapshot.Execution = RelayExecutionStateIdle
	}
}

// completedReplyMessage is the transcript id of the open assistant reply, if
// any.
func completedReplyMessage(snapshot *RelaySnapshot) *string {
	if snapshot.AssessmentContext == nil || snapshot.AssessmentContext.OpenAssistantID == nil {
		return nil
	}
	if id, ok := TranscriptStableID(*snapshot.AssessmentContext.OpenAssistantID); ok {
		return &id
	}
	return nil
}

func applyNativeAgentEvent(snapshot *RelaySnapshot, event NativeAgentEvent) {
	target := func() map[string]NativeAgent {
		if snapshot.NativeAgentReplay != nil {
			return *snapshot.NativeAgentReplay
		}
		if snapshot.NativeAgents == nil {
			snapshot.NativeAgents = map[string]NativeAgent{}
		}
		return snapshot.NativeAgents
	}
	switch event := event.(type) {
	case NativeAgentEventAvailability:
		for id, agent := range snapshot.NativeAgents {
			agent.ApplyAvailability(event.Reports, event.Complete)
			snapshot.NativeAgents[id] = agent
		}
	case NativeAgentEventSpawned:
		target()[event.SessionID] = NativeAgent{
			OwnerSessionID:  snapshot.SessionID,
			SessionID:       event.SessionID,
			ParentSessionID: cloneStringPtr(event.ParentSessionID),
			Name:            event.Name,
			Task:            event.Task,
			Capabilities:    event.Capabilities,
			State:           NativeAgentStateRunning,
		}
	case NativeAgentEventState:
		agents := target()
		if agent, ok := agents[event.SessionID]; ok {
			agent.State = event.State
			agents[event.SessionID] = agent
		}
	case NativeAgentEventReplayBegin, NativeAgentEventDisconnected:
		snapshot.NativeAgentReplay = nil
		if _, begin := event.(NativeAgentEventReplayBegin); begin {
			snapshot.NativeAgentReplay = &map[string]NativeAgent{}
		}
		for id, agent := range snapshot.NativeAgents {
			agent.InvalidateAvailability()
			if agent.State == NativeAgentStateRunning {
				agent.State = NativeAgentStateDisconnected
			}
			snapshot.NativeAgents[id] = agent
		}
	case NativeAgentEventReplayCommit:
		if replayed := snapshot.NativeAgentReplay; replayed != nil {
			snapshot.NativeAgentReplay = nil
			if snapshot.NativeAgents == nil {
				snapshot.NativeAgents = map[string]NativeAgent{}
			}
			for id, agent := range *replayed {
				agent.FinishReplay()
				snapshot.NativeAgents[id] = agent
			}
		}
	case NativeAgentEventUpdate:
	}
}

func applyMailboxEventsDelivered(snapshot *RelaySnapshot, event *RelayEvent, delivered *RelayObservationMailboxEventsDelivered) error {
	eventKeys, steering := delivered.EventKeys, delivered.Steering
	if len(eventKeys) == 0 && steering == nil {
		return errors.New("mailbox delivery claim has no event keys")
	}
	uniqueKeys := map[string]bool{}
	for _, key := range eventKeys {
		uniqueKeys[key] = true
	}
	if len(uniqueKeys) != len(eventKeys) {
		return errors.New("mailbox delivery claim repeats an event key")
	}
	claimPending := func(missing string) ([]MailboxEvent, error) {
		claimed := make([]MailboxEvent, 0, len(eventKeys))
		for _, key := range eventKeys {
			index := slices.IndexFunc(snapshot.PendingMailboxEvents, func(pending MailboxEvent) bool { return pending.Key == key })
			if index < 0 {
				return nil, errors.New(missing)
			}
			claimed = append(claimed, snapshot.PendingMailboxEvents[index])
		}
		return claimed, nil
	}
	hookEvent := ""
	if delivered.HookEvent != nil {
		hookEvent = *delivered.HookEvent
	}
	toolBoundary := delivered.HookEvent != nil && (hookEvent == "PostToolUse" || hookEvent == "PostToolBatch")
	var claimed []MailboxEvent
	switch delivered.Path {
	case MailboxDeliveryPathPrompt:
		if delivered.PromptCommandID == nil || delivered.HookEvent != nil || delivered.LeaseID != nil {
			return errors.New("prompt mailbox claim has no command ID")
		}
		commandID := *delivered.PromptCommandID
		if steering != nil {
			return errors.New("prompt mailbox claim cannot complete a steer")
		}
		var err error
		if claimed, err = claimPending("prompt mailbox claim names an event that is not pending"); err != nil {
			return err
		}
		dispatch, ok := snapshot.Dispatches[commandID]
		if !ok {
			return errors.New("prompt mailbox claim names an unknown command")
		}
		if dispatch.State != RelayDispatchStatePending {
			return errors.New("prompt mailbox claim names a command that is not pending")
		}
		prompt, ok := RelayCommandPromptBlocks(dispatch.Command)
		if !ok {
			return errors.New("prompt mailbox claim names a non-prompt command")
		}
		if _, _, context := ContextCommandOf(prompt); PromptRequestsCompaction(prompt) || PromptIsSlashCommand(prompt) || context {
			return errors.New("prompt mailbox claim names a context command")
		}
		if _, owned := snapshot.MailboxPromptContexts[commandID]; owned {
			return errors.New("prompt already owns a mailbox delivery claim")
		}
		if snapshot.MailboxPromptContexts == nil {
			snapshot.MailboxPromptContexts = map[string]string{}
		}
		snapshot.MailboxPromptContexts[commandID] = RenderMailboxEvents(claimed)
	case MailboxDeliveryPathToolHook:
		switch {
		case delivered.PromptCommandID == nil && toolBoundary && delivered.LeaseID != nil:
			lease := snapshot.MailboxHookLease
			if lease == nil {
				return errors.New("tool-hook acknowledgement names no active lease")
			}
			leasedKeys := make([]string, 0, len(lease.Events))
			for _, leased := range lease.Events {
				leasedKeys = append(leasedKeys, leased.Key)
			}
			if lease.LeaseID != *delivered.LeaseID || lease.HookEvent != hookEvent ||
				!reflect.DeepEqual(lease.Steering, steering) || !slices.Equal(leasedKeys, eventKeys) {
				return errors.New("tool-hook acknowledgement does not match its active lease")
			}
			if len(eventKeys) == 0 && lease.Steering == nil {
				return errors.New("tool-hook acknowledgement has no leased delivery")
			}
			claimed = slices.Clone(lease.Events)
		// Older revision-16 journal entries claimed hook delivery directly
		// from pending. Keep them readable when upgrading.
		case delivered.PromptCommandID == nil && toolBoundary:
			if steering != nil {
				return errors.New("legacy tool-hook claim cannot complete a steer")
			}
			var err error
			if claimed, err = claimPending("legacy mailbox delivery claim names an event that is not pending"); err != nil {
				return err
			}
		default:
			return errors.New("tool-hook mailbox claim has invalid hook metadata")
		}
	case MailboxDeliveryPathWake:
		return errors.New("wake mailbox claims are recorded by MailboxWake commands")
	default:
		return fmt.Errorf("unknown mailbox delivery path %q", delivered.Path)
	}
	if len(delivered.Events) > 0 && !mailboxEventsEqual(delivered.Events, claimed) {
		return errors.New("mailbox delivery event details do not match the claimed events")
	}
	if delivered.LeaseID != nil && !mailboxEventsEqual(delivered.Events, claimed) {
		return errors.New("leased hook delivery is missing its event details")
	}
	recordHandbackMailboxAssignments(snapshot, claimed)
	if delivered.Path == MailboxDeliveryPathToolHook && delivered.LeaseID != nil {
		snapshot.MailboxHookLease = nil
	} else {
		snapshot.PendingMailboxEvents = slices.DeleteFunc(snapshot.PendingMailboxEvents, func(pending MailboxEvent) bool {
			return uniqueKeys[pending.Key]
		})
	}
	retainDeliveredMailboxEventKeys(snapshot, eventKeys, event.Ordinal)
	if steering != nil {
		return completeHookSteering(snapshot, steering, event.Ordinal)
	}
	return nil
}

// recordCatalogueMode: a catalogue and a legacy CurrentModeUpdate are both
// ordered relay facts. Keep one effective value so a later recovery cut need
// not guess precedence.
func recordCatalogueMode(snapshot *RelaySnapshot) {
	if mode := ConfigCurrentValue(snapshot.ConfigOptions, "mode"); mode != nil {
		snapshot.Config["mode"] = *mode
	}
}

// CompactTerminalCommands drops execution payloads only after every side
// effect of a terminal event. Also upgrades legacy receipts when a stored
// snapshot is opened.
func (s *RelaySnapshot) CompactTerminalCommands() (bool, error) {
	changed := false
	for _, id := range slices.Sorted(maps.Keys(s.HandledCommands)) {
		handled := s.HandledCommands[id]
		if handled.TerminalOrdinal == nil {
			continue
		}
		if command := handled.Command; command != nil {
			digest, err := relayCommandDigest(command)
			if err != nil {
				return changed, err
			}
			var mailboxEventKey *string
			if delivery, ok := command.(RelayCommandDeliverMailboxEvent); ok {
				key := delivery.Event.Key
				mailboxEventKey = &key
			}
			handled.TerminalIdentity = &TerminalRelayCommandIdentity{
				Digest:               digest,
				MailboxEventKey:      mailboxEventKey,
				Prompt:               RelayCommandKindOf(command) == RelayCommandKindPrompt,
				CancelsCapacityRetry: cancelsCapacityRetry(command),
				ReleasesHistory:      ReleasesHistory(command),
			}
			handled.Command = nil
			s.HandledCommands[id] = handled
			changed = true
		}
		if _, ok := s.Dispatches[id]; ok {
			delete(s.Dispatches, id)
			changed = true
		}
	}
	return changed, nil
}

// ReleasesHistory: legacy archive completions also retry collection of an
// acknowledged prefix. Their coverage marker no longer determines current
// journal retention.
func ReleasesHistory(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandCompleteCheckpoint, RelayCommandAdvanceRecoveryFloor:
		return true
	}
	return false
}

func terminalizeRemovedPrompts(snapshot *RelaySnapshot, removedCommandIDs []string, terminalOrdinal uint64) error {
	for _, commandID := range removedCommandIDs {
		dispatch, ok := snapshot.Dispatches[commandID]
		if !ok {
			return fmt.Errorf("removed unknown queued command %s", commandID)
		}
		if !RelayCommandIsQueueEntry(dispatch.Command) || dispatch.State != RelayDispatchStateQueued {
			return fmt.Errorf("removed command %s is not a queued command", commandID)
		}
		setDispatchState(snapshot, commandID, RelayDispatchStateRejected)
		if !updateHandled(snapshot, commandID, func(handled *HandledRelayCommand) {
			handled.TerminalOrdinal = &terminalOrdinal
			failure := "queued command was removed"
			handled.Failure = &failure
		}) {
			return fmt.Errorf("removed command %s is not in the ledger", commandID)
		}
		// A removed entry never starts, so nothing else will release the
		// mailbox context it holds (a restart continuation inherits one).
		delete(snapshot.MailboxPromptContexts, commandID)
	}
	snapshot.QueuedPrompts = slices.DeleteFunc(snapshot.QueuedPrompts, func(queued StoredQueuedRelayCommand) bool {
		return slices.Contains(removedCommandIDs, queued.CommandID)
	})
	return nil
}

func ValidateRelaySnapshotFrontiers(snapshot *RelaySnapshot) error {
	if snapshot.AcknowledgedThrough > snapshot.LatestOrdinal {
		return errors.New("relay acknowledgement is ahead of the event frontier")
	}
	if snapshot.RecoveryFloorOrdinal > snapshot.LatestOrdinal {
		return errors.New("relay recovery floor is ahead of the event frontier")
	}
	for _, check := range []struct{ digest, name string }{
		{snapshot.LatestDigest, "relay latest digest"},
		{snapshot.AcknowledgedDigest, "relay acknowledgement digest"},
		{snapshot.RecoveryFloorDigest, "relay recovery floor digest"},
	} {
		if err := ValidateRelayDigest(check.digest, check.name); err != nil {
			return err
		}
	}
	if (snapshot.LatestOrdinal == 0) != (snapshot.LatestDigest == RelayEventGenesisDigest) {
		return errors.New("relay latest frontier and genesis digest disagree")
	}
	if (snapshot.AcknowledgedThrough == 0) != (snapshot.AcknowledgedDigest == RelayEventGenesisDigest) {
		return errors.New("relay acknowledgement frontier and genesis digest disagree")
	}
	if (snapshot.RecoveryFloorOrdinal == 0) != (snapshot.RecoveryFloorDigest == RelayEventGenesisDigest) {
		return errors.New("relay recovery floor and genesis digest disagree")
	}
	return nil
}

// cancelsCapacityRetry: explicit user work and lifecycle admission supersede
// automated recovery.
func cancelsCapacityRetry(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandPrompt, RelayCommandCancel, RelayCommandCancelTurn, RelayCommandSetConfig,
		RelayCommandRestoreExecutionMode, RelayCommandGoalControl, RelayCommandSetSessionMode, RelayCommandClose:
		return true
	}
	return false
}

func recordHandbackMailboxAssignments(snapshot *RelaySnapshot, events []MailboxEvent) {
	for i := range events {
		id, text, ok := events[i].HandbackAssignment()
		if !ok {
			continue
		}
		assignmentID := id
		snapshot.HandbackChecks.AssignmentID = &assignmentID
		handbackContext(snapshot).HandbackUser(id, []acpschema.ContentBlock{relayTextBlock(text)})
	}
}

// handbackContext is the handback instructions history, allocated on first
// use (Rust's get_or_insert_default).
func handbackContext(snapshot *RelaySnapshot) *ContextHistory {
	if snapshot.HandbackChecks.Context == nil {
		context := DefaultContextHistory()
		snapshot.HandbackChecks.Context = &context
	}
	return snapshot.HandbackChecks.Context
}

// editAssessmentContext applies edit to a private copy of the assessment
// history, when there is one; published snapshots may share the old one.
func editAssessmentContext(snapshot *RelaySnapshot, edit func(*ContextHistory)) {
	if snapshot.AssessmentContext == nil {
		return
	}
	context := *snapshot.AssessmentContext
	context.Messages = slices.Clone(context.Messages)
	edit(&context)
	snapshot.AssessmentContext = &context
}

// editAssessment applies edit to a private copy of the assessment, when
// there is one.
func editAssessment(snapshot *RelaySnapshot, edit func(*TurnAssessment)) {
	if snapshot.Assessment == nil {
		return
	}
	assessment := *snapshot.Assessment
	edit(&assessment)
	snapshot.Assessment = &assessment
}

// applyAssessmentContext: classification boundaries and action invalidation
// share the journal owner.
func applyAssessmentContext(snapshot *RelaySnapshot, event *RelayEvent) {
	var delivered *string
	switch observation := event.Observation.(type) {
	case RelayObservationCommandStarted:
		if dispatch, ok := snapshot.Dispatches[observation.CommandID]; ok && RelayCommandKindOf(dispatch.Command) == RelayCommandKindPrompt {
			// A withdrawn old permission may resolve after the next turn
			// starts. It must never mark that newer prompt cancelled.
			snapshot.AssessmentQuestions = slices.DeleteFunc(snapshot.AssessmentQuestions, func(id string) bool {
				return strings.HasPrefix(id, ToolPermissionIDPrefix)
			})
		}
		id := observation.CommandID
		delivered = &id
	case RelayObservationCommandCompleted:
		if steered, ok := observation.Outcome.(RelayCommandOutcomeSteered); ok {
			id := steered.QueuedCommandID
			delivered = &id
		}
	case RelayObservationMailboxEventsDelivered:
		if observation.Steering != nil {
			id := observation.Steering.QueuedPromptID
			delivered = &id
		}
	}
	if delivered != nil {
		id := *delivered
		dispatch, known := snapshot.Dispatches[id]
		if wake, ok := dispatch.Command.(RelayCommandMailboxWake); known && ok && !IsRestartContinuation(id) {
			recordHandbackMailboxAssignments(snapshot, slices.Clone(wake.Events))
		}
		if prompt, ok := relayDispatchPrompt(snapshot, id); ok {
			// The initial assignment can be queued before the worker records
			// that it is managed. Establish its quota frontier on dispatch.
			producer, generated := GeneratedPromptOf(id)
			if snapshot.ManagedSubagent &&
				(GeneratedPromptIsSubagentAssignment(id) || (generated && producer == GeneratedPromptQuotaRetry)) {
				beginDeliveredMessageTurn(&snapshot.Continuation, id)
			}
			if GeneratedPromptIsSubagentAssignment(id) {
				assignmentID := id
				snapshot.HandbackChecks.AssignmentID = &assignmentID
				handbackContext(snapshot)
			}
			if snapshot.HandbackChecks.Context != nil {
				snapshot.HandbackChecks.Context.HandbackUser(id, prompt)
			}
		}
		switch dispatch.Command.(type) {
		case RelayCommandHandbackReminder, RelayCommandHoldFollowUp:
		default:
			if prompt, ok := relayDispatchPrompt(snapshot, id); ok {
				editAssessmentContext(snapshot, func(context *ContextHistory) { context.User(id, prompt) })
			}
		}
	}
	switch observation := event.Observation.(type) {
	case RelayObservationElicitationRequested:
		snapshot.AssessmentQuestions = insertSorted(snapshot.AssessmentQuestions, observation.Request.ID)
		if snapshot.PendingElicitations == nil {
			snapshot.PendingElicitations = map[string]*ElicitationRequest{}
		}
		request := observation.Request
		snapshot.PendingElicitations[observation.Request.ID] = &request
	case RelayObservationElicitationResolved:
		var pending bool
		snapshot.AssessmentQuestions, pending = removeSorted(snapshot.AssessmentQuestions, observation.ElicitationID)
		if pending && observation.Action == "cancel" && strings.HasPrefix(observation.ElicitationID, ToolPermissionIDPrefix) {
			// Permission Cancel and relay Cancel stop the same accepted
			// prompt. Restart recovery reads this single durable fact.
			snapshot.CancellingPromptID = nil
			if snapshot.ActivePrompt != nil {
				id := snapshot.ActivePrompt.CommandID
				snapshot.CancellingPromptID = &id
			}
		}
		delete(snapshot.PendingElicitations, observation.ElicitationID)
		if observation.Reply != nil {
			clearNotifications(snapshot, event.Ordinal)
			editAssessmentContext(snapshot, func(context *ContextHistory) { context.SetAsideReplies(true) })
		}
	case RelayObservationElicitationsCleared:
		snapshot.AssessmentQuestions = []string{}
		snapshot.PendingElicitations = nil
	case RelayObservationSessionUpdate:
		update := observation.Update
		if update.Kind == acpschema.SessionUpdateKindAgentMessageChunk && update.AgentMessageChunk != nil {
			chunk := update.AgentMessageChunk
			if chunk.Content.Kind == acpschema.ContentBlockKindText && chunk.Content.Text != nil {
				var messageID *string
				if chunk.MessageID != nil {
					id := string(*chunk.MessageID)
					messageID = &id
				}
				text := chunk.Content.Text.Text
				editAssessmentContext(snapshot, func(context *ContextHistory) {
					context.Assistant(messageID, event.Ordinal, text)
				})
			}
		}
	case RelayObservationCommandCompleted:
		switch outcome := observation.Outcome.(type) {
		case RelayCommandOutcomePrompt:
			commandID := observation.CommandID
			snapshot.CompletedReplyMessage = completedReplyMessage(snapshot)
			assessment := TurnAssessmentPending(commandID, event.Ordinal, event.RecordedAtMs,
				CompletionEvidenceBounded(outcome.StopReason, outcome.Diagnostic))
			completion := ClassifyPromptCompletion(outcome.StopReason)
			if completion == PromptCompletionCancelled || completion == PromptCompletionInputRequired {
				assessment.Status = StatusSuperseded
				assessment.Reason = "explicit_handoff_or_cancellation"
			}
			if !snapshot.ManagedSubagent && completion == PromptCompletionInputRequired {
				snapshot.Request = &OpenRequest{
					Question:        InferredQuestion{TurnID: commandID, CompletedOrdinal: event.Ordinal},
					QuestionMessage: cloneStringPtr(snapshot.CompletedReplyMessage),
				}
				var message EntryMessage = EntryMessageNone{}
				if snapshot.CompletedReplyMessage != nil {
					message = EntryMessageFound{Value: *snapshot.CompletedReplyMessage}
				}
				recordNotification(snapshot, event.Ordinal, event.RecordedAtMs, NotificationKindNeedsInput, message, event.Ordinal)
			}
			snapshot.Assessment = &assessment
			if !snapshot.Goal.Running() {
				editAssessmentContext(snapshot, func(context *ContextHistory) { context.OpenAssistantID = nil })
			}
		case RelayCommandOutcomeContextCleared:
			context := DefaultContextHistory()
			snapshot.AssessmentContext = &context
			snapshot.Assessment = nil
			// The conversation that held any open request is gone, so fence
			// it like person input; a fresh conversation inherits no
			// question.
			clearNotifications(snapshot, event.Ordinal)
			snapshot.CompletedReplyMessage = nil
		}
	case RelayObservationSessionRestarted:
		editAssessmentContext(snapshot, func(context *ContextHistory) { context.OpenAssistantID = nil })
	case RelayObservationHarnessTurnSettled:
		if !observation.PromptInFlight {
			editAssessmentContext(snapshot, func(context *ContextHistory) { context.OpenAssistantID = nil })
		}
	case RelayObservationCommandQueued:
		commandID, command := observation.CommandID, observation.Command
		automatic := IsGeneratedPrompt(commandID) || IsQuotaGoalResume(commandID)
		switch command.(type) {
		case RelayCommandHandbackReminder, RelayCommandHoldFollowUp, RelayCommandMailboxWake:
			automatic = true
		}
		editAssessment(snapshot, func(a *TurnAssessment) {
			_, isPrompt := RelayCommandPromptBlocks(command)
			if recovery, ok := command.(RelayCommandSetQuotaRecovery); ok {
				if a.Current() && a.Action != nil && *a.Action == AssessmentActionRecoverQuota {
					scheduled := recovery.Recovery != nil && recovery.Recovery.RetryAtMs != nil
					a.Status = StatusAssessed
					if scheduled {
						a.Status = StatusScheduled
					}
					switch {
					case scheduled:
						a.Reason = "quota_retry_scheduled"
					case recovery.Recovery != nil:
						a.Reason = "quota_deadline_unavailable"
					default:
						a.Reason = "quota_recovery_cleared"
					}
				}
				return
			}
			if automatic && (isPrompt || IsQuotaGoalResume(commandID)) {
				a.Status = StatusConsumed
				a.Reason = "automatic_action_admitted"
				return
			}
			lifecycle := false
			switch command.(type) {
			case RelayCommandClearContext, RelayCommandCancelTurnFor, RelayCommandApplyQueuedFor:
				lifecycle = true
			}
			if cancelsCapacityRetry(command) || lifecycle {
				a.Status = StatusSuperseded
				a.Reason = "user_or_lifecycle_action"
			}
		})
	case RelayObservationHarnessTurnStarted, RelayObservationClosing, RelayObservationClosed:
		editAssessment(snapshot, func(a *TurnAssessment) {
			a.Status = StatusSuperseded
			a.Reason = "turn_or_lifecycle_changed"
		})
		if snapshot.CapacityRetry == nil || !snapshot.CapacityRetry.Submitted {
			snapshot.CapacityRetry = nil
		}
	}
}

// relayDispatchPrompt is the prompt the dispatched command sends, if the
// command is still dispatched and sends one.
func relayDispatchPrompt(snapshot *RelaySnapshot, commandID string) ([]acpschema.ContentBlock, bool) {
	dispatch, ok := snapshot.Dispatches[commandID]
	if !ok {
		return nil, false
	}
	return RelayCommandPromptBlocks(dispatch.Command)
}

// PromptMayHaveReachedAgent reports whether this prompt may already have
// reached the harness, so the native session may hold its history.
func (r *RelayDispatchRecord) PromptMayHaveReachedAgent() bool {
	_, prompt := RelayCommandPromptBlocks(r.Command)
	return prompt && r.State != RelayDispatchStateQueued && r.State != RelayDispatchStatePending
}

// beginDeliveredMessageTurn: the accepted delivery owns a new turn even after
// restore or cancellation. It may recover from quota without granting new
// authorization or renewing the ordinary nudge allowance.
func beginDeliveredMessageTurn(continuation *ContinuationState, commandID string) {
	if continuation.UserCommandID == nil {
		id := commandID
		continuation.UserCommandID = &id
		continuation.Suppressed = true
	}
	continuation.CompletedCommandID = nil
	continuation.HarnessTurn = nil
	continuation.QuotaSuppressed = false
	continuation.QuotaRecovery = nil
}
