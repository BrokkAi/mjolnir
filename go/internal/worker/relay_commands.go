// Port of mj-worker/src/relay/commands.rs.
//
// The relay's durable command queue: acceptance, the claim scheduler,
// terminal receipts, mailbox delivery and the lifecycle barriers.

package worker

import (
	"cmp"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"reflect"
	"slices"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"
	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

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

// relayRandomHex is 16 random bytes in lowercase hex (Rust's getrandom
// followed by mj_core::hex::lower_hex).
func relayRandomHex(purpose string) (string, error) {
	var random [16]byte
	if _, err := rand.Read(random[:]); err != nil {
		return "", fmt.Errorf("generate %s: %w", purpose, err)
	}
	return core.LowerHex(random[:]), nil
}

// relayProtocolRefusal is Rust's Ok(Err(relay_protocol_error(...))): a
// command refused with an answer rather than a failure.
func relayProtocolRefusal(code core.RelayErrorCode, message string, retryable bool) *core.RelayProtocolError {
	refusal := core.NewRelayProtocolError(code, message, retryable, nil)
	return &refusal
}

// relayProtocolErrorDebug renders a refusal as Rust's {:?} does, for the
// error messages that embed one.
func relayProtocolErrorDebug(refusal *core.RelayProtocolError) string {
	detail := "None"
	if refusal.Detail != nil {
		detail = fmt.Sprintf("Some(%+v)", refusal.Detail)
	}
	return fmt.Sprintf("RelayProtocolError { code: %s, message: %s, retryable: %t, detail: %s }",
		relayCommandKindDebug(core.RelayCommandKind(refusal.Code)), core.RustDebugString(refusal.Message), refusal.Retryable, detail)
}

// relayCommandEqual is Rust's RelayCommand equality.
func relayCommandEqual(a, b core.RelayCommand) bool {
	return core.RelayCommandsEqual(a, b)
}

// relayStagedSnapshot is a copy of the snapshot whose maps, slices and
// in-place-edited records can change without touching the original (Rust's
// RelaySnapshot::clone before a staged commit).
func relayStagedSnapshot(snapshot *core.RelaySnapshot) core.RelaySnapshot {
	next := *snapshot
	next.PendingElicitations = maps.Clone(snapshot.PendingElicitations)
	next.NativeAgents = maps.Clone(snapshot.NativeAgents)
	if snapshot.NativeAgentReplay != nil {
		replay := maps.Clone(*snapshot.NativeAgentReplay)
		next.NativeAgentReplay = &replay
	}
	next.CommandRetention = maps.Clone(snapshot.CommandRetention)
	next.Config = maps.Clone(snapshot.Config)
	next.DeliveredMailboxEventKeys = maps.Clone(snapshot.DeliveredMailboxEventKeys)
	next.MailboxPromptContexts = maps.Clone(snapshot.MailboxPromptContexts)
	next.ActiveUserShells = maps.Clone(snapshot.ActiveUserShells)
	next.HandledCommands = maps.Clone(snapshot.HandledCommands)
	next.Dispatches = maps.Clone(snapshot.Dispatches)
	next.AssessmentQuestions = slices.Clone(snapshot.AssessmentQuestions)
	next.InitialCommandIDs = slices.Clone(snapshot.InitialCommandIDs)
	next.QueuedPrompts = slices.Clone(snapshot.QueuedPrompts)
	next.PendingUserShellContexts = slices.Clone(snapshot.PendingUserShellContexts)
	next.PendingMailboxEvents = slices.Clone(snapshot.PendingMailboxEvents)
	if snapshot.PendingPromptContext != nil {
		context := *snapshot.PendingPromptContext
		next.PendingPromptContext = &context
	}
	return next
}

// relayAttachmentReferences is mj_core::attachment::references for a prompt
// in the stable ACP schema. The attachment module reads the unstable schema;
// the two share the wire form, so the prompt crosses by its JSON.
func relayAttachmentReferences(prompt []acpschema.ContentBlock) ([]core.AttachmentRef, error) {
	encoded, err := json.Marshal(prompt)
	if err != nil {
		return nil, err
	}
	var unstable []acpunstable.ContentBlock
	if err := json.Unmarshal(encoded, &unstable); err != nil {
		return nil, err
	}
	return core.References(unstable)
}

func (r *DurableRelay) returnBoundarySteerAfterTurnEnd(activePromptID string) error {
	if r.steeringDeliveryMode.usesAcp() {
		return nil
	}
	steering := r.snapshot.Steering
	if steering == nil || steering.ActivePromptID != activePromptID || steering.Status != core.SteeringStatusPending {
		return nil
	}
	operation := *steering
	dispatch, ok := r.snapshot.Dispatches[operation.CommandID]
	if !ok {
		return errors.New("boundary steer command disappeared before its turn ended")
	}
	steer, isSteer := dispatch.Command.(core.RelayCommandSteer)
	if !isSteer || steer.ActivePromptID != operation.ActivePromptID || steer.QueuedPromptID != operation.QueuedPromptID {
		return errors.New("boundary steer command changed before its turn ended")
	}
	if lease := r.snapshot.MailboxHookLease; lease != nil && lease.Steering != nil &&
		lease.Steering.CommandID == operation.CommandID &&
		lease.Steering.ActivePromptID == operation.ActivePromptID &&
		lease.Steering.QueuedPromptID == operation.QueuedPromptID {
		if err := r.returnMailboxHookLease(core.MailboxHookLeaseReturnReasonTurnEnded); err != nil {
			return err
		}
	}
	commandID := operation.CommandID
	state := r.snapshot.Dispatches[commandID].State
	if state == core.RelayDispatchStateQueued {
		if _, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandStarted{
			CommandID:   commandID,
			StartedAtMs: core.EpochMillis(),
		}); err != nil {
			return err
		}
	} else if state != core.RelayDispatchStatePending {
		return errors.New("boundary steer command is not pending at its turn end")
	}
	kind := core.RelayCommandKindSteer
	_, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandCompleted{
		Command:   &kind,
		CommandID: commandID,
		Outcome:   core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: operation.QueuedPromptID},
	})
	return err
}

// turnControls: one serialized owner chooses the actions we publish and
// admit.
func (r *DurableRelay) turnControls() core.TurnControls {
	message := func(text string) *string { return &text }
	controls := core.TurnControls{Status: core.TurnControlStatusUnavailable}
	active := r.snapshot.ActivePrompt
	if active == nil {
		if r.snapshot.Steering != nil && r.snapshot.Steering.Status == core.SteeringStatusUnconfirmed {
			controls.Status = core.TurnControlStatusUnconfirmed
			controls.Message = message("Delivery unconfirmed")
		}
		return controls
	}
	if r.checkpointOnly || r.snapshot.CheckpointBarrier != nil {
		controls.Message = message("A checkpoint is in progress.")
		return controls
	}
	if r.turnCancellationPending() {
		controls.Status = core.TurnControlStatusPending
		controls.Message = message("Interrupting…")
		return controls
	}
	var interrupt core.RelayCommand
	if r.hasHeldClaudeTurn() {
		interrupt = core.RelayCommandCancelTurn{}
	} else {
		interrupt = core.RelayCommandCancelTurnFor{ActivePromptID: active.CommandID}
	}
	controls.Interrupt = interrupt
	if steering := r.snapshot.Steering; steering != nil && steering.HoldsQueue() {
		if steering.Status == core.SteeringStatusUnconfirmed {
			controls.Status = core.TurnControlStatusUnconfirmed
			controls.Message = message("Delivery unconfirmed")
		} else {
			controls.Status = core.TurnControlStatusPending
			controls.Message = message("Steering…")
		}
		return controls
	}
	controls.Status = core.TurnControlStatusReady
	primary := core.TurnControlAction{Kind: core.TurnControlKindInterrupt, Command: interrupt}
	if r.hasHeldClaudeTurn() {
		controls.Primary = &primary
		return controls
	}
	if len(r.snapshot.QueuedPrompts) > 0 {
		head := r.snapshot.QueuedPrompts[0]
		canSteer := false
		if payload, isPrompt := head.Payload.(core.StoredQueuedRelayPayloadPrompt); isPrompt {
			var deliverable bool
			switch r.steeringDeliveryMode {
			case SteeringDeliveryModeAcp, SteeringDeliveryModeAcpWithMailbox:
				deliverable = r.steeringSupported == nil || *r.steeringSupported
			case SteeringDeliveryModeHookPostToolBatch:
				_, deliverable = core.SteeringTextContext(payload.Prompt)
			case SteeringDeliveryModeHookTurnEnd:
				deliverable = false
			}
			dispatch, dispatched := r.snapshot.Dispatches[head.CommandID]
			canSteer = !core.PromptIsSlashCommand(payload.Prompt) && deliverable &&
				dispatched && dispatch.State == core.RelayDispatchStateQueued
		}
		if canSteer {
			primary = core.TurnControlAction{
				Kind:    core.TurnControlKindSteer,
				Command: core.RelayCommandSteer{ActivePromptID: active.CommandID, QueuedPromptID: head.CommandID},
			}
		} else {
			if _, isConfig := head.Payload.(core.StoredQueuedRelayPayloadSetConfig); isConfig {
				var settings []string
				for _, queued := range r.snapshot.QueuedPrompts {
					config, ok := queued.Payload.(core.StoredQueuedRelayPayloadSetConfig)
					if !ok {
						break
					}
					settings = append(settings, config.Key+": "+config.Value)
				}
				controls.Message = message(fmt.Sprintf(
					"Interrupts this turn to apply %s and queued input. Sub-agents continue.", strings.Join(settings, ", ")))
			} else {
				controls.Message = message("Interrupts this turn to run queued input. Sub-agents continue.")
			}
			primary = core.TurnControlAction{
				Kind:    core.TurnControlKindApplyQueued,
				Command: core.RelayCommandApplyQueuedFor{ActivePromptID: active.CommandID, QueuedCommandID: head.CommandID},
			}
		}
	}
	controls.Primary = &primary
	return controls
}

// validateTurnControl returns the refusal text as an error (Rust's
// Result<(), String>).
func (r *DurableRelay) validateTurnControl(command core.RelayCommand) error {
	holdsQueue := r.snapshot.Steering != nil && r.snapshot.Steering.HoldsQueue()
	switch command := command.(type) {
	case core.RelayCommandDismissQuestion:
		request := r.snapshot.OpenRequest()
		if request == nil || request.Question != command.Question ||
			r.snapshot.ActivePrompt != nil ||
			r.snapshot.HarnessTurn != nil ||
			len(r.snapshot.QueuedPrompts) > 0 ||
			len(r.snapshot.AssessmentQuestions) > 0 {
			return errors.New("The prose question is no longer active")
		}
	case core.RelayCommandBeginCheckpoint:
		if holdsQueue {
			return errors.New("Resolve uncertain steering delivery before checkpointing or moving this session")
		}
	case core.RelayCommandSteer, core.RelayCommandApplyQueuedFor, core.RelayCommandCancelTurnFor:
		controls := r.turnControls()
		if (controls.Primary != nil && relayCommandEqual(controls.Primary.Command, command)) ||
			(controls.Interrupt != nil && relayCommandEqual(controls.Interrupt, command)) {
			return nil
		}
		return errors.New("Turn or queue changed; refresh the available controls")
	case core.RelayCommandCancel, core.RelayCommandCancelTurn:
		if r.turnCancellationPending() {
			return errors.New("Cancellation is already pending")
		}
	case core.RelayCommandResolveSteering:
		steering := r.snapshot.Steering
		if steering == nil || steering.CommandID != command.SteeringID {
			return errors.New("The steering operation changed")
		}
		if steering.Status != core.SteeringStatusUnconfirmed || r.snapshot.ActivePrompt != nil {
			return errors.New("Wait for the original turn to settle before retrying uncertain input")
		}
	case core.RelayCommandRemoveQueuedPrompt:
		if holdsQueue && r.snapshot.Steering.QueuedPromptID == command.QueuedCommandID && r.snapshot.ActivePrompt != nil {
			return errors.New("Wait for steering to settle before removing its prompt")
		}
	case core.RelayCommandClearQueuedPrompts:
		if holdsQueue && r.snapshot.ActivePrompt != nil {
			return errors.New("Wait for steering to settle before clearing the queue")
		}
	}
	return nil
}

