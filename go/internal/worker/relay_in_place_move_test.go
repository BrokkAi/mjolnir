// Port of mj-worker/src/relay/in_place_move_tests.rs.

package worker

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func inPlaceConfigured(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpen(root, session, "1.0.0"))
	rtMust(relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "native-session"}))
	rtMust(relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}))
	return relay
}

func inPlaceStoreID(t *testing.T, relay *DurableRelay) string {
	t.Helper()
	if relay.snapshot.StoreID == nil {
		t.Fatal("store_id assigned")
	}
	return *relay.snapshot.StoreID
}

func inPlaceOk(t *testing.T, response core.RelayResponseEnvelope) core.RelayResponsePayload {
	t.Helper()
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("expected Ok, got %#v", response.Body)
	}
	return ok.Payload
}

func inPlaceRefusal(t *testing.T, response core.RelayResponseEnvelope, code core.RelayErrorCode) core.RelayProtocolError {
	t.Helper()
	refused, isError := response.Body.(core.RelayResponseBodyError)
	if !isError || refused.Error.Code != code {
		t.Fatalf("expected %v error, got %#v", code, response.Body)
	}
	return refused.Error
}

func inPlaceQueuedIDs(relay *DurableRelay) []string {
	ids := []string{}
	for _, queued := range relay.snapshot.QueuedPrompts {
		ids = append(ids, queued.CommandID)
	}
	return ids
}

func inPlaceHeld(t *testing.T, payload core.RelayResponsePayload) []string {
	t.Helper()
	held, ok := payload.(core.RelayResponsePayloadInPlaceMoveDispatchHeld)
	if !ok {
		t.Fatalf("expected InPlaceMoveDispatchHeld, got %#v", payload)
	}
	return held.CapturedPendingCommandIDs
}

func TestInPlaceMoveHoldIsAtomicIdempotentAndEnforcesStoreIDOnAllCommands(t *testing.T) {
	relay := inPlaceConfigured(t, t.TempDir())
	storeID := inPlaceStoreID(t, relay)

	// Start one prompt so subsequent prompts stay queued.
	inPlaceOk(t, relay.Handle(relayRequest("r-active", core.RelayRequestSubmit{CommandID: "cmd-active", Command: prompt("active turn")})))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)

	// Queue two pending prompts.
	for _, queued := range [][3]string{{"r-q1", "prompt-1", "queued 1"}, {"r-q2", "prompt-2", "queued 2"}} {
		inPlaceOk(t, relay.Handle(relayRequest(queued[0], core.RelayRequestSubmit{CommandID: queued[1], Command: prompt(queued[2])})))
	}

	// Hold dispatch: captures exact pending IDs.
	captured := inPlaceHeld(t, inPlaceOk(t, relay.Handle(relayRequest("r-hold-1", core.RelayRequestHoldInPlaceMoveDispatch{
		OperationID: "move-op-1", ExpectedStoreID: storeID,
	}))))
	rtEqual(t, captured, []string{"prompt-1", "prompt-2"})

	// Another prompt arrives while held; repeating Hold with the same
	// operation_id must idempotently return the originally captured IDs.
	inPlaceOk(t, relay.Handle(relayRequest("r-q3", core.RelayRequestSubmit{CommandID: "prompt-3", Command: prompt("queued 3")})))
	captured = inPlaceHeld(t, inPlaceOk(t, relay.Handle(relayRequest("r-hold-2", core.RelayRequestHoldInPlaceMoveDispatch{
		OperationID: "move-op-1", ExpectedStoreID: storeID,
	}))))
	rtEqual(t, captured, []string{"prompt-1", "prompt-2"})

	// A different operation_id is refused while move-op-1 holds dispatch.
	inPlaceRefusal(t, relay.Handle(relayRequest("r-hold-other", core.RelayRequestHoldInPlaceMoveDispatch{
		OperationID: "move-op-2", ExpectedStoreID: storeID,
	})), core.RelayErrorCodeInvalidState)

	// Mismatched expected_store_id is rejected with StoreMismatch on ALL four
	// commands.
	wrongStore := "wrong-store-id"
	for _, request := range []struct {
		id      string
		request core.RelayRequest
	}{
		{"r-bad-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: wrongStore}},
		{"r-bad-discard", core.RelayRequestDiscardHeldMoveCommands{OperationID: "move-op-1", ExpectedStoreID: wrongStore, CommandIDs: []string{"prompt-1"}}},
		{"r-bad-front", core.RelayRequestSubmitFront{OperationID: "move-op-1", ExpectedStoreID: wrongStore, CommandID: "cmd-cont", Command: prompt("continuation")}},
		{"r-bad-release", core.RelayRequestReleaseInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: wrongStore}},
	} {
		refusal := inPlaceRefusal(t, relay.Handle(relayRequest(request.id, request.request)), core.RelayErrorCodeInvalidState)
		if _, mismatch := refusal.Detail.(core.RelayErrorDetailStoreMismatch); !mismatch {
			t.Fatalf("expected StoreMismatch for %s, got %#v", request.id, refusal)
		}
	}

	// Idempotent discard: removing prompt-1 twice succeeds without error.
	for _, requestID := range []string{"r-discard-1", "r-discard-2"} {
		payload := inPlaceOk(t, relay.Handle(relayRequest(requestID, core.RelayRequestDiscardHeldMoveCommands{
			OperationID: "move-op-1", ExpectedStoreID: storeID, CommandIDs: []string{"prompt-1"},
		})))
		if _, ok := payload.(core.RelayResponsePayloadHeldMoveCommandsDiscarded); !ok {
			t.Fatalf("expected HeldMoveCommandsDiscarded, got %#v", payload)
		}
	}
	rtEqual(t, inPlaceQueuedIDs(relay), []string{"prompt-2", "prompt-3"})
}

