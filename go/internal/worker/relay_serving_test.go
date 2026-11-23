// Port of the tests in mj-worker/src/relay/serving.rs.

package worker

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// servingRefusalCode is the refusal a response carries, if any.
func servingRefusalCode(body core.RelayResponseBody) (core.RelayProtocolError, bool) {
	refusal, ok := body.(core.RelayResponseBodyError)
	return refusal.Error, ok
}

func TestRelayHasAHardProtocolV1Floor(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "hello-old",
		ProtocolVersion: 0,
		Request: core.RelayRequestHello{
			ControllerVersion: "old",
			Supported:         core.RelayVersionRange{Min: 0, Max: 0},
		},
	})
	refusal, ok := servingRefusalCode(response.Body)
	if !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol || refusal.Retryable {
		t.Fatalf("hello below protocol 1 must be refused, got %#v", response.Body)
	}
}

func TestHelloReportsTheWorkerInputCompatibilityIdentity(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "2.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "hello-current",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestHello{
			ControllerVersion: "2.0.0",
			Supported:         core.RelayVersionRangeCurrent,
		},
	})
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	hello, isHello := ok.Payload.(core.RelayResponsePayloadHello)
	if !isOk || !isHello {
		t.Fatalf("current worker must answer hello, got %#v", response.Body)
	}
	if hello.WorkerBuild == nil || *hello.WorkerBuild != core.WorkerBuildID() {
		t.Fatalf("hello worker build = %v, want %q", hello.WorkerBuild, core.WorkerBuildID())
	}
}

// Hard-won: 0a278358: protocol 2 only prevented a new controller from attaching to a live v1 worker.
func TestCurrentRangeOverlapsProtocolV1(t *testing.T) {
	v1 := core.RelayVersionRange{Min: 1, Max: 1}
	v2 := core.RelayVersionRange{Min: 2, Max: 2}
	expectNegotiated := func(left, right core.RelayVersionRange, want uint32, wantOK bool) {
		t.Helper()
		got, ok := left.Negotiate(right)
		if ok != wantOK || (ok && got != want) {
			t.Fatalf("%v.Negotiate(%v) = %d, %v; want %d, %v", left, right, got, ok, want, wantOK)
		}
	}
	expectNegotiated(core.RelayVersionRangeCurrent, v1, 1, true)
	expectNegotiated(v1, core.RelayVersionRangeCurrent, 1, true)
	expectNegotiated(core.RelayVersionRangeCurrent, core.RelayVersionRangeCurrent, core.RelayProtocolVersion, true)
	expectNegotiated(v1, v2, 0, false)
	for _, version := range []uint32{1, 2, 3, 4} {
		if !core.RelayVersionRangeCurrent.Contains(version) {
			t.Fatalf("current range must contain %d", version)
		}
	}
	if core.RelayVersionRangeCurrent.Contains(0) || core.RelayVersionRangeCurrent.Contains(core.RelayProtocolVersion+1) {
		t.Fatal("current range must not contain 0 or a future protocol")
	}
	if !core.RelayRequestSupportedAt(core.RelayRequestStatus{}, 1) {
		t.Fatal("status is supported at protocol 1")
	}
	if got := core.RelayCommandMinimumProtocol(core.RelayCommandCancel{}); got != 1 {
		t.Fatalf("cancel minimum protocol = %d", got)
	}
	if core.RelayRequestSupportedAt(core.RelayRequestRespondElicitation{
		Response: core.ElicitationResponse{Action: core.ElicitationResponseCancel},
	}, 1) {
		t.Fatal("respond_elicitation must not be supported at protocol 1")
	}
	expectMinimum := func(request core.RelayRequest, minimum uint32) {
		t.Helper()
		if got := core.RelayRequestMinimumProtocol(request); got != minimum {
			t.Fatalf("%T minimum protocol = %d, want %d", request, got, minimum)
		}
		if core.RelayRequestSupportedAt(request, minimum-1) {
			t.Fatalf("%T must not be supported at %d", request, minimum-1)
		}
	}
	cancelTurn := core.RelayRequestSubmit{CommandID: "cancel-turn", Command: core.RelayCommandCancelTurn{}}
	expectMinimum(cancelTurn, 7)
	if !core.RelayRequestSupportedAt(cancelTurn, core.RelayProtocolVersion) {
		t.Fatal("cancel_turn is supported at the current protocol")
	}
	stopBackground := core.RelayRequestStopBackgroundTask{BackgroundTaskID: "terminal:task-1"}
	expectMinimum(stopBackground, 9)
	if !core.RelayRequestSupportedAt(stopBackground, core.RelayProtocolVersion) {
		t.Fatal("stop_background_task is supported at the current protocol")
	}

	drainMailbox := core.RelayRequestDrainMailbox{HookEvent: "PostToolUse"}
	expectMinimum(drainMailbox, 33)
	if !core.RelayRequestSupportedAt(drainMailbox, 33) {
		t.Fatal("drain_mailbox is supported at 33")
	}
	ackMailbox := core.RelayRequestAckMailbox{LeaseID: "mailbox-hook-lease"}
	expectMinimum(ackMailbox, 33)
	if !core.RelayRequestSupportedAt(ackMailbox, 33) {
		t.Fatal("ack_mailbox is supported at 33")
	}
	mailboxEvent := core.RelayRequestSubmit{
		CommandID: "event",
		Command: core.RelayCommandDeliverMailboxEvent{Event: core.MailboxEvent{
			Key:         "parent:message:1",
			Source:      "parent",
			Wake:        true,
			CreatedAtMs: 1,
			Body:        core.MailboxEventBodyParentMessage{Text: "message"},
		}},
	}
	expectMinimum(mailboxEvent, 34)
	if !core.RelayRequestSupportedAt(mailboxEvent, 34) {
		t.Fatal("parent mailbox event is supported at 34")
	}
	sessionMessage := core.RelayRequestSubmit{
		CommandID: "session-message",
		Command: core.RelayCommandDeliverMailboxEvent{Event: core.MailboxEvent{
			Key:         "session-message:1",
			Source:      "session_message",
			Wake:        true,
			CreatedAtMs: 1,
			Body: core.MailboxEventBodySessionMessage{
				From: core.Sender{Type: core.SenderSession, ID: "sender", Title: "Sender"},
				Text: "message",
			},
		}},
	}
	expectMinimum(sessionMessage, 36)
	if !core.RelayRequestSupportedAt(sessionMessage, 36) {
		t.Fatal("session mailbox message is supported at 36")
	}
}

