// Port of mj-worker/src/stand_in_relay.rs.
//
// Stand-in relays for the controller and daemon tests.
//
// A test that needs a worker relay without a worker starts its test binary
// again as the relay: the child runs one exact test that calls a function
// here, which serves the relay protocol on standard input and output from a
// real relay journal. The test binaries of the controller and daemon both
// define those tests, under the names in core's test hooks, so either can
// start the relay. The environment each relay reads is defined there.
//
// Built only with the testhooks tag (Rust's test-hooks feature), as core's
// test_hooks_on.go is.

//go:build testhooks

package worker

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ServeLatchRelay is the relay server half of the checkpoint latch test.
//
// A durable relay only reports a checkpoint barrier ready once a dispatch
// driver claims it, so this also runs the one step the worker runtime
// performs for a barrier. It does nothing unless a parent test points it at
// a relay journal root.
func ServeLatchRelay() {
	panic("unported: mj-worker/src/stand_in_relay.rs serve_latch_relay")
}

// holdNamedRelayCommand: see core.LatchRelayHoldCommand. Only the first
// matching request waits. Rust runs it on Unix only.
func holdNamedRelayCommand(request core.RelayRequest) {
	panic("unported: mj-worker/src/stand_in_relay.rs hold_named_relay_command")
}

func testSubagentQueuePath(relayRoot string) string {
	return filepath.Join(filepath.Dir(relayRoot), core.LatchRelaySession, "subagents.json")
}

func readTestSubagentQueue(path string) map[string]any {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return map[string]any{"requests": map[string]any{}, "results": map[string]any{}}
	}
	if err != nil {
		panic(fmt.Sprintf("read fixture sub-agent queue: %v", err))
	}
	queue, err := subagentMCPDecodeValue(data)
	object, ok := queue.(map[string]any)
	if err != nil || !ok {
		panic(fmt.Sprintf("parse fixture sub-agent queue: %v", err))
	}
	return object
}

func recordTestSubagentAdmission(relayRoot string, open bool) {
	queuePath := testSubagentQueuePath(relayRoot)
	queue := readTestSubagentQueue(queuePath)
	if transitions, ok := queue["admission_transitions"].([]any); ok {
		queue["admission_transitions"] = append(transitions, open)
	} else {
		queue["admission_transitions"] = []any{open}
	}
	queue["mutating_admission_open"] = open
	body, err := json.Marshal(queue)
	if err != nil {
		panic(err)
	}
	if err := core.AtomicWrite(queuePath, body); err != nil {
		panic(fmt.Sprintf("persist fake worker sub-agent admission: %v", err))
	}
}

// projectMemoryRefusal: a project-memory connection runs beside the
// session's own, which owns the control journal. A worker serves memory at
// the connection layer, without the journal; these stand-ins have no memory
// endpoint, so they answer as DurableRelay does without opening a second
// journal writer. Opening one is not read-only: it saves the snapshot and
// settles interrupted work. Nil for any other request.
func projectMemoryRefusal(request *core.RelayRequestEnvelope) *core.RelayResponseEnvelope {
	switch request.Request.(type) {
	case core.RelayRequestProjectMemorySnapshot, core.RelayRequestProjectMemoryTransfer,
		core.RelayRequestInstallProjectMemorySnapshot, core.RelayRequestReplaceProjectMemoryTree:
	default:
		return nil
	}
	return &core.RelayResponseEnvelope{
		RequestID:       request.RequestID,
		ProtocolVersion: request.ProtocolVersion,
		Body:            core.RelayError(core.RelayErrorCodeInvalidState, "connection-only requests must be handled by the live relay transport", false, nil),
	}
}

// subagentConnectionResponse answers the sub-agent queue requests from the
// fixture queue beside the relay root; nil for any other request.
func subagentConnectionResponse(request *core.RelayRequestEnvelope) *core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/stand_in_relay.rs subagent_connection_response")
}

func requestsCheckpointRelease(request *core.RelayRequestEnvelope) bool {
	submit, ok := request.Request.(core.RelayRequestSubmit)
	if !ok {
		return false
	}
	_, ok = submit.Command.(core.RelayCommandReleaseCheckpoint)
	return ok
}

// unparseableRequestResponse is the answer a worker gives for a frame its
// own protocol cannot decode. An older RelayCommand has no
// release_checkpoint variant, and the enum denies unknown ones, so the
// request never reaches its relay.
func unparseableRequestResponse(request *core.RelayRequestEnvelope) core.RelayResponseEnvelope {
	return core.RelayResponseEnvelope{
		RequestID:       request.RequestID,
		ProtocolVersion: request.ProtocolVersion,
		Body: core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInvalidRequest,
			Message:   "unknown variant `release_checkpoint`",
			Retryable: false,
		}},
	}
}

// ServeLeasedRelay is the relay server half of the leased-submission tests.
// It does nothing unless a parent test points it at a relay journal root.
func ServeLeasedRelay() {
	panic("unported: mj-worker/src/stand_in_relay.rs serve_leased_relay")
}

func policyTestOptions() []acpschema.SessionConfigOption {
	panic("unported: mj-worker/src/stand_in_relay.rs policy_test_options")
}

// SeedSourceRelay is the source relay journal an in-place move starts from.
func SeedSourceRelay(workerRoot string) string {
	panic("unported: mj-worker/src/stand_in_relay.rs seed_source_relay")
}

// KeepQuestionLive keeps a stand-in harness's real responder registration
// alive across relay requests. The result holds it (Rust: Box<dyn Send>).
func KeepQuestionLive(relay *DurableRelay, question core.ElicitationRequest) any {
	return relay.operatorAway.keepFixtureQuestionLive(question)
}