func TestSubmitFrontRequiresHoldInsertsAtHeadAndDeduplicates(t *testing.T) {
	relay := inPlaceConfigured(t, t.TempDir())
	storeID := inPlaceStoreID(t, relay)

	// Rejected when dispatch is NOT held.
	inPlaceRefusal(t, relay.Handle(relayRequest("r-front-unheld", core.RelayRequestSubmitFront{
		OperationID: "move-op-1", ExpectedStoreID: storeID, CommandID: "cmd-cont", Command: prompt("resume work"),
	})), core.RelayErrorCodeInvalidState)

	// Hold dispatch, queue a normal prompt, then SubmitFront a continuation.
	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID}))
	relay.Handle(relayRequest("r-normal", core.RelayRequestSubmit{CommandID: "cmd-normal", Command: prompt("normal queued prompt")}))
	front := core.RelayRequestSubmitFront{OperationID: "move-op-1", ExpectedStoreID: storeID, CommandID: "cmd-cont", Command: prompt("resume work")}
	first, ok := inPlaceOk(t, relay.Handle(relayRequest("r-front-1", front))).(core.RelayResponsePayloadAccepted)
	if !ok {
		t.Fatal("expected Accepted")
	}

	// Repeat submission of the same command_id deduplicates without
	// double-enqueueing.
	second, ok := inPlaceOk(t, relay.Handle(relayRequest("r-front-2", front))).(core.RelayResponsePayloadAccepted)
	if !ok {
		t.Fatal("expected deduplicated Accepted")
	}
	rtEqual(t, second.Ordinal, first.Ordinal)

	rtEqual(t, inPlaceQueuedIDs(relay), []string{"cmd-cont", "cmd-normal"})
	// Still held: nothing is claimable.
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 0)
}

func TestInPlaceMoveHoldPersistsAcrossReopenAndReleasesSubmitFrontFirst(t *testing.T) {
	temp := t.TempDir()
	relay := inPlaceConfigured(t, temp)
	storeID := inPlaceStoreID(t, relay)
	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-reopen", ExpectedStoreID: storeID}))
	relay.Handle(relayRequest("r-q1", core.RelayRequestSubmit{CommandID: "cmd-queued", Command: prompt("queued behind continuation")}))
	relay.Handle(relayRequest("r-front", core.RelayRequestSubmitFront{
		OperationID: "move-op-reopen", ExpectedStoreID: storeID, CommandID: "cmd-continuation", Command: prompt("continuation first"),
	}))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 0)

	// Reopen from relay-state.json: hold survives and nothing claims while
	// held.
	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if reopened.snapshot.InPlaceMoveHold == nil {
		t.Fatal("the hold did not survive reopen")
	}
	rtEqual(t, len(rtMust(reopened.ClaimPendingCommands(true))), 0)

	// Release the hold: SubmitFront continuation claims first ahead of
	// cmd-queued. Releasing again is idempotent.
	for _, requestID := range []string{"r-release", "r-release-2"} {
		payload := inPlaceOk(t, reopened.Handle(relayRequest(requestID, core.RelayRequestReleaseInPlaceMoveDispatch{
			OperationID: "move-op-reopen", ExpectedStoreID: storeID,
		})))
		if _, ok := payload.(core.RelayResponsePayloadInPlaceMoveDispatchReleased); !ok {
			t.Fatalf("expected InPlaceMoveDispatchReleased, got %#v", payload)
		}
	}

	claimed := rtMust(reopened.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cmd-continuation")
}

