// Port of the test_support module of mj-worker/src/relay.rs, and of its
// other cfg(test) items.
//
// Like the Rust helpers, these panic on an unexpected result instead of
// taking a *testing.T, so callers pass the same arguments as in Rust.

package worker

import (
	"bytes"
	"encoding/json"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const session = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// glideAnswer is a recorded GLiDE answer whose frozen rule gives expected
// (await_input, finished, wait, retry_provider, recover_quota or
// uncertain). background replaces the leftover-process distribution
// [needed, unneeded, unclear] when a test needs a particular quiet judgment.
func glideAnswer(expected string, background *[3]float64) core.Probabilities {
	encoded, err := jsonvalue.Marshal(testSamples())
	if err != nil {
		panic(err)
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	var samples []map[string]any
	if err := decoder.Decode(&samples); err != nil {
		panic(err)
	}
	index := slices.IndexFunc(samples, func(sample map[string]any) bool { return sample["expected"] == expected })
	if index < 0 {
		panic(fmt.Sprintf("no recorded GLiDE answer for %s", expected))
	}
	answer := samples[index]
	if background != nil {
		answers := answer["answers"].(map[string]any)
		axis, _ := answers["background"].(map[string]any)
		if axis == nil {
			axis = map[string]any{}
			answers["background"] = axis
		}
		axis["probabilities"] = map[string]any{
			"needed":   background[0],
			"unneeded": background[1],
			"unclear":  background[2],
		}
	}
	rewritten, err := jsonvalue.Marshal(answer)
	if err != nil {
		panic(err)
	}
	value, err := jsonvalue.Parse(rewritten)
	if err != nil {
		panic(err)
	}
	probabilities, err := core.ProbabilitiesParse(value)
	if err != nil {
		panic(err)
	}
	return probabilities
}

// relayTempDir is t.TempDir for the root of a relay that is given a verdict
// harness. Such a relay writes Jev diagnostics from the decision log's own
// goroutine (Rust's writer thread), so the cleanup first drains that writer:
// the directory is removed only after the writes queued during the test have
// landed.
func relayTempDir(t *testing.T) string {
	root := t.TempDir()
	t.Cleanup(func() {
		// The read is ordered after every queued write; its answer is unused.
		_, _ = core.JevRead(filepath.Join(root, "jev-decisions"), session, nil)
	})
	return root
}

func relayRequest(requestID string, request core.RelayRequest) core.RelayRequestEnvelope {
	return core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	}
}

func submitRelay(relay *DurableRelay, commandID string, command core.RelayCommand) uint64 {
	response := relay.Handle(relayRequest("request-"+commandID, core.RelayRequestSubmit{CommandID: commandID, Command: command}))
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	accepted, isAccepted := ok.Payload.(core.RelayResponsePayloadAccepted)
	if !isOk || !isAccepted {
		panic(fmt.Sprintf("expected accepted relay command, got %#v", response.Body))
	}
	if accepted.CommandID != commandID {
		panic(fmt.Sprintf("accepted command %q, expected %q", accepted.CommandID, commandID))
	}
	return accepted.Ordinal
}

func relayTestDigestAt(relay *DurableRelay, ordinal uint64) string {
	digest, ok, err := relay.digestAt(ordinal)
	if err != nil || !ok {
		panic(fmt.Sprintf("no digest at ordinal %d: %v", ordinal, err))
	}
	return digest
}

func attachRelay(relay *DurableRelay, requestID string, afterOrdinal uint64) core.RelayResponseEnvelope {
	afterDigest := relayTestDigestAt(relay, afterOrdinal)
	return relay.Handle(relayRequest(requestID, core.RelayRequestAttach{AfterOrdinal: afterOrdinal, AfterDigest: afterDigest}))
}

func acknowledgeRelay(relay *DurableRelay, requestID string, throughOrdinal uint64) core.RelayResponseEnvelope {
	throughDigest := relayTestDigestAt(relay, throughOrdinal)
	return relay.Handle(relayRequest(requestID, core.RelayRequestAcknowledge{ThroughOrdinal: throughOrdinal, ThroughDigest: throughDigest}))
}

func prompt(text string) core.RelayCommand {
	block := acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
	return core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{block}}
}

func setConfig(key, value string) core.RelayCommand {
	return core.RelayCommandSetConfig{Key: key, Value: value}
}

func queuedCommandIDs(relay *DurableRelay) []string {
	var ids []string
	for _, queued := range relay.OperationalState().QueuedPrompts {
		ids = append(ids, queued.CommandID)
	}
	return ids
}

func finishPrompt(relay *DurableRelay, commandID string) {
	if _, err := relay.RecordCommandCompleted(commandID, core.RelayCommandOutcomePrompt{StopReason: "end_turn"}); err != nil {
		panic(err)
	}
}

func retainedEvents(relay *DurableRelay) []core.RelayEvent {
	events, err := relay.EventsAfter(relay.snapshot.RetainedThrough(), relay.snapshot.RetainedDigest())
	if err != nil {
		panic(err)
	}
	return events
}

func submitRelease(relay *DurableRelay, commandID, barrierCommandID string) core.RelayResponseEnvelope {
	return relay.Handle(relayRequest("request-"+commandID, core.RelayRequestSubmit{
		CommandID: commandID,
		Command:   core.RelayCommandReleaseCheckpoint{BarrierCommandID: barrierCommandID},
	}))
}

func submitFloor(relay *DurableRelay, commandID string, through core.RelayCursor) core.RelayResponseEnvelope {
	return relay.Handle(relayRequest("request-"+commandID, core.RelayRequestSubmit{
		CommandID: commandID,
		Command:   core.RelayCommandAdvanceRecoveryFloor{Through: through},
	}))
}

func readyCheckpoint(relay *DurableRelay, commandID string) core.RelayCursor {
	submitRelay(relay, commandID, core.RelayCommandBeginCheckpoint{})
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		panic(err)
	}
	if len(claimed) != 1 || claimed[0].CommandID != commandID {
		panic(fmt.Sprintf("expected only %s claimed, got %#v", commandID, claimed))
	}
	if _, err := relay.RecordCheckpointReady(commandID); err != nil {
		panic(err)
	}
	return *relay.OperationalState().CheckpointReady
}

// liveBaselineWaitMessage is the wait notice a live baseline would publish
// now, if anything blocks it.
func (r *DurableRelay) liveBaselineWaitMessage() (string, bool) {
	blockers := r.adoptionBlockers()
	if len(blockers) == 0 {
		return "", false
	}
	return "waiting for " + strings.Join(blockers, "; "), true
}
