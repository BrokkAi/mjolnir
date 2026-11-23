// Port of the tests in mj-worker/src/relay/native_history.rs.

package worker

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// nativeHistoryEditSnapshot rewrites relay-state.json as JSON, the way the
// Rust tests edit a serde_json::Value.
func nativeHistoryEditSnapshot(t *testing.T, root string, edit func(saved map[string]any)) {
	t.Helper()
	path := filepath.Join(root, core.RelayStateFile)
	encoded, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	var saved map[string]any
	if err := decoder.Decode(&saved); err != nil {
		t.Fatal(err)
	}
	edit(saved)
	rewritten, err := json.Marshal(saved)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, rewritten, 0o600); err != nil {
		t.Fatal(err)
	}
}

func nativeHistoryRemoveEvidence(t *testing.T, root string) {
	t.Helper()
	nativeHistoryEditSnapshot(t, root, func(saved map[string]any) {
		delete(saved, "native_use_evidence")
		saved["format_version"] = 27
		delete(saved, "native_session_used")
		delete(saved, "native_session_opened_ordinal")
	})
}

func nativeHistoryOpenSession(t *testing.T, relay *DurableRelay, resumed bool) {
	t.Helper()
	if _, err := relay.RecordObservation(core.RelayObservationSessionOpened{
		NativeSessionID: "legacy-thread",
		Resumed:         resumed,
	}); err != nil {
		t.Fatal(err)
	}
}

func nativeHistoryOpen(t *testing.T, root, version string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, session, version)
	if err != nil {
		t.Fatal(err)
	}
	return relay
}

