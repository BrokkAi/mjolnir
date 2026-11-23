// Port of mj-worker/src/relay/requests.rs.
//
// Answers relay request envelopes.

package worker

import (
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (r *DurableRelay) Handle(envelope core.RelayRequestEnvelope) core.RelayResponseEnvelope {
	return r.handleWithAntigravityStopBaseline(envelope, nil)
}

// handleWithAntigravityStopBaseline answers envelope. observedBaselineDigest
// is nil unless the live worker read SQLite before an Antigravity Stop; a
// completed read can report that the row did not exist (a nil inner
// pointer).
func (r *DurableRelay) handleWithAntigravityStopBaseline(envelope core.RelayRequestEnvelope, observedBaselineDigest **[32]byte) core.RelayResponseEnvelope {
	requestID := envelope.RequestID
	body, err := r.handleInner(&envelope, observedBaselineDigest)
	if err != nil {
		body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInternal,
			Message:   err.Error(),
			Retryable: true,
		}}
	}
	// A completed or released barrier ends the hold on the worker's own
	// writes. The request itself already succeeded, so a journal failure here
	// is reported rather than turned into its answer.
	if err := r.settleHeldWrites(); err != nil {
		slog.Error(fmt.Sprintf("could not journal writes held behind a checkpoint barrier: %v", err),
			"session_id", r.snapshot.SessionID)
	}
	protocolVersion := envelope.ProtocolVersion
	if ok, isOk := body.(core.RelayResponseBodyOk); isOk {
		if hello, isHello := ok.Payload.(core.RelayResponsePayloadHello); isHello {
			protocolVersion = hello.Negotiated
		}
	}
	return core.RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body:            body,
	}
}

// envelopeRejection is the answer to an envelope the relay refuses before
// handling it, or nil: a usable request ID, protocol negotiation, and a
// method this peer's protocol version admits. TakeDeferredAttach consults the
// same checks so an attach it defers is one the normal path would have
// accepted.
func (r *DurableRelay) envelopeRejection(envelope *core.RelayRequestEnvelope) core.RelayResponseBody {
	if strings.TrimSpace(envelope.RequestID) == "" || len(envelope.RequestID) > 256 {
		return core.RelayError(core.RelayErrorCodeInvalidRequest, "request_id is required", false, nil)
	}
	if hello, ok := envelope.Request.(core.RelayRequestHello); ok {
		writerRange := core.RelayVersionRange{Min: core.RelayProtocolVersion, Max: core.RelayProtocolVersion}
		negotiated, ok := writerRange.Negotiate(hello.Supported)
		if !ok {
			return core.RelayError(
				core.RelayErrorCodeIncompatibleProtocol,
				fmt.Sprintf("controller supports %d-%d, relay supports protocol %d-%d",
					hello.Supported.Min, hello.Supported.Max, writerRange.Min, writerRange.Max),
				false,
				nil,
			)
		}
		var workerBuild *string
		if r.workerBuild != nil {
			build := *r.workerBuild
			workerBuild = &build
		}
		return core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadHello{
			Negotiated:   negotiated,
			RelayVersion: r.relayVersion,
			SessionID:    r.snapshot.SessionID,
			WorkerBuild:  workerBuild,
		}}
	}
	return core.RelayProtocolRejection(envelope)
}

