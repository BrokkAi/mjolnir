// Port of mj-worker-bin/tests/managed_child_questions.rs: a managed child's
// questions go to its parent; its approvals stay with a person.

//go:build unix

package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// controlRequest queries or answers through the running worker's control
// socket, like a daemon; nil when the worker gives no answer.
func controlRequest(root string, request core.RelayRequest, requestID string) *core.RelayResponseEnvelope {
	connection, err := net.Dial("unix", filepath.Join(root, "control.sock"))
	if err != nil {
		return nil
	}
	defer connection.Close()
	if err := connection.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		return nil
	}
	data, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	})
	if err != nil {
		return nil
	}
	if _, err := connection.Write(append(data, '\n')); err != nil {
		return nil
	}
	line, err := bufio.NewReader(connection).ReadString('\n')
	if err != nil {
		return nil
	}
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal([]byte(line), &response); err != nil {
		return nil
	}
	return &response
}

func escalatePermissionReview(root string) (bool, error) {
	response := controlRequest(root, core.RelayRequestGuardianRequests{}, "permission-control")
	if response == nil {
		return false, nil
	}
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	requests, isRequests := ok.Payload.(core.RelayResponsePayloadGuardianRequests)
	if !isOk || !isRequests {
		return false, fmt.Errorf("unexpected guardian requests response: %#v", response.Body)
	}
	var found *core.GuardianReviewRequest
	for index := range requests.Requests {
		if requests.Requests[index].ToolCallID == "call-1" {
			found = &requests.Requests[index]
			break
		}
	}
	if found == nil {
		return false, nil
	}
	completed := controlRequest(root, core.RelayRequestCompleteGuardianRequest{
		Result: core.GuardianReviewResult{
			RequestID: found.RequestID,
			Decision:  core.GuardianReviewOutcomeAskPerson{Reason: "the fixture requires a person's approval"},
		},
	}, "permission-control")
	if completed == nil {
		return false, errors.New("worker did not acknowledge the guardian result")
	}
	ok, isOk = completed.Body.(core.RelayResponseBodyOk)
	if _, isCompleted := ok.Payload.(core.RelayResponsePayloadGuardianRequestCompleted); !isOk || !isCompleted {
		return false, fmt.Errorf("unexpected guardian completion response: %#v", completed.Body)
	}
	return true, nil
}

func pendingPermission(root string) (string, bool) {
	response := controlRequest(root, core.RelayRequestAttach{
		AfterOrdinal: 0,
		AfterDigest:  core.RelayEventGenesisDigest,
	}, "permission-control")
	if response == nil {
		return "", false
	}
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	attached, isAttached := ok.Payload.(core.RelayResponsePayloadAttached)
	if !isOk || !isAttached {
		return "", false
	}
	for index := len(attached.Events) - 1; index >= 0; index-- {
		if requested, ok := attached.Events[index].Observation.(core.RelayObservationElicitationRequested); ok &&
			requested.Request.ToolPermission != nil {
			return requested.Request.ID, true
		}
	}
	return "", false
}

func answerForm(root, elicitationID, choice string) bool {
	response := controlRequest(root, core.RelayRequestRespondElicitation{
		ElicitationID: elicitationID,
		Response: core.ElicitationResponse{
			Action:  core.ElicitationResponseAccept,
			Content: map[string]core.ElicitationValue{"choice": core.ElicitationValueString(choice)},
		},
	}, "permission-control")
	if response == nil {
		return false
	}
	_, ok := response.Body.(core.RelayResponseBodyOk)
	return ok
}

func textPrompt(text string) core.RelayCommand {
	block := acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
	return core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{block}}
}

