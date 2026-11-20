// Port of the tests in mj-transcript/src/projection/api_events.rs.

package transcript

import (
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func apiEventsTestOutcome(stopReason string) core.MaterializedSessionMutation {
	accepted, start := uint64(1), uint64(2)
	return core.MaterializedSessionMutation{
		LastTurnOutcome: &core.MaterializedTurnOutcome{
			CommandID:         "prompt",
			AcceptedOrdinal:   &accepted,
			TurnStartPosition: &start,
			CompletedOrdinal:  3,
			CompletedAtMs:     1000,
			Outcome:           core.TurnOutcomeKindCompleted{StopReason: stopReason},
		},
	}
}

func apiEventsTestEvent() core.RelayEvent {
	return core.RelayEvent{
		Format:       core.RelayEventFormatV1,
		Ordinal:      3,
		RecordedAtMs: 1000,
		Observation:  core.RelayObservationHarnessTurnStarted{StartedAtMs: 1000},
	}
}

func TestAwaitingInputEmitsInputRequiredBeforeTurnEnded(t *testing.T) {
	current := core.MaterializedSessionEmpty("session")
	event := apiEventsTestEvent()
	mutation := apiEventsTestOutcome(core.AwaitingInputStopReason)
	events := apiEventsDerive(&current, &event, &mutation)
	if len(events) != 2 {
		t.Fatalf("events = %#v", events)
	}
	required, ok := events[0].(core.ApiEventDataInputRequired)
	if !ok || required.Request != nil || required.TurnID == nil || *required.TurnID != 1 {
		t.Fatalf("first event = %#v", events[0])
	}
	if _, ok := events[1].(core.ApiEventDataTurnEnded); !ok {
		t.Fatalf("second event = %#v", events[1])
	}
	encoded, err := core.MarshalApiEventData(events[0])
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	data, _ := parsed.(jsonvalue.Object).Get("data")
	if _, ok := data.(jsonvalue.Object).Get("request"); ok {
		t.Fatalf("input required without a form serialized a request: %s", encoded)
	}
}

func TestInferredFinishedEmitsTurnEndedWithoutInputRequired(t *testing.T) {
	current := core.MaterializedSessionEmpty("session")
	event := apiEventsTestEvent()
	mutation := apiEventsTestOutcome(core.InferredFinishedStopReason)
	events := apiEventsDerive(&current, &event, &mutation)
	if len(events) != 1 {
		t.Fatalf("events = %#v", events)
	}
	if _, ok := events[0].(core.ApiEventDataTurnEnded); !ok {
		t.Fatalf("event = %#v", events[0])
	}
}