func nativeHistoryQuestion(t *testing.T, id, message string) core.ElicitationRequest {
	t.Helper()
	question, err := core.ElicitationRequestFromACPParams(id, json.RawMessage(`{
		"mode":"form", "sessionId":"`+session+`", "message":"`+message+`",
		"requestedSchema":{"type":"object", "properties":{}}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	return question
}

func nativeHistoryWarnings(t *testing.T, relay *DurableRelay, count int) {
	t.Helper()
	for range count {
		if _, err := relay.RecordObservation(core.RelayObservationWarning{Message: strings.Repeat("x", 64*1024)}); err != nil {
			t.Fatal(err)
		}
	}
}

func nativeHistoryReadQuestion(t *testing.T, relay *DurableRelay, requestID, id string) *core.ElicitationRequest {
	t.Helper()
	response := relay.Handle(relayRequest(requestID, core.RelayRequestReadPendingElicitation{ElicitationID: id}))
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	pending, isPending := ok.Payload.(core.RelayResponsePayloadPendingElicitation)
	if !isOk || !isPending {
		t.Fatalf("read pending elicitation answered %#v", response.Body)
	}
	return pending.Request
}

// Hard-won: b62fe3b: a legacy question below the recovery floor blocked worker upgrade.
func TestUpgradeKeepsAPrunedLegacyQuestionIDWithoutBlockingWorkerOpen(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	question := nativeHistoryQuestion(t, "pruned-question", "Original question")
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}); err != nil {
		t.Fatal(err)
	}
	frontier := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	acknowledgeRelay(relay, "old-acked-question", frontier)
	nativeHistoryEditSnapshot(t, root, func(saved map[string]any) {
		saved["format_version"] = 27
		saved["recovery_floor_ordinal"] = frontier
		saved["recovery_floor_digest"] = digest
		delete(saved, "pending_elicitations")
	})
	for range 2 {
		relay := nativeHistoryOpen(t, root, "upgraded")
		if !relaySortedSetContains(relay.snapshot.AssessmentQuestions, question.ID) {
			t.Fatal("the legacy question stays open")
		}
		body, ok := relay.snapshot.PendingElicitations[question.ID]
		if !ok || body != nil {
			t.Fatalf("pending elicitation = %v, %v; want an unavailable body", body, ok)
		}
		baseline, err := relay.LiveSessionBaseline()
		if err != nil {
			t.Fatal(err)
		}
		if baseline.UnavailableQuestions != 0 || len(baseline.Questions) != 0 {
			t.Fatalf("baseline questions = %v, unavailable %d", baseline.Questions, baseline.UnavailableQuestions)
		}
	}
	relay = nativeHistoryOpen(t, root, "upgraded")
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}); err != nil {
		t.Fatal(err)
	}
	request := nativeHistoryReadQuestion(t, relay, "reissued-question", question.ID)
	if request == nil || !reflect.DeepEqual(*request, question) {
		t.Fatalf("reissued question = %#v", request)
	}
}

// Hard-won: 363ef03: ACK pruning discarded the only durable pending-question body.
func TestLegacyPendingQuestionSurvivesMigrationPruningAndWorkerRestart(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	question := nativeHistoryQuestion(t, "legacy-question", "Which destination?")
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}); err != nil {
		t.Fatal(err)
	}
	nativeHistoryWarnings(t, relay, 40)
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}
	frontier := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	nativeHistoryEditSnapshot(t, root, func(saved map[string]any) {
		saved["format_version"] = 27
		saved["acknowledged_through"] = frontier
		saved["acknowledged_digest"] = digest
		delete(saved, "pending_elicitations")
	})
	relay = nativeHistoryOpen(t, root, "new")
	acknowledged := acknowledgeRelay(relay, "prune-migrated-question", frontier)
	if ok, isOk := acknowledged.Body.(core.RelayResponseBodyOk); !isOk {
		t.Fatalf("acknowledge answered %#v", acknowledged.Body)
	} else if _, isAck := ok.Payload.(core.RelayResponsePayloadAcknowledged); !isAck {
		t.Fatalf("acknowledge answered %#v", ok.Payload)
	}
	info, err := os.Stat(filepath.Join(root, core.RelayJournalDir, core.RelayActiveSegment))
	if err != nil {
		t.Fatal(err)
	}
	if info.Size() != 0 {
		t.Fatalf("active segment holds %d bytes after pruning", info.Size())
	}
	relay = nativeHistoryOpen(t, root, "new")
	recovered := nativeHistoryReadQuestion(t, relay, "read-question", question.ID)
	if recovered == nil || !reflect.DeepEqual(*recovered, question) {
		t.Fatalf("recovered question = %#v", recovered)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationResolved{
		ElicitationID: question.ID,
		Action:        "accept",
	}); err != nil {
		t.Fatal(err)
	}
	if len(relay.snapshot.PendingElicitations) != 0 {
		t.Fatalf("pending elicitations = %v", relay.snapshot.PendingElicitations)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationsCleared{}); err != nil {
		t.Fatal(err)
	}
	relay = nativeHistoryOpen(t, root, "new")
	if len(relay.snapshot.PendingElicitations) != 0 {
		t.Fatalf("pending elicitations after restart = %v", relay.snapshot.PendingElicitations)
	}
}

// Hard-won: 581ae9e: startup agent text overwrote an existing legacy unused proof.
func TestUpgradePreservesExistingUnusedProofDespiteStartupText(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	nativeHistoryOpenSession(t, relay, false)
	if _, err := relay.RecordSessionUpdate(acpschema.SessionUpdate{
		Kind:              acpschema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: &acpschema.ContentChunk{Content: relayTextBlock("startup greeting")},
	}); err != nil {
		t.Fatal(err)
	}
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}
	nativeHistoryEditSnapshot(t, root, func(saved map[string]any) {
		delete(saved, "native_use_evidence")
		saved["format_version"] = 27
	})
	for range 2 {
		relay := nativeHistoryOpen(t, root, "new")
		if relay.NativeSessionMayHaveHistory() {
			t.Fatal("startup text must not count as native history")
		}
		if evidence := relay.snapshot.NativeUseEvidence; evidence == nil || evidence.Usage != core.NativeSessionUseUnused {
			t.Fatalf("native use evidence = %#v", evidence)
		}
	}
}

func TestUpgradeRecoversUnusedNativeHistoryFromSealedJournalAndKeepsQueuedWork(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	nativeHistoryOpenSession(t, relay, false)
	// Put the opening outside both the snapshot tail and the hot window.
	nativeHistoryWarnings(t, relay, 40)
	if _, err := relay.RecordObservation(core.RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
		Kind:              acpschema.SessionUpdateKindSessionInfoUpdate,
		SessionInfoUpdate: &acpschema.SessionInfoUpdate{},
	}}); err != nil {
		t.Fatal(err)
	}
	submitRelay(relay, "pending-work", prompt("preserve this work"))
	frontierOrdinal, frontierDigest := relay.LatestOrdinal(), relay.LatestDigest()
	commands := relayStagedSnapshot(&relay.snapshot).Dispatches
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}
	nativeHistoryRemoveEvidence(t, root)

	for range 2 {
		relay := nativeHistoryOpen(t, root, "upgraded")
		if relay.NativeSessionMayHaveHistory() {
			t.Fatal("an unused native session must be recovered as unused")
		}
		if opened := relay.snapshot.NativeSessionOpenedOrdinal; opened == nil || *opened != 1 {
			t.Fatalf("native session opened ordinal = %v", opened)
		}
		if relay.LatestOrdinal() != frontierOrdinal || relay.LatestDigest() != frontierDigest {
			t.Fatalf("frontier = %d %s, want %d %s", relay.LatestOrdinal(), relay.LatestDigest(), frontierOrdinal, frontierDigest)
		}
		if !reflect.DeepEqual(relay.snapshot.Dispatches, commands) {
			t.Fatalf("dispatches = %#v, want %#v", relay.snapshot.Dispatches, commands)
		}
		if id := relay.snapshot.NativeSessionID; id == nil || *id != "legacy-thread" {
			t.Fatalf("native session id = %v", id)
		}
	}
}

// Hard-won: 14b4b00de: restart recovery discarded the only evidence that a prompt reached the agent.
func TestRestartPreservesNativeHistoryEvidenceFromAnInFlightPrompt(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	nativeHistoryOpenSession(t, relay, false)
	submitRelay(relay, "sent-work", prompt("possibly delivered"))
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	if len(claimed) != 1 {
		t.Fatalf("claimed %d commands", len(claimed))
	}
	if relay.snapshot.Dispatches["sent-work"].State != core.RelayDispatchStateInFlight {
		t.Fatalf("dispatch state = %s", relay.snapshot.Dispatches["sent-work"].State)
	}
	// Claim-time durable uncertainty already protects the thread here.
	if relay.NativeSessionHistory() != NativeSessionHistoryMayHaveHistory {
		t.Fatal("an in-flight prompt may have reached the native thread")
	}

	// Legacy claims had no durable use marker. Keep the opening and in-flight
	// dispatch so upgrade must recover that evidence before interrupting the
	// old command and compacting its payload.
	nativeHistoryEditSnapshot(t, root, func(saved map[string]any) {
		saved["format_version"] = 27
		delete(saved, "native_session_used")
		delete(saved, "native_use_evidence")
		state := saved["dispatches"].(map[string]any)["sent-work"].(map[string]any)["state"]
		if state != "in_flight" {
			t.Fatalf("saved dispatch state = %v", state)
		}
	})

	// First open interrupts the claim, and the second loads the compacted snapshot.
	for range 2 {
		relay := nativeHistoryOpen(t, root, "restarted")
		if _, ok := relay.snapshot.Dispatches["sent-work"]; ok {
			t.Fatal("restart recovery must terminalize the in-flight prompt")
		}
		if relay.NativeSessionHistory() != NativeSessionHistoryMayHaveHistory {
			t.Fatal("the recovered evidence must keep the thread")
		}
	}
}

func TestUpgradeDoesNotReplaceNativeHistoryAfterAPromptOrAgentContent(t *testing.T) {
	for _, promptWasSent := range []bool{false, true} {
		root := t.TempDir()
		relay := nativeHistoryOpen(t, root, "old")
		nativeHistoryOpenSession(t, relay, false)
		if promptWasSent {
			submitRelay(relay, "sent-work", prompt("already delivered"))
			if _, err := relay.ClaimPendingCommands(true); err != nil {
				t.Fatal(err)
			}
			if _, err := relay.RecordCommandCompleted("sent-work", core.RelayCommandOutcomePrompt{StopReason: "end_turn"}); err != nil {
				t.Fatal(err)
			}
		} else {
			if _, err := relay.RecordObservation(core.RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
				Kind:              acpschema.SessionUpdateKindAgentMessageChunk,
				AgentMessageChunk: &acpschema.ContentChunk{Content: relayTextBlock("existing conversation")},
			}}); err != nil {
				t.Fatal(err)
			}
		}
		if err := relay.persistSnapshot(); err != nil {
			t.Fatal(err)
		}
		nativeHistoryRemoveEvidence(t, root)
		relay = nativeHistoryOpen(t, root, "upgraded")
		if !relay.NativeSessionMayHaveHistory() {
			t.Fatalf("prompt sent %v: the native thread may have history", promptWasSent)
		}
	}
}

func TestUpgradeKeepsResumedOrArchivedNativeHistoryConservative(t *testing.T) {
	for _, resumed := range []bool{false, true} {
		root := t.TempDir()
		relay := nativeHistoryOpen(t, root, "old")
		nativeHistoryOpenSession(t, relay, resumed)
		if !resumed {
			cursor := readyCheckpoint(relay, "checkpoint")
			submitFloor(relay, "archived", cursor)
		}
		if err := relay.persistSnapshot(); err != nil {
			t.Fatal(err)
		}
		nativeHistoryRemoveEvidence(t, root)
		relay = nativeHistoryOpen(t, root, "upgraded")
		if !relay.NativeSessionMayHaveHistory() {
			t.Fatalf("resumed %v: the native thread may have history", resumed)
		}
	}
}

func TestUpgradeKeepsUnreadableNativeHistoryConservativeWithoutBlockingResume(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	nativeHistoryOpenSession(t, relay, false)
	// Startup only needs the latest segments; migration also needs the
	// oldest one. Damage there must not authorize an empty replacement.
	nativeHistoryWarnings(t, relay, 80)
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}
	oldest := relay.journalSpans[0].path
	if filepath.Ext(oldest) != ".gz" {
		t.Fatalf("oldest segment %s is not sealed", oldest)
	}
	nativeHistoryRemoveEvidence(t, root)
	if err := os.WriteFile(oldest, []byte("invalid compressed journal"), 0o600); err != nil {
		t.Fatal(err)
	}
	for range 2 {
		relay := nativeHistoryOpen(t, root, "upgraded")
		if !relay.NativeSessionMayHaveHistory() {
			t.Fatal("unreadable history must keep the native thread")
		}
		if id := relay.snapshot.NativeSessionID; id == nil || *id != "legacy-thread" {
			t.Fatalf("native session id = %v", id)
		}
	}
}

func TestUpgradeRecoversAnEmptySessionOpenedAfterAnArchivedConversation(t *testing.T) {
	root := t.TempDir()
	relay := nativeHistoryOpen(t, root, "old")
	cursor := readyCheckpoint(relay, "checkpoint")
	submitFloor(relay, "archived", cursor)
	nativeHistoryOpenSession(t, relay, false)
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}
	nativeHistoryRemoveEvidence(t, root)
	relay = nativeHistoryOpen(t, root, "upgraded")
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a session opened above the recovery floor is unused")
	}
}
