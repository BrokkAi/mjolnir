// Port of the tests in mj-core/src/relay/snapshot/apply.rs.

package core

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

// applyTestObservation records observation as the snapshot's next v2 event.
func applyTestObservation(t *testing.T, snapshot *RelaySnapshot, observation RelayObservation) {
	t.Helper()
	event := RelayEvent{
		Format:       RelayEventFormatV2,
		Ordinal:      snapshot.LatestOrdinal + 1,
		RecordedAtMs: 1,
		Observation:  observation,
	}
	digest, err := RelayEventDigest(&event)
	if err != nil {
		t.Fatal(err)
	}
	event.Digest = digest
	if err := ApplyRelayEvent(snapshot, &event); err != nil {
		t.Fatal(err)
	}
}

func roundTripRelaySnapshot(t *testing.T, value any) RelaySnapshot {
	t.Helper()
	encoded, err := relayMarshalJSON(value)
	if err != nil {
		t.Fatal(err)
	}
	var restored RelaySnapshot
	if err := json.Unmarshal(encoded, &restored); err != nil {
		t.Fatal(err)
	}
	return restored
}

// relaySnapshotObject is a snapshot's JSON object, for removing fields as an
// older writer would have left them out.
func relaySnapshotObject(t *testing.T, snapshot RelaySnapshot) map[string]json.RawMessage {
	t.Helper()
	encoded, err := relayMarshalJSON(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &object); err != nil {
		t.Fatal(err)
	}
	return object
}

func TestAutomaticFormResolutionPreservesRequestButPersonAnswerClearsIt(t *testing.T) {
	snapshot := NewRelaySnapshot("forms")
	snapshot.Request = &OpenRequest{Question: InferredQuestion{TurnID: "original", CompletedOrdinal: 1}}
	reply := "Use the old format"
	for _, reply := range []*string{nil, &reply} {
		human := reply != nil
		applyTestObservation(t, &snapshot, RelayObservationElicitationResolved{
			ElicitationID: "form",
			Action:        "accept",
			Reply:         reply,
		})
		if (snapshot.OpenRequest() == nil) != human {
			t.Fatalf("open request after reply %v: %#v", reply, snapshot.OpenRequest())
		}
	}
}

func TestLegacySnapshotWithoutRequestAndRetainedRequestRoundTrip(t *testing.T) {
	snapshot := NewRelaySnapshot("compatibility")
	legacy := relaySnapshotObject(t, snapshot)
	delete(legacy, "request")
	delete(legacy, "managed_subagent")
	delete(legacy, "person_input_through")
	restored := roundTripRelaySnapshot(t, legacy)
	if restored.OpenRequest() != nil {
		t.Fatal("a legacy snapshot has no open request")
	}
	question := "agent:question"
	snapshot.Request = &OpenRequest{
		Question:        InferredQuestion{TurnID: "earlier-turn", CompletedOrdinal: 7},
		QuestionMessage: &question,
	}
	restored = roundTripRelaySnapshot(t, snapshot)
	if !reflect.DeepEqual(restored.OpenRequest(), snapshot.OpenRequest()) {
		t.Fatalf("restored %#v, want %#v", restored.OpenRequest(), snapshot.OpenRequest())
	}
}

func testLiveness(completedOrdinal uint64, alive uint32) *WorkLiveness {
	return &WorkLiveness{CompletedOrdinal: completedOrdinal, Alive: alive, CheckedAtMs: 9}
}

func TestARecountForAnOlderTurnIsDroppedAndASnapshotRoundTrips(t *testing.T) {
	snapshot := NewRelaySnapshot("liveness")
	snapshot.WorkLiveness = testLiveness(7, 3)
	applyTestObservation(t, &snapshot, RelayObservationWorkLivenessChanged{Liveness: *testLiveness(5, 0)})
	if !reflect.DeepEqual(snapshot.WorkLiveness, testLiveness(7, 3)) {
		t.Fatalf("liveness = %#v", snapshot.WorkLiveness)
	}
	applyTestObservation(t, &snapshot, RelayObservationWorkLivenessChanged{Liveness: *testLiveness(7, 1)})
	if !reflect.DeepEqual(snapshot.WorkLiveness, testLiveness(7, 1)) {
		t.Fatalf("liveness = %#v", snapshot.WorkLiveness)
	}
	restored := roundTripRelaySnapshot(t, snapshot)
	if !reflect.DeepEqual(restored.WorkLiveness, snapshot.WorkLiveness) {
		t.Fatalf("restored liveness = %#v", restored.WorkLiveness)
	}

	// A snapshot written before protocol 67 has no field and no liveness.
	legacy := relaySnapshotObject(t, NewRelaySnapshot("old"))
	if _, present := legacy["work_liveness"]; present {
		t.Fatal("an unset liveness was written")
	}
	if restored := roundTripRelaySnapshot(t, legacy); restored.WorkLiveness != nil {
		t.Fatalf("legacy liveness = %#v", restored.WorkLiveness)
	}
}