func TestRelayStateVersion28WithoutInPlaceMoveHoldDecodesCleanly(t *testing.T) {
	temp := t.TempDir()
	inPlaceConfigured(t, temp)
	statePath := filepath.Join(temp, core.RelayStateFile)
	var saved map[string]any
	if err := json.Unmarshal(rtMust(os.ReadFile(statePath)), &saved); err != nil {
		t.Fatal(err)
	}
	saved["format_version"] = 28
	delete(saved, "in_place_move_hold")
	body := rtMust(json.MarshalIndent(saved, "", "  "))
	if err := os.WriteFile(statePath, body, 0o600); err != nil {
		t.Fatal(err)
	}

	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtEqual(t, reopened.snapshot.FormatVersion, core.RelayStateVersion)
	if reopened.snapshot.InPlaceMoveHold != nil {
		t.Fatal("a version-28 snapshot gained a hold")
	}
}

func TestInPlaceHarnessRestartRequiresMatchingHoldAndIsIdempotentAfterMark(t *testing.T) {
	temp := t.TempDir()
	relay := inPlaceConfigured(t, temp)
	storeID := inPlaceStoreID(t, relay)

	// Rejected when no hold is active.
	if _, refusal := relay.admitInPlaceHarnessRestart("move-op-1", storeID); refusal == nil || refusal.Code != core.RelayErrorCodeInvalidState {
		t.Fatalf("refusal = %#v", refusal)
	}

	// Take hold for move-op-1.
	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID}))

	// Rejected when operation_id mismatches.
	if _, refusal := relay.admitInPlaceHarnessRestart("move-op-other", storeID); refusal == nil || refusal.Code != core.RelayErrorCodeInvalidState {
		t.Fatalf("refusal = %#v", refusal)
	}

	// Admitted for matching operation_id and store_id when idle.
	decision, refusal := relay.admitInPlaceHarnessRestart("move-op-1", storeID)
	if refusal != nil {
		t.Fatalf("refusal = %#v", refusal)
	}
	rtEqual(t, decision, InPlaceHarnessRestartProceed)

	// Mark restarted durably; reopen verifies harness_restarted persists and
	// repeat admission returns AlreadyRestarted idempotently.
	rtNoErr(relay.markInPlaceHarnessRestarted("move-op-1", storeID))
	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	decision, refusal = reopened.admitInPlaceHarnessRestart("move-op-1", storeID)
	if refusal != nil {
		t.Fatalf("refusal = %#v", refusal)
	}
	rtEqual(t, decision, InPlaceHarnessRestartAlreadyRestarted)
}

func TestInPlaceHarnessRestartInFlightBlocksReleaseAndMutationsUntilCompleted(t *testing.T) {
	relay := inPlaceConfigured(t, t.TempDir())
	storeID := inPlaceStoreID(t, relay)

	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID}))
	relay.Handle(relayRequest("r-q1", core.RelayRequestSubmit{CommandID: "prompt-queued", Command: prompt("queued prompt")}))

	// Admit harness restart -> in-flight reservation held.
	decision, refusal := relay.admitInPlaceHarnessRestart("move-op-1", storeID)
	if refusal != nil {
		t.Fatalf("refusal = %#v", refusal)
	}
	rtEqual(t, decision, InPlaceHarnessRestartProceed)

	// Second concurrent restart admission is refused.
	if _, refusal := relay.admitInPlaceHarnessRestart("move-op-1", storeID); refusal == nil || refusal.Code != core.RelayErrorCodeInvalidState {
		t.Fatalf("refusal = %#v", refusal)
	}

	// Release, Discard, and SubmitFront are all refused while restart is in
	// flight.
	for _, request := range []struct {
		id      string
		request core.RelayRequest
	}{
		{"r-rel-busy", core.RelayRequestReleaseInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID}},
		{"r-disc-busy", core.RelayRequestDiscardHeldMoveCommands{OperationID: "move-op-1", ExpectedStoreID: storeID, CommandIDs: []string{}}},
		{"r-front-busy", core.RelayRequestSubmitFront{OperationID: "move-op-1", ExpectedStoreID: storeID, CommandID: "prompt-cont", Command: prompt("continuation")}},
	} {
		inPlaceRefusal(t, relay.Handle(relayRequest(request.id, request.request)), core.RelayErrorCodeInvalidState)
	}
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 0)

	// Complete restart -> Release now succeeds and promotes queued prompt.
	rtNoErr(relay.markInPlaceHarnessRestarted("move-op-1", storeID))
	payload := inPlaceOk(t, relay.Handle(relayRequest("r-rel-ok", core.RelayRequestReleaseInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID})))
	if _, ok := payload.(core.RelayResponsePayloadInPlaceMoveDispatchReleased); !ok {
		t.Fatalf("expected InPlaceMoveDispatchReleased, got %#v", payload)
	}
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-queued")
}

