// Port of mj-worker-bin/tests/empty_session_recovery.rs: native recovery
// through a real, disposable worker process.

//go:build unix

package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

const recoverySession = "018f9dd2-a3b4-7c8d-9000-123456789abc"

func recoveredPromptFinished(root, sessionID string) bool {
	response := controlRequest(root, core.RelayRequestStatus{}, "recovery-status")
	if response == nil {
		return false
	}
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	status, isStatus := ok.Payload.(core.RelayResponsePayloadStatus)
	if !isOk || !isStatus {
		return false
	}
	state := status.State
	return state.NativeSessionID != nil && *state.NativeSessionID == sessionID &&
		state.ActivePrompt == nil && len(state.QueuedPrompts) == 0
}

// relayOrigin is where the worker's relay state comes from before it
// starts.
type relayOrigin int

const (
	// originJournal is a previous worker's journal, which opened the native
	// session itself.
	originJournal relayOrigin = iota
	// originRestoredSeed is a checkpoint restore: only the relay seed
	// exists, and the native identity arrives with the launch
	// configuration.
	originRestoredSeed
)

// codexMissingThread is Codex's answer for a thread it never wrote:
// codex-acp wraps the reason in an internal error.
func codexMissingThread(details string) map[string]any {
	return map[string]any{"code": -32603, "message": "Internal error", "data": map[string]any{"details": details}}
}

// claudeMissingSession is claude-agent-acp 0.99's answer for a session with
// no transcript on disk.
func claudeMissingSession() map[string]any {
	return map[string]any{
		"code":    -32002,
		"message": "Resource not found",
		"data":    map[string]any{"sessionId": "missing-thread"},
	}
}

func recoverMissingNativeSession(t *testing.T, harness string, missingError map[string]any, used bool) {
	t.Helper()
	recoverNativeSessionFrom(t, originJournal, harness, missingError, used)
}