func TestProjectMemoryReplaceIsGatedAndUsesTheRelayWireShape(t *testing.T) {
	request := core.RelayRequestReplaceProjectMemoryTree{
		ExpectedReplica: core.TreeVersion("tree-v1"),
		Tree:            core.ProjectMemoryWireTreePlain{Snapshot: core.ProjectMemorySnapshot{Files: map[string]string{}}},
	}
	if got := core.RelayRequestMinimumProtocol(request); got != core.RelayProjectMemoryReplaceProtocol {
		t.Fatalf("minimum protocol = %d", got)
	}
	if core.RelayRequestSupportedAt(request, core.RelayProjectMemoryReplaceProtocol-1) {
		t.Fatal("replace must not be supported before its protocol")
	}
	if !core.RelayRequestSupportedAt(request, core.RelayProtocolVersion) {
		t.Fatal("replace is supported at the current protocol")
	}

	encoded, err := json.Marshal(request)
	if err != nil {
		t.Fatal(err)
	}
	var requestJSON map[string]any
	if err := json.Unmarshal(encoded, &requestJSON); err != nil {
		t.Fatal(err)
	}
	if requestJSON["method"] != "replace_project_memory_tree" {
		t.Fatalf("method = %v", requestJSON["method"])
	}
	if params := requestJSON["params"].(map[string]any); params["expected_replica"] != "tree-v1" {
		t.Fatalf("expected_replica = %v", params["expected_replica"])
	}
	decoded, err := core.UnmarshalRelayRequestJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, core.RelayRequest(request)) {
		t.Fatalf("request round trip = %#v, want %#v", decoded, request)
	}

	response := core.RelayResponsePayloadProjectMemoryTreeReplaced{Outcome: core.ReplicaReplaceOutcomeReplicaChanged}
	encoded, err = json.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	var responseJSON map[string]any
	if err := json.Unmarshal(encoded, &responseJSON); err != nil {
		t.Fatal(err)
	}
	if responseJSON["type"] != "project_memory_tree_replaced" {
		t.Fatalf("type = %v", responseJSON["type"])
	}
	if data := responseJSON["data"].(map[string]any); data["outcome"] != "replica_changed" {
		t.Fatalf("outcome = %v", data["outcome"])
	}
	decodedPayload, err := core.UnmarshalRelayResponsePayloadJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decodedPayload, core.RelayResponsePayload(response)) {
		t.Fatalf("response round trip = %#v, want %#v", decodedPayload, response)
	}
}