func (r *DurableRelay) handleInner(envelope *core.RelayRequestEnvelope, observedBaselineDigest **[32]byte) (core.RelayResponseBody, error) {
	if body := r.envelopeRejection(envelope); body != nil {
		return body, nil
	}
	refused := func(refusal *core.RelayProtocolError) (core.RelayResponseBody, error) {
		return core.RelayResponseBodyError{Error: *refusal}, nil
	}

	var payload core.RelayResponsePayload
	switch request := envelope.Request.(type) {
	case core.RelayRequestHello:
		panic("unreachable: hello is answered by envelopeRejection")
	case core.RelayRequestAntigravityStop:
		if err := r.antigravityStop(request.ConversationID, request.FullyIdle, request.TerminationReason, observedBaselineDigest); err != nil {
			return nil, err
		}
		payload = core.RelayResponsePayloadAntigravityStopObserved{}
	case core.RelayRequestAttach:
		answer, refusal, err := r.attach(request.AfterOrdinal, request.AfterDigest, request.KnownStateDigest)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestAcknowledge:
		answer, refusal, err := r.acknowledge(request.ThroughOrdinal, request.ThroughDigest)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestSubmit:
		answer, refusal, err := r.submitCommand(request.CommandID, request.Command)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestHoldInPlaceMoveDispatch:
		answer, refusal, err := r.holdInPlaceMoveDispatch(request.OperationID, request.ExpectedStoreID)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestDiscardHeldMoveCommands:
		answer, refusal, err := r.discardHeldMoveCommands(request.OperationID, request.ExpectedStoreID, request.CommandIDs)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestSubmitFront:
		answer, refusal, err := r.submitFrontCommand(request.OperationID, request.ExpectedStoreID, request.CommandID, request.Command)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestReleaseInPlaceMoveDispatch:
		answer, refusal, err := r.releaseInPlaceMoveDispatch(request.OperationID, request.ExpectedStoreID)
		if err != nil {
			return nil, err
		}
		if refusal != nil {
			return refused(refusal)
		}
		payload = answer
	case core.RelayRequestRestartHarnessInPlace:
		return core.RelayError(core.RelayErrorCodeInvalidRequest,
			"restart_harness_in_place is handled by the worker runtime", false, nil), nil
	case core.RelayRequestReadQueuedCommand:
		// One admitted command is bounded by RelayCommandByteBudget,
		// independently of the total size of the durable queue.
		queued := core.RelayResponsePayloadQueuedCommand{}
		if dispatch, ok := r.snapshot.Dispatches[request.CommandID]; ok && core.RelayCommandIsQueueEntry(dispatch.Command) {
			queued.Command = dispatch.Command
		}
		if handled, ok := r.snapshot.HandledCommands[request.CommandID]; ok {
			accepted := handled.AcceptedOrdinal
			queued.AcceptedOrdinal = &accepted
		}
		payload = queued
	case core.RelayRequestReadCheckpointMailbox:
		mailbox, refusal, err := r.checkpointMailbox(request.BarrierCommandID, &request.Cut)
		if err != nil {
			return nil, err
		}
		if refusal != "" {
			return core.RelayError(core.RelayErrorCodeInvalidState, refusal, false, nil), nil
		}
		payload = core.RelayResponsePayloadCheckpointMailbox{Mailbox: mailbox}
	case core.RelayRequestReadCheckpointNativeUse:
		evidence, refusal := r.checkpointNativeUse(request.BarrierCommandID, &request.Cut)
		if refusal != "" {
			return core.RelayError(core.RelayErrorCodeInvalidState, refusal, false, nil), nil
		}
		payload = core.RelayResponsePayloadCheckpointNativeUse{NativeUseEvidence: evidence}
	case core.RelayRequestReadPendingElicitation:
		pending := core.RelayResponsePayloadPendingElicitation{}
		if body := r.snapshot.PendingElicitations[request.ElicitationID]; body != nil {
			question := *body
			pending.Request = &question
		}
		payload = pending
	case core.RelayRequestBeginLiveBaseline, core.RelayRequestReadLiveBaselinePage, core.RelayRequestAbortLiveBaseline:
		return nil, errors.New("live baseline download requires its owning connection")
	case core.RelayRequestCommandRetentionStatus:
		payload = core.RelayResponsePayloadCommandRetentionState{
			LegacyPending: r.snapshot.LegacyCommandRetention,
			OperationIds:  slices.Sorted(maps.Keys(r.snapshot.CommandRetention)),
		}
	case core.RelayRequestUpdateCommandRetention:
		if !request.ReconcileLegacy || r.snapshot.LegacyCommandRetention {
			next := relayStagedSnapshot(&r.snapshot)
			if operationID := request.OperationID; operationID != nil {
				if err := validateIdentifier(*operationID, "retention operation id"); err != nil {
					return nil, err
				}
				if len(request.CommandIds) == 0 {
					delete(next.CommandRetention, *operationID)
				} else {
					if next.CommandRetention == nil {
						next.CommandRetention = map[string][]string{}
					}
					next.CommandRetention[*operationID] = slices.Clone(request.CommandIds)
				}
			} else if !request.ReconcileLegacy || len(request.CommandIds) != 0 {
				return nil, errors.New("retention requires an operation identity")
			}
			// This publication precedes any following Submit on the
			// connection. A stale upgrade reconciliation becomes a no-op after
			// this point. Operation pin updates do not own the upgrade hold: a
			// different pending Move may still need reconciliation.
			if request.ReconcileLegacy {
				next.LegacyCommandRetention = false
			}
			if err := r.commitSnapshot(next); err != nil {
				return nil, err
			}
		}
		payload = core.RelayResponsePayloadCommandRetentionUpdated{}
	case core.RelayRequestStatus:
		state := r.OperationalState()
		if err := core.EnsureSerializedBudget(state, core.RelayStateByteBudget, "relay operational state"); err != nil {
			return nil, err
		}
		payload = core.RelayResponsePayloadStatus{State: state}
	case core.RelayRequestDrainMailbox:
		answer, err := r.drainMailboxWithInitialSteps(request.HookEvent, request.ConversationID, request.InitialNumSteps)
		if err != nil {
			return nil, err
		}
		payload = answer
	case core.RelayRequestAckMailbox:
		answer, err := r.ackMailbox(request.LeaseID)
		if err != nil {
			return nil, err
		}
		payload = answer
	case core.RelayRequestReserveIdle:
		_, alreadyAccepted := r.snapshot.HandledCommands[request.CommandID]
		idle := false
		if r.verdictHarness != nil {
			state := r.OperationalState()
			idle = state.SafeToReplace(*r.verdictHarness)
		}
		if len(r.reviewerAdmissions) > 0 || (!alreadyAccepted && !idle) {
			payload = core.RelayResponsePayloadIdleReservation{}
		} else {
			reason := "idle worker replacement"
			answer, refusal, err := r.submitCommand(request.CommandID, core.RelayCommandBeginCheckpoint{Reason: &reason})
			if err != nil {
				return nil, err
			}
			if refusal != nil {
				return refused(refusal)
			}
			accepted, ok := answer.(core.RelayResponsePayloadAccepted)
			if !ok {
				panic("barrier submission returns its accepted ordinal")
			}
			ordinal := accepted.Ordinal
			payload = core.RelayResponsePayloadIdleReservation{Ordinal: &ordinal}
		}
	case core.RelayRequestInstallPromptContext:
		if err := r.InstallPromptContext(request.Text); err != nil {
			return nil, err
		}
		payload = core.RelayResponsePayloadPromptContextInstalled{}
	case core.RelayRequestAttachmentPresent, core.RelayRequestInstallAttachment, core.RelayRequestReadAttachment,
		core.RelayRequestCpuUsage, core.RelayRequestCredentialState, core.RelayRequestAwsCredentialsState,
		core.RelayRequestInstallAwsCredentials, core.RelayRequestRemoveAwsCredentials, core.RelayRequestAwsCredentialsScope,
		core.RelayRequestSlopcopLoginState, core.RelayRequestInstallSlopcopLogin, core.RelayRequestRemoveSlopcopLogin,
		core.RelayRequestReadCredentials, core.RelayRequestInstallCredentials, core.RelayRequestSkillsState,
		core.RelayRequestInstallSkills, core.RelayRequestGithubTokenState, core.RelayRequestInstallGithubToken,
		core.RelayRequestRemoveGithubToken, core.RelayRequestProjectMemorySnapshot, core.RelayRequestProjectMemoryTransfer,
		core.RelayRequestHistoryQuery, core.RelayRequestCompleteHistoryRequest, core.RelayRequestCompleteGuardianRequest,
		core.RelayRequestInstallProjectMemorySnapshot,
		core.RelayRequestReplaceProjectMemoryTree, core.RelayRequestCompleteSubagentRequest, core.RelayRequestPollSubagentQueue,
		core.RelayRequestWaitForChange, core.RelayRequestSetSubagentAdmission, core.RelayRequestSetOperatorAway:
		return core.RelayError(core.RelayErrorCodeInvalidState,
			"connection-only requests must be handled by the live relay transport", false, nil), nil
	case core.RelayRequestSubagentRequests:
		payload = core.RelayResponsePayloadSubagentRequests{
			Requests: []core.SubagentToolRequest{},
			Results:  []core.SubagentToolResult{},
		}
	case core.RelayRequestHistoryRequests:
		// A durable-only relay has no live history broker. The worker's
		// connection transport intercepts this when a broker is present.
		payload = core.RelayResponsePayloadHistoryRequests{Requests: []core.HistoryRequest{}}
	// The live connection owns the worker's pending reviews. A sealed relay
	// serving only the journal has none.
	case core.RelayRequestGuardianRequests:
		payload = core.RelayResponsePayloadGuardianRequests{Requests: []core.GuardianReviewRequest{}}
	case core.RelayRequestRespondElicitation:
		return core.RelayError(core.RelayErrorCodeInvalidState,
			"elicitation responses must be handled by the live relay transport", false, nil), nil
	case core.RelayRequestStopBackgroundTask:
		return core.RelayError(core.RelayErrorCodeInvalidState,
			"background task stops must be handled by the live relay transport", false, nil), nil
	case core.RelayRequestReviewer:
		// The reviewer has its own relay and its own harness process. Only
		// the live worker transport owns both, so this relay never answers
		// for it.
		return core.RelayError(core.RelayErrorCodeInvalidState,
			"reviewer requests must be handled by the live relay transport", false, nil), nil
	default:
		return nil, fmt.Errorf("unhandled relay request %T", request)
	}
	return core.RelayResponseBodyOk{Payload: payload}, nil
}