// DeliverAgentMessage queues a message from Mjolnir for this worker's own
// agent through the same mailbox that carries peer and sub-agent messages:
// it arrives at the agent's next tool boundary or turn start and never
// starts a turn. key must be unique among this session's messages.
func (r *DurableRelay) DeliverAgentMessage(source, key, text string) error {
	createdAtMs := uint64(0)
	if now := core.EpochMillis(); now >= 0 {
		createdAtMs = uint64(now)
	}
	event := core.MailboxEvent{
		Key:         key,
		Source:      source,
		Wake:        false,
		CreatedAtMs: createdAtMs,
		Body:        core.MailboxEventBodyPlainText{Text: text},
	}
	_, refusal, err := r.submitCommand("mailbox-"+key, core.RelayCommandDeliverMailboxEvent{Event: event})
	if err != nil {
		return err
	}
	if refusal != nil {
		return fmt.Errorf("queue agent message %s: %s", key, relayProtocolErrorDebug(refusal))
	}
	return nil
}

// submitCommand accepts a command. The protocol error is the command's
// refusal (Rust's inner Err); err is a failure to apply it (the outer Err).
func (r *DurableRelay) submitCommand(commandID string, command core.RelayCommand) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	return r.submitCommandInner(commandID, command, false)
}

// verifyExpectedStoreID refuses an in-place Move request bound to another
// relay store.
func (r *DurableRelay) verifyExpectedStoreID(expectedStoreID string) *core.RelayProtocolError {
	if strings.TrimSpace(expectedStoreID) == "" {
		return relayProtocolRefusal(core.RelayErrorCodeInvalidRequest, "expected_store_id is required", false)
	}
	if r.snapshot.StoreID == nil || *r.snapshot.StoreID != expectedStoreID {
		actual := "<none>"
		var actualStoreID *string
		if r.snapshot.StoreID != nil {
			actual = *r.snapshot.StoreID
			copied := *r.snapshot.StoreID
			actualStoreID = &copied
		}
		refusal := core.NewRelayProtocolError(
			core.RelayErrorCodeInvalidState,
			fmt.Sprintf("relay store_id mismatch: expected %s, actual %s", expectedStoreID, actual),
			false,
			core.RelayErrorDetailStoreMismatch{ExpectedStoreID: expectedStoreID, ActualStoreID: actualStoreID},
		)
		return &refusal
	}
	return nil
}

// holdInPlaceMoveDispatch atomically holds queue dispatch for a same-target
// in-place Move and returns the pending commands captured with the hold.
func (r *DurableRelay) holdInPlaceMoveDispatch(operationID, expectedStoreID string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	if strings.TrimSpace(operationID) == "" {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidRequest, "operation_id is required", false), nil
	}
	if refusal := r.verifyExpectedStoreID(expectedStoreID); refusal != nil {
		return nil, refusal, nil
	}
	if existing := r.snapshot.InPlaceMoveHold; existing != nil {
		if existing.OperationID == operationID {
			return core.RelayResponsePayloadInPlaceMoveDispatchHeld{
				CapturedPendingCommandIDs: slices.Clone(existing.CapturedPendingCommandIDs),
			}, nil, nil
		}
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("dispatch is already held by in-place move operation %s", existing.OperationID), false), nil
	}
	captured := make([]string, 0, len(r.snapshot.QueuedPrompts))
	for _, queued := range r.snapshot.QueuedPrompts {
		captured = append(captured, queued.CommandID)
	}
	r.snapshot.InPlaceMoveHold = &core.InPlaceMoveHold{
		OperationID:               operationID,
		CapturedPendingCommandIDs: slices.Clone(captured),
	}
	if err := r.persistSnapshot(); err != nil {
		return nil, nil, err
	}
	return core.RelayResponsePayloadInPlaceMoveDispatchHeld{CapturedPendingCommandIDs: captured}, nil, nil
}

// discardHeldMoveCommands idempotently discards held pending commands for
// the active in-place Move hold of operationID.
func (r *DurableRelay) discardHeldMoveCommands(operationID, expectedStoreID string, commandIDs []string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	if refusal := r.verifyExpectedStoreID(expectedStoreID); refusal != nil {
		return nil, refusal, nil
	}
	if r.inPlaceHarnessRestartInFlight != nil {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"cannot discard held commands while an in-place harness restart is in progress", true), nil
	}
	hold := r.snapshot.InPlaceMoveHold
	if hold == nil {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState, "no active in-place move dispatch hold", false), nil
	}
	if hold.OperationID != operationID {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("dispatch is held by a different in-place move operation %s", hold.OperationID), false), nil
	}
	captured := slices.Clone(hold.CapturedPendingCommandIDs)
	for _, commandID := range commandIDs {
		if !slices.Contains(captured, commandID) {
			return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidRequest,
				fmt.Sprintf("command %s was not captured by in-place move hold %s", commandID, operationID), false), nil
		}
	}
	for _, commandID := range commandIDs {
		if slices.ContainsFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
			return queued.CommandID == commandID
		}) {
			if _, err := r.RecordCommandRejected(commandID, core.OutcomeReasonAdmissionRejected, "Discarded by in-place move"); err != nil {
				return nil, nil, err
			}
		}
	}
	return core.RelayResponsePayloadHeldMoveCommandsDiscarded{}, nil, nil
}

// submitFrontCommand submits a continuation to the head of the pending
// queue while operationID holds dispatch.
func (r *DurableRelay) submitFrontCommand(operationID, expectedStoreID, commandID string, command core.RelayCommand) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	if refusal := r.verifyExpectedStoreID(expectedStoreID); refusal != nil {
		return nil, refusal, nil
	}
	if r.inPlaceHarnessRestartInFlight != nil {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"cannot submit to front while an in-place harness restart is in progress", true), nil
	}
	hold := r.snapshot.InPlaceMoveHold
	if hold == nil {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"SubmitFront requires an active in-place move dispatch hold", false), nil
	}
	if hold.OperationID != operationID {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("SubmitFront rejected: dispatch is held by a different in-place move operation %s", hold.OperationID), false), nil
	}
	return r.submitCommandInner(commandID, command, true)
}

// releaseInPlaceMoveDispatch idempotently releases the hold of operationID
// and resumes queue promotion.
func (r *DurableRelay) releaseInPlaceMoveDispatch(operationID, expectedStoreID string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	if refusal := r.verifyExpectedStoreID(expectedStoreID); refusal != nil {
		return nil, refusal, nil
	}
	if r.inPlaceHarnessRestartInFlight != nil {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"cannot release in-place move dispatch hold while an in-place harness restart is in progress", true), nil
	}
	hold := r.snapshot.InPlaceMoveHold
	if hold == nil {
		return core.RelayResponsePayloadInPlaceMoveDispatchReleased{}, nil, nil
	}
	if hold.OperationID != operationID {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("dispatch is held by a different in-place move operation %s", hold.OperationID), false), nil
	}
	r.snapshot.InPlaceMoveHold = nil
	if err := r.persistSnapshot(); err != nil {
		return nil, nil, err
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return nil, nil, err
	}
	return core.RelayResponsePayloadInPlaceMoveDispatchReleased{}, nil, nil
}

// admitInPlaceHarnessRestart is the serialized admission of a harness-only
// restart under the in-place Move hold of operationID.
func (r *DurableRelay) admitInPlaceHarnessRestart(operationID, expectedStoreID string) (InPlaceHarnessRestartDecision, *core.RelayProtocolError) {
	if refusal := r.verifyExpectedStoreID(expectedStoreID); refusal != nil {
		return 0, refusal
	}
	hold := r.snapshot.InPlaceMoveHold
	if hold == nil {
		return 0, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"RestartHarnessInPlace requires an active in-place move dispatch hold", false)
	}
	if hold.OperationID != operationID {
		return 0, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("RestartHarnessInPlace rejected: dispatch is held by a different in-place move operation %s", hold.OperationID), false)
	}
	if r.inPlaceHarnessRestartInFlight != nil {
		return 0, relayProtocolRefusal(core.RelayErrorCodeInvalidState, "an in-place harness restart is already in progress", true)
	}
	if hold.HarnessRestarted {
		return InPlaceHarnessRestartAlreadyRestarted, nil
	}
	if r.turnInProgress() || r.effectfulCommandInProgress() {
		return 0, relayProtocolRefusal(core.RelayErrorCodeInvalidState,
			"cannot restart harness in place while a turn or effectful command is in progress", true)
	}
	inFlight := operationID
	r.inPlaceHarnessRestartInFlight = &inFlight
	return InPlaceHarnessRestartProceed, nil
}

func (r *DurableRelay) cancelInPlaceHarnessRestart(operationID string) {
	if r.inPlaceHarnessRestartInFlight != nil && *r.inPlaceHarnessRestartInFlight == operationID {
		r.inPlaceHarnessRestartInFlight = nil
	}
}

func (r *DurableRelay) markInPlaceHarnessRestarted(operationID, expectedStoreID string) error {
	if r.snapshot.StoreID == nil || *r.snapshot.StoreID != expectedStoreID {
		return fmt.Errorf("relay store_id changed before completing in-place harness restart for %s", operationID)
	}
	if r.inPlaceHarnessRestartInFlight == nil || *r.inPlaceHarnessRestartInFlight != operationID {
		return fmt.Errorf("in-place harness restart for %s is no longer in flight", operationID)
	}
	hold := r.snapshot.InPlaceMoveHold
	if hold == nil {
		return fmt.Errorf("in-place move dispatch hold disappeared before completing harness restart for %s", operationID)
	}
	if hold.OperationID != operationID {
		return fmt.Errorf("in-place move dispatch hold owner changed to %s before completing harness restart for %s", hold.OperationID, operationID)
	}
	hold.HarnessRestarted = true
	if err := r.persistSnapshot(); err != nil {
		return err
	}
	r.inPlaceHarnessRestartInFlight = nil
	return nil
}