// A real worker process reads the launch file as a daemon writes it. The
// scripted bridge asks a question and then a tool permission during one
// prompt, and records the answer to each. The question comes back
// cancelled at once, which only a worker that knows it is a managed child
// does. The simulated daemon escalates the guardian review to a person,
// whose answer selects "allow".
// Hard-won: #1451: the worker dropped the managed-child marker before building the ACP launch, and the parent could not grant a child's permission.
// Hard-won: a3a103608: guessing the old permission ID left the real worker waiting forever.
func TestAManagedChildSendsQuestionsToItsParentAndKeepsApprovalsForAPerson(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	answers := filepath.Join(temp, "answers.jsonl")
	script := filepath.Join(temp, "bridge.py")
	answersJSON, err := json.Marshal(answers)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(script, []byte(fmt.Sprintf(`import json, sys
answers = %s
def send(message):
    print(json.dumps(message), flush=True)
def ask(ident, method, params):
    send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
    # The answer arrives on stdin before the prompt can finish.
    for line in sys.stdin:
        answer = json.loads(line)
        if answer.get("id") == ident:
            with open(answers, "a") as output:
                output.write(json.dumps(answer) + "\n")
            return
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {}}
    elif method == "session/new":
        result = {"sessionId": "child-session", "modes": {"currentModeId": "agent",
                  "availableModes": [{"id": "agent", "name": "Guardian"}]}}
    elif method == "session/prompt":
        ask("question-1", "elicitation/create", {
            "sessionId": "child-session", "mode": "form", "message": "Which file?",
            "requestedSchema": {"type": "object", "properties": {
                "file": {"type": "string", "title": "File"}}}})
        ask("permission-1", "session/request_permission", {
            "sessionId": "child-session",
            "toolCall": {"toolCallId": "call-1", "title": "Bash", "kind": "execute"},
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]})
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    if ident is not None:
        send({"jsonrpc": "2.0", "id": ident, "result": result})
`, answersJSON)), 0o666); err != nil {
		t.Fatal(err)
	}

	relay, err := worker.DurableRelayOpen(root, stopTestSession, "prior-worker")
	if err != nil {
		t.Fatal(err)
	}
	accepted := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "submit-prompt",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestSubmit{CommandID: "child-prompt", Command: textPrompt("run a command")},
	})
	acceptedOk(t, accepted, true)

	harnessHome := filepath.Join(temp, "harness-home")
	if err := os.MkdirAll(harnessHome, 0o777); err != nil {
		t.Fatal(err)
	}
	config := filepath.Join(temp, "launch.json")
	writeJSON(t, config, map[string]any{
		"session_id": stopTestSession, "harness": "codex",
		"bridge_command": "python3", "bridge_args": []string{script},
		"environment":        map[string]any{"MJ_SESSION_MANAGED_SUBAGENT": "1"},
		"target_environment": map[string]any{"MJ_INSTANCE": "qa-managed-child-1451"},
		"cwd":                temp, "execution_policy": "configured_approvals",
		"handback_tool": true, "harness_home": harnessHome,
	})
	// Stage both the running worker and this build's lifecycle control
	// worker.
	stageWorkers(t, root)
	command := core.NewCommandSpec(workerExecutable, []string{
		"worker", "run", "--root", root, "--config", config,
	})
	command.Env = map[string]string{"MJ_INSTANCE": "wrong-launcher-instance"}
	running := executeInBackground(command, ioTimeout*2)

	recorded := func() []map[string]any {
		data, _ := os.ReadFile(answers)
		var seen []map[string]any
		for _, line := range strings.Split(string(data), "\n") {
			var value map[string]any
			if json.Unmarshal([]byte(line), &value) == nil {
				seen = append(seen, value)
			}
		}
		return seen
	}
	var result executed
	received := false
	seen, observation := func() ([]map[string]any, error) {
		deadline := time.Now().Add(ioTimeout)
		reviewCompleted, answered := false, false
		for {
			seen := recorded()
			if len(seen) == 2 {
				if !reviewCompleted || !answered {
					return nil, fmt.Errorf("the bridge received a permission answer before the fixture granted it: %v", seen)
				}
				return seen, nil
			}
			if len(seen) == 1 {
				if !reviewCompleted {
					completed, err := escalatePermissionReview(root)
					if err != nil {
						return nil, err
					}
					reviewCompleted = completed
				} else if !answered {
					if id, ok := pendingPermission(root); ok {
						answered = answerForm(root, id, "allow")
					}
				}
			}
			if finished(running, &result, &received) {
				return nil, fmt.Errorf("worker exited before the bridge received both answers: %v", seen)
			}
			if !time.Now().Before(deadline) {
				return nil, fmt.Errorf("the bridge did not receive both answers: %v", seen)
			}
			time.Sleep(25 * time.Millisecond)
		}
	}()
	stopped, stopErr := runControlScript(core.NewBoundedProcessExecutor(ioTimeout), targets.StopWorkerDaemonScript(root))
	if !received {
		result = <-running
	}
	workerLog, _ := os.ReadFile(filepath.Join(root, "worker.log"))
	if observation != nil {
		owner := ""
		if result.err != nil {
			owner = result.err.Error()
		} else {
			owner = string(result.output.Stderr)
		}
		t.Fatalf("%v; worker: %s; worker log: %s; stop: %v %v", observation, owner, workerLog, stopped, stopErr)
	}
	if seen[0]["id"] != "question-1" {
		t.Fatalf("%v", seen)
	}
	if jsonPath(seen[0], "result", "action") != "cancel" {
		t.Fatalf("a managed child's question goes to its parent: %v", seen)
	}
	if seen[1]["id"] != "permission-1" {
		t.Fatalf("%v", seen)
	}
	if jsonPath(seen[1], "result", "outcome", "optionId") != "allow" {
		t.Fatalf("a person answers a managed child's permission: %v", seen)
	}
}