// recoverNativeSessionFrom runs a bridge that loads the original thread
// unless given a JSON-RPC error.
func recoverNativeSessionFrom(t *testing.T, origin relayOrigin, harness string, missingError map[string]any, used bool) {
	t.Helper()
	original := "saved-thread"
	if missingError != nil {
		original = "missing-thread"
	}
	ready := original
	if missingError != nil {
		ready = "replacement"
	}
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	log := filepath.Join(temp, "bridge.log")
	script := filepath.Join(temp, "bridge.py")
	logJSON := mustJSON(t, log)
	var errorJSON []byte
	if missingError == nil {
		errorJSON = mustJSON(t, "null")
	} else {
		errorJSON = mustJSON(t, string(mustJSON(t, missingError)))
	}
	if err := os.WriteFile(script, []byte(fmt.Sprintf(`import json, sys
log = %[1]s
error = json.loads(%[2]s)
modes = {"currentModeId": "agent",
          "availableModes": [{"id": "agent", "name": "Guardian"},
                             {"id": "auto", "name": "Auto"}]}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    with open(log, "a") as output:
        output.write(method + "\n")
    ident = request.get("id")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}
    elif method in ("session/load", "session/resume"):
        assert request["params"]["sessionId"] == %[3]s, request
        if error is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": ident,
                              "error": error}), flush=True)
            continue
        result = {"modes": modes}
    elif method == "session/new":
        result = {"sessionId": "replacement", "modes": modes}
    elif method == "session/prompt":
        assert request["params"]["sessionId"] == %[4]s, request
        # Answer the way a real agent does: the reply streams as an update on
        # the session the prompt named, then the prompt returns (R8-1).
        print(json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": request["params"]["sessionId"],
            "update": {"sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "recovered reply"}}}}),
              flush=True)
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    if ident is not None:
        print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
`, logJSON, errorJSON, core.RustDebugString(original), core.RustDebugString(ready))), 0o666); err != nil {
		t.Fatal(err)
	}

	switch origin {
	case originJournal:
		relay, err := worker.DurableRelayOpen(root, recoverySession, "prior-worker")
		if err != nil {
			t.Fatal(err)
		}
		if _, err := relay.RecordObservation(core.RelayObservationSessionOpened{
			NativeSessionID:               original,
			Resumed:                       false,
			NativeContinuityLost:          false,
			ReplacedUnusedNativeSessionID: nil,
		}); err != nil {
			t.Fatal(err)
		}
		accepted := relay.Handle(core.RelayRequestEnvelope{
			RequestID:       "submit-queued-prompt",
			ProtocolVersion: core.RelayProtocolVersion,
			Request:         core.RelayRequestSubmit{CommandID: "queued-prompt", Command: textPrompt("do the queued work")},
		})
		acceptedOk(t, accepted, true)
		if used {
			if err := relay.MarkNativeSessionUsed(); err != nil {
				t.Fatal(err)
			}
		}
		if relay.NativeSessionMayHaveHistory() != used {
			t.Fatalf("native session may have history = %v", !used)
		}
	case originRestoredSeed:
		// What restore_checkpoint leaves for a session suspended with one
		// prompt still queued and none ever sent.
		if err := os.MkdirAll(root, 0o777); err != nil {
			t.Fatal(err)
		}
		writeJSON(t, core.RestoredRelaySeedPath(root), core.RestoredRelaySeed{
			EventFrontier:       4,
			EventFrontierDigest: strings.Repeat("c", 64),
			QueuedPrompts: []core.CanonicalQueuedPrompt{{
				CommandID:  "queued-prompt",
				Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
				Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"do the queued work"}`)},
				QueuedAtMs: 1,
			}},
			AcceptedConfig:      map[string]string{},
			NativeSessionUnused: !used,
		})
	}

	config := filepath.Join(temp, "launch.json")
	// Every launch config the controller writes names the harness home; a
	// Codex worker registers its delegation tools there before starting.
	harnessHome := filepath.Join(temp, "harness-home")
	if err := os.MkdirAll(harnessHome, 0o777); err != nil {
		t.Fatal(err)
	}
	// Controller staging registers the top-level session's message-only
	// server in Claude's private profile before the worker resolves its
	// target-local socket path.
	writeJSON(t, filepath.Join(harnessHome, ".claude.json"), map[string]any{
		"mcpServers": map[string]any{
			"mj-agents": map[string]any{
				"type":    "stdio",
				"command": "hel",
				"args":    []string{"worker", "subagent-mcp", "--socket", "placeholder"},
			},
		},
	})
	launch := map[string]any{
		"session_id": recoverySession, "harness": harness,
		"bridge_command": "python3", "bridge_args": []string{script},
		"environment": map[string]any{}, "target_environment": map[string]any{"MJ_INSTANCE": "qa-empty-recovery-1063"},
		"cwd": temp, "execution_policy": "configured_approvals",
		"harness_home": harnessHome,
	}
	if origin == originRestoredSeed {
		launch["native_session_id"] = original
	}
	writeJSON(t, config, launch)
	// Stage both the running worker and this build's lifecycle control
	// worker.
	stageWorkers(t, root)
	command := core.NewCommandSpec(workerExecutable, []string{
		"worker", "run", "--root", root, "--config", config,
	})
	command.Env = map[string]string{"MJ_INSTANCE": "wrong-launcher-instance"}
	// Leave time for both the observation and the subsequent stop.
	running := executeInBackground(command, ioTimeout*2)

	var result executed
	received := false
	// Hard-won: fdd49cc1e: Startup under load exhausted the fixture budget before the bridge received work.
	methods, observation := func() (string, error) {
		deadline := time.Now().Add(ioTimeout)
		for {
			data, _ := os.ReadFile(log)
			methods := string(data)
			if used && strings.Contains(methods, "session/load") && finished(running, &result, &received) {
				return methods, nil
			}
			if !used && strings.Contains(methods, "session/prompt") && recoveredPromptFinished(root, ready) {
				return methods, nil
			}
			if finished(running, &result, &received) {
				return "", fmt.Errorf("worker exited before recovering queued work: %s", methods)
			}
			if !time.Now().Before(deadline) {
				return "", fmt.Errorf("bridge never received queued work: %s", methods)
			}
			time.Sleep(25 * time.Millisecond)
		}
	}()
	stopped, stopErr := runControlScript(core.NewBoundedProcessExecutor(ioTimeout), targets.StopWorkerDaemonScript(root))
	if !received {
		result = <-running
	}
	workerLog, err := os.ReadFile(filepath.Join(root, "worker.log"))
	if err != nil {
		t.Fatalf("read the owning worker's diagnostics: %v", err)
	}
	if observation != nil {
		owner := ""
		if result.err != nil {
			owner = result.err.Error()
		} else {
			owner = string(result.output.Stderr)
		}
		t.Fatalf("%v; worker: %s; worker log: %s; stop: %v %v", observation, owner, workerLog, stopped, stopErr)
	}
	if stopErr != nil || stopped.Status != 0 {
		t.Fatalf("stop: %v %v", stopErr, stopped)
	}
	if result.err != nil {
		t.Fatalf("worker did not stop within its deadline: %v", result.err)
	}
	output := result.output
	reloads := 0
	for _, method := range strings.Split(methods, "\n") {
		if method == "session/load" || method == "session/resume" {
			reloads++
		}
	}
	expectedReloads := 0
	if used || harness != "codex" || origin == originRestoredSeed {
		expectedReloads = 1
	}
	if reloads != expectedReloads {
		t.Fatalf("reloads %d: %s", reloads, methods)
	}
	if used {
		if output.Status == 0 {
			t.Fatal("a worker that could not reload used history exited cleanly")
		}
		if !strings.Contains(string(workerLog), "has no native history") {
			t.Fatalf("%s", workerLog)
		}
		if strings.Contains(methods, "session/new") {
			t.Fatal(methods)
		}
		if origin == originJournal {
			relay, err := worker.DurableRelayOpen(root, recoverySession, "recovered-worker")
			if err != nil {
				t.Fatal(err)
			}
			if id := relay.OperationalState().NativeSessionID; id == nil || *id != "missing-thread" {
				t.Fatalf("native session %v", id)
			}
		}
		return
	}
	if output.Status != 0 {
		t.Fatalf("%s", workerLog)
	}
	newSessions := 0
	if missingError != nil {
		newSessions = 1
	}
	if strings.Count(methods, "session/new") != newSessions {
		t.Fatal(methods)
	}
	if strings.Count(methods, "session/prompt") != 1 {
		t.Fatal(methods)
	}

	relay, err := worker.DurableRelayOpen(root, recoverySession, "recovered-worker")
	if err != nil {
		t.Fatal(err)
	}
	after, digest := uint64(0), core.RelayEventGenesisDigest
	if origin == originRestoredSeed {
		after, digest = 4, strings.Repeat("c", 64)
	}
	events, err := relay.EventsAfter(after, digest)
	if err != nil {
		t.Fatal(err)
	}
	var replaced *string
	if missingError != nil {
		replaced = &original
	}
	sameOptional := func(a, b *string) bool { return a == nil && b == nil || a != nil && b != nil && *a == *b }
	// The controller accepts a new identity on resume only when the journal
	// names the unused thread that was replaced (R7-5).
	if !slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		opened, ok := event.Observation.(core.RelayObservationSessionOpened)
		return ok && opened.NativeSessionID == ready && opened.Resumed == (replaced == nil) &&
			sameOptional(opened.ReplacedUnusedNativeSessionID, replaced)
	}) {
		t.Fatalf("%#v", events)
	}
	if !sameOptional(relay.OperationalState().ReplacedUnusedNativeSessionID, replaced) {
		t.Fatalf("replaced unused native session %v", relay.OperationalState().ReplacedUnusedNativeSessionID)
	}
	warned := slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		warning, ok := event.Observation.(core.RelayObservationWarning)
		return ok && strings.Contains(warning.Message, "new empty session")
	})
	if warned != (replaced != nil) {
		t.Fatalf("warned %v: %#v", warned, events)
	}
	// The opened session is live: the reply reaches the journal and the
	// turn ends answered rather than `prompt_unanswered` (R8-1).
	if !slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		update, ok := event.Observation.(core.RelayObservationSessionUpdate)
		if !ok || update.Update.Kind != acpschema.SessionUpdateKindAgentMessageChunk || update.Update.AgentMessageChunk == nil {
			return false
		}
		content := update.Update.AgentMessageChunk.Content
		return content.Kind == acpschema.ContentBlockKindText && content.Text != nil && content.Text.Text == "recovered reply"
	}) {
		t.Fatalf("%#v", events)
	}
	if !slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		completed, ok := event.Observation.(core.RelayObservationCommandCompleted)
		if !ok || completed.CommandID != "queued-prompt" {
			return false
		}
		outcome, ok := completed.Outcome.(core.RelayCommandOutcomePrompt)
		return ok && outcome.StopReason == "EndTurn"
	}) {
		t.Fatalf("%#v", events)
	}
}

func TestCodexMissingUnusedThreadRecoversAfterWorkerRestart(t *testing.T) {
	recoverMissingNativeSession(t, "codex", codexMissingThread("thread not loaded: missing-thread"), false)
}

func TestClaudeMissingUnusedSessionRecoversAfterWorkerRestart(t *testing.T) {
	recoverMissingNativeSession(t, "claude", claudeMissingSession(), false)
}

func TestUsedNativeHistoryIsNotReplacedAfterWorkerRestart(t *testing.T) {
	recoverMissingNativeSession(t, "codex", codexMissingThread("no rollout found for thread id missing-thread"), true)
	recoverMissingNativeSession(t, "claude", claudeMissingSession(), true)
}

// I2-7: resume after suspending a session that was never prompted. The
// worker starts from the checkpoint's relay seed with the native identity
// in its launch configuration, and the harness has no record of the
// session. Codex's wording is the one R4 recorded ("thread not found:
// <id>").
// Hard-won: 0bb9439: a restored session with no prompts could not resume its unused native thread.
func TestANeverPromptedSessionResumesFreshFromItsCheckpoint(t *testing.T) {
	recoverNativeSessionFrom(t, originRestoredSeed, "codex", codexMissingThread("thread not found: missing-thread"), false)
	recoverNativeSessionFrom(t, originRestoredSeed, "claude", claudeMissingSession(), false)
	// A checkpoint that cannot vouch for the session keeps it protected.
	recoverNativeSessionFrom(t, originRestoredSeed, "codex", codexMissingThread("thread not found: missing-thread"), true)
}

// Hard-won: dc5d9891a: a restored session whose only turns were mailbox wakes was replaced although loadable
func TestCodexLoadsARestoredThreadMarkedUnusedByTheSeed(t *testing.T) {
	recoverNativeSessionFrom(t, originRestoredSeed, "codex", nil, false)
}