func (r *DurableRelay) submitCommandInner(commandID string, command core.RelayCommand, insertAtFront bool) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	invalidRequest := func(message string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidRequest, message, false), nil
	}
	invalidState := func(message string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
		return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState, message, false), nil
	}
	if err := core.EnsureSerializedBudget(command, core.RelayCommandByteBudget, "relay command"); err != nil {
		return invalidRequest(err.Error())
	}
	if !core.ValidCommandID(commandID) {
		return invalidRequest("invalid command ID")
	}
	// A wake is known by its ID wherever its type is gone: restored from a
	// checkpoint or replayed, it is a plain prompt under its wake ID. So a
	// wake needs that ID, and that ID names only a wake or such a prompt.
	wakeID := core.CarriesMailboxWake(commandID)
	var misnamed bool
	switch command.(type) {
	case core.RelayCommandMailboxWake:
		misnamed = !wakeID
	case core.RelayCommandPrompt:
		misnamed = false
	default:
		misnamed = wakeID
	}
	if misnamed {
		return invalidRequest("a mailbox-wake command ID names only a mailbox wake or the prompt it was saved as")
	}
	if prompt, ok := command.(core.RelayCommandPrompt); ok {
		if maintenance, args, isContext := core.ContextCommandOf(prompt.Prompt); isContext {
			if len(prompt.Prompt) != 1 ||
				(maintenance == core.ContextCommandClear && args != "") ||
				(maintenance == core.ContextCommandCompact && args != "" &&
					r.verdictHarness != nil && *r.verdictHarness == core.HarnessKindCodex) {
				return invalidRequest("This context command does not support these arguments or attachments")
			}
			if maintenance == core.ContextCommandClear {
				command = core.RelayCommandClearContext{}
			} else {
				text := "/compact"
				if args != "" {
					text = "/compact " + args
				}
				command = core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{relayTextBlock(text)}}
			}
		}
	}
	if handled, ok := r.snapshot.HandledCommands[commandID]; ok {
		matches, err := handled.MatchesCommand(command)
		if err != nil {
			return nil, nil, err
		}
		if !matches {
			return invalidRequest("command ID was already used for a different command")
		}
		acceptedOrdinal := handled.AcceptedOrdinal
		// A journal append can succeed before snapshot persistence reports
		// an error. Retrying the durable command must resume any remaining
		// relay-local transition instead of merely echoing its first ACK.
		if core.RelayCommandIsRelayLocal(command) || r.isQueuedRecoveryCancel(commandID) {
			if err := r.finishRelayLocalCommand(commandID); err != nil {
				return nil, nil, err
			}
		}
		return core.RelayResponsePayloadAccepted{CommandID: commandID, Ordinal: acceptedOrdinal}, nil, nil
	}
	if delivery, ok := command.(core.RelayCommandDeliverMailboxEvent); ok {
		key := delivery.Event.Key
		_, delivered := r.snapshot.DeliveredMailboxEventKeys[key]
		known := delivered || slices.ContainsFunc(r.snapshot.PendingMailboxEvents, func(pending core.MailboxEvent) bool {
			return pending.Key == key
		})
		if lease := r.snapshot.MailboxHookLease; lease != nil {
			known = known || slices.ContainsFunc(lease.Events, func(leased core.MailboxEvent) bool {
				return leased.Key == key
			})
		}
		if known {
			// A producer may retry after losing the ACK, or independently
			// submit the same event under a fresh command ID. Pending events
			// are always deduplicated; delivered keys cover the retry window.
			return core.RelayResponsePayloadAccepted{CommandID: commandID, Ordinal: r.snapshot.LatestOrdinal}, nil, nil
		}
	}
	if r.snapshot.ClearContextInProgress() {
		return invalidState("Context is being cleared; wait for the new conversation")
	}
	if install, ok := command.(core.RelayCommandInstallPromptContext); ok &&
		(strings.TrimSpace(install.Text) == "" ||
			r.snapshot.ActivePrompt != nil ||
			(r.snapshot.PendingPromptContext != nil && r.snapshot.PendingPromptContext.AttachedCommandID != nil)) {
		return invalidRequest("cannot install empty prompt context or replace context attached to an active prompt")
	}
	if _, clear := command.(core.RelayCommandClearContext); clear {
		if !r.supportsClearContext() {
			return invalidState("This harness does not support /clear")
		}
		if r.snapshot.NativeSessionID == nil ||
			r.snapshot.Execution != core.RelayExecutionStateIdle ||
			r.unfinishedDispatch() ||
			len(r.snapshot.QueuedPrompts) > 0 ||
			r.snapshot.Goal.Running() ||
			r.snapshot.Goal.Active() ||
			r.snapshot.Goal.PendingResume != nil ||
			r.snapshot.Goal.Decision != nil ||
			r.snapshot.HarnessTurn != nil ||
			len(r.snapshot.ActiveUserShells) > 0 ||
			len(r.backgroundCommands()) > 0 ||
			r.nativeAgentCount() > 0 ||
			r.snapshot.CheckpointBarrier != nil ||
			r.snapshot.CapacityRetry != nil {
			return invalidState("/clear requires an idle session with no queued or background work; finish or cancel that work first")
		}
	}
	if prompt, ok := command.(core.RelayCommandPrompt); ok {
		references, err := relayAttachmentReferences(prompt.Prompt)
		if err == nil {
			store := core.AttachmentStoreWorker(r.root)
			for i := range references {
				if _, err = store.Read(&references[i]); err != nil {
					break
				}
			}
		}
		if err != nil {
			return invalidRequest(err.Error())
		}
	}
	pendingCloseBarrier, closePending := r.pendingCloseBarrierID()
	completesPendingClose := false
	if complete, ok := command.(core.RelayCommandCompleteCheckpoint); ok && closePending {
		completesPendingClose = complete.BarrierCommandID == pendingCloseBarrier
	}
	if closePending && !completesPendingClose {
		return invalidState("relay session is sealed for close")
	}
	if (r.snapshot.Execution == core.RelayExecutionStateClosing || r.snapshot.Execution == core.RelayExecutionStateClosed) &&
		!completesPendingClose {
		return invalidState("relay session is closing")
	}
	if prompt, ok := command.(core.RelayCommandPrompt); ok && len(prompt.Prompt) == 0 {
		return invalidRequest("prompt is empty")
	}
	if shell, ok := command.(core.RelayCommandRunUserShell); ok && strings.TrimSpace(shell.Command) == "" {
		return invalidRequest("shell command is empty")
	}
	if cancel, ok := command.(core.RelayCommandCancelUserShell); ok {
		if _, active := r.snapshot.ActiveUserShells[cancel.ShellCommandID]; !active {
			return invalidState("there is no active shell command with that ID")
		}
	}
	if config, ok := command.(core.RelayCommandSetConfig); ok &&
		(strings.TrimSpace(config.Key) == "" || strings.TrimSpace(config.Value) == "") {
		return invalidRequest("configuration key and value are required")
	}
	if notice, ok := command.(core.RelayCommandRecordNotice); ok && strings.TrimSpace(notice.Text) == "" {
		return invalidRequest("notice text is required")
	}
	if delivery, ok := command.(core.RelayCommandDeliverMailboxEvent); ok {
		event := delivery.Event
		if strings.TrimSpace(event.Key) == "" ||
			len(event.Key) > 1024 ||
			strings.TrimSpace(event.Source) == "" ||
			!event.HasRenderableContent() {
			return invalidRequest("mailbox event key, source, and content are required")
		}
		pending := slices.Clone(r.snapshot.PendingMailboxEvents)
		if lease := r.snapshot.MailboxHookLease; lease != nil {
			pending = append(pending, lease.Events...)
		}
		pending = append(pending, event)
		if core.EnsureSerializedBudget(pending, core.RelayMailboxByteBudget, "pending mailbox events") != nil {
			return invalidRequest("session mailbox is full; deliver pending events before adding another")
		}
	}
	if wake, ok := command.(core.RelayCommandMailboxWake); ok &&
		(len(wake.Events) == 0 ||
			!slices.ContainsFunc(wake.Events, func(event core.MailboxEvent) bool { return event.Wake }) ||
			!relayMailboxEventsEqual(wake.Events, r.snapshot.PendingMailboxEvents)) {
		return invalidState("mailbox wake must carry all currently pending events, including a waking event")
	}
	// A late cancellation must not advance the checkpoint cursor or leave a
	// cancellation queued for a future turn after the barrier releases.
	switch command.(type) {
	case core.RelayCommandCancelTurn, core.RelayCommandGoalControl:
		if r.snapshot.CheckpointBarrier != nil {
			return invalidState("the checkpoint barrier is already admitted")
		}
	}
	refusal := r.snapshot.ValidateNotificationCommand(command)
	if refusal == nil {
		refusal = r.validateTurnControl(command)
	}
	if refusal != nil {
		return invalidState(refusal.Error())
	}
	// Claude and Antigravity can cancel a cycle they started on their own.
	// Codex native goals have their own controls. An in-flight native resume
	// is also cancellable without a prompt.
	cancellableHarnessTurn := (r.harnessTurns == HarnessTurnPolicyClaudeAdapter || r.harnessTurns == HarnessTurnPolicyAntigravity) &&
		r.snapshot.HarnessTurn != nil
	if _, cancel := command.(core.RelayCommandCancel); cancel {
		recovery := r.snapshot.Continuation.QuotaRecovery
		resumeInFlight := false
		for _, dispatch := range r.snapshot.Dispatches {
			control, ok := dispatch.Command.(core.RelayCommandGoalControl)
			if dispatch.State == core.RelayDispatchStateInFlight && ok && control.Action == core.GoalControlActionResume {
				resumeInFlight = true
				break
			}
		}
		retry := r.snapshot.CapacityRetry
		if (recovery == nil || recovery.Submitted) &&
			r.snapshot.ActivePrompt == nil &&
			!resumeInFlight &&
			!cancellableHarnessTurn &&
			(retry == nil || retry.Submitted) {
			message := "there is no active prompt to cancel"
			if r.snapshot.HarnessTurn != nil {
				message = "the agent is working on its own after a background task; there is no prompt to cancel"
			}
			return invalidState(message)
		}
	}
	if remove, ok := command.(core.RelayCommandRemoveQueuedPrompt); ok &&
		!slices.ContainsFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
			return queued.CommandID == remove.QueuedCommandID
		}) {
		return invalidRequest("unknown queued prompt")
	}
	var barrierCommandID *string
	switch barrier := command.(type) {
	case core.RelayCommandCompleteCheckpoint:
		barrierCommandID = &barrier.BarrierCommandID
	case core.RelayCommandReleaseCheckpoint:
		barrierCommandID = &barrier.BarrierCommandID
	}
	if barrierCommandID != nil &&
		(r.snapshot.CheckpointBarrier == nil || *r.snapshot.CheckpointBarrier != *barrierCommandID ||
			r.snapshot.CheckpointReadyThrough == nil) {
		return invalidState("checkpoint barrier is not active")
	}
	if floor, ok := command.(core.RelayCommandAdvanceRecoveryFloor); ok {
		message, refused, err := r.recoveryFloorRejection(&floor.Through)
		if err != nil {
			return nil, nil, err
		}
		if refused {
			return invalidState(message)
		}
	}
	if closeCommand, ok := command.(core.RelayCommandClose); ok {
		if message, refused := r.closeCutRefusal(closeCommand.BarrierCommandID, &closeCommand.Expected); refused {
			return invalidState(message)
		}
	}

	if r.checkpointOnly && !core.RelayCommandIsRelayLocal(command) {
		switch command.(type) {
		case core.RelayCommandBeginCheckpoint, core.RelayCommandClose:
		default:
			return invalidState("session is being preserved for Move; resume it to run commands")
		}
	}
	if _, prompt := command.(core.RelayCommandPrompt); r.snapshot.ManagedSubagent && prompt {
		if ordinal, retry := core.GeneratedPromptQuotaRetry.RetryOrdinal(commandID); retry {
			continuation := &r.snapshot.Continuation
			assessment := r.snapshot.Assessment
			if assessment == nil || assessment.CompletedOrdinal != ordinal ||
				continuation.UserCommandID == nil ||
				!r.quotaRecoveryAdmissible(*continuation.UserCommandID, assessment.TurnID) {
				return invalidRequest("the child quota continuation no longer matches its completed turn")
			}
		}
	}
	if schedule, ok := command.(core.RelayCommandSetQuotaRecovery); ok {
		valid := schedule.Expected.Ordinal == r.snapshot.LatestOrdinal &&
			schedule.Expected.Digest == r.snapshot.LatestDigest
		if recovery := schedule.Recovery; valid && recovery != nil {
			deadlines := false
			switch {
			case recovery.ResetAtMs != nil && recovery.RetryAtMs != nil:
				reset := *recovery.ResetAtMs
				deadlines = reset <= math.MaxInt64-60_000 && reset+60_000 == *recovery.RetryAtMs
			case recovery.ResetAtMs == nil && recovery.RetryAtMs == nil:
				deadlines = true
			}
			valid = r.quotaRecoveryAdmissible(recovery.UserCommandID, recovery.CompletedCommandID) &&
				!recovery.Submitted &&
				len(recovery.Notice) <= 4096 &&
				recovery.ProfileID != "" &&
				deadlines
		}
		if !valid {
			slog.Info("quota recovery evidence is stale",
				"command_id", commandID,
				"expected_ordinal", schedule.Expected.Ordinal,
				"expected_digest", schedule.Expected.Digest,
				"actual_ordinal", r.snapshot.LatestOrdinal,
				"actual_digest", r.snapshot.LatestDigest)
			return invalidState("quota recovery evidence is stale")
		}
	}
	if control, ok := command.(core.RelayCommandGoalControl); ok && core.IsQuotaGoalResume(commandID) {
		facts := r.ActivityFacts()
		recovery := r.snapshot.Continuation.QuotaRecovery
		valid := control.Action == core.GoalControlActionResume &&
			r.snapshot.Goal.ResumableAfterQuota() &&
			core.CanSubmit(&facts) &&
			recovery != nil &&
			!recovery.Submitted &&
			recovery.RetryAtMs != nil && *recovery.RetryAtMs <= core.EpochMillis() &&
			r.quotaRecoveryAdmissible(recovery.UserCommandID, recovery.CompletedCommandID)
		if !valid {
			return invalidState("quota goal resume is not due or its goal changed")
		}
	}
	if resume, ok := command.(core.RelayCommandResumeAfterQuota); ok {
		valid := resume.Expected.Ordinal == r.snapshot.LatestOrdinal &&
			resume.Expected.Digest == r.snapshot.LatestDigest
		if recovery := r.snapshot.Continuation.QuotaRecovery; valid {
			valid = recovery != nil &&
				!recovery.Submitted &&
				recovery.CompletedCommandID == resume.CompletedCommandID &&
				recovery.RetryAtMs != nil && *recovery.RetryAtMs <= core.EpochMillis()
			if valid {
				facts := r.ActivityFacts()
				valid = core.CanSubmit(&facts) &&
					r.quotaRecoveryAdmissible(recovery.UserCommandID, resume.CompletedCommandID)
			}
		}
		if !valid {
			return invalidState("quota recovery is not due or its evidence changed")
		}
	}
	var completedCommandID string
	var completedOrdinal uint64
	handback := false
	switch reminder := command.(type) {
	case core.RelayCommandHandbackReminder:
		completedCommandID, completedOrdinal, handback = reminder.CompletedCommandID, reminder.CompletedOrdinal, true
	case core.RelayCommandHoldFollowUp:
		completedCommandID, completedOrdinal, handback = reminder.CompletedCommandID, reminder.CompletedOrdinal, true
	}
	if handback {
		turn := r.snapshot.TurnCompletion
		current := turn != nil && turn.CommandID == completedCommandID && turn.CompletedOrdinal == completedOrdinal
		latestPrompt := r.snapshot.LatestPromptAcceptedOrdinal
		lastHarnessTurn := r.snapshot.LastHarnessTurnStartedOrdinal
		nativeOpened := r.snapshot.NativeSessionOpenedOrdinal
		if !current ||
			(latestPrompt != nil && *latestPrompt >= completedOrdinal) ||
			(lastHarnessTurn != nil && *lastHarnessTurn > completedOrdinal) ||
			(nativeOpened != nil && *nativeOpened > completedOrdinal) ||
			r.snapshot.ActivePrompt != nil ||
			len(r.snapshot.QueuedPrompts) > 0 {
			return invalidState("handback reminder or hold follow-up completion is stale or the worker has accepted newer input")
		}
		// The completion is still current; the worker cannot take a prompt
		// right now. Resending the same command can succeed later, so this
		// is retryable and, unlike a stale completion, does not end the hold.
		facts := r.ActivityFacts()
		if _, closing := r.pendingCloseBarrierID(); !core.CanSubmit(&facts) || closing {
			return nil, relayProtocolRefusal(core.RelayErrorCodeInvalidState, "the worker cannot take a prompt right now", true), nil
		}
	}
	// Legacy: automatic continuation was removed (#1532). Journals still
	// replay the command, but a live one is never accepted.
	if _, legacy := command.(core.RelayCommandContinueAuthorizedWork); legacy {
		return invalidState("automatic continuation is no longer supported")
	}
	if seed, ok := command.(core.RelayCommandSeedAssessmentContext); ok {
		evidence := seed.Context.Evidence()
		if seed.Expected.Ordinal != r.snapshot.LatestOrdinal ||
			seed.Expected.Digest != r.snapshot.LatestDigest ||
			evidence.Validate() != nil {
			return invalidState("assessment context frontier changed or evidence is invalid")
		}
	}
	acceptedOrdinal, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandQueued{
		CommandID:     commandID,
		Command:       command,
		CreatedAtMs:   core.EpochMillis(),
		InsertAtFront: insertAtFront,
	})
	if err != nil {
		return nil, nil, err
	}

	if core.RelayCommandIsRelayLocal(command) || r.isQueuedRecoveryCancel(commandID) {
		if err := r.finishRelayLocalCommand(commandID); err != nil {
			return nil, nil, err
		}
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return nil, nil, err
	}
	return core.RelayResponsePayloadAccepted{CommandID: commandID, Ordinal: acceptedOrdinal}, nil, nil
}

