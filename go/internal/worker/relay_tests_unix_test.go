// Port of the #[cfg(unix)] tests of mj-worker/src/relay/tests.rs. They drive
// the Unix worker runtime's event recording.

//go:build unix

package worker

import (
	"encoding/json"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 2a0aae9: failed /clear rollback retained a goal resume owned by the exited bridge.
func TestClearBridgeRetirementReleasesResumesBeforeSuccessOrRollback(t *testing.T) {
	for _, replacementSucceeds := range []bool{false, true} {
		for _, replacementHasResume := range []bool{false, true} {
			temp := t.TempDir()
			cell := NewRelayCell(clearableRelay(t, temp))
			rtWithRelayLock(cell, func(state *DurableRelay) {
				submitRelay(state, "clear-request", core.RelayCommandClearContext{})
				rtEqual(t, len(rtMust(state.ClaimPendingCommands(true))), 1)
				state.nativeGoalResumeSent(1, nil)
			})
			var cut *LiveBaselineReceiver
			rtWithRelayLock(cell, func(state *DurableRelay) { cut = state.ReserveLiveBaseline() })
			rtNoCut(t, cut)
			inFlight := map[string]core.RelayCommand{"clear-request": core.RelayCommandClearContext{}}
			rtMust(recordRuntimeEvent(cell, inFlight, core.RuntimeEventContextClearing{RequestID: "clear-request"}))
			rtWithRelayLock(cell, func(state *DurableRelay) {
				if len(state.pendingNativeResumes) != 0 {
					t.Fatalf("retiring the bridge releases its resume: %#v", state.pendingNativeResumes)
				}
			})
			// Retiring the old bridge must not retire a later bridge's work
			// when either outcome of the clear is subsequently applied.
			if replacementHasResume {
				rtWithRelayLock(cell, func(state *DurableRelay) { state.nativeGoalResumeSent(2, nil) })
			}
			rtNoCut(t, cut)
			var outcome core.RuntimeEvent = core.RuntimeEventCommandRejected{
				RequestID: "clear-request",
				Reason:    core.OutcomeReasonCommandFailed,
				Message:   "Could not clear context; restoring the previous conversation",
			}
			if replacementSucceeds {
				outcome = core.RuntimeEventContextCleared{
					RequestID:       "clear-request",
					NativeSessionID: "replacement",
					Memory:          nil,
				}
			}
			rtMust(recordRuntimeEvent(cell, inFlight, outcome))
			if replacementHasResume {
				rtNoCut(t, cut)
				rtWithRelayLock(cell, func(state *DurableRelay) { state.nativeGoalResumeRejected(2) })
			}
			rtCut(t, cut)
			expected := "original"
			if replacementSucceeds {
				expected = "replacement"
			}
			rtWithRelayLock(cell, func(state *DurableRelay) {
				rtEqual(t, state.OperationalState().NativeSessionID, rtString(expected))
			})
		}
	}
}

// Hard-won: ad4c24b: delayed old restart erased a new bridge's synchronous terminal reservation.
func TestOldBridgeRetirementPreservesReplacementTerminalAdmission(t *testing.T) {
	for _, lifecycle := range []core.RuntimeEvent{
		core.RuntimeEventHarnessRestarting{Message: "old bridge exited"},
		core.RuntimeEventContextClearing{RequestID: "clear-request"},
		core.RuntimeEventStopped{},
	} {
		temp := t.TempDir()
		cell := NewRelayCell(rtMust(DurableRelayOpen(temp, session, "test")))
		old := rtMust(NewTerminalRegistry())
		replacement := rtMust(NewTerminalRegistry())
		oldID := old.allocateID()
		replacementID := replacement.allocateID()
		terminal := func(id string) core.ActiveAgentTerminal {
			return core.ActiveAgentTerminal{TerminalID: id, Command: "sleep 30", StartedAtMs: 1}
		}
		inFlight := map[string]core.RelayCommand{}
		rtWithRelayLock(cell, func(relay *DurableRelay) { rtNoErr(relay.AgentTerminalStarted(terminal(oldID))) })
		rtMust(recordRuntimeEvent(cell, inFlight, core.RuntimeEventTerminalStarted{
			TerminalID:  oldID,
			Command:     "sleep 30",
			StartedAtMs: 1,
		}))
		// Replacement registration overtakes queued retirement/lifecycle events,
		// just as terminal/create reserves before spawn and FIFO application.
		rtWithRelayLock(cell, func(relay *DurableRelay) { rtNoErr(relay.AgentTerminalStarted(terminal(replacementID))) })
		var cut *LiveBaselineReceiver
		rtWithRelayLock(cell, func(relay *DurableRelay) { cut = relay.ReserveLiveBaseline() })
		retirement := core.RuntimeEventBridgeRetired{TerminalConnectionID: old.connectionID()}
		// Historical RuntimeEvent readers cannot be sent this worker-local event.
		if _, err := json.Marshal(retirement); err == nil {
			t.Fatal("a bridge retirement must not serialize")
		}
		rtMust(recordRuntimeEvent(cell, inFlight, retirement))
		rtMust(recordRuntimeEvent(cell, inFlight, lifecycle))
		rtNoCut(t, cut)
		rtWithRelayLock(cell, func(relay *DurableRelay) {
			rtEqual(t, relay.OperationalState().ActiveAgentTerminals, []core.ActiveAgentTerminal{terminal(replacementID)})
		})
		rtMust(recordRuntimeEvent(cell, inFlight, core.RuntimeEventTerminalStarted{
			TerminalID:  replacementID,
			Command:     "sleep 30",
			StartedAtMs: 1,
		}))
		rtNoCut(t, cut)
		rtMust(recordRuntimeEvent(cell, inFlight, core.RuntimeEventBridgeRetired{TerminalConnectionID: replacement.connectionID()}))
		rtCut(t, cut)
	}
}