func TestHoldFollowUpReplaysAsAGuardedPromptAndClearsLivenessWhenItStarts(t *testing.T) {
	snapshot := NewRelaySnapshot("follow-up")
	snapshot.WorkLiveness = testLiveness(7, 2)
	command := RelayCommandHoldFollowUp{
		CompletedCommandID: "handback-reminder-6",
		CompletedOrdinal:   7,
		Kind:               HoldFollowUpKindGiveUp,
		Running:            []string{"pid 4 sleep 1000"},
	}
	// The journal stores the command with its kind in snake_case.
	encoded, err := relayMarshalJSON(command)
	if err != nil {
		t.Fatal(err)
	}
	var wire struct {
		Data struct {
			Kind string `json:"kind"`
		} `json:"data"`
	}
	if err := json.Unmarshal(encoded, &wire); err != nil || wire.Data.Kind != "give_up" {
		t.Fatalf("kind = %q (%v)", wire.Data.Kind, err)
	}
	decoded, err := UnmarshalRelayCommandJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, RelayCommand(command)) {
		t.Fatalf("decoded %#v", decoded)
	}
	if RelayCommandMinimumProtocol(command) != RelayWorkLivenessProtocol {
		t.Fatalf("minimum protocol = %d", RelayCommandMinimumProtocol(command))
	}
	blocks, ok := RelayCommandPromptBlocks(command)
	if !ok {
		t.Fatal("a hold follow-up is a prompt")
	}
	text, err := relayMarshalJSON(blocks[0])
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(text), "[background wait ended] You have waited 30 minutes") {
		t.Fatal(string(text))
	}
	if !strings.Contains(string(text), "- pid 4 sleep 1000") {
		t.Fatal(string(text))
	}
	applyTestObservation(t, &snapshot, RelayObservationCommandQueued{
		CommandID:   "hold-give-up-7",
		Command:     command,
		CreatedAtMs: 1,
	})
	if snapshot.LatestPromptAcceptedOrdinal == nil || *snapshot.LatestPromptAcceptedOrdinal != snapshot.LatestOrdinal {
		t.Fatalf("latest prompt accepted = %v", snapshot.LatestPromptAcceptedOrdinal)
	}
	// Queued is not running: a follow-up the queue drops leaves the child
	// held. Only its start supersedes the turn the liveness describes.
	if !reflect.DeepEqual(snapshot.WorkLiveness, testLiveness(7, 2)) {
		t.Fatalf("liveness = %#v", snapshot.WorkLiveness)
	}
	applyTestObservation(t, &snapshot, RelayObservationCommandStarted{CommandID: "hold-give-up-7", StartedAtMs: 2})
	if snapshot.WorkLiveness != nil {
		t.Fatalf("liveness = %#v", snapshot.WorkLiveness)
	}
}

// Protocol-66 workers decode RelayCommand refusing unknown fields, so a
// reminder carrying running is rejected by them. The daemon therefore fills
// running only for workers at protocol 67 or later, which the minimum
// protocol enforces.
func TestAReminderWithProcessesNeedsProtocol67AndWithoutThemStaysOld(t *testing.T) {
	plain := RelayCommandHandbackReminder{CompletedCommandID: "t", CompletedOrdinal: 1}
	if RelayCommandMinimumProtocol(plain) != 26 {
		t.Fatalf("minimum protocol = %d", RelayCommandMinimumProtocol(plain))
	}
	if encoded, _ := relayMarshalJSON(plain); strings.Contains(string(encoded), "running") {
		t.Fatal(string(encoded))
	}
	named := RelayCommandHandbackReminder{CompletedCommandID: "t", CompletedOrdinal: 1, Running: []string{"pid 1 sleep"}}
	if RelayCommandMinimumProtocol(named) != RelayWorkLivenessProtocol {
		t.Fatalf("minimum protocol = %d", RelayCommandMinimumProtocol(named))
	}
}