// relayTextBlock is a text content block, Rust's ContentBlock::from(String).
func relayTextBlock(text string) acpschema.ContentBlock {
	return acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
}

// relayMailboxEventsEqual is Rust's Vec<MailboxEvent> equality.
func relayMailboxEventsEqual(left, right []core.MailboxEvent) bool {
	return len(left) == len(right) && (len(left) == 0 || reflect.DeepEqual(left, right))
}

// recoveryFloorRejection is the refusal of an AdvanceRecoveryFloor to
// through, if any.
//
// Journal garbage collection retains history through this ordinal, so a
// cursor off this relay's own event chain would discard events that no
// installed archive covers. The floor therefore only moves forward, only
// within the durable frontier, and only to a matching digest.
func (r *DurableRelay) recoveryFloorRejection(through *core.RelayCursor) (string, bool, error) {
	if through.Ordinal > r.snapshot.LatestOrdinal {
		return fmt.Sprintf("recovery floor %d is ahead of the relay frontier %d", through.Ordinal, r.snapshot.LatestOrdinal), true, nil
	}
	if through.Ordinal < r.snapshot.RecoveryFloorOrdinal {
		return fmt.Sprintf("recovery floor %d is behind the current floor %d", through.Ordinal, r.snapshot.RecoveryFloorOrdinal), true, nil
	}
	if core.ValidateRelayDigest(through.Digest, "recovery floor digest") != nil {
		return "recovery floor digest is malformed", true, nil
	}
	expected, ok, err := r.digestAt(through.Ordinal)
	if err != nil {
		return "", false, err
	}
	if !ok {
		return fmt.Sprintf("relay digest is unavailable at event %d", through.Ordinal), true, nil
	}
	if through.Digest != expected {
		return fmt.Sprintf("recovery floor digest does not match the relay event chain at event %d", through.Ordinal), true, nil
	}
	return "", false, nil
}

// isQueuedRecoveryCancel: the durable target survives until the control
// completes, including a restart after the queued prompt was already
// terminalized.
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

// finishRelayLocalCommand finishes a relay-local command from its durable
// dispatch record. This is deliberately restartable: every intermediate
// mutation is an event, so reopening the relay can resume after any append
// without duplicating or skipping the remaining queue transition.
func (r *DurableRelay) finishRelayLocalCommand(commandID string) error {
	if handled, ok := r.snapshot.HandledCommands[commandID]; ok && handled.TerminalOrdinal != nil {
		if handled.Outcome != nil && handled.TerminalIdentity != nil && handled.TerminalIdentity.ReleasesHistory {
			return r.garbageCollectRelayHistory()
		}
		return nil
	}
	if r.isQueuedRecoveryCancel(commandID) {
		target := *r.snapshot.CancellingPromptID
		if r.snapshot.HandledCommands[target].TerminalOrdinal == nil {
			if _, err := r.RecordCommandInterrupted(target, core.OutcomeReasonRequestedCancellation,
				"User cancelled queued restart recovery"); err != nil {
				return err
			}
		}
		kind := core.RelayCommandKindCancelTurn
		_, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandCompleted{
			CommandID: commandID,
			Command:   &kind,
			Outcome:   core.RelayCommandOutcomeCancelled{},
		})
		return err
	}
	dispatch, ok := r.snapshot.Dispatches[commandID]
	if !ok {
		return fmt.Errorf("unknown relay-local command %s", commandID)
	}
	if !core.RelayCommandIsRelayLocal(dispatch.Command) {
		return fmt.Errorf("command %s is not relay-local", commandID)
	}
	command := dispatch.Command
	state := dispatch.State
	// Recorded before the command starts, and unguarded by dispatch state: a
	// retry that repeats this append is harmless because the projection keys
	// the transcript line on this command, not on the event ordinal.
	var notice *string
	switch command := command.(type) {
	case core.RelayCommandRecordNotice:
		notice = &command.Text
	case core.RelayCommandSetQuotaRecovery:
		if command.Recovery != nil {
			notice = &command.Recovery.Notice
		}
	}
	if notice != nil {
		if _, err := r.appendRelayEvent(&commandID, core.RelayObservationNotice{Message: *notice}); err != nil {
			return err
		}
	}
	if state == core.RelayDispatchStateQueued {
		if _, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandStarted{
			CommandID:   commandID,
			StartedAtMs: core.EpochMillis(),
		}); err != nil {
			return err
		}
	}

	removedCommandIDs := []string{}
	switch command := command.(type) {
	case core.RelayCommandRemoveQueuedPrompt:
		if slices.ContainsFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
			return queued.CommandID == command.QueuedCommandID
		}) {
			removedCommandIDs = append(removedCommandIDs, command.QueuedCommandID)
		}
	case core.RelayCommandClearQueuedPrompts:
		for _, queued := range r.snapshot.QueuedPrompts {
			removedCommandIDs = append(removedCommandIDs, queued.CommandID)
		}
	}
	var outcome core.RelayCommandOutcome
	switch command.(type) {
	case core.RelayCommandCompleteCheckpoint:
		outcome = core.RelayCommandOutcomeCheckpointCompleted{}
	case core.RelayCommandReleaseCheckpoint:
		outcome = core.RelayCommandOutcomeCheckpointReleased{}
	case core.RelayCommandAdvanceRecoveryFloor:
		outcome = core.RelayCommandOutcomeRecoveryFloorAdvanced{}
	case core.RelayCommandRecordNotice, core.RelayCommandSetSubagentWait, core.RelayCommandSetQuotaRecovery,
		core.RelayCommandSeedAssessmentContext, core.RelayCommandDismissQuestion, core.RelayCommandClearNotifications,
		core.RelayCommandNotificationsRead, core.RelayCommandInstallPromptContext, core.RelayCommandDeliverMailboxEvent,
		core.RelayCommandResolveSteering:
		outcome = core.RelayCommandOutcomeNoticeRecorded{}
	default:
		outcome = core.RelayCommandOutcomeQueueChanged{RemovedCommandIds: removedCommandIDs}
	}
	current := r.snapshot.Dispatches[commandID].Command
	kind := core.RelayCommandKindOf(current)
	if _, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandCompleted{
		BarrierCommandID: checkpointBarrierOf(current),
		Command:          &kind,
		CommandID:        commandID,
		Outcome:          outcome,
	}); err != nil {
		return err
	}
	if core.ReleasesHistory(command) {
		return r.garbageCollectRelayHistory()
	}
	return nil
}

// checkpointBarrierOf is the barrier a CompleteCheckpoint or
// ReleaseCheckpoint names, which its completion records.
func checkpointBarrierOf(command core.RelayCommand) *string {
	switch command := command.(type) {
	case core.RelayCommandCompleteCheckpoint:
		barrier := command.BarrierCommandID
		return &barrier
	case core.RelayCommandReleaseCheckpoint:
		barrier := command.BarrierCommandID
		return &barrier
	}
	return nil
}

// ClaimPendingCommands durably claims commands before they are handed to
// the live ACP driver. A checkpoint barrier is admitted only after every
// previously-started ACP effect is terminal. Once admitted, it is the sole
// claimable command and all ACP dispatch remains frozen until that exact
// barrier completes.
func (r *DurableRelay) ClaimPendingCommands(acpSessionConfigured bool) ([]core.ClaimedRelayCommand, error) {
	return r.ClaimPendingCommandsUpTo(acpSessionConfigured, math.MaxInt)
}

