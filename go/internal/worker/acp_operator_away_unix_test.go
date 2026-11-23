// Port of mj-worker/src/acp/operator_away.rs: its #[cfg(unix)] test.

//go:build unix

package worker

import (
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: c1da39f: the last terminal closed before a dead bridge's form-resolution event.
func TestRecoveryCutAsksResponderOwnerBeforeTerminalCloseReleasesIt(t *testing.T) {
	durable, err := DurableRelayOpen(t.TempDir(), "question-cut", "test")
	if err != nil {
		t.Fatal(err)
	}
	relay := NewRelayCell(durable)
	guard := relay.Lock()
	owner := guard.operatorAway
	guard.Unlock()
	old := owner.pendingForBridge()
	oldLifetime := old.bridgeLifetime()
	replacement := owner.pendingForBridge()
	replacementLifetime := replacement.bridgeLifetime()
	defer replacementLifetime.Close()
	oldQuestion := afkQuestion("old", "Old bridge question")
	liveQuestion := afkQuestion("live", "Replacement bridge question")
	abandoned := afkQuestion("abandoned", "Responder disappeared")
	oldReusedQuestion := afkQuestion("reused", "Previous bridge's form")
	currentReusedQuestion := afkQuestion("reused", "Replacement bridge's different form")
	reusedAnswer := replacement.register(currentReusedQuestion, "", AwayRequestQuestion{})
	defer reusedAnswer.Close()

	oldAnswer := old.register(oldQuestion, "", AwayRequestQuestion{})
	defer oldAnswer.Close()
	liveAnswer := replacement.register(liveQuestion, "", AwayRequestQuestion{})
	defer liveAnswer.Close()
	replacement.register(abandoned, "", AwayRequestQuestion{}).Close()
	{
		state := relay.Lock()
		for _, request := range []core.ElicitationRequest{oldQuestion, liveQuestion, abandoned, oldReusedQuestion} {
			if _, err := state.RecordObservation(core.RelayObservationElicitationRequested{Request: request}); err != nil {
				t.Fatal(err)
			}
		}
		baseline, err := state.LiveSessionBaseline()
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(baseline.Questions, []core.ElicitationRequest{liveQuestion, oldQuestion}) {
			t.Fatalf("questions %+v", baseline.Questions)
		}
		if err := state.AgentTerminalStarted(core.ActiveAgentTerminal{
			TerminalID:  "old-terminal",
			Command:     "sleep 30",
			StartedAtMs: 1,
		}); err != nil {
			t.Fatal(err)
		}
		state.Unlock()
	}
	guard = relay.Lock()
	cut := guard.ReserveLiveBaseline()
	guard.Unlock()
	if _, ok := cut.TryRecv(); ok {
		t.Fatal("the cut was taken with a terminal open")
	}
	// Responder clones and their resolution tasks still exist. The actual
	// bridge lifetime owner is decisive even without HarnessRestarting.
	oldLifetime.Close()
	exitCode := uint32(0)
	if _, err := recordRuntimeEvent(relay, map[string]core.RelayCommand{}, core.RuntimeEventTerminalClosed{
		TerminalID: "old-terminal",
		ExitCode:   &exitCode,
	}); err != nil {
		t.Fatal(err)
	}
	result, ok := cut.TryRecv()
	if !ok || result.Err != nil {
		t.Fatalf("cut %v %v", ok, result.Err)
	}
	if !reflect.DeepEqual(result.Baseline.Questions, []core.ElicitationRequest{liveQuestion}) {
		t.Fatalf("questions %+v", result.Baseline.Questions)
	}
	// A restarted bridge can reuse IDs. Its live responder cannot bless the
	// old body before the new Requested event reaches the frontier.
	guard = relay.Lock()
	if _, err := guard.RecordObservation(core.RelayObservationElicitationRequested{Request: currentReusedQuestion}); err != nil {
		t.Fatal(err)
	}
	guard.Unlock()
	guard = relay.Lock()
	baseline, err := guard.LiveSessionBaseline()
	guard.Unlock()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(baseline.Questions, []core.ElicitationRequest{liveQuestion, currentReusedQuestion}) {
		t.Fatalf("questions %+v", baseline.Questions)
	}
	guard = relay.Lock()
	history, err := guard.EventsAfter(0, core.RelayEventGenesisDigest)
	guard.Unlock()
	if err != nil {
		t.Fatal(err)
	}
	for _, event := range history {
		if _, resolved := event.Observation.(core.RelayObservationElicitationResolved); resolved {
			t.Fatal("a form-resolution event was recorded")
		}
	}
}
