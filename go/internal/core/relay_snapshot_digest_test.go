// Port of the tests in mj-core/src/relay/snapshot/digest.rs.

package core

import (
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