// ClaimPendingCommandsUpTo claims no more work than the caller already
// holds transport permits for. The dispatcher reserves one ACP command
// permit per claimed command before calling, so every claim can be handed
// over without waiting even when other senders share that channel. Bounding
// the durable in-flight batch that way is what keeps command backpressure
// from parking the coordinator that must keep draining ACP's bounded event
// channel.
func (r *DurableRelay) ClaimPendingCommandsUpTo(acpSessionConfigured bool, maximum int) ([]core.ClaimedRelayCommand, error) {
	if r.checkpointOnly || !acpSessionConfigured || maximum == 0 {
		return nil, nil
	}
	if err := r.expireMailboxHookLeaseAt(core.EpochMillis()); err != nil {
		return nil, err
	}
	// Reject controls whose targets settled between admission and dispatch.
	var stale []string
	for _, id := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[id]
		if dispatch.State != core.RelayDispatchStateQueued && dispatch.State != core.RelayDispatchStatePending {
			continue
		}
		var target string
		queueChanged := false
		switch command := dispatch.Command.(type) {
		case core.RelayCommandSteer:
			target = command.ActivePromptID
		case core.RelayCommandCancelTurnFor:
			target = command.ActivePromptID
		case core.RelayCommandApplyQueuedFor:
			target = command.ActivePromptID
			queueChanged = len(r.snapshot.QueuedPrompts) == 0 || r.snapshot.QueuedPrompts[0].CommandID != command.QueuedCommandID
		default:
			continue
		}
		if queueChanged || r.snapshot.ActivePrompt == nil || r.snapshot.ActivePrompt.CommandID != target {
			stale = append(stale, id)
		}
	}
	for _, id := range stale {
		if _, err := r.RecordCommandRejected(id, core.OutcomeReasonAdmissionRejected,
			"Turn or queue changed before control delivery"); err != nil {
			return nil, err
		}
	}
	// Mailbox and user input share the same serialized steering decision.
	// Claim the events into the durable queue before dispatch, so a hook,
	// retry, restart or idle wake cannot also deliver them.
	if r.snapshot.InPlaceMoveHold == nil &&
		r.steeringDeliveryMode == SteeringDeliveryModeAcpWithMailbox && len(r.snapshot.QueuedPrompts) == 0 {
		if _, steering := r.automaticSteerActivePrompt(); steering &&
			slices.ContainsFunc(r.snapshot.PendingMailboxEvents, func(event core.MailboxEvent) bool { return event.Wake }) {
			if err := r.enqueueMailboxWake(); err != nil {
				return nil, err
			}
		}
	}
	if err := r.submitAutomaticSteer(); err != nil {
		return nil, err
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return nil, err
	}
	if r.snapshot.Steering != nil && r.snapshot.Steering.HoldsQueue() {
		for {
			barrierID, _, ok := r.nextQueuedCheckpoint()
			if !ok {
				break
			}
			if _, err := r.RecordCommandRejected(barrierID, core.OutcomeReasonAdmissionRejected,
				"Resolve uncertain steering delivery before checkpointing or moving this session"); err != nil {
				return nil, err
			}
		}
	}
	if r.snapshot.CheckpointBarrier == nil {
		if barrierID, barrierOrdinal, ok := r.nextQueuedCheckpoint(); ok {
			earlierControls := r.queuedControlsBefore(barrierOrdinal)
			if len(earlierControls) > 0 {
				if len(earlierControls) > maximum {
					earlierControls = earlierControls[:maximum]
				}
				if err := r.startQueuedControls(earlierControls); err != nil {
					return nil, err
				}
				// Checkpoint admission and command promotion use the same
				// relay-owned turn-boundary predicate.
			} else if !r.effectfulCommandInProgress() &&
				!(r.snapshot.Steering != nil && r.snapshot.Steering.HoldsQueue()) &&
				!r.turnInProgress() {
				if _, err := r.appendRelayEvent(&barrierID, core.RelayObservationCommandStarted{
					CommandID:   barrierID,
					StartedAtMs: core.EpochMillis(),
				}); err != nil {
					return nil, err
				}
			}
		} else {
			controls := r.queuedControlsBefore(math.MaxUint64)
			if len(controls) > maximum {
				controls = controls[:maximum]
			}
			if err := r.startQueuedControls(controls); err != nil {
				return nil, err
			}
		}
	}

	type claimableCommand struct {
		accepted  uint64
		commandID string
	}
	var claimable []claimableCommand
	activeBarrier := r.snapshot.CheckpointBarrier
	for _, commandID := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[commandID]
		if dispatch.State != core.RelayDispatchStatePending {
			continue
		}
		if _, steer := dispatch.Command.(core.RelayCommandSteer); steer && !r.steeringDeliveryMode.usesAcp() {
			// Claude's steer is completed by its PostToolBatch hook, or
			// returned when the current turn ends. It never enters the ACP
			// command channel.
			continue
		}
		if activeBarrier != nil && commandID != *activeBarrier {
			continue
		}
		if handled, ok := r.snapshot.HandledCommands[commandID]; ok {
			claimable = append(claimable, claimableCommand{accepted: handled.AcceptedOrdinal, commandID: commandID})
		}
	}
	slices.SortStableFunc(claimable, func(a, b claimableCommand) int { return cmp.Compare(a.accepted, b.accepted) })
	if len(claimable) > maximum {
		claimable = claimable[:maximum]
	}
	mailboxPromptIndex := slices.IndexFunc(claimable, func(candidate claimableCommand) bool {
		dispatch, ok := r.snapshot.Dispatches[candidate.commandID]
		return ok && mailboxPromptEligible(dispatch.Command)
	})
	if mailboxPromptIndex >= 0 {
		commandID := claimable[mailboxPromptIndex].commandID
		if _, claimed := r.snapshot.MailboxPromptContexts[commandID]; !claimed && len(r.snapshot.PendingMailboxEvents) > 0 {
			eventKeys := make([]string, 0, len(r.snapshot.PendingMailboxEvents))
			for _, event := range r.snapshot.PendingMailboxEvents {
				eventKeys = append(eventKeys, event.Key)
			}
			promptCommandID := commandID
			if _, err := r.appendRelayEvent(nil, core.RelayObservationMailboxEventsDelivered{
				EventKeys:       eventKeys,
				Path:            core.MailboxDeliveryPathPrompt,
				PromptCommandID: &promptCommandID,
				Events:          slices.Clone(r.snapshot.PendingMailboxEvents),
			}); err != nil {
				return nil, err
			}
		}
	}
	claimed := make([]core.ClaimedRelayCommand, 0, len(claimable))
	next := relayStagedSnapshot(&r.snapshot)
	claimsNativePrompt := false
	for _, candidate := range claimable {
		commandID, acceptedOrdinal := candidate.commandID, candidate.accepted
		var steeringPrompt *core.ClaimedSteeringPrompt
		switch next.Dispatches[commandID].Command.(type) {
		case core.RelayCommandCancel, core.RelayCommandSteer:
			if r.steeringDeliveryMode.usesAcp() && len(next.QueuedPrompts) > 0 {
				queued := next.QueuedPrompts[0]
				if payload, isPrompt := queued.Payload.(core.StoredQueuedRelayPayloadPrompt); isPrompt {
					steeringPrompt = &core.ClaimedSteeringPrompt{
						QueuedCommandID: queued.CommandID,
						Prompt:          slices.Clone(payload.Prompt),
					}
				}
			}
		}
		dispatch, ok := next.Dispatches[commandID]
		if !ok {
			panic("claimable command disappeared")
		}
		dispatch.State = core.RelayDispatchStateInFlight
		next.Dispatches[commandID] = dispatch
		if _, prompt := core.RelayCommandPromptBlocks(dispatch.Command); prompt {
			claimsNativePrompt = true
		}
		var hiddenPromptContext *string
		if mailboxPromptEligible(dispatch.Command) {
			var contexts []string
			if context, ok := next.MailboxPromptContexts[commandID]; ok {
				contexts = append(contexts, context)
			}
			if context := next.PendingPromptContext; context != nil {
				if context.AttachedCommandID == nil {
					attached := commandID
					context.AttachedCommandID = &attached
				}
				if *context.AttachedCommandID == commandID {
					contexts = append(contexts, context.Text)
				}
			}
			for i := range next.PendingUserShellContexts {
				context := &next.PendingUserShellContexts[i]
				if context.AcceptedOrdinal >= acceptedOrdinal {
					continue
				}
				if context.AttachedCommandID == nil {
					attached := commandID
					context.AttachedCommandID = &attached
				}
				if *context.AttachedCommandID == commandID {
					contexts = append(contexts, context.Text)
				}
			}
			if len(contexts) > 0 {
				joined := strings.Join(contexts, "\n\n")
				hiddenPromptContext = &joined
			}
		}
		claimed = append(claimed, core.ClaimedRelayCommand{
			CommandID:           commandID,
			AcceptedOrdinal:     acceptedOrdinal,
			Command:             dispatch.Command,
			HiddenPromptContext: hiddenPromptContext,
			SteeringPrompt:      steeringPrompt,
		})
	}
	if len(claimed) > 0 {
		// Persist uncertainty in the same transaction as the in-flight claim,
		// before ACP can see it. Pending queue entries remain unused.
		if claimsNativePrompt && next.NativeUseEvidence != nil && next.NativeUseEvidence.Usage == core.NativeSessionUseUnused {
			next.NativeSessionUsed = true
			next.RecordNativeUse(core.NativeSessionUseUnknown)
		}
		// An in-flight claim is not in the journal, so it is only durable
		// once the snapshot itself is.
		if err := r.commitSnapshot(next); err != nil {
			return nil, err
		}
	}
	return claimed, nil
}

// submitAutomaticSteer steers the head of the queue into the running prompt
// without being asked. It goes through admission like an Escape steer, so
// the journal and every client see the same steering operation.
func (r *DurableRelay) submitAutomaticSteer() error {
	if r.liveBaselinePending() || r.snapshot.InPlaceMoveHold != nil {
		return nil
	}
	activePromptID, queuedPromptID, ok := r.automaticSteerTarget()
	if !ok {
		return nil
	}
	suffix, err := relayRandomHex("automatic steer id")
	if err != nil {
		return err
	}
	_, refusal, err := r.submitCommand("auto-steer-"+suffix, core.RelayCommandSteer{
		ActivePromptID: activePromptID,
		QueuedPromptID: queuedPromptID,
	})
	if err != nil {
		return err
	}
	if refusal != nil {
		// Admission saw something the target check did not; the prompt simply
		// stays queued, as it would without automatic steering.
		slog.Debug("automatic steer was not admitted", "error", relayProtocolErrorDebug(refusal))
	}
	return nil
}

// automaticSteerTarget is the (active prompt, queued prompt) pair the relay
// would steer on its own: the running prompt and the queued prompt to steer
// into it, when the bridge advertises steering and nothing is waiting for
// the running turn to end. The bridge queues the steer to its next safe
// boundary.
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
	if !r.automaticSteering || r.checkpointOnly || r.hasHeldClaudeTurn() {
		return "", false
	}
	if r.steeringDeliveryMode == SteeringDeliveryModeHookTurnEnd {
		return "", false
	}
	// Automatic steering is only useful for a Hel-owned prompt. In a
	// harness-initiated turn, the agent cannot read the steer until its
	// current tool call returns; Esc can cancel that turn instead.
	if r.snapshot.ActivePrompt == nil {
		return "", false
	}
	active := r.snapshot.ActivePrompt.CommandID
	dispatch, ok := r.snapshot.Dispatches[active]
	if !ok || dispatch.State != core.RelayDispatchStateInFlight {
		return "", false
	}
	if _, _, queuedCheckpoint := r.nextQueuedCheckpoint(); r.snapshot.CancellingPromptID != nil ||
		r.snapshot.CheckpointBarrier != nil ||
		queuedCheckpoint {
		return "", false
	}
	if _, closing := r.pendingCloseBarrierID(); closing {
		return "", false
	}
	// One steer at a time, and none after a steer into this same turn
	// failed or came back: that turn is ending or cannot take input.
	if steering := r.snapshot.Steering; steering != nil &&
		(steering.HoldsQueue() || (steering.ActivePromptID == active && steering.Status != core.SteeringStatusApplied)) {
		return "", false
	}
	return active, true
}

// DispatchCheckpointOnly advances only lifecycle commands after the old
// owning process was stopped. No ACP channel or harness readiness is
// involved in this mode.
func (r *DurableRelay) DispatchCheckpointOnly() error {
	if !r.checkpointOnly {
		return fmt.Errorf("worker is not in checkpoint-only mode")
	}
	return r.dispatchRelayLocalLifecycle()
}

// DispatchPreparationLifecycle advances checkpoint and close commands while
// harness preparation owns no ACP process. Pending prompts and other ACP
// commands stay queued.
func (r *DurableRelay) DispatchPreparationLifecycle() error {
	if r.checkpointOnly {
		return fmt.Errorf("checkpoint-only workers use dispatch_checkpoint_only")
	}
	return r.dispatchRelayLocalLifecycle()
}

