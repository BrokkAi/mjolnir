// Port of mj-worker/src/acp/session_directories_tests.rs.

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// grokInitialize is what Grok Build 1.0.89 answers to `initialize`,
// recorded from the pinned binary: its `sessionCapabilities` has no
// `additionalDirectories`.
func grokInitialize(t *testing.T) map[string]any {
	t.Helper()
	data, err := os.ReadFile("testdata/acp/grok_1_0_89_initialize.json")
	if err != nil {
		t.Fatal(err)
	}
	var initialize map[string]any
	if err := json.Unmarshal(data, &initialize); err != nil {
		t.Fatal(err)
	}
	return initialize
}

// openSessionWithDirectories opens a session against a bridge that answers
// `initialize` with the given result and rejects a non-empty
// `additionalDirectories` the way Grok does, unless that result advertises
// the capability. Returns the warnings the worker raised and the directory
// lists the bridge received on the session request.
func openSessionWithDirectories(t *testing.T, initialize map[string]any, spec LaunchSpec) ([]string, []any) {
	t.Helper()
	client, bridge := acpDriveDuplex(64 * 1024)
	_, acceptsDirectories := jsonAt(initialize, "agentCapabilities", "sessionCapabilities", "additionalDirectories").(map[string]any)
	seenDone := make(chan []any, 1)
	go func() {
		var seen []any
		defer func() { seenDone <- seen }()
		lines := bufio.NewScanner(bridge)
		lines.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
		for lines.Scan() {
			var request map[string]any
			if err := json.Unmarshal(lines.Bytes(), &request); err != nil {
				panic(err)
			}
			id := request["id"]
			var response map[string]any
			switch jsonString(request["method"]) {
			case "initialize":
				response = map[string]any{"jsonrpc": "2.0", "id": id, "result": initialize}
			case "session/new", "session/load", "session/resume":
				directories := jsonAt(request, "params", "additionalDirectories")
				seen = append(seen, directories)
				if list, ok := directories.([]any); !acceptsDirectories && ok && len(list) > 0 {
					response = map[string]any{"jsonrpc": "2.0", "id": id, "error": map[string]any{
						"code": -32602, "message": "Invalid params",
						"data": "additionalDirectories is not supported: a session works only in its cwd"}}
				} else {
					response = map[string]any{"jsonrpc": "2.0", "id": id, "result": map[string]any{"sessionId": "grok-test"}}
				}
			default:
				continue
			}
			encoded, err := json.Marshal(response)
			if err != nil {
				panic(err)
			}
			if _, err := bridge.Write(append(encoded, '\n')); err != nil {
				break
			}
		}
	}()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 32)
	driverDone := make(chan struct{})
	go func() {
		defer close(driverDone)
		_, _ = drive(context.Background(), client.clientTransport(), spec, requests, events, NewShared[*OpenedSession](nil), false)
	}()
	var warnings []string
	for opened := false; !opened; {
		select {
		case event := <-events:
			switch event := event.(type) {
			case core.RuntimeEventWarning:
				warnings = append(warnings, event.Message)
			case core.RuntimeEventSessionStarted:
				opened = true
			}
		case <-time.After(10 * time.Second):
			t.Fatal("the session opens or the worker reports why not")
		}
	}
	close(requests)
	// Rust drops the event receiver: nothing more is read.
	go func() {
		for range events {
		}
	}()
	select {
	case <-driverDone:
	case <-time.After(10 * time.Second):
	}
	bridge.Close()
	var seen []any
	select {
	case seen = <-seenDone:
	case <-time.After(10 * time.Second):
		t.Fatal("the bridge did not finish")
	}
	return warnings, seen
}

func specWithDirectories(t *testing.T) LaunchSpec {
	t.Helper()
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	spec := acpDriveSpec("fake", core.HarnessKindGrok, core.ExecutionPolicyConfiguredApprovals, cwd)
	spec.Environment = map[string]string{"MJ_TOOL_CACHE_DIR": "/cache/tools"}
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		ProjectKey:      "project",
		Root:            "/profile/memory",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	spec.AdditionalDirectories = []string{"/workspace/second", "/profile/memory", "/cache/tools"}
	return spec
}

// Hard-won: #1433: Grok Build rejects additionalDirectories and the session
// never started; the worker sent the memory root and tool cache to every
// harness that is not Claude or Muse.
func TestAnAgentWithoutTheAdditionalDirectoriesCapabilityGetsNoneAndThePersonIsTold(t *testing.T) {
	initialize := grokInitialize(t)
	if _, present := jsonAt(initialize, "agentCapabilities", "sessionCapabilities").(map[string]any)["additionalDirectories"]; present {
		t.Fatal("the recorded Grok response is the one that lacks the capability")
	}

	warnings, seen := openSessionWithDirectories(t, initialize, specWithDirectories(t))

	// An empty list is left off the wire.
	if len(seen) != 1 || seen[0] != nil {
		t.Fatalf("seen = %#v", seen)
	}
	var notice string
	for _, message := range warnings {
		if strings.Contains(message, "additional directories") {
			notice = message
			break
		}
	}
	if notice == "" {
		t.Fatalf("the dropped directories are reported: %#v", warnings)
	}
	if !strings.HasPrefix(notice, "Grok Build does not accept") {
		t.Fatal(notice)
	}
	for _, named := range []string{
		"workspace directory /workspace/second",
		"project memory /profile/memory",
		"tool cache /cache/tools",
	} {
		if !strings.Contains(notice, named) {
			t.Fatalf("%s missing from: %s", named, notice)
		}
	}
}

func TestAnAgentThatAdvertisesTheCapabilityReceivesEveryDirectory(t *testing.T) {
	initialize := grokInitialize(t)
	jsonAt(initialize, "agentCapabilities", "sessionCapabilities").(map[string]any)["additionalDirectories"] = map[string]any{}
	spec := specWithDirectories(t)
	expected := make([]any, len(spec.AdditionalDirectories))
	for i, directory := range spec.AdditionalDirectories {
		expected[i] = directory
	}

	warnings, seen := openSessionWithDirectories(t, initialize, spec)

	if len(seen) != 1 || !jsonEqual(seen[0], expected) {
		t.Fatalf("seen = %#v, want [%#v]", seen, expected)
	}
	for _, message := range warnings {
		if strings.Contains(message, "additional directories") {
			t.Fatalf("%#v", warnings)
		}
	}
}
