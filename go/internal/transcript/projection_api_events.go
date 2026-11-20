// Port of mj-transcript/src/projection/api_events.rs.

package transcript

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// apiEventsDerive is api_events::derive: it retains transitions before the
// database page coalesces its final projection.
func apiEventsDerive(current *core.MaterializedSession, event *core.RelayEvent, mutation *core.MaterializedSessionMutation) []core.ApiEventData {
	var events []core.ApiEventData
	if mutation.ActiveTurn != nil && *mutation.ActiveTurn != nil {
		turn := *mutation.ActiveTurn
		if current.ActiveTurn == nil || !reflect.DeepEqual(*current.ActiveTurn, *turn) {
			events = append(events, core.ApiEventDataTurnStarted{Turn: *turn})
		}
	}
	switch observation := event.Observation.(type) {
	case core.RelayObservationAgentInitialized:
		if observation.Runtime != nil {
			events = append(events, core.ApiEventDataRuntimeResolved{Receipt: core.RuntimeReceipt{
				Identity:     *observation.Runtime,
				EventOrdinal: event.Ordinal,
				ObservedAtMs: event.RecordedAtMs,
			}})
		}
	case core.RelayObservationCommandRejected:
		if observation.Command != core.RelayCommandKindPrompt {
			events = append(events, commandFailureEvent(observation.CommandID, observation.Command, observation.Message, observation.Reason))
		}
	case core.RelayObservationCommandInterrupted:
		if observation.Command != core.RelayCommandKindPrompt {
			events = append(events, commandFailureEvent(observation.CommandID, observation.Command, observation.Message, observation.Reason))
		}
	case core.RelayObservationCommandCompleted:
		if observation.Command != nil && *observation.Command != core.RelayCommandKindPrompt {
			if barrier := observation.BarrierCommandID; barrier != nil {
				events = append(events, core.ApiEventDataCommandEnded{Result: core.CommandResultWorker(
					*barrier, core.RelayCommandKindBeginCheckpoint, core.CommandResultKindSucceeded, nil, nil)})
			}
			events = append(events, core.ApiEventDataCommandEnded{Result: core.CommandResultCompleted(
				observation.CommandID, *observation.Command, observation.Outcome)})
		}
	case core.RelayObservationMailboxEventsDelivered:
		if steering := observation.Steering; steering != nil {
			outcome := core.RelayCommandOutcomeSteered{QueuedCommandID: steering.QueuedPromptID}
			events = append(events, core.ApiEventDataCommandEnded{Result: core.CommandResultCompleted(
				steering.CommandID, core.RelayCommandKindSteer, outcome)})
		}
	case core.RelayObservationSessionFault:
		events = append(events, core.ApiEventDataSessionFault{
			Reason:    observation.Reason,
			Message:   observation.Message,
			CommandID: event.CommandID,
		})
	}
	if turn := mutation.LastTurnOutcome; turn != nil {
		if turn.Result().Kind == core.TurnResultKindInputRequired {
			events = append(events, core.ApiEventDataInputRequired{TurnID: turn.AcceptedOrdinal})
		}
		events = append(events, core.ApiEventDataTurnEnded{Turn: core.ApiTurnOutcomeFrom(turn)})
	}
	var turnID *uint64
	if current.ActiveTurn != nil {
		turnID = current.ActiveTurn.AcceptedOrdinal
	}
	if mutation.PendingElicitations != nil {
		pending := *mutation.PendingElicitations
		for i := range pending {
			known := false
			for j := range current.PendingElicitations {
				if reflect.DeepEqual(current.PendingElicitations[j], pending[i]) {
					known = true
					break
				}
			}
			if !known {
				request := pending[i]
				events = append(events, core.ApiEventDataInputRequired{Request: &request, TurnID: turnID})
			}
		}
		for i := range current.PendingElicitations {
			request := &current.PendingElicitations[i]
			stillPending := false
			for j := range pending {
				if pending[j].ID == request.ID {
					stillPending = true
					break
				}
			}
			if stillPending {
				continue
			}
			action := "cleared"
			if resolved, ok := event.Observation.(core.RelayObservationElicitationResolved); ok {
				action = resolved.Action
			}
			events = append(events, core.ApiEventDataInputResolved{
				ElicitationID: request.ID,
				TurnID:        turnID,
				Action:        action,
			})
		}
	}
	return events
}

// commandFailureEvent is the API event for a control command the relay
// rejected or interrupted: a typed command result when the worker classified
// the failure, otherwise the legacy error notice.
func commandFailureEvent(commandID string, command core.RelayCommandKind, message string, reason *core.OutcomeReason) core.ApiEventData {
	if reason != nil && *reason != core.OutcomeReasonLegacyUnclassified {
		outcome := core.CommandResultKindFailed
		if reason.ExpectedCancellation() {
			outcome = core.CommandResultKindCancelled
		} else if *reason == core.OutcomeReasonAdmissionRejected {
			outcome = core.CommandResultKindRejected
		}
		classified := *reason
		text := message
		return core.ApiEventDataCommandEnded{Result: core.CommandResultWorker(commandID, command, outcome, &classified, &text)}
	}
	id := commandID
	return core.ApiEventDataLegacyNotice{OriginalType: "error", Message: message, CommandID: &id}
}