func (r *DurableRelay) dispatchRelayLocalLifecycle() error {
	type lifecycleCommand struct {
		accepted uint64
		id       string
	}
	var commands []lifecycleCommand
	for id, dispatch := range r.snapshot.Dispatches {
		switch dispatch.Command.(type) {
		case core.RelayCommandBeginCheckpoint, core.RelayCommandClose:
		default:
			continue
		}
		if dispatch.State != core.RelayDispatchStateQueued && dispatch.State != core.RelayDispatchStatePending {
			continue
		}
		commands = append(commands, lifecycleCommand{accepted: r.snapshot.HandledCommands[id].AcceptedOrdinal, id: id})
	}
	slices.SortFunc(commands, func(a, b lifecycleCommand) int {
		if order := cmp.Compare(a.accepted, b.accepted); order != 0 {
			return order
		}
		return strings.Compare(a.id, b.id)
	})
	for _, entry := range commands {
		id := entry.id
		command := r.snapshot.Dispatches[id].Command
		// Close is sealed by acceptance but executes only after the controller
		// releases its verified barrier, just like the ordinary coordinator.
		if r.snapshot.CheckpointBarrier != nil {
			continue
		}
		if r.snapshot.Dispatches[id].State == core.RelayDispatchStateQueued {
			if _, err := r.appendRelayEvent(&id, core.RelayObservationCommandStarted{
				CommandID:   id,
				StartedAtMs: core.EpochMillis(),
			}); err != nil {
				return err
			}
		}
		next := relayStagedSnapshot(&r.snapshot)
		dispatch, ok := next.Dispatches[id]
		if !ok {
			panic("lifecycle command")
		}
		dispatch.State = core.RelayDispatchStateInFlight
		next.Dispatches[id] = dispatch
		if err := r.commitSnapshot(next); err != nil {
			return err
		}
		switch command.(type) {
		case core.RelayCommandBeginCheckpoint:
			if _, err := r.RecordCheckpointReady(id); err != nil {
				return err
			}
		case core.RelayCommandClose:
			if _, err := r.RecordCommandCompleted(id, core.RelayCommandOutcomeClosed{}); err != nil {
				return err
			}
		default:
			panic("unreachable")
		}
	}
	return nil
}

// ClaimPendingUserShellCommandsUpTo claims user shell work independently of
// ACP turns. Run commands honor the caller's concurrency limit; cancellation
// controls bypass it so a full shell pool can always be stopped.
func (r *DurableRelay) ClaimPendingUserShellCommandsUpTo(maximumRuns int) ([]core.ClaimedRelayCommand, error) {
	if r.liveBaselinePending() || r.snapshot.InPlaceMoveHold != nil {
		maximumRuns = 0
	}
	if r.checkpointOnly || r.snapshot.CheckpointBarrier != nil {
		return nil, nil
	}
	barrierOrdinal := uint64(math.MaxUint64)
	if _, ordinal, ok := r.nextQueuedCheckpoint(); ok {
		barrierOrdinal = ordinal
	}
	cancelledShells := map[string]struct{}{}
	for _, dispatch := range r.snapshot.Dispatches {
		if cancel, ok := dispatch.Command.(core.RelayCommandCancelUserShell); ok && dispatch.State == core.RelayDispatchStateQueued {
			cancelledShells[cancel.ShellCommandID] = struct{}{}
		}
	}
	type shellCommand struct {
		accepted  uint64
		commandID string
	}
	compare := func(a, b shellCommand) int {
		if order := cmp.Compare(a.accepted, b.accepted); order != 0 {
			return order
		}
		return strings.Compare(a.commandID, b.commandID)
	}
	var cancels, runs []shellCommand
	for commandID, dispatch := range r.snapshot.Dispatches {
		if dispatch.State != core.RelayDispatchStateQueued {
			continue
		}
		handled, ok := r.snapshot.HandledCommands[commandID]
		if !ok {
			continue
		}
		switch dispatch.Command.(type) {
		case core.RelayCommandCancelUserShell:
			cancels = append(cancels, shellCommand{accepted: handled.AcceptedOrdinal, commandID: commandID})
		case core.RelayCommandRunUserShell:
			if _, cancelled := cancelledShells[commandID]; handled.AcceptedOrdinal < barrierOrdinal && !cancelled {
				runs = append(runs, shellCommand{accepted: handled.AcceptedOrdinal, commandID: commandID})
			}
		}
	}
	slices.SortFunc(cancels, compare)
	slices.SortFunc(runs, compare)
	if len(runs) > maximumRuns {
		runs = runs[:maximumRuns]
	}
	selected := append(cancels, runs...)
	slices.SortFunc(selected, compare)
	for _, entry := range selected {
		commandID := entry.commandID
		if _, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandStarted{
			CommandID:   commandID,
			StartedAtMs: core.EpochMillis(),
		}); err != nil {
			return nil, err
		}
	}
	next := relayStagedSnapshot(&r.snapshot)
	claimed := make([]core.ClaimedRelayCommand, 0, len(selected))
	for _, entry := range selected {
		dispatch, ok := next.Dispatches[entry.commandID]
		if !ok {
			panic("claimed shell command disappeared")
		}
		dispatch.State = core.RelayDispatchStateInFlight
		next.Dispatches[entry.commandID] = dispatch
		claimed = append(claimed, core.ClaimedRelayCommand{
			CommandID:       entry.commandID,
			AcceptedOrdinal: entry.accepted,
			Command:         dispatch.Command,
		})
	}
	if len(claimed) > 0 {
		if err := r.commitSnapshot(next); err != nil {
			return nil, err
		}
	}
	return claimed, nil
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
	var activePromptOrdinal *uint64
	if active := r.snapshot.ActivePrompt; active != nil {
		if handled, ok := r.snapshot.HandledCommands[active.CommandID]; ok {
			ordinal := handled.AcceptedOrdinal
			activePromptOrdinal = &ordinal
		}
	}
	type control struct {
		accepted  uint64
		commandID string
	}
	var controls []control
	for _, commandID := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[commandID]
		if dispatch.State != core.RelayDispatchStateQueued ||
			!core.RelayCommandIsEffectfulACP(dispatch.Command) ||
			core.RelayCommandIsQueueEntry(dispatch.Command) {
			continue
		}
		handled, ok := r.snapshot.HandledCommands[commandID]
		if !ok {
			continue
		}
		accepted := handled.AcceptedOrdinal
		// Preserve controls accepted before the active prompt (they must
		// reach ACP first), but keep later controls queued until that prompt
		// finishes. The legacy Cancel control deliberately bypasses a running
		// prompt and may carry steering; CancelTurn bypasses a pending
		// checkpoint as well, but never steers.
		if activePromptOrdinal != nil && accepted > *activePromptOrdinal {
			switch dispatch.Command.(type) {
			case core.RelayCommandCancel, core.RelayCommandSteer, core.RelayCommandCancelTurnFor,
				core.RelayCommandApplyQueuedFor, core.RelayCommandCancelTurn, core.RelayCommandGoalControl:
			default:
				continue
			}
		}
		runningTurn := r.turnInProgress()
		// ACP rejects session configuration requests while its prompt loop
		// is running. Keep these queued until that turn ends; GoalControl and
		// turn controls have dedicated live-turn handlers, and user shells
		// run outside the ACP command loop.
		if runningTurn {
			switch dispatch.Command.(type) {
			case core.RelayCommandClearContext, core.RelayCommandRestoreExecutionMode, core.RelayCommandSetSessionMode:
				continue
			}
		}
		_, cancelTurn := dispatch.Command.(core.RelayCommandCancelTurn)
		_, goalControl := dispatch.Command.(core.RelayCommandGoalControl)
		if accepted < beforeOrdinal || (runningTurn && cancelTurn) || goalControl {
			controls = append(controls, control{accepted: accepted, commandID: commandID})
		}
	}
	slices.SortStableFunc(controls, func(a, b control) int { return cmp.Compare(a.accepted, b.accepted) })
	ids := make([]string, 0, len(controls))
	for _, control := range controls {
		ids = append(ids, control.commandID)
	}
	return ids
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
	var (
		best     string
		ordinal  uint64
		hasFound bool
	)
	for _, commandID := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[commandID]
		if _, checkpoint := dispatch.Command.(core.RelayCommandBeginCheckpoint); !checkpoint ||
			dispatch.State != core.RelayDispatchStateQueued {
			continue
		}
		handled, ok := r.snapshot.HandledCommands[commandID]
		if !ok {
			continue
		}
		if !hasFound || handled.AcceptedOrdinal < ordinal {
			best, ordinal, hasFound = commandID, handled.AcceptedOrdinal, true
		}
	}
	return best, ordinal, hasFound
}

// quotaRecoveryAdmissible is whether a quota recovery for this completion
// may be recorded. A turn, background work, a goal or plan mode does not
// prevent that, since none gets past the limit; resuming also needs
// CanSubmit.
func (r *DurableRelay) quotaRecoveryAdmissible(user, completed string) bool {
	c := &r.snapshot.Continuation
	assessment := r.snapshot.Assessment
	_, closing := r.pendingCloseBarrierID()
	return !c.QuotaSuppressed &&
		(assessment == nil ||
			(assessment.Current() && assessment.Action != nil && *assessment.Action == core.AssessmentActionRecoverQuota)) &&
		len(r.snapshot.AssessmentQuestions) == 0 &&
		!r.snapshot.Goal.BudgetLimited() &&
		c.UserCommandID != nil && *c.UserCommandID == user &&
		c.CompletedCommandID != nil && *c.CompletedCommandID == completed &&
		len(r.snapshot.QueuedPrompts) == 0 &&
		!closing
}

// authenticationRetryIntent is the accepted ordinal and the rejected login
// of a retry the worker still owes.
//
// One owner decides whether the interrupted turn still belongs to us. The
// completion and its login evidence are durable; a newer prompt,
// cancellation or another driver always wins over credential recovery.
//
// This is the durable intent: what the session still owes, whether or not it
// can submit this instant. Every term here either waits on a person or on a
// later decision about the work (so a child holding it must not look busy),
// or means the session is ending. Terms that clear by themselves belong to
// authenticationRetryGateOpen.
func (r *DurableRelay) authenticationRetryIntent() (uint64, string, bool) {
	// Waits on a person (assessment questions, a paused or budget-limited
	// goal) or ends the session (close barrier): withdrawn, not delayed.
	_, closing := r.pendingCloseBarrierID()
	if r.verdictHarness == nil || *r.verdictHarness != core.HarnessKindClaude ||
		len(r.snapshot.AssessmentQuestions) > 0 ||
		r.snapshot.Goal.BudgetLimited() ||
		(r.snapshot.Goal.Snapshot != nil && r.snapshot.Goal.Snapshot.Status == "paused") ||
		closing {
		return 0, "", false
	}
	if r.snapshot.TurnCompletion == nil {
		return 0, "", false
	}
	receipt, ok := r.snapshot.HandledCommands[r.snapshot.TurnCompletion.CommandID]
	if !ok || receipt.TerminalOrdinal == nil {
		return 0, "", false
	}
	ordinal := *receipt.TerminalOrdinal
	// Continuation withdrawn at or after the failed prompt's acceptance (a
	// cancellation, a control command, a context command) ends recovery. One
	// before it, such as a spawn's model configure, does not.
	withdrawn := r.snapshot.Continuation.WithdrawnOrdinal
	input := r.snapshot.LatestPromptAcceptedOrdinal
	start := r.snapshot.LastHarnessTurnStartedOrdinal
	if (withdrawn != nil && *withdrawn >= receipt.AcceptedOrdinal) ||
		(input != nil && *input > receipt.AcceptedOrdinal) ||
		(start != nil && *start > ordinal) {
		return 0, "", false
	}
	outcome, ok := receipt.Outcome.(core.RelayCommandOutcomePrompt)
	if !ok || outcome.Diagnostic == nil {
		return 0, "", false
	}
	if !core.TurnDiagnosticReportsAuthFailure(outcome.Diagnostic) || outcome.Diagnostic.LoginFingerprint == nil {
		return 0, "", false
	}
	return ordinal, *outcome.Diagnostic.LoginFingerprint, true
}

// authenticationRetryGateOpen is the submit gate: whether a prompt could
// start now. Every term clears by itself (a checkpoint barrier ends, a tool
// or command finishes, a driver such as an active goal or running background
// work stops), so none of them withdraws the intent; they only delay the
// submit.
func (r *DurableRelay) authenticationRetryGateOpen() bool {
	facts := r.ActivityFacts()
	return core.CanSubmit(&facts) && !core.DriverPresent(&facts) && !r.pendingCheckpointBarrier()
}