func TestDiscardHeldMoveCommandsRejectsPostHoldPromptsWithoutMutating(t *testing.T) {
	relay := inPlaceConfigured(t, t.TempDir())
	storeID := inPlaceStoreID(t, relay)

	// Start an active turn so prompt-1 stays queued.
	relay.Handle(relayRequest("r-active", core.RelayRequestSubmit{CommandID: "prompt-active", Command: prompt("active")}))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	relay.Handle(relayRequest("r-p1", core.RelayRequestSubmit{CommandID: "prompt-1", Command: prompt("captured prompt")}))

	// Take hold -> captures only prompt-1.
	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-1", ExpectedStoreID: storeID}))

	// A new prompt arrives after the hold was taken.
	relay.Handle(relayRequest("r-post", core.RelayRequestSubmit{CommandID: "prompt-post-hold", Command: prompt("post-hold prompt")}))

	// Attempting to discard a batch containing the post-hold prompt is
	// rejected with InvalidRequest and mutates nothing.
	inPlaceRefusal(t, relay.Handle(relayRequest("r-bad-discard", core.RelayRequestDiscardHeldMoveCommands{
		OperationID: "move-op-1", ExpectedStoreID: storeID, CommandIDs: []string{"prompt-1", "prompt-post-hold"},
	})), core.RelayErrorCodeInvalidRequest)
	rtEqual(t, inPlaceQueuedIDs(relay), []string{"prompt-1", "prompt-post-hold"})

	// Discarding only the captured prompt-1 succeeds and leaves
	// prompt-post-hold intact.
	payload := inPlaceOk(t, relay.Handle(relayRequest("r-ok-discard", core.RelayRequestDiscardHeldMoveCommands{
		OperationID: "move-op-1", ExpectedStoreID: storeID, CommandIDs: []string{"prompt-1"},
	})))
	if _, ok := payload.(core.RelayResponsePayloadHeldMoveCommandsDiscarded); !ok {
		t.Fatalf("expected HeldMoveCommandsDiscarded, got %#v", payload)
	}
	rtEqual(t, inPlaceQueuedIDs(relay), []string{"prompt-post-hold"})
}

func TestInPlaceMoveHoldAllowsActiveTurnCancelWhileBlockingQueuedPrompts(t *testing.T) {
	relay := inPlaceConfigured(t, t.TempDir())
	storeID := inPlaceStoreID(t, relay)

	// Start an active prompt and queue a second prompt behind it.
	relay.Handle(relayRequest("r-active", core.RelayRequestSubmit{CommandID: "prompt-active", Command: prompt("active turn")}))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-active")
	relay.Handle(relayRequest("r-queued", core.RelayRequestSubmit{CommandID: "prompt-queued", Command: prompt("queued behind active")}))

	// Take an in-place move hold while prompt-active is still running.
	relay.Handle(relayRequest("r-hold", core.RelayRequestHoldInPlaceMoveDispatch{OperationID: "move-op-cancel", ExpectedStoreID: storeID}))

	// Submit CancelTurn for the active turn: it must still be claimable
	// during the hold, while prompt-queued stays held.
	inPlaceOk(t, relay.Handle(relayRequest("r-cancel", core.RelayRequestSubmit{CommandID: "cancel-active", Command: core.RelayCommandCancelTurn{}})))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cancel-active")
	if _, cancel := claimed[0].Command.(core.RelayCommandCancelTurn); !cancel {
		t.Fatalf("claimed %#v, want CancelTurn", claimed[0].Command)
	}

	// Even after the active turn finishes, prompt-queued must remain held.
	rtMust(relay.RecordCommandCompleted("prompt-active", core.RelayCommandOutcomePrompt{StopReason: "cancelled"}))
	rtMust(relay.RecordCommandCompleted("cancel-active", core.RelayCommandOutcomeCancelled{}))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 0)
	if !slices.Contains(inPlaceQueuedIDs(relay), "prompt-queued") {
		t.Fatal("the held prompt left the queue")
	}
}
