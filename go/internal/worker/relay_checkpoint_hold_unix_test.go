// Port of the cfg(unix) tests in mj-worker/src/relay/checkpoint_hold_tests.rs.
// Runtime events reach the relay only through the Unix worker runtime.

//go:build unix

package worker

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Claude Code ends a cancelled cycle with a result and the adapter's
// usage_update origin marker, in that order, right after a barrier became
// ready. With no harness turn open the result journals nothing, so it must
// not end the hold: the marker waits and the Close still matches its cut.
// Hard-won: 634af0a36: a runtime event that journaled nothing ended the cut hold, so a late usage_update moved the cut
func TestARuntimeEventThatJournalsNothingLeavesTheCutHeld(t *testing.T) {
	durable := holdConfigured(t, t.TempDir())
	durable.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	cut := readyCheckpoint(durable, "cancelled-turn-barrier")
	relay := NewRelayCell(durable)
	inFlight := map[string]core.RelayCommand{}

	message, err := jsonvalue.Parse([]byte(`{
		"type": "result", "subtype": "error_during_execution", "is_error": true,
		"num_turns": 1, "stop_reason": "cancelled",
		"usage": {"input_tokens": 10, "output_tokens": 5},
		"origin": {"kind": "human"}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	result, err := core.ClaudeTurnResultFromSDKMessage(message)
	if err != nil || result == nil {
		t.Fatalf("parse Claude result: %v, %v", result, err)
	}
	if _, err := recordRuntimeEvent(relay, inFlight, core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: *result}); err != nil {
		t.Fatal(err)
	}
	usageUpdate := json.RawMessage(`{
		"sessionUpdate": "usage_update",
		"used": 10,
		"size": 200,
		"_meta": {"_claude/origin": {"kind": "human"}}
	}`)
	if _, err := recordRuntimeEvent(relay, inFlight, core.RuntimeEventSessionUpdate{Update: usageUpdate}); err != nil {
		t.Fatal(err)
	}

	guard := relay.Lock()
	defer guard.Unlock()
	owner := guard.DurableRelay
	if !owner.WorkerWritesHeld() {
		t.Fatal("an event that journals nothing leaves the hold")
	}
	if got := owner.OperationalState().LatestOrdinal; got != cut.Ordinal {
		t.Fatalf("latest ordinal = %d, want the cut %d", got, cut.Ordinal)
	}

	submitRelay(owner, "close-command", core.RelayCommandClose{BarrierCommandID: "cancelled-turn-barrier", Expected: cut})
	submitRelay(owner, "close-checkpoint-complete", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "cancelled-turn-barrier"})
	if owner.WorkerWritesHeld() {
		t.Fatal("the completed barrier ends the hold")
	}
	for _, event := range holdJournal(t, owner) {
		if _, update := event.Observation.(core.RelayObservationSessionUpdate); update {
			t.Fatalf("a session update reached the sealed journal at %d", event.Ordinal)
		}
	}
}

// A background task the adapter announces after the barrier became ready
// journals nothing, so the frontier still equals the cut. Close must still
// be refused: sealing there would stop work the cut never saw.
// Hard-won: 15e0e5b35: background work that started after the cut and journaled nothing let Close stop it
func TestBackgroundWorkStartedAfterTheCutRefusesTheClose(t *testing.T) {
	durable := holdConfigured(t, t.TempDir())
	durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	cut := readyCheckpoint(durable, "spoiled-barrier")
	relay := NewRelayCell(durable)
	inFlight := map[string]core.RelayCommand{}

	if _, err := recordRuntimeEvent(relay, inFlight, core.RuntimeEventAirTaskChanged{
		SessionID: "native-session",
		Update: core.AirTaskUpdateSpawned{
			TaskID:      "build",
			Description: "cargo build",
			CanStop:     true,
		},
	}); err != nil {
		t.Fatal(err)
	}

	guard := relay.Lock()
	defer guard.Unlock()
	owner := guard.DurableRelay
	if got := owner.OperationalState().LatestOrdinal; got != cut.Ordinal {
		t.Fatalf("latest ordinal = %d, want the cut %d", got, cut.Ordinal)
	}
	response := owner.Handle(relayRequest("request-close", core.RelayRequestSubmit{
		CommandID: "close-command",
		Command:   core.RelayCommandClose{BarrierCommandID: "spoiled-barrier", Expected: cut},
	}))
	refusal := fmt.Sprintf("%#v", response.Body)
	if !strings.Contains(refusal, "background work started after the checkpoint cut") {
		t.Fatal(refusal)
	}
}