func (r *DurableRelay) submitAuthenticationRetry(expected uint64, login string) (bool, error) {
	ordinal, rejected, ok := r.authenticationRetryIntent()
	if !ok {
		return false, nil
	}
	if ordinal != expected || rejected == login || !r.authenticationRetryGateOpen() {
		return false, nil
	}
	id := core.GeneratedPromptAuthenticationRetry.CommandID(ordinal)
	_, refusal, err := r.submitCommand(id, core.RelayCommandPrompt{Prompt: core.PromptBlocks()})
	if err != nil {
		return false, err
	}
	if refusal != nil {
		return false, fmt.Errorf("submit authentication recovery: %s", relayProtocolErrorDebug(refusal))
	}
	if _, err := r.appendRelayEvent(&id, core.RelayObservationNotice{
		Message: "Claude's login changed after an authentication failure. Automatically continuing the interrupted work.",
	}); err != nil {
		return false, err
	}
	return true, nil
}

func (r *DurableRelay) CapacityRetryDeadline() (int64, bool) {
	retry := r.snapshot.CapacityRetry
	if retry == nil || retry.Submitted {
		return 0, false
	}
	return retry.RetryAtMs, true
}

// SubmitDueCapacityRetry admits a due retry through the same durable queue
// as an external prompt.
func (r *DurableRelay) SubmitDueCapacityRetry(nowMs int64) (bool, error) {
	retry := r.snapshot.CapacityRetry
	if retry == nil || retry.Submitted {
		return false, nil
	}
	// Nothing else may be happening: a retry submits a prompt of its own, so
	// anything the session still owns would collide with it. That is the
	// shared quiet predicate, not a list of its own. The retry's own armed
	// state is cleared first, the same way a caller holding a checkpoint
	// barrier asks whether anything *else* is running: it is the reason this
	// is being asked, not a reason to refuse.
	facts := r.ActivityFacts()
	facts.CapacityRetryArmed = false
	_, closing := r.pendingCloseBarrierID()
	if len(r.snapshot.AssessmentQuestions) > 0 ||
		r.snapshot.Goal.BudgetLimited() ||
		(r.snapshot.Goal.Snapshot != nil && r.snapshot.Goal.Snapshot.Status == "paused") ||
		retry.RetryAtMs > nowMs ||
		!core.IsQuiet(&facts) ||
		closing {
		return false, nil
	}
	id := retry.CommandID
	prompt := []acpschema.ContentBlock{relayTextBlock("Continue")}
	_, refusal, err := r.submitCommand(id, core.RelayCommandPrompt{Prompt: prompt})
	if err != nil {
		return false, err
	}
	if refusal != nil {
		return false, fmt.Errorf("submit capacity retry: %s", relayProtocolErrorDebug(refusal))
	}
	return true, nil
}

func (r *DurableRelay) RecordCommandCompleted(commandID string, outcome core.RelayCommandOutcome) (uint64, error) {
	if err := r.requireInFlight(commandID); err != nil {
		return 0, err
	}
	dispatch := r.snapshot.Dispatches[commandID]
	if _, checkpoint := dispatch.Command.(core.RelayCommandBeginCheckpoint); checkpoint {
		return 0, errors.New("checkpoint barriers complete through record_checkpoint_ready")
	}
	prompt, finishesTurn := outcome.(core.RelayCommandOutcomePrompt)
	classifierHandoff := finishesTurn &&
		(prompt.StopReason == core.AwaitingInputStopReason || prompt.StopReason == core.InferredFinishedStopReason)
	// Report confirmed changes in the conversation on every surface. The
	// command identity makes retries of this append project only once.
	var notice *string
	switch command := dispatch.Command.(type) {
	case core.RelayCommandSetConfig:
		if _, configured := outcome.(core.RelayCommandOutcomeConfigured); configured {
			message := fmt.Sprintf("%s set to %s", command.Key, command.Value)
			notice = &message
		}
	case core.RelayCommandRestoreExecutionMode:
		if _, configured := outcome.(core.RelayCommandOutcomeConfigured); configured {
			message := "Execution policy restored"
			notice = &message
		}
	case core.RelayCommandSetSessionMode:
		if _, set := outcome.(core.RelayCommandOutcomeSessionModeSet); set {
			message := "Session mode: " + command.ModeID
			notice = &message
		}
	case core.RelayCommandGoalControl:
		if _, controlled := outcome.(core.RelayCommandOutcomeGoalControlled); controlled {
			message := "Goal: " + command.Action.AsStr()
			notice = &message
		}
	}
	if notice != nil {
		if _, err := r.appendRelayEvent(&commandID, core.RelayObservationNotice{Message: *notice}); err != nil {
			return 0, err
		}
	}
	current := r.snapshot.Dispatches[commandID].Command
	kind := core.RelayCommandKindOf(current)
	ordinal, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandCompleted{
		BarrierCommandID: checkpointBarrierOf(current),
		Command:          &kind,
		CommandID:        commandID,
		Outcome:          outcome,
	})
	if err != nil {
		return 0, err
	}
	if finishesTurn && !r.snapshot.Goal.Running() {
		if err := r.finishTurnActivity(); err != nil {
			return 0, err
		}
	}
	if classifierHandoff {
		// The running classifier ended the tracked turn. Keep independently
		// tracked children and their controls while making the session ready.
		if _, err := r.applyRepliedDecision(r.turnContext.Generation(), core.DecisionInferIdle, core.EpochMillis()); err != nil {
			return 0, err
		}
	}
	if finishesTurn {
		if err := r.returnBoundarySteerAfterTurnEnd(commandID); err != nil {
			return 0, err
		}
		if err := r.promoteHeldClaudeTurn(); err != nil {
			return 0, err
		}
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return 0, err
	}
	return ordinal, nil
}

func (r *DurableRelay) RecordCommandRejected(commandID string, reason core.OutcomeReason, message string) (uint64, error) {
	if err := r.requireDispatch(commandID); err != nil {
		return 0, err
	}
	command := core.RelayCommandKindOf(r.snapshot.Dispatches[commandID].Command)
	ordinal, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandRejected{
		Reason:    &reason,
		CommandID: commandID,
		Command:   command,
		Message:   message,
	})
	if err != nil {
		return 0, err
	}
	if err := r.ensureInitialCommandsSucceeded(); err != nil {
		return 0, err
	}
	if command == core.RelayCommandKindPrompt {
		if err := r.finishTurnActivity(); err != nil {
			return 0, err
		}
		if err := r.returnBoundarySteerAfterTurnEnd(commandID); err != nil {
			return 0, err
		}
		if err := r.promoteHeldClaudeTurn(); err != nil {
			return 0, err
		}
	}
	if err := r.settleHeldWrites(); err != nil {
		return 0, err
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return 0, err
	}
	return ordinal, nil
}

func (r *DurableRelay) RecordCommandInterrupted(commandID string, reason core.OutcomeReason, message string) (uint64, error) {
	// Terminal payloads have already been compacted out of dispatches.
	// Repeated restart recovery must not create another event or wake.
	if reason == core.OutcomeReasonWorkerRestarted {
		if handled, ok := r.snapshot.HandledCommands[commandID]; ok && handled.TerminalOrdinal != nil {
			return *handled.TerminalOrdinal, nil
		}
	}
	if err := r.requireDispatch(commandID); err != nil {
		return 0, err
	}
	command := core.RelayCommandKindOf(r.snapshot.Dispatches[commandID].Command)
	var continuationID *string
	if reason == core.OutcomeReasonWorkerRestarted {
		if id, ok := r.restartContinuationFor(commandID); ok {
			continuationID = &id
		}
	}
	observation := core.RelayObservationCommandInterrupted{
		ContinuationID: continuationID,
		Reason:         &reason,
		CommandID:      commandID,
		Command:        command,
		Message:        message,
	}
	ordinal, err := r.appendRelayEvent(&commandID, observation)
	if err != nil {
		var budget *core.RelayBudgetExceeded
		_, dispatched := r.snapshot.Dispatches[commandID]
		if !errors.As(err, &budget) || !dispatched || observation.ContinuationID == nil {
			return 0, err
		}
		// Staging rejected the larger receipt before any journal write.
		// Terminalizing the original releases its payload.
		observation.ContinuationID = nil
		ordinal, err = r.appendRelayEvent(&commandID, observation)
		if err != nil {
			return 0, err
		}
		if _, err := r.RecordObservation(core.RelayObservationWarning{
			Message: "The turn could not be resumed automatically because the session's queue is full. Remove queued input or continue the task manually.",
		}); err != nil {
			return 0, err
		}
	}
	if err := r.ensureInitialCommandsSucceeded(); err != nil {
		return 0, err
	}
	if command == core.RelayCommandKindPrompt {
		if err := r.finishTurnActivity(); err != nil {
			return 0, err
		}
		if err := r.returnBoundarySteerAfterTurnEnd(commandID); err != nil {
			return 0, err
		}
		if err := r.promoteHeldClaudeTurn(); err != nil {
			return 0, err
		}
	}
	if err := r.settleHeldWrites(); err != nil {
		return 0, err
	}
	if reason != core.OutcomeReasonWorkerRestarted || r.isAcpReady() {
		if _, _, err := r.promoteNextQueuedCommand(); err != nil {
			return 0, err
		}
	}
	return ordinal, nil
}

// restartContinuationFor: the interruption event transfers this payload to
// recovery atomically.
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
	if err := r.requireInFlight(commandID); err != nil {
		return 0, err
	}
	if r.snapshot.CheckpointBarrier == nil || *r.snapshot.CheckpointBarrier != commandID {
		return 0, fmt.Errorf("checkpoint barrier %s is not active", commandID)
	}
	if r.snapshot.CheckpointReadyThrough != nil {
		return 0, fmt.Errorf("checkpoint barrier %s is already ready", commandID)
	}
	if _, checkpoint := r.snapshot.Dispatches[commandID].Command.(core.RelayCommandBeginCheckpoint); !checkpoint {
		return 0, fmt.Errorf("command %s is not a checkpoint barrier", commandID)
	}
	if r.snapshot.LatestOrdinal == math.MaxUint64 {
		return 0, errors.New("relay event ordinal exhausted")
	}
	through := r.snapshot.LatestOrdinal + 1
	r.baselineBackgroundWorkAtCut()
	return r.appendRelayEvent(&commandID, core.RelayObservationCheckpointReady{
		CommandID: commandID,
		Through:   through,
	})
}

// CancelCheckpointBarrierOnDisconnect releases checkpoint barriers owned by
// a controller connection that disappeared. The runtime calls this when that
// connection drops so an offline prompt queue can never remain paused
// indefinitely. It returns the ordinal of the event that cancelled the
// barrier, if one was recorded.
func (r *DurableRelay) CancelCheckpointBarrierOnDisconnect(commandID string) (uint64, bool, error) {
	dispatch, ok := r.snapshot.Dispatches[commandID]
	if !ok {
		return 0, false, nil
	}
	if _, checkpoint := dispatch.Command.(core.RelayCommandBeginCheckpoint); !checkpoint {
		return 0, false, nil
	}
	switch dispatch.State {
	case core.RelayDispatchStateQueued, core.RelayDispatchStatePending, core.RelayDispatchStateInFlight:
	default:
		return 0, false, nil
	}
	reason := core.OutcomeReasonControllerDisconnected
	ordinal, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandInterrupted{
		Reason:    &reason,
		CommandID: commandID,
		Command:   core.RelayCommandKindBeginCheckpoint,
		Message:   "checkpoint barrier cancelled because its controller disconnected",
	})
	if err != nil {
		return 0, false, err
	}
	if err := r.settleHeldWrites(); err != nil {
		return 0, false, err
	}
	if _, _, err := r.promoteNextQueuedCommand(); err != nil {
		return 0, false, err
	}
	return ordinal, true, nil
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

// ClosePending is whether an accepted Close is waiting to run. The relay is
// sealed at its checkpoint cut from the moment it accepts one.
func (r *DurableRelay) ClosePending() bool {
	return r.closeRequested()
}

// SealedByClose is whether an accepted Close has sealed the relay, so no
// harness may start for it. The one predicate both sides of the preparation
// handoff read under the relay lock: the request handler, to cancel
// preparation, and the bridge start, to refuse publishing Started.
func (r *DurableRelay) SealedByClose() bool {
	return r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed
}

// UndeliveredCancellationOrClosePending is whether a cancellation is
// accepted but not yet delivered to the harness, or a Close is accepted and
// not finished. Work the worker does for its own purposes, such as observing
// a finished turn, ends when this is true, so that the command is not held
// behind it. A cancellation already delivered is not held by anything, and
// is how a cancelled turn ends.
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
	for _, commandID := range slices.Sorted(maps.Keys(r.snapshot.Dispatches)) {
		dispatch := r.snapshot.Dispatches[commandID]
		switch dispatch.State {
		case core.RelayDispatchStateCompleted, core.RelayDispatchStateRejected, core.RelayDispatchStateInterrupted:
			continue
		}
		if closeCommand, ok := dispatch.Command.(core.RelayCommandClose); ok {
			return closeCommand.BarrierCommandID, true
		}
	}
	return "", false
}