func (r *DurableRelay) InstallPromptContext(text string) error {
	if strings.TrimSpace(text) == "" {
		return errors.New("pending prompt context is empty")
	}
	if r.snapshot.ActivePrompt != nil ||
		(r.snapshot.PendingPromptContext != nil && r.snapshot.PendingPromptContext.AttachedCommandID != nil) {
		return errors.New("cannot replace prompt context while its prompt is active")
	}
	next := relayStagedSnapshot(&r.snapshot)
	switch context := next.PendingPromptContext; {
	case context != nil && context.Text != text:
		context.Text += "\n\n" + text
	case context != nil:
	default:
		next.PendingPromptContext = &core.PendingPromptContext{Text: text}
	}
	if err := core.EnsureSerializedBudget(next, core.RelaySnapshotByteBudget, "relay snapshot with pending prompt context"); err != nil {
		return err
	}
	return r.commitSnapshot(next)
}

func (r *DurableRelay) attach(afterOrdinal uint64, afterDigest string, knownStateDigest *string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	state := r.OperationalState()
	plan := r.replayPlan()
	return plan.attach(afterOrdinal, afterDigest, state, knownStateDigest)
}

func (r *DurableRelay) acknowledge(throughOrdinal uint64, throughDigest string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	plan := r.replayPlan()
	if err := plan.validateCursor(throughOrdinal, throughDigest); err != nil {
		refusal := core.NewRelayProtocolError(
			core.RelayErrorCodeDesynchronized,
			relayErrorDisplay(err),
			false,
			plan.desynchronizedDetail(throughOrdinal, throughDigest),
		)
		return nil, &refusal, nil
	}
	if throughOrdinal > r.snapshot.AcknowledgedThrough {
		next := relayStagedSnapshot(&r.snapshot)
		next.AcknowledgedThrough = throughOrdinal
		next.AcknowledgedDigest = throughDigest
		// The acknowledgement becomes durable before any journal GC.
		if err := r.commitSnapshot(next); err != nil {
			return nil, nil, err
		}
	}
	// An earlier attempt may have durably advanced the ACK and then failed
	// while rewriting or pruning history. Retrying the exact ACK must retry
	// that cleanup instead of treating it as wholly complete.
	if throughOrdinal == r.snapshot.AcknowledgedThrough {
		if err := r.garbageCollectRelayHistory(); err != nil {
			return nil, nil, err
		}
	}
	return core.RelayResponsePayloadAcknowledged{
		ThroughOrdinal: r.snapshot.AcknowledgedThrough,
		ThroughDigest:  r.snapshot.AcknowledgedDigest,
	}, nil, nil
}

// relayContextError is an error with an outer context message over a cause, the
// way anyhow's Context attaches one. Display (relayErrorDisplay) shows only the
// outer message, as anyhow's `to_string` does; Error shows the whole chain,
// as anyhow's `{:#}` does.
type relayContextError struct {
	context string
	cause   error
}

func (e *relayContextError) Error() string { return e.context + ": " + e.cause.Error() }

func (e *relayContextError) Unwrap() error { return e.cause }

// relayWithContext attaches context to err, as anyhow's Context does.
func relayWithContext(err error, context string) error {
	return &relayContextError{context: context, cause: err}
}

// relayErrorDisplay is the outermost message of err, as anyhow's Display shows
// it without the alternate flag: the context of a relayContextError, or the whole
// message of any other error.
func relayErrorDisplay(err error) string {
	switch err := err.(type) {
	case *relayContextError:
		return err.context
	case *unreadableSpanError:
		return err.span.Error()
	}
	return err.Error()
}
