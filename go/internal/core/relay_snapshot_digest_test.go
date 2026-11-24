// Port of the tests in mj-core/src/relay/snapshot/digest.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"
)

// oldObservation preserves the exact historical wire names independently of
// RelayObservation: each value is one variant's name and its fields.
type oldObservation struct {
	name   string
	fields any
}

func (o oldObservation) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", o.name, o.fields)
}

// Hard-won: #1450 review: absent settlement metadata must preserve old digests.
func TestPreClassificationJournalsKeepTheirOriginalDigestInBothFormats(t *testing.T) {
	type agentInitialized struct {
		ProtocolVersion acpschema.ProtocolVersion   `json:"protocol_version"`
		Capabilities    acpschema.AgentCapabilities `json:"capabilities"`
		AgentInfo       *acpschema.Implementation   `json:"agent_info"`
	}
	type harnessTurnSettled struct {
		Origin         *string `json:"origin"`
		PromptInFlight bool    `json:"prompt_in_flight"`
	}
	type terminated struct {
		CommandID string           `json:"command_id"`
		Command   RelayCommandKind `json:"command"`
		Message   string           `json:"message"`
	}
	type commandCompleted struct {
		CommandID string              `json:"command_id"`
		Outcome   RelayCommandOutcome `json:"outcome"`
	}
	type elicitationResolved struct {
		ElicitationID string `json:"elicitation_id"`
		Action        string `json:"action"`
	}
	for _, format := range []uint8{RelayEventFormatV1, RelayEventFormatV2} {
		for _, old := range []oldObservation{
			{"agent_initialized", agentInitialized{ProtocolVersion: 1}},
			{"harness_turn_settled", harnessTurnSettled{}},
			{"command_interrupted", terminated{"opaque", RelayCommandKindBeginCheckpoint, "old cleanup"}},
			{"command_rejected", terminated{"opaque", RelayCommandKindPrompt, "old failure"}},
			{"command_completed", commandCompleted{"opaque", RelayCommandOutcomeCancelled{}}},
			{"elicitation_resolved", elicitationResolved{"form-1", "accept"}},
		} {
			raw, err := relayMarshalJSON(old)
			if err != nil {
				t.Fatal(err)
			}
			observation, err := UnmarshalRelayObservationJSON(raw)
			if err != nil {
				t.Fatalf("%s: %v", raw, err)
			}
			commandID := "opaque"
			event := RelayEvent{
				Format:       format,
				Ordinal:      1,
				RecordedAtMs: 123,
				CommandID:    &commandID,
				Observation:  observation,
			}
			if format == RelayEventFormatV1 {
				event.PreviousDigest = RelayEventGenesisDigest
			}
			// Hard-won: ef6c07081: a new ACP capability changed protocol-71 event digests.
			if initialized, ok := event.Observation.(RelayObservationAgentInitialized); ok {
				if initialized.Capabilities.SessionCapabilities == nil {
					initialized.Capabilities.SessionCapabilities = &acpschema.SessionCapabilities{}
				} else {
					session := *initialized.Capabilities.SessionCapabilities
					initialized.Capabilities.SessionCapabilities = &session
				}
				initialized.Capabilities.SessionCapabilities.SystemPrompt = &acpschema.SessionSystemPromptCapabilities{}
				event.Observation = initialized
			}
			if event.Digest, err = relayEventDigestOver(&event, old); err != nil {
				t.Fatal(err)
			}
			if encoded, err := relayMarshalJSON(event.Observation); err != nil || string(encoded) != string(raw) {
				t.Fatalf("observation encoding changed: %s, want %s (%v)", encoded, raw, err)
			}
			if err := ValidateRelayEventSelf(&event); err != nil {
				t.Fatal(err)
			}
			if interrupted, ok := event.Observation.(RelayObservationCommandInterrupted); ok {
				reason := OutcomeReasonControllerDisconnected
				interrupted.Reason = &reason
				event.Observation = interrupted
				if ValidateRelayEventSelf(&event) == nil {
					t.Fatal("classification must be covered by the digest")
				}
				interrupted.Reason = nil
				continuation := RestartContinuationID("opaque")
				interrupted.ContinuationID = &continuation
				event.Observation = interrupted
				if ValidateRelayEventSelf(&event) == nil {
					t.Fatal("the recovery transfer must be covered by the digest")
				}
			}
		}
	}
}

// Hard-won: 4a5cfbd03: a prompt holding U+2028 left the Go worker's session
// unreachable ("relay event 8 digest is invalid"): encoding/json escapes
// U+2028 and U+2029, serde_json writes them raw, and the Rust daemon
// recomputes every event's digest from serde's bytes. The frames were
// recorded from the Rust worker at 3dde0c11b: a submitted prompt holding line
// and paragraph separators, markup, quotes, control characters and non-ASCII
// text, then an attach that returns its events. Go must compute the digest
// the Rust worker wrote for each event.
func TestRelayEventDigestsMatchARecordedRustWorker(t *testing.T) {
	recorded, err := os.ReadFile(filepath.Join("testdata", "relay_snapshot_digest", "rust_worker_unicode_prompt_frames.txt"))
	if err != nil {
		t.Fatal(err)
	}
	frames := strings.Split(strings.TrimSuffix(string(recorded), "\n"), "\n")
	if len(frames) != 4 {
		t.Fatalf("the recording holds %d frames, not a submit and an attach exchange", len(frames))
	}
	var request RelayRequestEnvelope
	if err := json.Unmarshal([]byte(frames[2]), &request); err != nil {
		t.Fatal(err)
	}
	attach, ok := request.Request.(RelayRequestAttach)
	if !ok {
		t.Fatalf("the third frame is %#v, not an attach request", request.Request)
	}
	var response RelayResponseEnvelope
	if err := json.Unmarshal([]byte(frames[3]), &response); err != nil {
		t.Fatal(err)
	}
	body, ok := response.Body.(RelayResponseBodyOk)
	if !ok {
		t.Fatalf("attach answered %#v", response.Body)
	}
	attached, ok := body.Payload.(RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("attach answered %#v", body.Payload)
	}
	if len(attached.Events) == 0 {
		t.Fatal("the recorded attach returned no events")
	}
	if !strings.Contains(frames[3], "\u2028") || !strings.Contains(frames[3], "\u2029") {
		t.Fatal("the recorded events do not hold the raw separators the recording was made for")
	}
	previousOrdinal, previousDigest := attach.AfterOrdinal, attach.AfterDigest
	for i := range attached.Events {
		event := &attached.Events[i]
		if err := ValidateRelayEvent(previousOrdinal, previousDigest, event); err != nil {
			t.Errorf("event %d: %v", event.Ordinal, err)
		}
		previousOrdinal, previousDigest = event.Ordinal, event.Digest
	}
	if previousOrdinal != attached.ThroughOrdinal || previousDigest != attached.ThroughDigest {
		t.Fatalf("events end at %d %s, the attach at %d %s", previousOrdinal, previousDigest,
			attached.ThroughOrdinal, attached.ThroughDigest)
	}
}