// promoteNextQueuedCommand starts the head of the durable command queue once
// the relay is idle. Entries run one at a time; restart recovery precedes
// previously queued input. It returns the ordinal of the promotion it
// journaled, if any.
func (r *DurableRelay) promoteNextQueuedCommand() (uint64, bool, error) {
	if r.liveBaselinePending() || r.snapshot.InPlaceMoveHold != nil {
		return 0, false, nil
	}
	var recovery *core.StoredQueuedRelayCommand
	if index := slices.IndexFunc(r.snapshot.QueuedPrompts, func(queued core.StoredQueuedRelayCommand) bool {
		return core.IsRestartContinuation(queued.CommandID)
	}); index >= 0 {
		queued := r.snapshot.QueuedPrompts[index]
		recovery = &queued
	}
	holdsQueue := r.snapshot.Steering != nil && r.snapshot.Steering.HoldsQueue()
	// Keep turn-starting entries and configuration queue entries in this
	// durable FIFO during a live turn. Claude's adapter queues mid-cycle
	// prompts internally and cancel may discard them; ACP rejects
	// configuration changes while its prompt loop is running.
	if r.checkpointOnly ||
		(recovery != nil && !r.isAcpReady()) ||
		r.turnInProgress() ||
		// A cancel can outlive its prompt result. Drain it before a
		// replacement turn starts, so it cannot reach the newer turn.
		r.turnCancellationPending() ||
		// Uncertain steering still holds its input, but must not block
		// recovery of the interrupted turn. We never redeliver that input.
		(recovery == nil && holdsQueue) ||
		r.configurationDispatchInProgress() ||
		r.snapshot.CheckpointBarrier != nil ||
		r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed ||
		(recovery == nil && r.pendingCheckpointBarrier()) ||
		r.closeRequested() {
		return 0, false, nil
	}
	if err := r.expireMailboxHookLeaseAt(core.EpochMillis()); err != nil {
		return 0, false, err
	}
	// Interruption atomically transfers the payload to the recovery queue
	// entry, preserving priority across process restarts.
	queued := recovery
	if queued == nil && len(r.snapshot.QueuedPrompts) > 0 {
		head := r.snapshot.QueuedPrompts[0]
		queued = &head
	}
	if queued == nil {
		if r.mailboxWakeIsAllowed() {
			if err := r.enqueueMailboxWake(); err != nil {
				return 0, false, err
			}
			return r.promoteNextQueuedCommand()
		}
		return 0, false, nil
	}
	queuedOrdinal := uint64(math.MaxUint64)
	if handled, ok := r.snapshot.HandledCommands[queued.CommandID]; ok {
		queuedOrdinal = handled.AcceptedOrdinal
	}
	for commandID := range r.snapshot.ActiveUserShells {
		if handled, ok := r.snapshot.HandledCommands[commandID]; ok && handled.AcceptedOrdinal < queuedOrdinal {
			return 0, false, nil
		}
	}
	commandID := queued.CommandID
	ordinal, err := r.appendRelayEvent(&commandID, core.RelayObservationCommandStarted{
		CommandID:   commandID,
		StartedAtMs: core.EpochMillis(),
	})
	if err != nil {
		return 0, false, err
	}
	return ordinal, true, nil
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
	events := slices.Clone(r.snapshot.PendingMailboxEvents)
	if len(events) == 0 || !slices.ContainsFunc(events, func(event core.MailboxEvent) bool { return event.Wake }) {
		return errors.New("mailbox wake lost its waking event")
	}
	suffix, err := relayRandomHex("mailbox wake ID")
	if err != nil {
		return err
	}
	commandID := core.GeneratedPromptMailboxWake.CommandID(suffix)
	_, err = r.appendRelayEvent(&commandID, core.RelayObservationCommandQueued{
		CommandID:   commandID,
		Command:     core.RelayCommandMailboxWake{Events: events},
		CreatedAtMs: core.EpochMillis(),
	})
	return err
}

// pendingHookSteering is the steering a mailbox hook for hookEvent would
// deliver, with the queued prompt's hidden context.
func (r *DurableRelay) pendingHookSteering(hookEvent string) (core.MailboxHookSteering, string, bool) {
	if hookEvent != "PostToolBatch" ||
		r.steeringDeliveryMode != SteeringDeliveryModeHookPostToolBatch ||
		// A turn being interrupted discards its input; the prompt runs next.
		r.turnCancellationPending() {
		return core.MailboxHookSteering{}, "", false
	}
	operation := r.snapshot.Steering
	if operation == nil || operation.Status != core.SteeringStatusPending {
		return core.MailboxHookSteering{}, "", false
	}
	if r.snapshot.ActivePrompt == nil || r.snapshot.ActivePrompt.CommandID != operation.ActivePromptID {
		return core.MailboxHookSteering{}, "", false
	}
	dispatch, ok := r.snapshot.Dispatches[operation.CommandID]
	if !ok || dispatch.State != core.RelayDispatchStatePending {
		return core.MailboxHookSteering{}, "", false
	}
	steer, isSteer := dispatch.Command.(core.RelayCommandSteer)
	if !isSteer || steer.ActivePromptID != operation.ActivePromptID || steer.QueuedPromptID != operation.QueuedPromptID {
		return core.MailboxHookSteering{}, "", false
	}
	if len(r.snapshot.QueuedPrompts) == 0 {
		return core.MailboxHookSteering{}, "", false
	}
	queued := r.snapshot.QueuedPrompts[0]
	if queued.CommandID != operation.QueuedPromptID {
		return core.MailboxHookSteering{}, "", false
	}
	payload, isPrompt := queued.Payload.(core.StoredQueuedRelayPayloadPrompt)
	if !isPrompt {
		return core.MailboxHookSteering{}, "", false
	}
	text, ok := core.SteeringTextContext(payload.Prompt)
	if !ok {
		return core.MailboxHookSteering{}, "", false
	}
	return core.MailboxHookSteering{
		CommandID:      operation.CommandID,
		ActivePromptID: operation.ActivePromptID,
		QueuedPromptID: operation.QueuedPromptID,
	}, text, true
}

// DrainMailbox atomically leases pending mailbox events and eligible Claude
// steers to one harness hook.
//
// A PostToolUse drain (Codex) carries a payload only when the worker
// resolved the session's provider as accepting tool-boundary delivery; see
// core.MailboxDeliveryModeForCodexProvider. Otherwise it answers empty and
// leaves events for turn-boundary delivery (wake turns and prompt context),
// so an unresolved or unsafe provider never receives a developer message
// between a tool call and its output (#1277). PostToolBatch drains are
// unaffected.
func (r *DurableRelay) DrainMailbox(hookEvent string, conversationID *string) (core.RelayResponsePayload, error) {
	return r.drainMailboxWithInitialSteps(hookEvent, conversationID, nil)
}

func (r *DurableRelay) drainMailboxWithInitialSteps(hookEvent string, conversationID *string, initialNumSteps *int64) (core.RelayResponsePayload, error) {
	if hookEvent != "PostToolUse" && hookEvent != "PostToolBatch" {
		return nil, errors.New("unsupported mailbox hook event")
	}
	empty := core.RelayResponsePayloadMailboxDrained{}
	if conversationID != nil && *conversationID != "" &&
		r.snapshot.NativeSessionID != nil && *conversationID != *r.snapshot.NativeSessionID {
		return empty, nil
	}
	if r.harnessTurns == HarnessTurnPolicyAntigravity && hookEvent == "PostToolBatch" {
		if err := r.antigravityPreInvocation(conversationID, initialNumSteps); err != nil {
			return nil, err
		}
	}
	if hookEvent == "PostToolUse" &&
		(r.mailboxDeliveryMode == nil || *r.mailboxDeliveryMode != core.MailboxDeliveryModeToolBoundary) {
		return empty, nil
	}
	if err := r.expireMailboxHookLeaseAt(core.EpochMillis()); err != nil {
		return nil, err
	}
	if r.snapshot.MailboxHookLease != nil {
		return empty, nil
	}
	events := slices.Clone(r.snapshot.PendingMailboxEvents)
	hookSteering, steeringText, steering := r.pendingHookSteering(hookEvent)
	if len(events) == 0 && !steering {
		return empty, nil
	}
	suffix, err := relayRandomHex("mailbox hook lease ID")
	if err != nil {
		return nil, err
	}
	expiresAtMs := core.EpochMillis()
	if expiresAtMs > math.MaxInt64-core.MailboxHookLeaseTimeoutMs {
		expiresAtMs = math.MaxInt64
	} else {
		expiresAtMs += core.MailboxHookLeaseTimeoutMs
	}
	lease := core.MailboxHookLease{
		LeaseID:     "mailbox-hook-" + suffix,
		Events:      slices.Clone(events),
		HookEvent:   hookEvent,
		ExpiresAtMs: expiresAtMs,
	}
	if steering {
		leased := hookSteering
		lease.Steering = &leased
	}
	contexts := make([]string, 0, 2)
	if len(events) > 0 {
		contexts = append(contexts, core.RenderMailboxEvents(events))
	}
	if steering {
		contexts = append(contexts, fmt.Sprintf(
			"<user_message_sent_while_working>\n%s\n</user_message_sent_while_working>", steeringText))
	}
	text := strings.Join(contexts, "\n\n")
	count := len(events)
	if steering {
		count++
	}
	if _, err := r.appendRelayEvent(nil, core.RelayObservationMailboxHookLeaseCreated{Lease: lease}); err != nil {
		return nil, err
	}
	hookContexts := make([]core.MailboxHookContext, 0, len(events))
	for _, event := range events {
		hookContexts = append(hookContexts, core.MailboxHookContext{
			Key:  event.Key,
			Text: core.RenderMailboxEvents([]core.MailboxEvent{event}),
		})
	}
	leaseID := lease.LeaseID
	return core.RelayResponsePayloadMailboxDrained{
		LeaseID:  &leaseID,
		Text:     &text,
		Count:    count,
		Contexts: hookContexts,
	}, nil
}

func (r *DurableRelay) ackMailbox(leaseID string) (core.RelayResponsePayload, error) {
	lease := r.snapshot.MailboxHookLease
	if lease == nil || lease.LeaseID != leaseID {
		return core.RelayResponsePayloadMailboxAcknowledged{Acknowledged: false}, nil
	}
	events := slices.Clone(lease.Events)
	eventKeys := make([]string, 0, len(events))
	for _, event := range events {
		eventKeys = append(eventKeys, event.Key)
	}
	hookEvent := lease.HookEvent
	var steering *core.MailboxHookSteering
	var commandID *string
	if lease.Steering != nil {
		copied := *lease.Steering
		steering = &copied
		id := copied.CommandID
		commandID = &id
	}
	acknowledged := leaseID
	if _, err := r.appendRelayEvent(commandID, core.RelayObservationMailboxEventsDelivered{
		EventKeys: eventKeys,
		Path:      core.MailboxDeliveryPathToolHook,
		HookEvent: &hookEvent,
		Events:    events,
		LeaseID:   &acknowledged,
		Steering:  steering,
	}); err != nil {
		return nil, err
	}
	return core.RelayResponsePayloadMailboxAcknowledged{Acknowledged: true}, nil
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

// ExpireMailboxHookLeaseAndPromote returns an expired hook lease and
// immediately reconsiders the idle wake.
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

// unfinishedDispatch is whether any dispatch is still waiting or running.
// Terminal records stay in the ledger until the daemon acknowledges their
// events, so they are history, not work.
func (r *DurableRelay) unfinishedDispatch() bool {
	for _, dispatch := range r.snapshot.Dispatches {
		switch dispatch.State {
		case core.RelayDispatchStateQueued, core.RelayDispatchStatePending, core.RelayDispatchStateInFlight:
			return true
		}
	}
	return false
}

// configurationDispatchInProgress: a promoted configuration change leaves
// execution idle while it reaches ACP, so the queue needs its own guard to
// stay sequential. Completion, rejection, and interruption all promote the
// next entry.
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