func TestHelloRefusesReadersThatCannotPreserveProviderDetails(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "hello-v1",
		ProtocolVersion: 1,
		Request: core.RelayRequestHello{
			ControllerVersion: "old",
			Supported:         core.RelayVersionRange{Min: 1, Max: 1},
		},
	})
	if response.ProtocolVersion != 1 {
		t.Fatalf("protocol version = %d", response.ProtocolVersion)
	}
	if refusal, ok := servingRefusalCode(response.Body); !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("a v1 reader must be refused, got %#v", response.Body)
	}
}

// A worker built before the field existed answers hello without it. That
// hello must still parse, reporting no build.
func TestAHelloWithoutAWorkerBuildParses(t *testing.T) {
	payload, err := core.UnmarshalRelayResponsePayloadJSON([]byte(`{
		"type": "hello",
		"data": {"negotiated": 1, "relay_version": "2.0.0", "session_id": "` + session + `"}
	}`))
	if err != nil {
		t.Fatalf("an old worker's hello must still parse: %v", err)
	}
	hello, ok := payload.(core.RelayResponsePayloadHello)
	if !ok || hello.WorkerBuild != nil {
		t.Fatalf("hello = %#v", payload)
	}
}

// A worker serves only the current protocol. An older controller is refused
// even for a request every protocol has had.
func TestARequestFromTheCheckpointMailboxProtocolIsRefused(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "status-old",
		ProtocolVersion: core.RelayCheckpointMailboxProtocol,
		Request:         core.RelayRequestStatus{},
	})
	if refusal, ok := servingRefusalCode(response.Body); !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("an older protocol must be refused, got %#v", response.Body)
	}
}

func TestProtocolV1CannotRespondToElicitationOnTheDurableRelay(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "elicit-v1",
		ProtocolVersion: 1,
		Request: core.RelayRequestRespondElicitation{
			ElicitationID: "form-1",
			Response:      core.ElicitationResponse{Action: core.ElicitationResponseCancel},
		},
	})
	refusal, ok := servingRefusalCode(response.Body)
	if !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol || refusal.Retryable {
		t.Fatalf("protocol 1 must be refused, got %#v", response.Body)
	}
}

func TestGoalControlRequiresProtocolElevenAndOldReadersCannotAttach(t *testing.T) {
	relay, err := DurableRelayOpen(t.TempDir(), session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "old-goal-control",
		ProtocolVersion: 10,
		Request: core.RelayRequestSubmit{
			CommandID: "clear",
			Command:   core.RelayCommandGoalControl{Action: core.GoalControlActionClear},
		},
	})
	if refusal, ok := servingRefusalCode(response.Body); !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("protocol 10 goal control must be refused, got %#v", response.Body)
	}
	response = relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "old-reader",
		ProtocolVersion: 10,
		Request: core.RelayRequestHello{
			ControllerVersion: "old",
			Supported:         core.RelayVersionRange{Min: 1, Max: 10},
		},
	})
	if refusal, ok := servingRefusalCode(response.Body); !ok || refusal.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("a protocol 10 reader must be refused, got %#v", response.Body)
	}
}
