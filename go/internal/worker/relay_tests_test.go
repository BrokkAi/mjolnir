// Port of mj-worker/src/relay/tests.rs.

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// relaySteeringAutomatically is a relay whose bridge advertises steering,
// with `prompt-first` running.
func relaySteeringAutomatically(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpen(root, session, "test"))
	relay.SetSteeringSupported(rtBool(true))
	relay.SetAutomaticSteering(true)
	submitRelay(relay, "prompt-first", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	return relay
}

func runningClaudeTurn(t *testing.T, root string, hookAvailable bool) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpenForHarness(root, session, "test", core.HarnessKindClaude, nil))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	relay.SetSteeringDeliveryMode(core.HarnessKindClaude, hookAvailable)
	submitRelay(relay, "prompt-first", prompt("Work on the first request."))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	if len(claimed) != 1 || claimed[0].CommandID != "prompt-first" {
		t.Fatalf("expected prompt-first claimed, got %#v", claimed)
	}
	return relay
}

func queuePrompt(t *testing.T, relay *DurableRelay, commandID, text string) {
	t.Helper()
	submitRelay(relay, commandID, prompt(text))
}

func claimedSteerOf(t *testing.T, claimed []core.ClaimedRelayCommand, queued string) string {
	t.Helper()
	if len(claimed) != 1 {
		t.Fatalf("expected one automatic steer, got %+v", claimed)
	}
	steer := claimed[0]
	if !strings.HasPrefix(steer.CommandID, "auto-steer-") {
		t.Fatalf("steer command ID %q does not start with auto-steer-", steer.CommandID)
	}
	command, ok := steer.Command.(core.RelayCommandSteer)
	if !ok || command.ActivePromptID != "prompt-first" || command.QueuedPromptID != queued {
		t.Fatalf("unexpected steer command %#v", steer.Command)
	}
	if steer.SteeringPrompt == nil || steer.SteeringPrompt.QueuedCommandID != queued {
		t.Fatalf("unexpected steering prompt %+v", steer.SteeringPrompt)
	}
	return steer.CommandID
}

func stoppedSubagentsNote(t *testing.T) string {
	t.Helper()
	task := "Fix the off-by-one in the parser."
	note, ok := core.StoppedSubagentsPromptContext([]core.StoppedSubagent{
		{ChildSessionID: "11111111111111111111111111111111", Title: "Fix the parser", Task: &task, HandedBack: false},
		{ChildSessionID: "22222222222222222222222222222222", Title: "Review the docs", Task: nil, HandedBack: true},
	})
	if !ok {
		t.Fatal("two stopped sub-agents make a note")
	}
	return note
}

// recordPagedHistory records a history large enough to seal several
// segments and to overflow one replay page, so an attach against it really
// does read and decompress.
func recordPagedHistory(t *testing.T, relay *DurableRelay, events int) {
	t.Helper()
	for index := range events {
		rtMust(relay.RecordObservation(core.RelayObservationWarning{
			Message: fmt.Sprintf("%04d:%s", index, strings.Repeat("x", 64*1024)),
		}))
	}
}

func attachEnvelope(t *testing.T, relay *DurableRelay, requestID string, afterOrdinal uint64) core.RelayRequestEnvelope {
	t.Helper()
	return relayRequest(requestID, core.RelayRequestAttach{
		AfterOrdinal:     afterOrdinal,
		AfterDigest:      relayTestDigestAt(relay, afterOrdinal),
		KnownStateDigest: nil,
	})
}

func successfulShell(command, stdout string) core.UserShellResult {
	exitCode := int32(0)
	return core.UserShellResult{
		Command:         command,
		Stdout:          stdout,
		Stderr:          "",
		StdoutTruncated: false,
		StderrTruncated: false,
		ExitCode:        &exitCode,
		Signal:          nil,
		DurationMs:      12,
		Status:          core.UserShellStatusExited,
		Error:           nil,
	}
}

// setVerdictHarness is relay.SetTurnVerdictHarness(harness) for a relay
// whose root the test removes. The harness opens the Jev decision log, whose
// goroutine (Rust's writer thread) appends after the call that queued the
// record returns. Rust's TempDir drop ignores a removal that races those
// writes; Go's t.TempDir cleanup fails the test on it. So the drain is
// registered here, where the writer starts: cleanups run last in, first out,
// so it runs before the removal of a root created before the relay. It holds
// the relay until the read is queued, so the writer cannot be collected
// first and leave its queued appends unordered with the read.
func setVerdictHarness(t *testing.T, relay *DurableRelay, harness core.HarnessKind) {
	t.Helper()
	relay.SetTurnVerdictHarness(harness)
	t.Cleanup(func() {
		// The read is ordered after every queued write; its answer is unused.
		_, _ = core.JevRead(filepath.Join(relay.Root(), "jev-decisions"), session, nil)
		runtime.KeepAlive(relay)
	})
}

// relayTestsClaudeRelay builds a relay that models the turns Claude Code
// starts on its own. Rust's claude_relay; prefixed because
// relay/verdict.rs's settled_task_tests has its own claude_relay.
func relayTestsClaudeRelay(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpen(root, session, "1.0.0"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	relay.snapshot.NativeSessionID = rtString("root")
	return relay
}

func toolCallUpdate() schema.SessionUpdate {
	call := schema.ToolCall{ToolCallID: "call-1", Title: "Bash"}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

// originMarker is the `usage_update` the Claude adapter sends when an SDK
// cycle ends and the cycle produced assistant usage.
func originMarker(origin string) schema.SessionUpdate {
	usage := schema.UsageUpdate{
		Used: 10,
		Size: 200,
		Meta: jsonvalue.Object{{Key: "_claude/origin", Value: jsonvalue.Object{{Key: "kind", Value: origin}}}},
	}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindUsageUpdate, UsageUpdate: &usage}
}

// cycleResult is the SDK `result` that ends a Claude Code model cycle.
func cycleResult(t *testing.T, origin string) core.ClaudeTurnResult {
	t.Helper()
	result, err := core.ClaudeTurnResultFromSDKMessage(map[string]any{
		"type": "result", "subtype": "success", "is_error": false, "num_turns": 1,
		"stop_reason": "end_turn", "result": "done",
		"usage":  map[string]any{"input_tokens": 10, "output_tokens": 5},
		"origin": map[string]any{"kind": origin},
	})
	if err != nil {
		t.Fatal(err)
	}
	if result == nil {
		t.Fatal("the SDK message is not a result")
	}
	return *result
}

func observations(t *testing.T, relay *DurableRelay) []core.RelayObservation {
	t.Helper()
	var observed []core.RelayObservation
	for _, event := range rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest)) {
		observed = append(observed, event.Observation)
	}
	return observed
}

// claudeAutoModeFallbackText is what the Claude adapter (claude-agent-acp
// 0.81.0, `dist/session-mode.js`, `publishFallbackWarning`) sends as agent
// text while it answers a switch to a model without Auto mode. Recorded in
// launch re-verification R4 (session 7214842e…, transcript item 30) after
// `/model haiku` on a session with no prompt yet.
const claudeAutoModeFallbackText = "**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead."

// execCard is a Codex `exec_command` card. The adapter reports the result
// under `rawOutput`, with `exit_code` null while the process is still
// running.
func execCard(toolCallID string, command []string, exitCode *int64) schema.SessionUpdate {
	rawInput, _ := json.Marshal(struct {
		Command []string `json:"command"`
	}{command})
	rawOutput, _ := json.Marshal(struct {
		Output   string `json:"output"`
		ExitCode *int64 `json:"exit_code"`
	}{"", exitCode})
	kind := schema.ToolKindExecute
	status := schema.ToolCallStatusCompleted
	call := schema.ToolCall{ToolCallID: schema.ToolCallId(toolCallID), Title: "shell", Kind: &kind, Status: &status, RawInput: rawInput, RawOutput: rawOutput}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

func kimiBackgroundAgentCard() schema.SessionUpdate {
	status := schema.ToolCallStatusCompleted
	call := schema.ToolCall{
		ToolCallID: "1:tool_agent",
		Title:      "Launching background coder agent: Fix memory use",
		Status:     &status,
		RawInput:   json.RawMessage(`{"description":"Fix memory use","prompt":"Fix the issue","run_in_background":true}`),
		RawOutput:  json.RawMessage(`"task_id: agent-deadbeef\nstatus: running\nagent_id: agent-1"`),
	}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

func kimiBackgroundAgentCardWithRunInBackgroundOnly() schema.SessionUpdate {
	status := schema.ToolCallStatusCompleted
	call := schema.ToolCall{
		ToolCallID: "run-only",
		Title:      "agent",
		Status:     &status,
		RawInput:   json.RawMessage(`{"description":"Run-only agent","run_in_background":true}`),
	}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

func kimiBackgroundAgentCardWithRunningTaskIDOnly() schema.SessionUpdate {
	status := schema.ToolCallStatusCompleted
	call := schema.ToolCall{
		ToolCallID: "task-only",
		Title:      "agent",
		Status:     &status,
		RawInput:   json.RawMessage(`{"description":"Task-only agent"}`),
		RawOutput:  json.RawMessage(`"task_id: agent-task-only\nstatus: running"`),
	}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

// kimiBackgroundShellCard is Kimi's `Bash` launcher card with
// `run_in_background`, optionally embedding the hosted terminal the detached
// shell runs in.
func kimiBackgroundShellCard(toolCallID string, terminalID *string) schema.SessionUpdate {
	status := schema.ToolCallStatusCompleted
	call := schema.ToolCall{
		ToolCallID: schema.ToolCallId(toolCallID),
		Title:      "Bash",
		Status:     &status,
		RawInput:   json.RawMessage(`{"command":"cargo build --release","description":"Build release runner","run_in_background":true}`),
	}
	if terminalID != nil {
		call.Content = []schema.ToolCallContent{{
			Kind:     schema.ToolCallContentKindTerminal,
			Terminal: &schema.Terminal{TerminalID: schema.TerminalId(*terminalID)},
		}}
	}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

func kimiProcessTask(parentToolCallID string) KimiBackgroundTask {
	return KimiBackgroundTask{
		IsAgent:          false,
		TaskID:           "bash-r5ae",
		Description:      "Build release runner",
		StartedAtMs:      2_000,
		ParentToolCallID: &parentToolCallID,
	}
}

// kimiRelayWithPromptInFlight puts a prompt of ours in flight so the
// turn-scoped terminal rule applies.
func kimiRelayWithPromptInFlight(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpen(root, session, "1.0.0"))
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	submitRelay(relay, "parent-command", prompt("build it"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	return relay
}

func airTask(taskID, description string) AirTaskFixture {
	return AirTaskFixture{TaskID: taskID, Description: description}
}

// claudeStopAcknowledgement is a plain agent chunk carrying only `text`, as
// the Claude adapter sends when it acknowledges a task stop.
func claudeStopAcknowledgement(name string) string {
	return claudeStopAcknowledgementPrefix + name + "."
}

func agentTextChunk(text string) schema.SessionUpdate {
	chunk := schema.ContentChunk{Content: schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}}
	return schema.SessionUpdate{Kind: schema.SessionUpdateKindAgentMessageChunk, AgentMessageChunk: &chunk}
}

// claudeRelayWithStoppableTask is a Claude relay with one stoppable task
// `sleeper` named "Sleep 600".
func claudeRelayWithStoppableTask(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := relayTestsClaudeRelay(t, root)
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("sleeper", "Sleep 600")}))
	rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateChanged{TaskID: "sleeper", Description: nil, CanStop: rtBool(true)}))
	return relay
}

func assertChunkOpenedAHarnessTurn(t *testing.T, relay *DurableRelay) {
	t.Helper()
	state := relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateRunning)
	if state.HarnessTurn == nil {
		t.Fatal("the chunk must open a turn")
	}
	observed := observations(t, relay)
	if len(observed) != 2 {
		t.Fatalf("%#v", observed)
	}
	_, started := observed[0].(core.RelayObservationHarnessTurnStarted)
	_, update := observed[1].(core.RelayObservationSessionUpdate)
	if !started || !update {
		t.Fatalf("%#v", observed)
	}
}

func clearableRelay(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay := rtMust(DurableRelayOpen(root, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindCodex)
	rtMust(relay.RecordObservation(rtOpenedSession("original")))
	rtNoErr(relay.MarkNativeSessionUsed())
	return relay
}

// projectedTranscript is the transcript a relay's events project to.
func projectedTranscript(t *testing.T, relay *DurableRelay) []core.TranscriptItem {
	t.Helper()
	materialized := rtProjectSession(relay)
	items := make([]core.TranscriptItem, 0, len(materialized.Transcript))
	for _, item := range materialized.Transcript {
		items = append(items, *item)
	}
	return items
}

func mailboxFixtureEvent(key string, wake bool) core.MailboxEvent {
	return core.MailboxEvent{
		Key:         key,
		Source:      "parent",
		Wake:        wake,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyParentMessage{Text: "message for " + key},
	}
}

// requestMailboxDrainForTest returns the drained text, the leased count and
// the lease ID.
func requestMailboxDrainForTest(t *testing.T, relay *DurableRelay, requestID string) (*string, int, *string) {
	t.Helper()
	response := relay.Handle(relayRequest(requestID, core.RelayRequestDrainMailbox{HookEvent: "PostToolBatch"}))
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	drained, isDrained := ok.Payload.(core.RelayResponsePayloadMailboxDrained)
	if !isOk || !isDrained {
		t.Fatalf("mailbox drain failed: %#v", response.Body)
	}
	return drained.Text, drained.Count, drained.LeaseID
}

// drainPostToolBatch returns the drained text, the leased count and the
// lease ID.
func drainPostToolBatch(t *testing.T, relay *DurableRelay) (*string, int, *string) {
	t.Helper()
	payload := rtMust(relay.DrainMailbox("PostToolBatch"))
	drained, ok := payload.(core.RelayResponsePayloadMailboxDrained)
	if !ok {
		t.Fatalf("unexpected PostToolBatch response: %#v", payload)
	}
	return drained.Text, drained.Count, drained.LeaseID
}

func acknowledgeMailboxForTest(t *testing.T, relay *DurableRelay, requestID, leaseID string) {
	t.Helper()
	response := relay.Handle(relayRequest(requestID, core.RelayRequestAckMailbox{LeaseID: leaseID}))
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	acknowledged, isAcknowledged := ok.Payload.(core.RelayResponsePayloadMailboxAcknowledged)
	if !isOk || !isAcknowledged || !acknowledged.Acknowledged {
		t.Fatalf("mailbox acknowledgement failed: %#v", response.Body)
	}
}

func drainMailboxForTest(t *testing.T, relay *DurableRelay, requestID string) (*string, int) {
	t.Helper()
	text, count, leaseID := requestMailboxDrainForTest(t, relay, requestID)
	if leaseID != nil {
		acknowledgeMailboxForTest(t, relay, requestID+"-ack", *leaseID)
	}
	return text, count
}

// rtMust is Rust's unwrap() on a Result: like the shared relay test
// helpers, it panics on an error, which fails the test.
func rtMust[T any](value T, err error) T {
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	return value
}

// rtNoErr is Rust's unwrap() on a Result<()>.
func rtNoErr(err error) {
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
}

// rtEqual is assert_eq! on values Rust compares structurally. Go has nil
// and empty slices and maps where Rust has one empty Vec or map, so those
// compare equal here.
func rtEqual(t *testing.T, got, want any) {
	t.Helper()
	if !rtLooseEqual(reflect.ValueOf(got), reflect.ValueOf(want)) {
		t.Fatalf("assertion failed: left == right\n  left: %#v\n right: %#v", got, want)
	}
}

func rtLooseEqual(left, right reflect.Value) bool {
	if !left.IsValid() || !right.IsValid() {
		return left.IsValid() == right.IsValid()
	}
	if left.Type() != right.Type() {
		return false
	}
	switch left.Kind() {
	case reflect.Slice:
		if left.Len() != right.Len() {
			return false
		}
		for index := range left.Len() {
			if !rtLooseEqual(left.Index(index), right.Index(index)) {
				return false
			}
		}
		return true
	case reflect.Array:
		for index := range left.Len() {
			if !rtLooseEqual(left.Index(index), right.Index(index)) {
				return false
			}
		}
		return true
	case reflect.Map:
		if left.Len() != right.Len() {
			return false
		}
		for _, key := range left.MapKeys() {
			other := right.MapIndex(key)
			if !other.IsValid() || !rtLooseEqual(left.MapIndex(key), other) {
				return false
			}
		}
		return true
	case reflect.Pointer, reflect.Interface:
		if left.IsNil() || right.IsNil() {
			return left.IsNil() == right.IsNil()
		}
		return rtLooseEqual(left.Elem(), right.Elem())
	case reflect.Struct:
		for index := range left.NumField() {
			if !rtLooseEqual(left.Field(index), right.Field(index)) {
				return false
			}
		}
		return true
	case reflect.Bool:
		return left.Bool() == right.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return left.Int() == right.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return left.Uint() == right.Uint()
	case reflect.Float32, reflect.Float64:
		return left.Float() == right.Float()
	case reflect.Complex64, reflect.Complex128:
		return left.Complex() == right.Complex()
	case reflect.String:
		return left.String() == right.String()
	default:
		return left.Pointer() == right.Pointer()
	}
}

// rtOkPayload is the payload of an Ok response, failing the test otherwise.
func rtOkPayload(t *testing.T, response core.RelayResponseEnvelope) core.RelayResponsePayload {
	t.Helper()
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("expected an ok response, got %#v", response.Body)
	}
	return ok.Payload
}

// rtRewriteRelayState edits the saved relay-state.json as JSON, the way the
// Rust tests rewrite it with serde_json::Value.
func rtRewriteRelayState(t *testing.T, root string, edit func(saved map[string]any)) {
	t.Helper()
	path := filepath.Join(root, core.RelayStateFile)
	data := rtMust(os.ReadFile(path))
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var saved map[string]any
	rtNoErr(decoder.Decode(&saved))
	edit(saved)
	rtNoErr(os.WriteFile(path, rtMust(json.Marshal(saved)), 0o600))
}

func rtString(value string) *string { return &value }

func rtUint64(value uint64) *uint64 { return &value }

func rtInt64(value int64) *int64 { return &value }

func rtBool(value bool) *bool { return &value }

func initialFifoCommands() []core.InitialWorkerCommand {
	return []core.InitialWorkerCommand{
		{CommandID: "initial-model", Command: setConfig("model", "chosen-model"), Optional: false},
		{CommandID: "initial-effort", Command: setConfig("effort", "medium"), Optional: false},
		{CommandID: "initial-fast-mode", Command: setConfig("fast-mode", "on"), Optional: true},
		{CommandID: "initial-brief", Command: prompt("Original task and constraints."), Optional: false},
	}
}

func rtOpenedSession(nativeSessionID string) core.RelayObservationSessionOpened {
	return core.RelayObservationSessionOpened{
		NativeSessionID:               nativeSessionID,
		Resumed:                       false,
		NativeContinuityLost:          false,
		ReplacedUnusedNativeSessionID: nil,
	}
}

// Hard-won: 6f0f854: a legacy receipt evicted before /clear allowed startup replay at upgrade.
func TestStartupUpgradeUsesLifetimePromptWatermarkAfterPruningAndClear(t *testing.T) {
	legacyStartupWithPrunedReceipt(t, true, false, false)
}

// Hard-won: 6f0f854: pre-watermark native use must still prove startup was seeded.
func TestStartupUpgradePreservesPreWatermarkNativeUseWithoutAReceipt(t *testing.T) {
	legacyStartupWithPrunedReceipt(t, false, true, false)
}

// Hard-won: 9a72411: a pruned removal receipt was displayed as delivery of a legacy brief.
func TestStartupUpgradeDoesNotDisplayARemovedBriefAfterItsReceiptIsPruned(t *testing.T) {
	legacyStartupWithPrunedReceipt(t, false, false, true)
}

func legacyStartupWithPrunedReceipt(t *testing.T, clear, omitWatermark, removed bool) {
	t.Helper()
	temp := t.TempDir()
	commands := []core.InitialWorkerCommand{{
		CommandID: "original-brief",
		Command:   prompt("original task"),
		Optional:  false,
	}}
	var startup []core.InitialWorkerCommand
	if !removed {
		startup = commands
	}
	relay := rtMust(DurableRelayOpenForHarness(temp, session, "old", core.HarnessKindCodex, startup))
	setVerdictHarness(t, relay, core.HarnessKindCodex)
	rtMust(relay.RecordObservation(rtOpenedSession("old-thread")))
	if removed {
		submitRelay(relay, "blocking-prompt", prompt("already running"))
		rtMust(relay.ClaimPendingCommands(true))
		relay.initialCommands = slices.Clone(commands)
		rtNoErr(relay.seedInitialCommands())
		submitRelay(relay, "remove-original", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "original-brief"})
		failure := relay.snapshot.HandledCommands["original-brief"].Failure
		if failure == nil || *failure != "queued command was removed" {
			t.Fatalf("unexpected removal failure %v", failure)
		}
		finishPrompt(relay, "blocking-prompt")
	} else {
		rtMust(relay.ClaimPendingCommands(true))
		finishPrompt(relay, "original-brief")
	}
	submitRelay(relay, "later-prompt", prompt("later"))
	rtMust(relay.ClaimPendingCommands(true))
	finishPrompt(relay, "later-prompt")
	// Emulate legacy retention, before startup owners were registered.
	relay.snapshot.InitialCommandIDs = nil
	for index := 0; index <= core.RelayRetryIDRetention; index++ {
		submitRelay(relay, fmt.Sprintf("later-command-%d", index), core.RelayCommandRecordNotice{Text: "later"})
	}
	acknowledged := relay.LatestOrdinal()
	acknowledgeRelay(relay, "prune-old-startup", acknowledged)
	if _, ok := relay.snapshot.HandledCommands["original-brief"]; ok {
		t.Fatal("the original brief receipt was not pruned")
	}
	if clear {
		submitRelay(relay, "clear-context", core.RelayCommandClearContext{})
		rtMust(relay.ClaimPendingCommands(true))
		rtMust(relay.RecordCommandCompleted("clear-context", core.RelayCommandOutcomeContextCleared{
			NativeSessionID: "cleared-thread",
			Memory:          nil,
		}))
		if relay.snapshot.NativeSessionUsed {
			t.Fatal("a cleared native session is unused")
		}
	} else if !relay.snapshot.NativeSessionUsed {
		t.Fatal("the native session was used")
	}
	frontier := relay.LatestOrdinal()
	rtNoErr(relay.persistSnapshot())
	rtRewriteRelayState(t, temp, func(saved map[string]any) {
		saved["format_version"] = 27
		// Legacy collection pruned only after a checkpoint covered this ACK.
		saved["recovery_floor_ordinal"] = saved["acknowledged_through"]
		saved["recovery_floor_digest"] = saved["acknowledged_digest"]
		delete(saved, "initial_command_ids")
		delete(saved, "native_use_evidence")
		if omitWatermark {
			delete(saved, "latest_prompt_accepted_ordinal")
		}
	})
	for range 2 {
		relay := rtMust(DurableRelayOpenForHarness(temp, session, "new", core.HarnessKindCodex, commands))
		rtEqual(t, relay.LatestOrdinal(), frontier)
		if !slices.Contains(relay.snapshot.InitialCommandIDs, "original-brief") {
			t.Fatalf("startup owner is not registered: %v", relay.snapshot.InitialCommandIDs)
		}
		if _, ok := relay.snapshot.HandledCommands["original-brief"]; ok {
			t.Fatal("a pruned receipt was invented")
		}
		cut := rtMust(relay.LiveSessionBaseline())
		// Missing legacy receipts prove neither completion nor removal. The
		// startup pin prevents replay without inventing a delivered user row.
		if len(cut.DeliveredInitialPrompts) != 0 {
			t.Fatalf("unexpected delivered initial prompts %#v", cut.DeliveredInitialPrompts)
		}
		if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
			t.Fatalf("startup replayed %#v", claimed)
		}
	}
}

// Hard-won: 6f0f854: a partially accepted selector prefix must not consume the unsubmitted brief.
func TestStartupUpgradeCompletesOneOfTwoAcceptedCommandsExactlyOnce(t *testing.T) {
	temp := t.TempDir()
	commands := []core.InitialWorkerCommand{
		{CommandID: "initial-model", Command: setConfig("model", "chosen"), Optional: false},
		{CommandID: "initial-brief", Command: prompt("owed task"), Optional: false},
	}
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	accepted := submitRelay(relay, "initial-model", commands[0].Command)
	if relay.snapshot.LatestPromptAcceptedOrdinal != nil {
		t.Fatal("no prompt was accepted")
	}
	rtNoErr(relay.persistSnapshot())
	rtRewriteRelayState(t, temp, func(saved map[string]any) {
		saved["format_version"] = 27
		delete(saved, "initial_command_ids")
	})
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "new", core.HarnessKindCodex, commands))
	rtEqual(t, relay.snapshot.HandledCommands["initial-model"].AcceptedOrdinal, accepted)
	if _, ok := relay.snapshot.HandledCommands["initial-brief"]; !ok {
		t.Fatal("the owed brief was not accepted")
	}
	frontier := relay.LatestOrdinal()
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "new", core.HarnessKindCodex, commands))
	rtEqual(t, relay.LatestOrdinal(), frontier)
	count := 0
	for _, queued := range relay.snapshot.QueuedPrompts {
		if queued.CommandID == "initial-brief" {
			count++
		}
	}
	rtEqual(t, count, 1)
}

// Hard-won: 92ab7be: clearing context and pruning receipts replayed the original child assignment.
func TestInitialBriefIsNotReseededAfterClearAndAckPruning(t *testing.T) {
	temp := t.TempDir()
	commands := []core.InitialWorkerCommand{{
		CommandID: "original-brief",
		Command:   prompt("original assignment"),
		Optional:  false,
	}}
	relay := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	setVerdictHarness(t, relay, core.HarnessKindCodex)
	rtMust(relay.RecordObservation(rtOpenedSession("original-thread")))
	rtMust(relay.ClaimPendingCommands(true))
	finishPrompt(relay, "original-brief")
	// The latest-turn owner must not accidentally protect the startup receipt.
	submitRelay(relay, "followup-prompt", prompt("later user request"))
	rtMust(relay.ClaimPendingCommands(true))
	finishPrompt(relay, "followup-prompt")
	submitRelay(relay, "clear-context", core.RelayCommandClearContext{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("clear-context", core.RelayCommandOutcomeContextCleared{
		NativeSessionID: "empty-after-clear",
		Memory:          nil,
	}))
	if relay.snapshot.NativeSessionUsed {
		t.Fatal("a cleared native session is unused")
	}
	for index := 0; index <= core.RelayRetryIDRetention; index++ {
		submitRelay(relay, fmt.Sprintf("later-command-%d", index), core.RelayCommandRecordNotice{Text: "later"})
	}
	latest := relay.LatestOrdinal()
	acknowledgeRelay(relay, "ack-after-clear", latest)
	if _, ok := relay.snapshot.HandledCommands["later-command-0"]; ok {
		t.Fatal("the oldest notice receipt was not pruned")
	}
	checkpointOnly := rtMust(DurableRelayOpenForCheckpoint(temp, session, "test"))
	acknowledgeRelay(checkpointOnly, "checkpoint-only-ack", latest)
	if _, ok := checkpointOnly.snapshot.HandledCommands["original-brief"]; !ok {
		t.Fatal("the startup receipt was pruned")
	}
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	rtEqual(t, relay.LatestOrdinal(), latest)
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("the brief was reseeded: %#v", claimed)
	}
}

// Hard-won: #1394: a mailbox wake ran between startup selectors without the child's original brief.
func TestInitialFifoCompletesACrashedPrefixAndKeepsMailAfterTheBrief(t *testing.T) {
	temp := t.TempDir()
	commands := initialFifoCommands()
	// Crash halfway through initialization, before any endpoint is published.
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	modelOrdinal := submitRelay(relay, commands[0].CommandID, commands[0].Command)
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	rtEqual(t, relay.snapshot.HandledCommands["initial-model"].AcceptedOrdinal, modelOrdinal)
	briefOrdinal := relay.snapshot.HandledCommands["initial-brief"].AcceptedOrdinal
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	rtEqual(t, relay.snapshot.HandledCommands["initial-brief"].AcceptedOrdinal, briefOrdinal)
	submitRelay(relay, "early-addendum", core.RelayCommandDeliverMailboxEvent{Event: mailboxFixtureEvent("early-addendum", true)})
	for _, expected := range []string{"initial-model", "initial-effort", "initial-fast-mode"} {
		claimed := rtMust(relay.ClaimPendingCommands(true))
		if len(claimed) != 1 {
			t.Fatalf("%#v", claimed)
		}
		rtEqual(t, claimed[0].CommandID, expected)
		if relay.snapshot.ActivePrompt != nil {
			t.Fatal("a configuration command is not a prompt")
		}
		if expected == "initial-fast-mode" {
			rtMust(relay.RecordCommandRejected(expected, core.OutcomeReasonCommandFailed, "fast mode is not offered"))
		} else {
			rtMust(relay.RecordCommandCompleted(expected, core.RelayCommandOutcomeConfigured{}))
		}
	}
	claimed := rtMust(relay.ClaimPendingCommands(true))
	if len(claimed) != 1 {
		t.Fatalf("%#v", claimed)
	}
	rtEqual(t, claimed[0].CommandID, "initial-brief")
	rtEqual(t, claimed[0].Command, commands[len(commands)-1].Command)
	if claimed[0].HiddenPromptContext == nil || !strings.Contains(*claimed[0].HiddenPromptContext, "early-addendum") {
		t.Fatalf("the brief does not carry the early mail: %v", claimed[0].HiddenPromptContext)
	}
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("mail is still pending: %#v", relay.snapshot.PendingMailboxEvents)
	}
	// Once the brief ran, a normal turn interruption must not make it an
	// initialization failure or cause it to be seeded again on recovery.
	rtMust(relay.RecordCommandInterrupted("initial-brief", core.OutcomeReasonWorkerRestarted, "worker stopped during its first turn"))
	relay = rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	recovery := rtMust(relay.ClaimPendingCommands(true))
	if len(recovery) != 1 {
		t.Fatalf("%#v", recovery)
	}
	rtEqual(t, recovery[0].CommandID, core.RestartContinuationID("initial-brief"))
	blocks, ok := core.RelayCommandPromptBlocksForCommand(recovery[0].Command, recovery[0].CommandID)
	if !ok {
		t.Fatal("the recovery carries no prompt")
	}
	brief, _ := core.RelayCommandPromptBlocks(commands[3].Command)
	rtEqual(t, blocks[1:], brief)
	if blocks[0].Text == nil || !strings.HasPrefix(blocks[0].Text.Text, "[HARNESS NOTE: interrupted turn recovery]") {
		t.Fatalf("unexpected recovery note %#v", blocks[0])
	}
	rtEqual(t, relay.snapshot.HandledCommands["initial-brief"].AcceptedOrdinal, briefOrdinal)
}

func TestInitialFifoDoesNotReleaseTheBriefAfterRequiredConfigurationFails(t *testing.T) {
	temp := t.TempDir()
	commands := initialFifoCommands()
	relay := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].CommandID, "initial-model")
	_, err := relay.RecordCommandRejected("initial-model", core.OutcomeReasonCommandFailed, "provider refused the selected model")
	if err == nil || !strings.Contains(err.Error(), "provider refused the selected model") {
		t.Fatalf("unexpected rejection result %v", err)
	}
	rtEqual(t, relay.snapshot.Dispatches["initial-brief"].State, core.RelayDispatchStateQueued)
	_, err = DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, commands)
	if err == nil {
		t.Fatal("failed startup must not run the brief after restart")
	}
	if !strings.Contains(err.Error(), "provider refused the selected model") {
		t.Fatalf("unexpected startup error %v", err)
	}
	// Checkpoint recovery still owns and can preserve this failed worker's data.
	rtMust(DurableRelayOpenForCheckpointWithInitialCommands(temp, session, "test", commands))
}

func TestIdleUpgradeReservationDefersWithoutDelayingSteering(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "running-command", prompt("work"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "next-command", prompt("change direction"))
	before := relay.OperationalState().LatestOrdinal
	response := relay.Handle(relayRequest("reserve", core.RelayRequestReserveIdle{CommandID: "upgrade-command"}))
	if reservation, ok := rtOkPayload(t, response).(core.RelayResponsePayloadIdleReservation); !ok || reservation.Ordinal != nil {
		t.Fatalf("unexpected reservation %#v", response.Body)
	}
	rtEqual(t, relay.OperationalState().LatestOrdinal, before)
	if relay.OperationalState().CheckpointBarrier != nil {
		t.Fatal("a deferred reservation must not raise a barrier")
	}
	submitRelay(relay, "steer-command", core.RelayCommandSteer{ActivePromptID: "running-command", QueuedPromptID: "next-command"})
	claimed := rtMust(relay.ClaimPendingCommands(true))
	if len(claimed) != 1 {
		t.Fatalf("%#v", claimed)
	}
	rtEqual(t, claimed[0].CommandID, "steer-command")
	rtEqual(t, claimed[0].SteeringPrompt.QueuedCommandID, "next-command")
}

// rtSessionUpdateJSON decodes a session update the way the Rust tests build
// one with serde_json::from_value(json!(...)).
func rtSessionUpdateJSON(text string) schema.SessionUpdate {
	var update schema.SessionUpdate
	if err := json.Unmarshal([]byte(text), &update); err != nil {
		panic(fmt.Sprintf("decode session update %s: %v", text, err))
	}
	return update
}

// rtIsIdle is activity_state().is_idle() on an operational state.
func rtIsIdle(state core.RelayOperationalState) bool {
	activity := state.ActivityState()
	return activity.IsIdle()
}

func rtPromptOutcome(stopReason string) core.RelayCommandOutcomePrompt {
	return core.RelayCommandOutcomePrompt{StopReason: stopReason, Diagnostic: nil, Usage: nil, WorkFacts: nil}
}

func TestIdleUpgradeReservationIsIdempotentAndDisconnectReleasesQueuedWork(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	reserve := core.RelayRequestReserveIdle{CommandID: "upgrade-command"}
	response := relay.Handle(relayRequest("reserve", reserve))
	reservation, ok := rtOkPayload(t, response).(core.RelayResponsePayloadIdleReservation)
	if !ok || reservation.Ordinal == nil {
		t.Fatalf("idle worker refused reservation: %#v", response.Body)
	}
	first := *reservation.Ordinal
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtMust(relay.RecordCheckpointReady("upgrade-command"))
	response = relay.Handle(relayRequest("retry", reserve))
	retried, ok := rtOkPayload(t, response).(core.RelayResponsePayloadIdleReservation)
	if !ok || retried.Ordinal == nil || *retried.Ordinal != first {
		t.Fatalf("a retried reservation must return the first ordinal: %#v", response.Body)
	}
	submitRelay(relay, "after-command", prompt("next turn"))
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("the barrier must hold queued work: %#v", claimed)
	}
	rtMust2(relay.CancelCheckpointBarrierOnDisconnect("upgrade-command"))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "after-command")
}

func TestClassifierInputHandoffPreservesRunningChildrenAndStopControls(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	submitRelay(relay, "approval", prompt("Prepare deployment while analyzing the heap independently."))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventSpawned{
		SessionID:       "heap",
		ParentSessionID: nil,
		Name:            "heap",
		Task:            "Independent heap analysis",
		Capabilities:    core.NativeAgentCapabilities{Cancel: true, Close: false},
	}}))
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventState{
		SessionID: "heap",
		State:     core.NativeAgentStateRunning,
	}}))
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("build", "Independent build")}))
	rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateChanged{
		TaskID:      "build",
		Description: nil,
		CanStop:     rtBool(true),
	}))
	before := relay.OperationalState()
	rtEqual(t, before.NativeAgentCount, 1)
	rtMust(relay.RecordCommandCompleted("approval", rtPromptOutcome(core.AwaitingInputStopReason)))
	after := relay.OperationalState()
	if !rtIsIdle(after) {
		t.Fatalf("%#v", after)
	}
	rtEqual(t, after.NativeAgents, before.NativeAgents)
	rtEqual(t, after.NativeAgentCount, 1)
	rtEqual(t, after.BackgroundCommands, before.BackgroundCommands)
	if _, err := relay.BackgroundTaskStopTarget("native-agent:heap"); err != nil {
		t.Fatalf("the running child must stay stoppable: %v", err)
	}
	if _, err := relay.BackgroundTaskStopTarget("air:4:root:build"); err != nil {
		t.Fatalf("the running task must stay stoppable: %v", err)
	}
	if after.IsQuiet() {
		t.Fatal("running children keep the session from being quiet")
	}
	if _, pending := relay.PendingRepliedVerdict(); pending {
		t.Fatal("a handoff must not ask for a replied verdict")
	}
	// Repeated child output must not undo the accepted parent handoff.
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventUpdate{
		SessionID: "heap",
		Update:    rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Still analyzing."}}`),
	}}))
	if !rtIsIdle(relay.OperationalState()) {
		t.Fatal("child output undid the parent handoff")
	}
}

func TestInferredFinishedHandoffMarksTheTrackedTurnIdle(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "finished-prompt", prompt("Finish the implementation."))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("finished-prompt", rtPromptOutcome(core.InferredFinishedStopReason)))
	if !rtIsIdle(relay.OperationalState()) {
		t.Fatal("an inferred finish is idle")
	}
}

func TestRunningTurnContextTracksDurableAuthorizationUpdates(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	turnContext := relay.TurnContext()
	submitRelay(relay, "authorization-prompt", prompt("Finish the report and include the test results."))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	authorization := turnContext.AuthorizationContext()
	if authorization == nil {
		t.Fatal("a running prompt has an authorization context")
	}
	rtEqual(t, len(authorization.Messages), 1)
	rtEqual(t, authorization.Messages[0].Role, "user")
	rtEqual(t, authorization.Messages[0].Text, "Finish the report and include the test results.")

	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"The report is complete."}}`)))
	authorization = turnContext.AuthorizationContext()
	if authorization == nil {
		t.Fatal("a running prompt has an authorization context")
	}
	last := authorization.Messages[len(authorization.Messages)-1]
	rtEqual(t, last.Role, "assistant")
	rtEqual(t, last.Text, "The report is complete.")
}

func TestNativeReplayDoesNotPublishProvisionalWorkOrLoseRetainedAgents(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	spawn := func(id string) core.NativeAgentEvent {
		return core.NativeAgentEventSpawned{
			SessionID:       id,
			ParentSessionID: nil,
			Name:            id,
			Task:            "task",
			Capabilities:    core.NativeAgentCapabilities{},
		}
	}
	for _, event := range []core.NativeAgentEvent{
		spawn("retained"),
		core.NativeAgentEventState{SessionID: "retained", State: core.NativeAgentStateCompleted},
		core.NativeAgentEventReplayBegin{},
		spawn("replayed"),
	} {
		rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: event}))
	}
	rtEqual(t, relay.OperationalState().NativeAgentCount, 0)
	rtEqual(t, len(relay.OperationalState().NativeAgents), 1)
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventReplayCommit{}}))
	rtEqual(t, len(relay.OperationalState().NativeAgents), 2)
	rtEqual(t, relay.OperationalState().NativeAgentCount, 0)
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventDisconnected{}}))
	rtEqual(t, relay.OperationalState().NativeAgentCount, 0)
}

func TestUncertainSteeringSurvivesRestartAndRequiresExplicitRetry(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "prompt-first", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "prompt-queued", prompt("next"))
	submitRelay(relay, "checkpoint-earlier", core.RelayCommandBeginCheckpoint{Reason: nil})
	steer := core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-queued"}
	submitRelay(relay, "steer-first", steer)
	_, refusal, err := relay.submitCommand("steer-repeat", steer)
	rtNoErr(err)
	if refusal == nil {
		t.Fatal("a second steer of the same prompt must be refused")
	}
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	if relay.snapshot.CheckpointBarrier != nil {
		t.Fatal("the steer runs before the queued checkpoint")
	}
	rtEqual(t, claimed[0].SteeringPrompt.QueuedCommandID, "prompt-queued")
	rtMust(relay.RecordCommandInterrupted("steer-first", core.OutcomeReasonRuntimeFailure, "connection lost"))
	rtMust(relay.RecordCommandInterrupted("prompt-first", core.OutcomeReasonRuntimeFailure, "harness restarted"))
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("uncertain steering must hold the queue: %#v", claimed)
	}
	_, refusal, err = relay.submitCommand("checkpoint-held", core.RelayCommandBeginCheckpoint{Reason: nil})
	rtNoErr(err)
	if refusal == nil {
		t.Fatal("a checkpoint must wait for uncertain steering")
	}
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusUnconfirmed)
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("uncertain steering must hold the queue after restart: %#v", claimed)
	}
	submitRelay(relay, "resolve-steering", core.RelayCommandResolveSteering{SteeringID: "steer-first"})
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-queued")
}

func TestQueuedPromptsAreSteeredIntoTheRunningTurnOneAtATime(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	queuePrompt(t, relay, "prompt-second", "also this")
	steer := claimedSteerOf(t, rtMust(relay.ClaimPendingCommands(true)), "prompt-second")
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusPending)

	queuePrompt(t, relay, "prompt-third", "and this")
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("a second steer waits for the first to settle: %#v", claimed)
	}

	rtMust(relay.RecordCommandCompleted(steer, core.RelayCommandOutcomeSteered{QueuedCommandID: "prompt-second"}))
	claimedSteerOf(t, rtMust(relay.ClaimPendingCommands(true)), "prompt-third")
}

func TestAutomaticSteeringStopsForATurnThatReturnedASteer(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	queuePrompt(t, relay, "prompt-second", "also this")
	steer := claimedSteerOf(t, rtMust(relay.ClaimPendingCommands(true)), "prompt-second")
	rtMust(relay.RecordCommandCompleted(steer, core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: "prompt-second"}))
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("%#v", claimed)
	}

	// The returned prompt runs as the next turn.
	rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("EndTurn")))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-second")
}

// Hard-won: #1473: DSH held ready oracle answers until the lead's hours-long turn ended.
func TestDshMailboxUsesDurableSteeringAndKeepsReturnedInputQueued(t *testing.T) {
	for _, returned := range []bool{false, true} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindDsh, nil))
		relay.SetSteeringSupported(rtBool(true))
		relay.SetAutomaticSteering(true)
		queuePrompt(t, relay, "prompt-first", "work continues")
		rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
		first := mailboxFixtureEvent("oracle-answer", true)
		submitRelay(relay, "mailbox-first", core.RelayCommandDeliverMailboxEvent{Event: first})
		claimed := rtMust(relay.ClaimPendingCommands(true))
		wake := claimed[0].SteeringPrompt.QueuedCommandID
		steer := claimedSteerOf(t, claimed, wake)
		firstWake, _ := core.RelayCommandPromptBlocks(core.RelayCommandMailboxWake{Events: []core.MailboxEvent{first}})
		rtEqual(t, claimed[0].SteeringPrompt.Prompt, firstWake)
		rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "prompt-first")
		if _, count := drainMailboxForTest(t, relay, "competing-hook"); count != 0 {
			t.Fatalf("a competing hook drained %d events", count)
		}

		submitRelay(relay, "mailbox-first-retry", core.RelayCommandDeliverMailboxEvent{Event: first})
		second := mailboxFixtureEvent("child-handback", true)
		submitRelay(relay, "mailbox-second", core.RelayCommandDeliverMailboxEvent{Event: second})
		if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
			t.Fatalf("%#v", claimed)
		}
		var outcome core.RelayCommandOutcome = core.RelayCommandOutcomeSteered{QueuedCommandID: wake}
		if returned {
			outcome = core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: wake}
		}
		rtMust(relay.RecordCommandCompleted(steer, outcome))
		if returned {
			if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
				t.Fatalf("%#v", claimed)
			}
			rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("EndTurn")))
			claimed := rtMust(relay.ClaimPendingCommands(true))
			rtEqual(t, len(claimed), 1)
			rtEqual(t, claimed[0].CommandID, wake)
			rtEqual(t, claimed[0].HiddenPromptContext, rtString(core.RenderMailboxEvents([]core.MailboxEvent{second})))
		} else {
			claimed := rtMust(relay.ClaimPendingCommands(true))
			rtEqual(t, len(claimed), 1)
			steering := claimed[0].SteeringPrompt
			secondWake, _ := core.RelayCommandPromptBlocks(core.RelayCommandMailboxWake{Events: []core.MailboxEvent{second}})
			rtEqual(t, steering.Prompt, secondWake)
			if steering.QueuedCommandID == wake {
				t.Fatalf("the second wake reused %s", wake)
			}
			rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "prompt-first")
		}
		if len(relay.snapshot.PendingMailboxEvents) != 0 {
			t.Fatalf("mail is still pending: %#v", relay.snapshot.PendingMailboxEvents)
		}
	}
}

func TestClaudeAutomaticSteerIsDeliveredAndCompletedByPostToolBatch(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	relay.SetAutomaticSteering(true)
	queuePrompt(t, relay, "prompt-second", "Please change direction.")

	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("%#v", claimed)
	}
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusPending)
	text, count, leaseID := drainPostToolBatch(t, relay)
	rtEqual(t, count, 1)
	rtEqual(t, text, rtString("<user_message_sent_while_working>\nPlease change direction.\n</user_message_sent_while_working>"))
	if leaseID == nil {
		t.Fatal("a boundary steer creates a hook lease")
	}
	queued := false
	for _, prompt := range relay.snapshot.QueuedPrompts {
		queued = queued || prompt.CommandID == "prompt-second"
	}
	if !queued {
		t.Fatal("the steered prompt stays queued until the lease is acknowledged")
	}
	acknowledgeMailboxForTest(t, relay, "boundary-ack", *leaseID)

	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusApplied)
	if len(relay.snapshot.QueuedPrompts) != 0 {
		t.Fatalf("%#v", relay.snapshot.QueuedPrompts)
	}
	if _, ok := relay.snapshot.Dispatches["prompt-second"]; ok {
		t.Fatal("the applied prompt is no longer dispatched")
	}
	if relay.snapshot.HandledCommands["prompt-second"].TerminalOrdinal == nil {
		t.Fatal("the applied prompt is terminal")
	}
	delivered := 0
	for _, event := range retainedEvents(relay) {
		if observation, ok := event.Observation.(core.RelayObservationMailboxEventsDelivered); ok && observation.Steering != nil {
			delivered++
		}
	}
	rtEqual(t, delivered, 1)

	replayed := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindClaude, nil))
	rtEqual(t, replayed.OperationalState().Steering.Status, core.SteeringStatusApplied)
	// Restart recovery waits for the replacement bridge; acknowledged steering
	// remains compacted and must not become another queued request.
	rtEqual(t, len(replayed.snapshot.QueuedPrompts), 1)
	if !core.IsRestartContinuation(replayed.snapshot.QueuedPrompts[0].CommandID) {
		t.Fatalf("unexpected queued prompt %s", replayed.snapshot.QueuedPrompts[0].CommandID)
	}
	rtMust(replayed.RecordObservation(rtSessionConfigured()))
	rtMust(replayed.ClaimPendingCommands(true))
	if len(replayed.snapshot.QueuedPrompts) != 0 {
		t.Fatalf("%#v", replayed.snapshot.QueuedPrompts)
	}
	if _, count, _ := drainPostToolBatch(t, replayed); count != 0 {
		t.Fatalf("the applied steer was delivered again (%d)", count)
	}
}

// rtSessionConfigured is RelayObservation::SessionConfigured with no options.
func rtSessionConfigured() core.RelayObservationSessionConfigured {
	return core.RelayObservationSessionConfigured{ConfigOptions: []schema.SessionConfigOption{}}
}

// rtMust2 is rtMust for a call with two results besides its error.
func rtMust2[A, B any](first A, second B, err error) (A, B) {
	rtNoErr(err)
	return first, second
}

// rtOnlyClaimed is `let [only] = claimed.as_slice() else { panic!(...) }`.
func rtOnlyClaimed(t *testing.T, claimed []core.ClaimedRelayCommand, why string) core.ClaimedRelayCommand {
	t.Helper()
	if len(claimed) != 1 {
		t.Fatalf("%s, got %#v", why, claimed)
	}
	return claimed[0]
}

func rtNoClaims(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("expected no claims, got %#v", claimed)
	}
}

// rtSteeredDeliveries counts retained MailboxEventsDelivered events that
// delivered a steer.
func rtSteeredDeliveries(relay *DurableRelay) int {
	delivered := 0
	for _, event := range retainedEvents(relay) {
		if observation, ok := event.Observation.(core.RelayObservationMailboxEventsDelivered); ok && observation.Steering != nil {
			delivered++
		}
	}
	return delivered
}

func TestClaudeExplicitSteerIsDeliveredAtThePostToolBatchBoundary(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	queuePrompt(t, relay, "prompt-second", "Please take this into account.")
	submitRelay(relay, "explicit-steer", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-second"})

	rtNoClaims(t, relay)
	text, count, leaseID := drainPostToolBatch(t, relay)
	rtEqual(t, count, 1)
	rtEqual(t, text, rtString("<user_message_sent_while_working>\nPlease take this into account.\n</user_message_sent_while_working>"))
	if leaseID == nil {
		t.Fatal("explicit steer needs a PostToolBatch lease")
	}
	acknowledgeMailboxForTest(t, relay, "explicit-steer-ack", *leaseID)
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusApplied)
	if len(relay.snapshot.QueuedPrompts) != 0 {
		t.Fatalf("%#v", relay.snapshot.QueuedPrompts)
	}
}

func TestAdmittedClaudeSteerReturnsIfItsBoundaryHookBecomesUnavailable(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	queuePrompt(t, relay, "prompt-second", "Continue with this request.")
	submitRelay(relay, "explicit-steer", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-second"})
	// Availability can change after an operation has already been admitted.
	relay.SetSteeringDeliveryMode(core.HarnessKindClaude, false)
	rtNoClaims(t, relay)

	rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("EndTurn")))
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusResolved)
	next := rtOnlyClaimed(t, rtMust(relay.ClaimPendingCommands(true)), "the returned prompt must run as the next normal command")
	rtEqual(t, next.CommandID, "prompt-second")
	if _, ok := next.Command.(core.RelayCommandPrompt); !ok {
		t.Fatalf("expected a prompt, got %#v", next.Command)
	}
	if next.SteeringPrompt != nil {
		t.Fatal("a returned steer runs as a normal prompt")
	}
}

// Hard-won: d3fde1066: a tool boundary during the interrupt fed the steer to the dying turn.
func TestInterruptingAPendingClaudeSteerRunsItsPromptNext(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	relay.SetAutomaticSteering(true)
	queuePrompt(t, relay, "prompt-second", "Please change direction.")
	rtNoClaims(t, relay)

	controls := relay.OperationalState().TurnControls
	rtEqual(t, controls.Status, core.TurnControlStatusPending)
	if controls.Interrupt == nil {
		t.Fatal("a pending steer stays interruptible")
	}
	submitRelay(relay, "interrupt", controls.Interrupt)
	cancel := rtOnlyClaimed(t, rtMust(relay.ClaimPendingCommands(true)), "the interrupt must reach the running turn")
	rtEqual(t, cancel.CommandID, "interrupt")

	if _, count, _ := drainPostToolBatch(t, relay); count != 0 {
		t.Fatalf("the interrupted turn received the steer (%d)", count)
	}
	rtMust(relay.RecordCommandCompleted("interrupt", core.RelayCommandOutcomeCancelled{}))
	rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("Cancelled")))

	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusResolved)
	next := rtOnlyClaimed(t, rtMust(relay.ClaimPendingCommands(true)), "the returned prompt must run next")
	rtEqual(t, next.CommandID, "prompt-second")
}

func TestClaudeAttachmentSteerIsLeftForTheNextNormalPrompt(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	submitRelay(relay, "prompt-with-attachment", core.RelayCommandPrompt{Prompt: []schema.ContentBlock{
		{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: "Review this image."}},
		{Kind: schema.ContentBlockKindImage, Image: &schema.ImageContent{Data: "aW1hZ2U=", MimeType: "image/png"}},
	}})
	rtEqual(t, relay.OperationalState().TurnControls.Primary.Kind, core.TurnControlKindApplyQueued)
	_, refusal, err := relay.submitCommand("attachment-steer", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-with-attachment"})
	rtNoErr(err)
	if refusal == nil {
		t.Fatal("an attachment prompt cannot be steered into a Claude turn")
	}
	rtNoClaims(t, relay)
	text, count, leaseID := drainPostToolBatch(t, relay)
	if text != nil || count != 0 || leaseID != nil {
		t.Fatalf("unexpected drain (%v, %d, %v)", text, count, leaseID)
	}

	rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("EndTurn")))
	next := rtOnlyClaimed(t, rtMust(relay.ClaimPendingCommands(true)), "the attachment prompt must run after the turn ends")
	rtEqual(t, next.CommandID, "prompt-with-attachment")
	if _, ok := next.Command.(core.RelayCommandPrompt); !ok {
		t.Fatalf("expected a prompt, got %#v", next.Command)
	}
	if next.SteeringPrompt != nil {
		t.Fatal("the attachment prompt runs as a normal prompt")
	}
}

func TestCodexSteerStillClaimsAnAcpSteeringPrompt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindCodex, nil))
	setVerdictHarness(t, relay, core.HarnessKindCodex)
	relay.SetSteeringDeliveryMode(core.HarnessKindCodex, false)
	submitRelay(relay, "prompt-first", prompt("work"))
	rtMust(relay.ClaimPendingCommands(true))
	queuePrompt(t, relay, "prompt-second", "Change direction.")
	submitRelay(relay, "codex-steer", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-second"})

	claimed := rtOnlyClaimed(t, rtMust(relay.ClaimPendingCommands(true)), "Codex must keep claiming its ACP steer command")
	rtEqual(t, claimed.CommandID, "codex-steer")
	if claimed.SteeringPrompt == nil || claimed.SteeringPrompt.QueuedCommandID != "prompt-second" {
		t.Fatalf("unexpected steering prompt %#v", claimed.SteeringPrompt)
	}
}

func TestBoundarySteerLeaseTimeoutThenAckReplayDoesNotDeliverTwice(t *testing.T) {
	temp := t.TempDir()
	relay := runningClaudeTurn(t, temp, true)
	queuePrompt(t, relay, "prompt-second", "One boundary delivery.")
	submitRelay(relay, "timeout-steer", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-second"})
	rtNoClaims(t, relay)
	_, _, firstLease := drainPostToolBatch(t, relay)
	if firstLease == nil {
		t.Fatal("first boundary hook leases the steer")
	}
	relay.snapshot.MailboxHookLease.ExpiresAtMs = 0
	if !rtMust(relay.ExpireMailboxHookLeaseAndPromote(1)) {
		t.Fatal("the expired lease must be returned")
	}
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusPending)

	_, _, retryLease := drainPostToolBatch(t, relay)
	if retryLease == nil {
		t.Fatal("an expired lease can be retried")
	}
	acknowledgeMailboxForTest(t, relay, "timeout-steer-ack", *retryLease)
	stale := rtMust(relay.ackMailbox(*firstLease))
	if acknowledged, ok := stale.(core.RelayResponsePayloadMailboxAcknowledged); !ok || acknowledged.Acknowledged {
		t.Fatalf("a stale lease must not acknowledge: %#v", stale)
	}
	if len(relay.snapshot.QueuedPrompts) != 0 {
		t.Fatalf("%#v", relay.snapshot.QueuedPrompts)
	}
	rtEqual(t, rtSteeredDeliveries(relay), 1)

	replayed := rtMust(DurableRelayOpenForHarness(temp, session, "test", core.HarnessKindClaude, nil))
	rtEqual(t, replayed.OperationalState().Steering.Status, core.SteeringStatusApplied)
	if _, count, _ := drainPostToolBatch(t, replayed); count != 0 {
		t.Fatalf("the applied steer was delivered again (%d)", count)
	}
}

func TestSlashCommandsAndWaitingCheckpointsAreNotSteered(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	queuePrompt(t, relay, "prompt-review", "/review the parser")
	rtNoClaims(t, relay)

	temp = t.TempDir()
	relay = relaySteeringAutomatically(t, temp)
	submitRelay(relay, "checkpoint-1", core.RelayCommandBeginCheckpoint{Reason: nil})
	queuePrompt(t, relay, "prompt-second", "/home/me/notes.txt is wrong")
	for _, claimed := range rtMust(relay.ClaimPendingCommands(true)) {
		if _, steer := claimed.Command.(core.RelayCommandSteer); steer {
			t.Fatal("a checkpoint is waiting for this turn to end")
		}
	}
}

func TestAPathIsAMessageAndACommandNameIsNot(t *testing.T) {
	prompt := func(text string) []schema.ContentBlock {
		return []schema.ContentBlock{{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}}
	}
	if !core.PromptIsSlashCommand(prompt("/review")) {
		t.Fatal("/review is a command")
	}
	if !core.PromptIsSlashCommand(prompt("  /plugin:run now")) {
		t.Fatal("/plugin:run is a command")
	}
	if core.PromptIsSlashCommand(prompt("/home/me/file")) {
		t.Fatal("a path is a message")
	}
	if core.PromptIsSlashCommand(prompt("/ nothing")) {
		t.Fatal("a bare slash is a message")
	}
	if core.PromptIsSlashCommand(prompt("fix /review")) {
		t.Fatal("a command after text is a message")
	}
}

func TestAReturnedSteerKeepsThePromptQueuedForTheNextTurn(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "prompt-first", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "prompt-queued", prompt("next"))
	submitRelay(relay, "steer-first", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-queued"})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("steer-first", core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: "prompt-queued"}))

	// Nothing was delivered and nothing waits on the user.
	state := relay.OperationalState()
	steering := state.Steering
	rtEqual(t, steering.Status, core.SteeringStatusResolved)
	if steering.Message != nil {
		t.Fatalf("unexpected steering message %q", *steering.Message)
	}
	rtEqual(t, len(state.QueuedPrompts), 1)
	rtEqual(t, state.QueuedPrompts[0].CommandID, "prompt-queued")

	rtMust(relay.RecordCommandCompleted("prompt-first", rtPromptOutcome("EndTurn")))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-queued")
}

// rtRefused asserts that submit_command answered with a protocol refusal.
func rtRefused(t *testing.T, relay *DurableRelay, commandID string, command core.RelayCommand) {
	t.Helper()
	_, refusal, err := relay.submitCommand(commandID, command)
	rtNoErr(err)
	if refusal == nil {
		t.Fatalf("%s must be refused", commandID)
	}
}

func TestSteeringRejectsChangedQueueAndConsumesLateConfirmedInputOnce(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "prompt-first", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "prompt-queued", prompt("next"))
	rtRefused(t, relay, "steer-stale", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "changed-queue"})
	submitRelay(relay, "steer-first", core.RelayCommandSteer{ActivePromptID: "prompt-first", QueuedPromptID: "prompt-queued"})
	rtMust(relay.ClaimPendingCommands(true))
	rtRefused(t, relay, "remove-pending", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "prompt-queued"})
	rtMust(relay.RecordObservation(core.RelayObservationSteeringUnconfirmed{CommandID: "steer-first", Message: "slow acknowledgment"}))
	rtMust(relay.RecordCommandCompleted("steer-first", core.RelayCommandOutcomeSteered{QueuedCommandID: "prompt-queued"}))
	if queued := relay.OperationalState().QueuedPrompts; len(queued) != 0 {
		t.Fatalf("%#v", queued)
	}
	rtEqual(t, relay.OperationalState().Steering.Status, core.SteeringStatusApplied)
	rtMust(relay.RecordCommandInterrupted("prompt-first", core.OutcomeReasonRuntimeFailure, "stopped"))
	rtNoClaims(t, relay)
	rtRefused(t, relay, "cancel-stale", core.RelayCommandCancelTurnFor{ActivePromptID: "prompt-first"})
}

// Hard-won: 0cfac73c: resuming an unused Codex thread without a rollout caused an endless worker restart loop.
func TestALocallyCreatedEmptyNativeSessionHasNoHistory(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtOpenedSession("unused")))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a fresh session has no history")
	}
	// A prompt waiting in the durable queue never reached the agent.
	submitRelay(relay, "queued-prompt", prompt("queued work"))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a queued prompt gives no history")
	}
}

func TestADispatchedPromptGivesTheNativeSessionHistory(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtOpenedSession("used")))
	submitRelay(relay, "dispatched-prompt", prompt("do work"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("a dispatched prompt gives history")
	}
}

func TestAReleasedRecoveryFloorDoesNotOverrideKnownUnusedNativeEvidence(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtOpenedSession("unused")))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a fresh session has no history")
	}
	cursor := readyCheckpoint(relay, "checkpoint")
	submitFloor(relay, "archive-installed", cursor)
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a released floor must not invent history")
	}
}

// Hard-won: c82d9df3: an empty Claude session opened above an archive floor was mistaken for used history.
func TestANativeSessionOpenedAfterARestoreFloorHasNoHistory(t *testing.T) {
	// A moved session starts from an archive seed, which sets the recovery
	// floor to the archive's frontier, and then opens a brand-new native
	// session above it. Nothing in the archive can belong to that session.
	temp := t.TempDir()
	rtNoErr(os.WriteFile(core.RestoredRelaySeedPath(temp), []byte(`{"event_frontier":227580,"event_frontier_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}`), 0o644))

	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, uint64(227_580))
	// Before any session is opened the answer is unknown, so conservative.
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("an unknown session may have history")
	}
	rtMust(relay.RecordObservation(rtOpenedSession("fresh")))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("a session opened above the floor has no history")
	}

	// The opening ordinal must survive a worker restart and journal replay.
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("the opening ordinal was lost on restart")
	}

	submitRelay(relay, "dispatched-prompt", prompt("do work"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("a dispatched prompt gives history")
	}
}

func TestAResumedNativeSessionHasHistoryAfterReopening(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	opened := rtOpenedSession("imported")
	opened.Resumed = true
	rtMust(relay.RecordObservation(opened))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("a resumed session may have history")
	}
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("a resumed session may have history after reopening")
	}
}

func TestAUsedNativeSessionStaysUsedAcrossPersistAndReplay(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtOpenedSession("used")))
	rtNoErr(relay.MarkNativeSessionUsed())
	// Transcript events after the mark are replayed on reopen, and replay
	// must not undo it.
	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "after the mark"}))
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("a used session may have history")
	}
	var saved map[string]any
	rtNoErr(json.Unmarshal(rtMust(os.ReadFile(filepath.Join(temp, "relay-state.json"))), &saved))
	if used, ok := saved["native_session_used"].(bool); !ok || !used {
		t.Fatalf("native_session_used is %v", saved["native_session_used"])
	}
}

// Hard-won: 0f070506: Zcode could not resume its native session and crash-looped after restart or restore.
func TestLostNativeContinuitySurvivesReopenAndClearsOnANormalOpen(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if relay.OperationalState().NativeContinuityLost {
		t.Fatal("a fresh relay has continuity")
	}
	lost := rtOpenedSession("fresh")
	lost.NativeContinuityLost = true
	rtMust(relay.RecordObservation(lost))
	if !relay.OperationalState().NativeContinuityLost {
		t.Fatal("continuity was lost")
	}

	// A controller that reconnects later still sees it.
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if !relay.OperationalState().NativeContinuityLost {
		t.Fatal("lost continuity must survive a reopen")
	}

	rtMust(relay.RecordObservation(rtOpenedSession("later")))
	if relay.OperationalState().NativeContinuityLost {
		t.Fatal("a normal open restores continuity")
	}
}

func TestJournalsWithoutLostNativeContinuityDefaultToIntact(t *testing.T) {
	observation := rtMust(core.UnmarshalRelayObservationJSON([]byte(`{"type":"session_opened","data":{"native_session_id":"native","resumed":false}}`)))
	rtEqual(t, observation, core.RelayObservation(rtOpenedSession("native")))

	snapshot := core.NewRelaySnapshot(session)
	snapshot.NativeContinuityLost = true
	var stored map[string]json.RawMessage
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(snapshot)), &stored))
	if _, ok := stored["native_continuity_lost"]; !ok {
		t.Fatal("the flag is serialized")
	}
	delete(stored, "native_continuity_lost")
	var legacy core.RelaySnapshot
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(stored)), &legacy))
	if legacy.NativeContinuityLost {
		t.Fatal("a legacy snapshot has continuity")
	}
	if state := legacy.OperationalState(); state.NativeContinuityLost {
		t.Fatal("a legacy state has continuity")
	}
}

func TestHiddenPromptContextIsRemovedFromHarnessVisibleText(t *testing.T) {
	text := "<mj-project-memory>private memory</mj-project-memory>\n\n" +
		"<user_shell_command>private output</user_shell_command>\n" +
		"ship the visible change"

	rtEqual(t, core.StripHiddenPromptContext(text), "ship the visible change")
	rtEqual(t, core.StripHiddenPromptContext("<mj-project-memory>truncated"), "")
	rtEqual(t, core.StripHiddenPromptContext("<user-request>keep me</user-request>"), "<user-request>keep me</user-request>")
}

// Codex receives hidden context as an embedded resource, and codex-acp gives
// the model that block as `{uri}\n<context ref="{uri}">\n{text}\n</context>`.
// Codex keeps that text in its history of the prompt, where the Codex import
// reads it (content items joined by line breaks) and where codex-acp takes a
// fallback title from it after a load (joined by spaces, runs of white space
// collapsed).
//
// Hard-won: b5ae15db: a live Codex title was generated from private project-memory context.
func TestCodexWrappingOfHiddenContextIsRemovedFromHarnessVisibleText(t *testing.T) {
	const uri = core.HiddenPromptContextURI

	wrapped := func(context string) string {
		return uri + "\n<context ref=\"" + uri + "\">\n" + context + "\n</context>"
	}
	memory := "<mj-project-memory>\nprivate memory\n</mj-project-memory>"
	shell := "<user_shell_command>private output</user_shell_command>"
	handoff := "Archived session restored from SessionWiki. The work so far."

	for _, context := range []string{memory, memory + "\n\n" + shell, handoff} {
		history := wrapped(context) + "\nfirst prompt"
		rtEqual(t, core.StripHiddenPromptContext(history), "first prompt")
		title := strings.Join(strings.Fields(history), " ")
		rtEqual(t, core.StripHiddenPromptContext(title), "first prompt")
		normalized, ok := core.NormalizeSessionTitle(title)
		if !ok || normalized != "first prompt" {
			t.Fatalf("normalized title %q (%v)", normalized, ok)
		}
	}
	// A cut-off wrapping is hidden, wherever it was cut.
	whole := wrapped(memory)
	for _, cut := range []int{len(uri), len(uri) + 12, len(whole) - 3} {
		if got := core.StripHiddenPromptContext(whole[:cut]); got != "" {
			t.Fatalf("%d: %q", cut, got)
		}
	}
	// The user's own words about the URI or a context tag stay visible.
	for _, text := range []string{
		uri + " is where Mjolnir keeps it",
		"explain <context ref=\"" + uri + "\"> to me",
	} {
		rtEqual(t, core.StripHiddenPromptContext(text), text)
	}
}

// The note about the sub-agents a suspend stopped goes to the first prompt
// after the resume, and to no later one, even across a worker restart.
//
// Hard-won: d0e60898: a resumed parent still waited on sub-agents that suspend had stopped.
func TestAStoppedSubAgentsNoteReachesOnlyTheFirstPromptAfterAResume(t *testing.T) {
	temp := t.TempDir()
	note := stoppedSubagentsNote(t)
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtNoErr(relay.InstallPromptContext(note))

	submitRelay(relay, "first-prompt", prompt("carry on"))
	first := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, first[0].HiddenPromptContext, rtString(note))
	rtEqual(t, first[0].Command, prompt("carry on"))
	rtMust(relay.RecordCommandCompleted("first-prompt", rtPromptOutcome("end_turn")))

	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "second-prompt", prompt("and again"))
	second := rtMust(relay.ClaimPendingCommands(true))
	if second[0].HiddenPromptContext != nil {
		t.Fatalf("the note reached a later prompt: %q", *second[0].HiddenPromptContext)
	}
}

// A harness copies the hidden context into text a person sees: the start of
// the prompt in its own history, and the title it takes from that history.
// The note is removed there like project memory, whether it came as a text
// block or in codex-acp's wrapping of the `mj://prompt-context` resource.
func TestAStoppedSubAgentsNoteIsRemovedFromHarnessVisibleText(t *testing.T) {
	const uri = core.HiddenPromptContextURI

	note := stoppedSubagentsNote(t)
	if !strings.Contains(note, "Mjolnir stopped 2 sub-agents when this session was suspended:") {
		t.Fatalf("unexpected note %q", note)
	}
	memory := "<mj-project-memory>\nprivate memory\n</mj-project-memory>"
	for _, context := range []string{note, memory + "\n\n" + note} {
		textBlock := context + "\nfirst prompt"
		rtEqual(t, core.StripHiddenPromptContext(textBlock), "first prompt")
		codexHistory := uri + "\n<context ref=\"" + uri + "\">\n" + context + "\n</context>\nfirst prompt"
		rtEqual(t, core.StripHiddenPromptContext(codexHistory), "first prompt")
		title := strings.Join(strings.Fields(codexHistory), " ")
		rtEqual(t, core.StripHiddenPromptContext(title), "first prompt")
	}
	// A note cut off part way is hidden, not shown.
	rtEqual(t, core.StripHiddenPromptContext(note[:len(note)/2]), "")
}

func TestAcpActivityClockIsSharedWithOperationalStatusButNotPersisted(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if relay.OperationalState().LastACPActivityAtMs != nil {
		t.Fatal("a fresh relay has no ACP activity")
	}
	relay.AcpActivityClock().Mark()
	if relay.OperationalState().LastACPActivityAtMs == nil {
		t.Fatal("the mark is published")
	}

	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if reopened.OperationalState().LastACPActivityAtMs != nil {
		t.Fatal("ACP activity is not persisted")
	}
}

// The worker assembles activity facts directly, because the idle clock runs
// on every journal append and building a whole operational state there would
// clone the session configuration once per streamed chunk. That makes two
// places that translate a session into facts, so this pins them together: if
// one ever stops reporting a fact the other reports, this fails.
//
// Hard-won: 92f96a7f: the watchdog used a private tool map and disagreed with published worker activity.
func TestWorkerFactsMatchThePublishedState(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	fresh := relay.OperationalState()
	rtEqual(t, relay.ActivityFacts(), fresh.Facts())

	relay.AcpActivityClock().Mark()
	// The handle the ACP driver's watchdog holds is taken before the session
	// runs, exactly as the worker runtime takes it, and must see what the
	// relay records afterwards. If these ever stop being the same tracker,
	// the watchdog goes blind and a turn blocked in a long tool call is
	// failed again (#1020).
	watchdogView := relay.ToolsInFlight()
	if !watchdogView.IsEmpty() {
		t.Fatal("no tool is in flight yet")
	}
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1_234}))
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	published := relay.OperationalState()
	publishedFacts := published.Facts()
	rtEqual(t, relay.ActivityFacts(), publishedFacts)
	// The published answer is the one a consumer would compute.
	rtEqual(t, published.Activity, core.Classify(&publishedFacts))
	rtEqual(t, len(published.ToolsInFlight), 1)
	// The watchdog's handle and the published list are one tracker.
	rtEqual(t, watchdogView.Snapshot(), published.ToolsInFlight)
	if rtIsIdle(published) {
		t.Fatal("a working relay is not idle")
	}
}

// rtJsonObject is serde_json::to_value of a value that serializes as an object.
func rtJsonObject(value any) map[string]any {
	decoder := json.NewDecoder(bytes.NewReader(rtMust(json.Marshal(value))))
	decoder.UseNumber()
	var object map[string]any
	rtNoErr(decoder.Decode(&object))
	return object
}

// rtWriteRelayState replaces relay-state.json with a JSON value.
func rtWriteRelayState(root string, value any) {
	rtNoErr(os.WriteFile(filepath.Join(root, core.RelayStateFile), rtMust(json.Marshal(value)), 0o600))
}

func TestRestoredNativeIdentityWaitsForCurrentAcpConfiguration(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtOpenedSession("native-session")))
	if state := relay.OperationalState(); state.NativeSessionIsReady() {
		t.Fatal("an unconfigured session is not ready")
	}
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	if state := relay.OperationalState(); !state.NativeSessionIsReady() {
		t.Fatal("a configured session is ready")
	}

	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtEqual(t, relay.OperationalState().NativeSessionID, rtString("native-session"))
	rtEqual(t, relay.OperationalState().ACPReady, rtBool(false))
	if state := relay.OperationalState(); state.NativeSessionIsReady() {
		t.Fatal("a restored identity waits for the current ACP configuration")
	}
	for _, transition := range []core.RelayObservation{
		core.RelayObservationSessionRestarted{},
		core.RelayObservationClosing{},
		core.RelayObservationClosed{},
	} {
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		rtEqual(t, relay.OperationalState().ACPReady, rtBool(true))
		rtMust(relay.RecordObservation(transition))
		rtEqual(t, relay.OperationalState().ACPReady, rtBool(false))
	}
}

func TestLegacySnapshotKeepsItsLiveTurnWhenActivityClocksAreUpgraded(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 12_000}))
	legacy := rtJsonObject(relay.snapshot)
	legacy["format_version"] = 4
	delete(legacy, "activity_turn_started_at_ms")
	rtWriteRelayState(temp, legacy)

	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, relay.snapshot.FormatVersion, core.RelayStateVersion)
	rtEqual(t, relay.OperationalState().ActivityTurnStartedAtMs, rtInt64(12_000))
	rtEqual(t, relay.OperationalState().HarnessTurn.StartedAtMs, int64(12_000))
	rtNoErr(relay.persistSnapshot())
	reopened := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, reopened.OperationalState().ActivityTurnStartedAtMs, rtInt64(12_000))
}

func TestRevision15SnapshotReadsItsExistingJournalWhenUpgradedTo16(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	revision15 := rtJsonObject(relay.snapshot)
	revision15["format_version"] = 15
	delete(revision15, "pending_mailbox_events")
	delete(revision15, "mailbox_event_keys")
	delete(revision15, "delivered_mailbox_event_keys")
	delete(revision15, "mailbox_prompt_contexts")

	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "journal event from the older snapshot"}))
	journalOrdinal := relay.LatestOrdinal()
	journalDigest := relay.LatestDigest()

	rtWriteRelayState(temp, revision15)
	upgraded := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, upgraded.snapshot.FormatVersion, core.RelayStateVersion)
	rtEqual(t, upgraded.LatestOrdinal(), journalOrdinal)
	rtEqual(t, upgraded.LatestDigest(), journalDigest)
	if len(upgraded.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", upgraded.snapshot.PendingMailboxEvents)
	}
	if len(upgraded.snapshot.DeliveredMailboxEventKeys) != 0 {
		t.Fatalf("%#v", upgraded.snapshot.DeliveredMailboxEventKeys)
	}
}

func TestIdleClockStartsAtSettlementSurvivesReopenAndIgnoresMetadata(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	if relay.OperationalState().IdleSinceMs != nil {
		t.Fatal("a fresh relay has no idle clock")
	}
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	if relay.OperationalState().IdleSinceMs != nil {
		t.Fatal("a working relay has no idle clock")
	}
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	idleSince := relay.OperationalState().IdleSinceMs
	if idleSince == nil {
		t.Fatal("settlement starts the idle clock")
	}
	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "metadata does not change activity"}))
	rtEqual(t, relay.OperationalState().IdleSinceMs, idleSince)
	relay = relayTestsClaudeRelay(t, temp)
	rtEqual(t, relay.OperationalState().IdleSinceMs, idleSince)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	if relay.OperationalState().IdleSinceMs != nil {
		t.Fatal("new work stops the idle clock")
	}
}

func TestRelayStoreIdentitySurvivesRestartButDistinguishesAFreshDestination(t *testing.T) {
	directory := t.TempDir()
	first := filepath.Join(directory, "first")
	relay := rtMust(DurableRelayOpen(first, "session-one", "test"))
	identity := *relay.OperationalState().StoreID
	reopened := rtMust(DurableRelayOpen(first, "session-one", "test"))
	rtEqual(t, reopened.OperationalState().StoreID, rtString(identity))
	fresh := rtMust(DurableRelayOpen(filepath.Join(directory, "fresh"), "session-one", "test"))
	if storeID := fresh.OperationalState().StoreID; storeID != nil && *storeID == identity {
		t.Fatal("a fresh destination has its own store identity")
	}
}

func TestIdleClockWaitsForBackgroundWorkAndPersistsItsCompletion(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	turnStart := relay.OperationalState().ActivityTurnStartedAtMs
	if turnStart == nil {
		t.Fatal("output starts a turn")
	}
	rtNoErr(relay.AgentTerminalStarted(core.ActiveAgentTerminal{TerminalID: "background", Command: "build", StartedAtMs: 1_000}))
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	if relay.OperationalState().IdleSinceMs != nil {
		t.Fatal("background work keeps the session busy")
	}
	rtEqual(t, relay.OperationalState().ActivityTurnStartedAtMs, turnStart)
	// A newly attached client receives the original clock from the worker.
	var reattached core.RelayOperationalState
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(relay.OperationalState())), &reattached))
	rtEqual(t, reattached.ActivityTurnStartedAtMs, turnStart)
	var stored core.RelaySnapshot
	rtNoErr(json.Unmarshal(rtMust(os.ReadFile(filepath.Join(temp, core.RelayStateFile))), &stored))
	rtEqual(t, stored.ActivityTurnStartedAtMs, turnStart)
	nextTurn := *turnStart + 1_000
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: nextTurn}))
	rtEqual(t, relay.OperationalState().ActivityTurnStartedAtMs, rtInt64(nextTurn))
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Autonomous: nil, Origin: nil, PromptInFlight: false}))
	rtEqual(t, relay.OperationalState().ActivityTurnStartedAtMs, rtInt64(nextTurn))
	rtNoErr(relay.AgentTerminalClosed("background"))
	if relay.OperationalState().ActivityTurnStartedAtMs != nil {
		t.Fatal("the turn clock ends with the background work")
	}
	idleSince := relay.OperationalState().IdleSinceMs
	if idleSince == nil {
		t.Fatal("the idle clock starts when background work ends")
	}
	rtEqual(t, relayTestsClaudeRelay(t, temp).OperationalState().IdleSinceMs, idleSince)
}

func TestLegacyIdleSnapshotDoesNotInventAnIdleStart(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	value := rtJsonObject(relay.snapshot)
	delete(value, "idle_since_ms")
	delete(value, "activity_was_idle")
	rtWriteRelayState(temp, value)
	relay = relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "old worker"}))
	if relay.OperationalState().IdleSinceMs != nil {
		t.Fatal("a legacy idle snapshot must not invent an idle start")
	}
	operational := rtJsonObject(relay.OperationalState())
	delete(operational, "idle_since_ms")
	var decoded core.RelayOperationalState
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(operational)), &decoded))
	if decoded.IdleSinceMs != nil {
		t.Fatal("a legacy operational state has no idle start")
	}
}

func TestHiddenContextWaitsForAPromptAndSurvivesAnInterruption(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	installed := relay.Handle(relayRequest("install-context", core.RelayRequestInstallPromptContext{
		Text: "<hel-background>memory</hel-background>",
	}))
	if _, ok := rtOkPayload(t, installed).(core.RelayResponsePayloadPromptContextInstalled); !ok {
		t.Fatalf("unexpected install response %#v", installed.Body)
	}

	submitRelay(relay, "configure-first", core.RelayCommandSetConfig{Key: "model", Value: "default"})
	config := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(config), 1)
	if config[0].HiddenPromptContext != nil {
		t.Fatal("configuration does not carry hidden context")
	}
	rtMust(relay.RecordCommandCompleted("configure-first", core.RelayCommandOutcomeConfigured{}))

	submitRelay(relay, "first-prompt", prompt("do it"))
	first := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, first[0].HiddenPromptContext, rtString("<hel-background>memory</hel-background>"))
	rtEqual(t, first[0].Command, prompt("do it"))
	rtMust(relay.RecordCommandInterrupted("first-prompt", core.OutcomeReasonRuntimeFailure, "restart"))

	submitRelay(relay, "second-prompt", prompt("continue"))
	second := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, second[0].HiddenPromptContext, rtString("<hel-background>memory</hel-background>"))
	rtMust(relay.RecordCommandCompleted("second-prompt", rtPromptOutcome("end_turn")))

	submitRelay(relay, "third-prompt", prompt("again"))
	third := rtMust(relay.ClaimPendingCommands(true))
	if third[0].HiddenPromptContext != nil {
		t.Fatalf("the context reached a later prompt: %q", *third[0].HiddenPromptContext)
	}
}

// A controller that names the digest of the state it holds is not sent that
// state again. A stale digest or a changed state brings the whole state back;
// the worker rebuilds the state for every attach, so the unchanged case also
// shows that equal states give equal digests.
func TestAnAttachSkipsAStateTheControllerAlreadyHolds(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	attach := func(id string, known *string) core.RelayResponseBody {
		latest := relay.LatestOrdinal()
		return relay.Handle(relayRequest(id, core.RelayRequestAttach{
			AfterOrdinal:     latest,
			AfterDigest:      relayTestDigestAt(relay, latest),
			KnownStateDigest: known,
		})).Body
	}
	full := func(body core.RelayResponseBody) (core.RelayOperationalState, *string) {
		ok, isOk := body.(core.RelayResponseBodyOk)
		attached, isAttached := ok.Payload.(core.RelayResponsePayloadAttached)
		if !isOk || !isAttached {
			t.Fatalf("expected the whole state, got %#v", body)
		}
		return attached.State, attached.StateDigest
	}
	state, named := full(attach("first", nil))
	if named == nil {
		t.Fatal("protocol 57 names the state's digest")
	}
	digest := *named
	rtEqual(t, digest, rtMust(core.RelayStateDigest(&state)))
	unchanged := attach("unchanged", &digest)
	if ok, isOk := unchanged.(core.RelayResponseBodyOk); !isOk {
		t.Fatalf("unexpected response %#v", unchanged)
	} else if _, known := ok.Payload.(core.RelayResponsePayloadAttachedWithKnownState); !known {
		t.Fatalf("an unchanged state is not sent again: %#v", ok.Payload)
	}
	_, again := full(attach("stale", rtString("stale")))
	rtEqual(t, again, rtString(digest))

	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "the state moves on"}))
	_, changed := full(attach("changed", &digest))
	if changed != nil && *changed == digest {
		t.Fatal("a changed state has a new digest")
	}
}

// Hard-won: 08e2dde6: disk replay under the relay mutex stalled live event recording during catch-up.
func TestADeferredAttachReadsItsPageWhileTheRelayKeepsRecording(t *testing.T) {
	temp := t.TempDir()
	var lock sync.Mutex
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	locked := func(body func()) {
		if !lock.TryLock() {
			t.Fatal("the relay lock is held by the pending replay")
		}
		defer lock.Unlock()
		body()
	}
	locked(func() { recordPagedHistory(t, relay, 80) })
	var plannedFrontier uint64
	locked(func() { plannedFrontier = relay.LatestOrdinal() })

	var deferred *DeferredRelayAttach
	locked(func() {
		envelope := attachEnvelope(t, relay, "catch-up", 0)
		deferred = relay.TakeDeferredAttach(&envelope)
	})
	if deferred == nil {
		t.Fatal("an attach is deferred off the relay lock")
	}

	// The catch-up page has not been assembled yet, and the relay is free.
	for index := range 8 {
		locked(func() {
			rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: fmt.Sprintf("live-%d", index)}))
		})
	}
	var liveFrontier uint64
	locked(func() { liveFrontier = relay.LatestOrdinal() })
	rtEqual(t, liveFrontier, plannedFrontier+8)

	response := deferred.Finish()
	attached, ok := rtOkPayload(t, response).(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatal("deferred attach failed")
	}
	if len(attached.Events) == 0 || attached.ThroughOrdinal >= plannedFrontier {
		t.Fatalf("the history should not fit in one page: %d of %d", attached.ThroughOrdinal, plannedFrontier)
	}
	// The page is one unbroken run of the chain the cursor asked for, and
	// it reports the frontier captured with the plan rather than the one
	// the live appends moved it to.
	cursor := core.RelayCursor{Ordinal: 0, Digest: core.RelayEventGenesisDigest}
	for index := range attached.Events {
		event := &attached.Events[index]
		rtNoErr(core.ValidateRelayEvent(cursor.Ordinal, cursor.Digest, event))
		cursor.Ordinal = event.Ordinal
		cursor.Digest = event.Digest
	}
	rtEqual(t, cursor.Ordinal, attached.ThroughOrdinal)
	rtEqual(t, cursor.Digest, attached.ThroughDigest)
	rtEqual(t, attached.State.LatestOrdinal, plannedFrontier)
}

func TestAProvenReplayCursorDoesNotRereadItsOldSegment(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	recordPagedHistory(t, relay, 80)
	first := relay.Handle(attachEnvelope(t, relay, "first", 0))
	attached, ok := rtOkPayload(t, first).(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("first replay page failed: %#v", first.Body)
	}
	throughOrdinal := attached.ThroughOrdinal
	if throughOrdinal >= relay.LatestOrdinal() {
		t.Fatalf("the first page reached the frontier %d", throughOrdinal)
	}
	relay.RememberReplayCursor(&first)

	oldSegment := ""
	for _, span := range relay.journalSpans {
		if throughOrdinal > span.afterOrdinal && throughOrdinal <= span.fileLastOrdinal {
			oldSegment = span.path
			break
		}
	}
	if oldSegment == "" {
		t.Fatal("the replay cursor belongs to a journal segment")
	}
	moved := strings.TrimSuffix(oldSegment, filepath.Ext(oldSegment)) + ".moved"
	rtNoErr(os.Rename(oldSegment, moved))

	digest, found, err := relay.digestAt(throughOrdinal)
	rtNoErr(err)
	if !found || digest != attached.ThroughDigest {
		t.Fatalf("validating the returned cursor must not reopen its old segment: %q (%v)", digest, found)
	}
}

// Hard-won: 0c505b8: ACK GC copied the entire live suffix under the relay owner lock.
func TestAcknowledgementCollectsOnlyCoveredFilesAndPreservesLiveSegments(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	recordPagedHistory(t, relay, 80)
	spans := slices.Clone(relay.journalSpans)
	if len(spans) < 4 {
		t.Fatalf("only %d spans", len(spans))
	}
	through := spans[0].fileLastOrdinal
	digest := relayTestDigestAt(relay, through)
	frontier := relay.LatestOrdinal()
	partial := spans[0].fileFirstOrdinal + 1
	if partial >= through {
		t.Fatalf("partial %d is not inside the first file ending at %d", partial, through)
	}
	partialDigest := relayTestDigestAt(relay, partial)
	envelope := attachEnvelope(t, relay, "before-partial-ack", 0)
	deferred := relay.TakeDeferredAttach(&envelope)
	if deferred == nil {
		t.Fatal("an attach from the start is deferred")
	}
	generation := deferred.JournalGeneration()

	live := spans[2].path
	saved := rtMust(os.ReadFile(live))
	// A later unreadable segment cannot prevent collection of the covered prefix.
	rtNoErr(os.WriteFile(live, []byte("unreadable live segment"), 0o600))
	response := relay.Handle(relayRequest("collect-boundary-prefix", core.RelayRequestAcknowledge{
		ThroughOrdinal: partial,
		ThroughDigest:  partialDigest,
	}))
	if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("%#v", response)
	}
	if _, err := os.Stat(spans[0].path); err != nil {
		t.Fatalf("a straddling file is retained until completely covered: %v", err)
	}
	rtEqual(t, relay.journalSpans[0].afterOrdinal, partial)
	if relay.JournalGeneration() == generation {
		t.Fatal("logical collection invalidates prepared reads")
	}
	response = relay.Handle(relayRequest("collect-prefix", core.RelayRequestAcknowledge{
		ThroughOrdinal: through,
		ThroughDigest:  digest,
	}))
	if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("%#v", response)
	}
	if _, err := os.Stat(spans[0].path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("the covered file was not collected: %v", err)
	}
	for _, span := range spans[1:] {
		if _, err := os.Stat(span.path); err != nil {
			t.Fatalf("a fully live segment was replaced: %v", err)
		}
	}
	rtEqual(t, rtMust(os.ReadFile(live)), []byte("unreadable live segment"))
	rtNoErr(os.WriteFile(live, saved, 0o600))
	relay = rtMust(DurableRelayOpen(temp, session, "reopened"))
	events := rtMust(relay.EventsAfter(through, digest))
	rtEqual(t, events[0].Ordinal, through+1)
	rtEqual(t, events[len(events)-1].Ordinal, frontier)
}

// rtSealedSegments counts the compressed journal segments under a relay root.
func rtSealedSegments(root string) int {
	entries := rtMust(os.ReadDir(filepath.Join(root, core.RelayJournalDir)))
	count := 0
	for _, entry := range entries {
		if filepath.Ext(entry.Name()) == ".gz" {
			count++
		}
	}
	return count
}

func TestADeferredAttachRefusesAPageFromACollectedJournal(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	recordPagedHistory(t, relay, 80)
	if rtSealedSegments(temp) < 2 {
		t.Fatal("history did not seal segments")
	}

	envelope := attachEnvelope(t, relay, "catch-up", 0)
	deferred := relay.TakeDeferredAttach(&envelope)
	if deferred == nil {
		t.Fatal("an attach is deferred off the relay lock")
	}
	generation := deferred.JournalGeneration()

	// Another controller acknowledges the whole history, which rewrites
	// the journal and deletes every sealed segment this plan named.
	frontier := relay.LatestOrdinal()
	frontierDigest := relay.LatestDigest()
	submitFloor(relay, "floor-command-0001", core.RelayCursor{Ordinal: frontier, Digest: frontierDigest})
	acknowledgeRelay(relay, "ack-everything", frontier)
	rtEqual(t, rtSealedSegments(temp), 0)

	response := deferred.Finish()
	failure, ok := response.Body.(core.RelayResponseBodyError)
	if !ok {
		t.Fatalf("a page read from a collected journal must not be served: %#v", response)
	}
	if !failure.Error.Retryable {
		t.Fatalf("a collected journal is retryable, not a controller fault: %#v", failure.Error)
	}
	if relay.JournalGeneration() == generation {
		t.Fatal("collection must mark captured replay plans stale")
	}
}

// A replay page is assembled from files the relay lock no longer guards,
// so a span whose file was pruned under the reader must fail the read.
// Silently contributing nothing would answer a catch-up with a page that
// claims to reach the frontier while carrying none of the events.
func TestADeferredAttachNeverServesATornPageAfterItsSegmentsArePruned(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	recordPagedHistory(t, relay, 80)
	frontier := relay.LatestOrdinal()

	envelope := attachEnvelope(t, relay, "catch-up", 0)
	deferred := relay.TakeDeferredAttach(&envelope)
	if deferred == nil {
		t.Fatal("an attach is deferred off the relay lock")
	}
	journal := filepath.Join(temp, core.RelayJournalDir)
	for _, entry := range rtMust(os.ReadDir(journal)) {
		rtNoErr(os.Remove(filepath.Join(journal, entry.Name())))
	}

	response := deferred.Finish()
	switch body := response.Body.(type) {
	case core.RelayResponseBodyError:
		if !body.Error.Retryable {
			t.Fatalf("a pruned segment is retryable, not a controller fault: %#v", body.Error)
		}
	case core.RelayResponseBodyOk:
		if attached, ok := body.Payload.(core.RelayResponsePayloadAttached); ok {
			t.Fatalf("served %d events but claimed to reach event %d of %d", len(attached.Events), attached.ThroughOrdinal, frontier)
		}
		t.Fatalf("unexpected attach response: %#v", body)
	default:
		t.Fatalf("unexpected attach response: %#v", body)
	}
}

func TestADeferredAttachRefusesAPageFromAResealedSegment(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	// Fill the active segment without crossing its seal threshold.
	recordPagedHistory(t, relay, 8)
	envelope := attachEnvelope(t, relay, "catch-up", 0)
	deferred := relay.TakeDeferredAttach(&envelope)
	if deferred == nil {
		t.Fatal("an attach is deferred off the relay lock")
	}
	generation := deferred.JournalGeneration()

	// The next append seals the active segment, moving every event the
	// plan expects to find in `active.jsonl` into a compressed file.
	recordPagedHistory(t, relay, 12)
	if relay.JournalGeneration() == generation {
		t.Fatal("sealing must mark captured replay plans stale")
	}

	response := deferred.Finish()
	failure, ok := response.Body.(core.RelayResponseBodyError)
	if !ok {
		t.Fatalf("a page read from a resealed segment must not be served: %#v", response)
	}
	if !failure.Error.Retryable {
		t.Fatalf("a resealed segment is retryable: %#v", failure.Error)
	}
}

func TestRelayRunsQueuedPromptsInOrderWithoutAController(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "command-one", prompt("one"))
	submitRelay(relay, "command-two", prompt("two"))
	submitRelay(relay, "command-three", prompt("three"))

	first := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(first), 1)
	rtEqual(t, first[0].CommandID, "command-one")
	rtMust(relay.RecordCommandCompleted("command-one", rtPromptOutcome("end_turn")))
	second := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(second), 1)
	rtEqual(t, second[0].CommandID, "command-two")

	// Recovery resumes the interrupted turn before the durable offline queue.
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	continuation := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(continuation), 1)
	rtEqual(t, continuation[0].CommandID, core.RestartContinuationID("command-two"))
	rtMust(relay.RecordCommandCompleted(continuation[0].CommandID, rtPromptOutcome("end_turn")))
	third := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(third), 1)
	rtEqual(t, third[0].CommandID, "command-three")
	interrupted := false
	for _, event := range retainedEvents(relay) {
		if observation, ok := event.Observation.(core.RelayObservationCommandInterrupted); ok && observation.CommandID == "command-two" {
			interrupted = true
		}
	}
	if !interrupted {
		t.Fatal("the interrupted turn is in the journal")
	}
}

func TestDuplicateImageCommandIsIdempotentAfterAttachmentLoss(t *testing.T) {
	temp := t.TempDir()
	store := core.AttachmentStoreWorker(temp)
	bytes := []byte("\x89PNG\r\n\x1a\nverified-image")
	reference := rtMust(core.NewAttachmentRef(bytes, "image/png", 100, 100))
	rtNoErr(store.Install(&reference, bytes))
	// core's attachment module reads the unstable ACP schema; a relay prompt
	// is the stable one. Both share the wire form, so the block crosses by
	// its JSON, as core's relayPromptHasAttachmentReferences does.
	var block schema.ContentBlock
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(reference.ContentBlock())), &block))
	command := core.RelayCommandPrompt{Prompt: []schema.ContentBlock{block}}
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	ordinal := submitRelay(relay, "image-command", command)
	rtNoErr(os.Remove(filepath.Join(store.Root(), reference.Sha256)))
	latestBeforeRetry := relay.LatestOrdinal()

	response := relay.Handle(relayRequest("image-command-retry", core.RelayRequestSubmit{
		CommandID: "image-command",
		Command:   command,
	}))
	accepted, ok := rtOkPayload(t, response).(core.RelayResponsePayloadAccepted)
	if !ok || accepted.CommandID != "image-command" || accepted.Ordinal != ordinal {
		t.Fatalf("a duplicate submit must return the first acceptance: %#v", response.Body)
	}
	rtEqual(t, relay.LatestOrdinal(), latestBeforeRetry)
}

func TestShellRunsDuringAnActiveTurnAndBarriersTheLaterPrompt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "prompt-command-1", prompt("first"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "prompt-command-1")
	submitRelay(relay, "shell-command-01", core.RelayCommandRunUserShell{Command: "printf ready"})
	submitRelay(relay, "prompt-command-2", prompt("after shell"))

	shell := rtMust(relay.ClaimPendingUserShellCommandsUpTo(4))
	rtEqual(t, shell[0].CommandID, "shell-command-01")
	rtMust(relay.RecordCommandCompleted("prompt-command-1", rtPromptOutcome("end_turn")))
	rtNoClaims(t, relay)

	rtMust(relay.RecordCommandCompleted("shell-command-01", core.RelayCommandOutcomeUserShell{
		Result: successfulShell("printf ready", "ready"),
	}))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].CommandID, "prompt-command-2")
	context := claimed[0].HiddenPromptContext
	if context == nil || !strings.Contains(*context, "<user_shell_command>") || !strings.Contains(*context, "ready") {
		t.Fatalf("the shell output must reach the next prompt: %v", context)
	}
}

func TestAPromptAcceptedBeforeAShellKeepsPriorityAndDoesNotConsumeIt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "prompt-command-1", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "prompt-command-2", prompt("already queued"))
	submitRelay(relay, "shell-command-01", core.RelayCommandRunUserShell{Command: "printf later"})
	rtMust(relay.ClaimPendingUserShellCommandsUpTo(4))
	rtMust(relay.RecordCommandCompleted("prompt-command-1", rtPromptOutcome("end_turn")))

	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].CommandID, "prompt-command-2")
	if claimed[0].HiddenPromptContext != nil {
		t.Fatalf("an earlier prompt consumed the shell output: %q", *claimed[0].HiddenPromptContext)
	}
}

func TestAShellCancelledBeforeLaunchStillReachesTheNextPrompt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "shell-command-01", core.RelayCommandRunUserShell{Command: "sleep 60"})
	submitRelay(relay, "cancel-shell-01", core.RelayCommandCancelUserShell{ShellCommandID: "shell-command-01"})

	claimed := rtMust(relay.ClaimPendingUserShellCommandsUpTo(4))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cancel-shell-01")
	rtMust(relay.RecordCommandInterrupted("shell-command-01", core.OutcomeReasonRuntimeFailure, "shell command was cancelled before it started"))
	rtMust(relay.RecordCommandCompleted("cancel-shell-01", core.RelayCommandOutcomeUserShellCancelled{}))

	submitRelay(relay, "prompt-command-1", prompt("what happened?"))
	next := rtMust(relay.ClaimPendingCommands(true))
	context := next[0].HiddenPromptContext
	if context == nil || !strings.Contains(*context, "status: interrupted") || !strings.Contains(*context, "cancelled before it started") {
		t.Fatalf("the cancelled shell must reach the next prompt: %v", context)
	}
}

func TestClaimsWaitForTheCurrentAcpSessionConfiguration(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "wait-for-config", prompt("later"))

	if claimed := rtMust(relay.ClaimPendingCommands(false)); len(claimed) != 0 {
		t.Fatalf("an unconfigured session claims nothing: %#v", claimed)
	}
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "wait-for-config")
}

func TestCheckpointBarrierPausesOfflinePromptPromotionUntilRelease(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "barrier-command", core.RelayCommandBeginCheckpoint{Reason: rtString("test")})
	submitRelay(relay, "after-barrier", prompt("later"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "barrier-command")
	rtMust(relay.RecordCheckpointReady("barrier-command"))
	rtNoClaims(t, relay)
	if relay.OperationalState().ActivePrompt != nil {
		t.Fatal("the barrier holds the queued prompt")
	}

	submitRelay(relay, "release-command", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "barrier-command"})
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "after-barrier")
}

func TestControllerDisconnectCannotLeaveCheckpointBarrierPaused(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "barrier-disconnect", core.RelayCommandBeginCheckpoint{Reason: nil})
	submitRelay(relay, "queued-offline", prompt("continue"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	rtMust(relay.RecordCheckpointReady("barrier-disconnect"))

	_, cancelled := rtMust2(relay.CancelCheckpointBarrierOnDisconnect("barrier-disconnect"))
	interrupted := false
	for _, event := range retainedEvents(relay) {
		observation, ok := event.Observation.(core.RelayObservationCommandInterrupted)
		if ok && observation.Reason != nil && *observation.Reason == core.OutcomeReasonControllerDisconnected && observation.CommandID == "barrier-disconnect" {
			interrupted = true
		}
	}
	if !interrupted {
		t.Fatal("the disconnected barrier is interrupted")
	}
	if !cancelled {
		t.Fatal("the ready barrier was cancelled")
	}
	if relay.OperationalState().CheckpointBarrier != nil {
		t.Fatal("the barrier is gone")
	}
	next := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(next), 1)
	rtEqual(t, next[0].CommandID, "queued-offline")
}

// rtInvalidState asserts a response is an InvalidState protocol error.
func rtInvalidState(t *testing.T, response core.RelayResponseEnvelope) {
	t.Helper()
	failure, ok := response.Body.(core.RelayResponseBodyError)
	if !ok || failure.Error.Code != core.RelayErrorCodeInvalidState {
		t.Fatalf("expected an invalid_state error, got %#v", response.Body)
	}
}

// The controller releases dispatch once the archive exists on the target,
// long before that archive is installed. Journal history must stay put
// until an installed archive covers it.
func TestReleasingACheckpointResumesDispatchWithoutMovingTheRecoveryFloor(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	ready := readyCheckpoint(relay, "released-barrier")
	submitRelay(relay, "queued-during-release", prompt("later"))
	attachRelay(relay, "attach-release", 0)
	acknowledgeRelay(relay, "ack-release", ready.Ordinal)
	rtNoClaims(t, relay)
	floorBefore := relay.snapshot.RecoveryFloorOrdinal
	retainedBefore := relay.snapshot.RetainedThrough()

	submitRelay(relay, "release-command", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "released-barrier"})

	if relay.OperationalState().CheckpointBarrier != nil {
		t.Fatal("the release ends the barrier")
	}
	if relay.OperationalState().CheckpointReady != nil {
		t.Fatal("the release ends the ready cut")
	}
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, floorBefore)
	rtEqual(t, relay.snapshot.RetainedThrough(), retainedBefore)
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "queued-during-release")
	// The released barrier is terminal, so the controller connection that
	// opened it can drop without cancelling anything.
	if _, cancelled := rtMust2(relay.CancelCheckpointBarrierOnDisconnect("released-barrier")); cancelled {
		t.Fatal("a released barrier cannot be cancelled")
	}
}

func TestReleasingACheckpointRequiresThatExactReadyBarrier(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtInvalidState(t, submitRelease(relay, "release-without-barrier", "no-such-barrier"))

	submitRelay(relay, "unready-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	rtInvalidState(t, submitRelease(relay, "release-unready", "unready-barrier"))

	rtMust(relay.RecordCheckpointReady("unready-barrier"))
	rtInvalidState(t, submitRelease(relay, "release-wrong", "another-barrier"))
	rtEqual(t, relay.OperationalState().CheckpointBarrier, rtString("unready-barrier"))
}

// Legacy archive coverage remains validated, independently of journal retention.
func TestLegacyRecoveryFloorAdvancesOnlyForwardWithoutControllingRetention(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	ready := readyCheckpoint(relay, "installed-barrier")
	submitRelay(relay, "release-installed", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "installed-barrier"})
	rtInvalidState(t, submitFloor(relay, "floor-wrong-digest", core.RelayCursor{Ordinal: ready.Ordinal, Digest: strings.Repeat("b", 64)}))
	beyondFrontier := core.RelayCursor{Ordinal: relay.LatestOrdinal() + 1, Digest: relay.snapshot.LatestDigest}
	rtInvalidState(t, submitFloor(relay, "floor-ahead", beyondFrontier))
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, uint64(0))

	submitRelay(relay, "floor-installed", core.RelayCommandAdvanceRecoveryFloor{Through: ready})
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, ready.Ordinal)
	rtEqual(t, relay.snapshot.RecoveryFloorDigest, ready.Digest)
	rtEqual(t, relay.snapshot.RetainedThrough(), uint64(0))

	rtInvalidState(t, submitFloor(relay, "floor-backwards", core.RelayCursor{Ordinal: 0, Digest: core.RelayEventGenesisDigest}))
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, ready.Ordinal)
	acknowledged := relay.LatestOrdinal()
	acknowledgeRelay(relay, "ack-floor", acknowledged)
	rtEqual(t, relay.snapshot.RetainedThrough(), acknowledged)
}

// The legacy one-step completion is unchanged: it both resumes dispatch
// and advances the recovery floor.
func TestCompletingACheckpointStillResumesDispatchAndAdvancesTheFloor(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	ready := readyCheckpoint(relay, "completed-barrier")
	submitRelay(relay, "queued-during-completion", prompt("later"))
	attachRelay(relay, "attach-completion", 0)
	acknowledgeRelay(relay, "ack-completion", ready.Ordinal)

	submitRelay(relay, "complete-command", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "completed-barrier"})

	if relay.OperationalState().CheckpointBarrier != nil {
		t.Fatal("completion ends the barrier")
	}
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, ready.Ordinal)
	rtEqual(t, relay.snapshot.RetainedThrough(), ready.Ordinal)
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "queued-during-completion")
}

func TestCheckpointBarriersAreSerializedAndOnlyExactCompletionReleasesThem(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "first-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})
	submitRelay(relay, "second-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})

	first := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(first), 1)
	rtEqual(t, first[0].CommandID, "first-barrier")
	rtMust(relay.RecordCheckpointReady("first-barrier"))
	rtNoClaims(t, relay)

	wrong := relay.Handle(relayRequest("wrong-completion", core.RelayRequestSubmit{
		CommandID: "wrong-complete-command",
		Command:   core.RelayCommandCompleteCheckpoint{BarrierCommandID: "second-barrier"},
	}))
	rtInvalidState(t, wrong)

	submitRelay(relay, "complete-first", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "first-barrier"})
	second := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(second), 1)
	rtEqual(t, second[0].CommandID, "second-barrier")
}

func TestCheckpointWaitsForEarlierQueuedControlAndFreezesLaterControl(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "config-before", core.RelayCommandSetConfig{Key: "model", Value: "before"})
	submitRelay(relay, "control-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})

	control := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(control), 1)
	rtEqual(t, control[0].CommandID, "config-before")
	rtMust(relay.RecordCommandCompleted("config-before", core.RelayCommandOutcomeConfigured{}))
	rtEqual(t, relay.OperationalState().Config["model"], "before")

	barrier := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(barrier), 1)
	rtEqual(t, barrier[0].CommandID, "control-barrier")
	rtMust(relay.RecordCheckpointReady("control-barrier"))
	submitRelay(relay, "config-after", core.RelayCommandSetConfig{Key: "model", Value: "after"})
	rtNoClaims(t, relay)

	submitRelay(relay, "complete-control-barrier", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "control-barrier"})
	later := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(later), 1)
	rtEqual(t, later[0].CommandID, "config-after")
}

// rtProjectSession projects every retained relay event into a materialized
// session, as the controller does.
func rtProjectSession(relay *DurableRelay) core.MaterializedSession {
	materialized := core.MaterializedSessionEmpty(session)
	for _, event := range rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest)) {
		projected := rtMust(transcript.ProjectRelayEvent(&materialized, &event))
		rtNoErr(transcript.ApplyCommittedProjectionEvent(&materialized, &event, projected.Mutation))
	}
	return materialized
}

func TestARepeatedNoticeAppendStillLeavesOneConversationLine(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	text := "The working tree moved while this session was stopped."
	submitRelay(relay, "resume-notice-1", core.RelayCommandRecordNotice{Text: text})
	// Stand in for a retry that re-appended the notice after a transient
	// persistence failure reported a durable append as unfinished.
	rtMust(relay.appendRelayEvent(rtString("resume-notice-1"), core.RelayObservationNotice{Message: text}))

	materialized := rtProjectSession(relay)
	rtEqual(t, len(materialized.Transcript), 1)
}

// rtSessionInfoMeta is a session_info_update carrying `_meta` (raw JSON).
func rtSessionInfoMeta(meta string) schema.SessionUpdate {
	return rtSessionUpdateJSON(`{"sessionUpdate":"session_info_update","_meta":` + meta + `}`)
}

// rtLastObservation is observations(relay).last().
func rtLastObservation(t *testing.T, relay *DurableRelay) core.RelayObservation {
	t.Helper()
	observed := observations(t, relay)
	if len(observed) == 0 {
		return nil
	}
	return observed[len(observed)-1]
}

func TestCodexRepliesPreserveTheUserTurnAndOnlyAutonomousWorkAddsANotice(t *testing.T) {
	for _, goal := range []string{
		`null`,
		`{"objective":"finish","status":"active","createdAt":1}`,
	} {
		root := t.TempDir()
		relay := rtMust(DurableRelayOpen(root, session, "test"))
		relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
		metadata := func(status, turnID string) schema.SessionUpdate {
			return rtSessionInfoMeta(fmt.Sprintf(`{"goal":%s,"execution":{"version":1,"status":%q,"turnId":%q}}`, goal, status, turnID))
		}
		markerCount := func(materialized core.MaterializedSession) int {
			count := 0
			for _, item := range materialized.Transcript {
				if strings.HasPrefix(item.StableID, core.HarnessTurnItemPrefix) {
					count++
				}
			}
			return count
		}
		commandID := "00000000000000000000000000000001"
		submitRelay(relay, commandID, prompt("Are those problems captured in a ticket?"))
		rtMust(relay.ClaimPendingCommands(true))
		before := rtProjectSession(relay)
		if before.ActiveTurn == nil {
			t.Fatal("the prompt opens a turn")
		}
		for range 2 {
			rtMust(relay.RecordSessionUpdate(metadata("running", "reply")))
		}
		replying := rtProjectSession(relay)
		rtEqual(t, markerCount(replying), 0)
		rtEqual(t, replying.ActiveTurn, before.ActiveTurn)
		rtEqual(t, replying.Execution, before.Execution)
		rtEqual(t, replying.Transcript, before.Transcript)

		// Native execution can outlast the ACP prompt result. Suppressing
		// the notice must not settle that work or permit replacement.
		rtMust(relay.RecordCommandCompleted(commandID, rtPromptOutcome("end_turn")))
		running := rtProjectSession(relay)
		if running.ActiveTurn != nil {
			t.Fatal("the prompt finished")
		}
		if _, ok := running.Execution.(core.MaterializedExecutionStateRunning); !ok {
			t.Fatalf("native execution is still running: %#v", running.Execution)
		}
		rtEqual(t, markerCount(running), 0)
		operational := relay.OperationalState()
		if operational.SafeToReplace(core.HarnessKindCodex) {
			t.Fatal("running native work is not safe to replace")
		}
		if operational.SafeForCheckpoint(core.HarnessKindCodex) {
			t.Fatal("running native work is not safe for a checkpoint")
		}
		rtMust(relay.RecordSessionUpdate(metadata("idle", "reply")))
		rtEqual(t, rtProjectSession(relay).Execution, core.MaterializedExecutionStateIdle{})

		for range 2 {
			rtMust(relay.RecordSessionUpdate(metadata("running", "autonomous")))
		}
		rtEqual(t, markerCount(rtProjectSession(relay)), 1)
		rtMust(relay.RecordSessionUpdate(metadata("idle", "autonomous")))
		settled := rtProjectSession(relay)
		rtEqual(t, markerCount(settled), 1)
		rtEqual(t, settled.Execution, core.MaterializedExecutionStateIdle{})
		rtEqual(t, settled.Transcript, rtProjectSession(relay).Transcript)
	}
}

// Hard-won: #1450 review: autonomous outcomes must preserve turn identity and queued work.
func TestCodexGoalTurnsBlockReplacementAfterThePromptFinishes(t *testing.T) {
	root := t.TempDir()
	relay := rtMust(DurableRelayOpen(root, session, "test"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
	if state := relay.OperationalState(); state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("an unconfigured session is not safe to replace")
	}
	submitRelay(relay, "00000000000000000000000000000001", prompt("/goal finish"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"goal":{"objective":"finish","status":"active","createdAt":1},"execution":{"version":1,"status":"running","turnId":"autonomous"}}`)))
	rtMust(relay.RecordCommandCompleted("00000000000000000000000000000001", rtPromptOutcome("end_turn")))
	state := relay.OperationalState()
	if state.ActivePrompt != nil {
		t.Fatal("the prompt finished")
	}
	if _, ok := relay.originatingCommandID(); ok {
		t.Fatal("no prompt originates the goal turn")
	}
	completion := rtCloneTurnCompletion(state.TurnCompletion)
	if state.HarnessTurn == nil {
		t.Fatal("the goal turn is a harness turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateRunning)
	if state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("a running goal is not safe to replace")
	}
	if state.SafeForCheckpoint(core.HarnessKindCodex) {
		t.Fatal("a running goal is not safe for a checkpoint")
	}
	submitRelay(relay, "stop-tail", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("stop-tail", core.RelayCommandOutcomeCancelled{}))
	if relay.snapshot.CancellingHarnessTurn != nil {
		t.Fatal("the cancel completed")
	}
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"execution":{"version":1,"status":"idle","turnId":"autonomous"}}`)))
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateIdle)
	rtEqual(t, relay.OperationalState().TurnCompletion, completion)
	if settled, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled); !ok || settled.Autonomous != nil {
		t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
	}
	if state := relay.OperationalState(); state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("between goal turns is not safe to kill")
	}
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"execution":{"version":1,"status":"running","turnId":"next"}}`)))
	start := relay.snapshot.AutonomousHarnessTurn().FirstOrdinal
	submitRelay(relay, "stop-goal", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("stop-goal", core.RelayCommandOutcomeCancelled{}))
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"execution":{"version":1,"status":"idle","turnId":"next"}}`)))
	settled, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled)
	if !ok || settled.Autonomous == nil || settled.Autonomous.StartOrdinal != start || settled.Autonomous.StopReason != "cancelled" {
		t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
	}
	relay = rtMust(DurableRelayOpen(root, session, "test"))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	resumed := rtOpenedSession("native")
	resumed.Resumed = true
	rtMust(relay.RecordObservation(resumed))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	goal := relay.OperationalState().Goal
	if !goal.Active() {
		t.Fatal("the goal survives the restart")
	}
	if goal.Synchronized() {
		t.Fatal("a restarted goal is not synchronized")
	}
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"goal":null,"execution":{"version":1,"status":"idle"}}`)))
	if state := relay.OperationalState(); !state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("a cleared idle goal is safe to replace")
	}
}

// rtCountObservations counts the observations of one variant.
func rtCountObservations[O core.RelayObservation](observed []core.RelayObservation) int {
	count := 0
	for _, observation := range observed {
		if _, ok := observation.(O); ok {
			count++
		}
	}
	return count
}

func rtTaskSettledAtMs(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if relay.taskSettledAtMs != nil {
		t.Fatalf("unexpected task settle at %d", *relay.taskSettledAtMs)
	}
}

func TestAutonomousSpawnAndOutputShareOneDurableTurnInEitherOrder(t *testing.T) {
	for _, outputFirst := range []bool{false, true} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		if outputFirst {
			rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		}
		// Native children may reuse task IDs without owning the root turn.
		rtNoErr(relay.AirTaskChanged("child", core.AirTaskUpdateTurnSpawned{TaskID: "turn-a", Replayed: false}))
		rtNoErr(relay.applyClaudeTaskFixture("turn-a", true, true, false, false))
		turn := *relay.snapshot.HarnessTurn
		rtNoErr(relay.AirTaskChanged("child", core.AirTaskUpdateFinished{TaskID: "turn-a", Settled: true, Failed: false}))
		rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, turn.FirstOrdinal)
		rtTaskSettledAtMs(t, relay)
		ordinal, ok := relay.turnTaskOrdinal("turn-a")
		if !ok || ordinal != turn.FirstOrdinal {
			t.Fatalf("turn-a names ordinal %d (%v), expected %d", ordinal, ok, turn.FirstOrdinal)
		}
		facts := relay.ActivityFacts()
		if state := core.Classify(&facts); state.Kind != core.ActivityStateTurn {
			t.Fatalf("expected a turn, got %#v", state)
		}
		if !outputFirst {
			observed := observations(t, relay)
			if len(observed) != 1 {
				t.Fatalf("%#v", observed)
			}
			if _, started := observed[0].(core.RelayObservationHarnessTurnStarted); !started {
				t.Fatalf("%#v", observed)
			}
			rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		}
		rtNoErr(relay.applyClaudeTaskFixture("turn-a", true, true, false, false))
		rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, turn.FirstOrdinal)
		rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnStarted](observations(t, relay)), 1)
		rtNoErr(relay.applyClaudeTaskFixture("turn-a", false, false, true, false))
		// A duplicate old task end cannot settle a later autonomous cycle.
		rtNoErr(relay.applyClaudeTaskFixture("turn-b", true, true, false, false))
		next := relay.snapshot.HarnessTurn.FirstOrdinal
		rtNoErr(relay.applyClaudeTaskFixture("turn-a", true, true, false, false))
		rtNoErr(relay.applyClaudeTaskFixture("turn-a", false, false, true, false))
		rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, next)
		rtTaskSettledAtMs(t, relay)
		rtNoErr(relay.applyClaudeTaskFixture("turn-b", false, false, true, false))
		if relay.snapshot.HarnessTurn != nil {
			t.Fatal("turn-b settled its turn")
		}
	}
}

func TestAnAutonomousTaskEndAfterOutputReleasesItsQueuedPromptOnce(t *testing.T) {
	for _, terminal := range []struct{ settled, failed bool }{{true, false}, {true, true}, {false, false}} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		rtNoErr(relay.applyClaudeTaskFixture("silent-turn", true, true, false, false))
		rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		submitRelay(relay, "next-prompt", prompt("answer next"))
		rtNoClaims(t, relay)
		// Every terminal state ends its own turn and preserves its outcome.
		rtNoErr(relay.AirTaskChanged(*relay.snapshot.NativeSessionID, core.AirTaskUpdateFinished{
			TaskID:  "silent-turn",
			Settled: terminal.settled,
			Failed:  terminal.failed,
		}))
		// Repeated terminal edges still name an autonomous task, never a new
		// background-task completion or a second completion boundary.
		rtNoErr(relay.applyClaudeTaskFixture("silent-turn", false, false, true, false))
		result := cycleResult(t, "task-notification")
		rtNoErr(relay.ClaudeTurnResult(&result))
		if relay.snapshot.HarnessTurn != nil {
			t.Fatal("the task end settled the turn")
		}
		rtTaskSettledAtMs(t, relay)
		rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "next-prompt")
		rtNoClaims(t, relay)
		if _, started := observations(t, relay)[0].(core.RelayObservationHarnessTurnStarted); !started {
			t.Fatalf("%#v", observations(t, relay)[0])
		}
		rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnSettled](observations(t, relay)), 1)
	}
}

// Hard-won: agentclientprotocol/claude-agent-acp#1390: a num_turns 0 placeholder result closed an announced autonomous turn early.
func TestAutonomousResultsKeepTheTurnAndQueuedPromptUntilTaskEnd(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("turn", true, true, false, false))
	ordinal := relay.snapshot.HarnessTurn.FirstOrdinal
	submitRelay(relay, "queued-prompt", prompt("next"))
	placeholder := cycleResult(t, "task-notification")
	placeholder.NumTurns = 0
	rtNoErr(relay.ClaudeTurnResult(&placeholder))
	rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, ordinal)
	rtNoClaims(t, relay)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, ordinal)
	rtNoClaims(t, relay)
	rtNoErr(relay.applyClaudeTaskFixture("turn", false, false, true, false))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("the task end settled the turn")
	}
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "queued-prompt")
	events := observations(t, relay)
	rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnStarted](events), 1)
	rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnSettled](events), 1)
}

func TestAutonomousPlaceholderWithoutOutputSettlesOnTaskEnd(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("turn", true, true, false, false))
	placeholder := cycleResult(t, "task-notification")
	placeholder.NumTurns = 0
	rtNoErr(relay.ClaudeTurnResult(&placeholder))
	if relay.snapshot.HarnessTurn == nil {
		t.Fatal("a placeholder keeps the turn")
	}
	rtNoErr(relay.applyClaudeTaskFixture("turn", false, false, true, false))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("the task end settled the turn")
	}
	rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnSettled](observations(t, relay)), 1)
}

// Hard-won: agentclientprotocol/claude-agent-acp#1453: trailing cycle events closed the next autonomous turn early.
func TestTrailingResultsAndOldTaskEndsCannotSettleTheNextAnnouncedTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("turn-a", true, true, false, false))
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	rtNoErr(relay.applyClaudeTaskFixture("turn-a", false, false, true, false))
	rtNoErr(relay.applyClaudeTaskFixture("turn-b", true, true, false, false))
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	ordinal := relay.snapshot.HarnessTurn.FirstOrdinal
	taskOrdinal, ok := relay.turnTaskOrdinal("turn-b")
	if !ok || taskOrdinal != ordinal {
		t.Fatalf("turn-b names ordinal %d (%v), expected %d", taskOrdinal, ok, ordinal)
	}
	submitRelay(relay, "queued-prompt", prompt("next"))
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtNoErr(relay.applyClaudeTaskFixture("turn-a", false, false, true, false))
	rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, ordinal)
	rtNoClaims(t, relay)
	rtNoErr(relay.applyClaudeTaskFixture("turn-b", false, false, true, false))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("turn-b settled its turn")
	}
	rtEqual(t, rtCountObservations[core.RelayObservationHarnessTurnSettled](observations(t, relay)), 2)
}

func TestReplayedAutonomousSpawnAndUntypedEndLeaveNoImminentTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	relay.setAcpReady(true)
	rtNoErr(relay.applyClaudeTaskFixture("replayed", true, true, false, true))
	if _, ok := relay.turnTaskOrdinal("replayed"); ok {
		t.Fatal("a replayed spawn names no turn")
	}
	rtNoErr(relay.applyClaudeTaskFixture("replayed", false, false, true, false))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("a replayed spawn opens no turn")
	}
	rtTaskSettledAtMs(t, relay)
	if state := relay.OperationalState(); !state.IsQuiet() {
		t.Fatalf("%#v", state)
	}
	for _, event := range observations(t, relay) {
		switch event.(type) {
		case core.RelayObservationHarnessTurnStarted, core.RelayObservationHarnessTurnSettled:
			t.Fatalf("unexpected harness turn observation %#v", event)
		}
	}
}

func TestZeroTurnResultsOnlySettleTurnsWithoutAnAnnouncedTask(t *testing.T) {
	for _, bound := range []bool{false, true} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		if bound {
			rtNoErr(relay.applyClaudeTaskFixture("turn", true, true, false, false))
		} else {
			rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		}
		result := cycleResult(t, "task-notification")
		result.NumTurns = 0
		result.IsError = bound
		rtNoErr(relay.ClaudeTurnResult(&result))
		if bound {
			if relay.snapshot.HarnessTurn == nil {
				t.Fatal("an announced task keeps its turn")
			}
			rtNoErr(relay.applyClaudeTaskFixture("turn", false, false, false, false))
		}
		if relay.snapshot.HarnessTurn != nil {
			t.Fatalf("bound=%v: the turn is still open", bound)
		}
	}
}

func TestAnAutonomousTaskEndLeavesAPromptThatTookOwnershipRunning(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("turn-a", true, true, false, false))
	// Legacy overlap: a queued mj prompt has already reached the bridge.
	submitRelay(relay, "owner-prompt", prompt("continue"))
	rtMust(relay.RecordObservation(core.RelayObservationCommandStarted{CommandID: "owner-prompt", StartedAtMs: 42}))
	rtNoErr(relay.applyClaudeTaskFixture("turn-b", true, true, false, false))
	if _, ok := relay.turnTaskOrdinal("turn-b"); ok {
		t.Fatal("a prompt owns the turn")
	}
	rtNoErr(relay.applyClaudeTaskFixture("turn-b", false, false, true, false))
	rtNoErr(relay.applyClaudeTaskFixture("turn-a", false, false, true, false))
	rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "owner-prompt")
	if relay.snapshot.HarnessTurn == nil {
		t.Fatal("the prompt's turn stays open")
	}
	rtEqual(t, relay.snapshot.Execution, core.RelayExecutionStateRunning)
	if rtCountObservations[core.RelayObservationHarnessTurnSettled](observations(t, relay)) != 0 {
		t.Fatal("no harness turn settled")
	}
}

func TestRestartingASilentAutonomousTurnClearsItsProcessLocalBinding(t *testing.T) {
	temp := t.TempDir()
	var ordinal uint64
	{
		relay := relayTestsClaudeRelay(t, temp)
		rtNoErr(relay.applyClaudeTaskFixture("old-turn", true, true, false, false))
		ordinal = relay.snapshot.HarnessTurn.FirstOrdinal
	}
	relay := relayTestsClaudeRelay(t, temp)
	rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, ordinal)
	if len(relay.airTasks) != 0 {
		t.Fatalf("AIR tasks are process-local: %#v", relay.airTasks)
	}
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("a restart clears the silent turn")
	}
	rtEqual(t, relay.snapshot.Execution, core.RelayExecutionStateIdle)
}

// F18: a turn the harness started on its own that the provider refuses for
// quota ends like any other cycle: the marker is gone, no tool is left in
// flight and a prompt could start. (Session e33442fe's recovery was lost to a
// checkpoint, not to a leftover fact; this pins that the facts are clean.)
//
// Hard-won: #1450 review: autonomous outcomes must preserve turn identity and queued work.
func TestAHarnessTurnRefusedForQuotaEndsWithNothingLeftSet(t *testing.T) {
	for _, announced := range []bool{false, true} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		if announced {
			rtNoErr(relay.applyClaudeTaskFixture("quota-turn", true, true, false, false))
		}
		rtMust(relay.RecordSessionUpdate(agentTextChunk("You've hit your session limit · resets 1:10pm")))
		if state := relay.OperationalState(); state.Facts().HarnessTurnStartedAtMs == nil {
			t.Fatal("the refusal text opens a harness turn")
		}
		refusal, err := core.ClaudeTurnResultFromSDKMessage(map[string]any{
			"type": "result", "subtype": "success", "is_error": true, "num_turns": 0,
			"stop_reason": "stop_sequence", "result": "You've hit your session limit",
			"usage":  map[string]any{"input_tokens": 0, "output_tokens": 0},
			"origin": map[string]any{"kind": "task-notification"},
		})
		rtNoErr(err)
		rtNoErr(relay.ClaudeTurnResult(refusal))
		if announced {
			if relay.snapshot.HarnessTurn == nil {
				t.Fatal("an announced task keeps its turn")
			}
			rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateFinished{TaskID: "quota-turn", Settled: true, Failed: true}))
		}
		// Hard-won: #1450 review: quota refusals were recorded as successful turns.
		settled, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled)
		if !ok || settled.Autonomous == nil || settled.Autonomous.StopReason != "error" {
			t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
		}
		state := relay.OperationalState()
		facts := state.Facts()
		if facts.HarnessTurnStartedAtMs != nil {
			t.Fatal("the turn marker is gone")
		}
		if len(facts.ToolsInFlight) != 0 {
			t.Fatalf("%#v", facts.ToolsInFlight)
		}
		rtEqual(t, facts.Execution, core.RelayExecutionStateIdle)
		// The bare test relay has no ACP connection, the one fact that may differ.
		ready := facts
		ready.AcpReady = rtBool(true)
		if !core.CanSubmit(&ready) {
			t.Fatalf("%#v", ready)
		}
	}
}

// Hard-won: 6b6e684c: Claude cycles without an origin marker left sessions running until a later marker.
func TestAgentOutputAtIdleOpensAHarnessTurnAndItsCycleResultSettlesIt(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	setVerdictHarness(t, relay, core.HarnessKindClaude)

	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))

	running := relay.OperationalState()
	rtEqual(t, running.Execution, core.RelayExecutionStateRunning)
	if running.ActivePrompt != nil {
		t.Fatal("no prompt is in flight")
	}
	if running.HarnessTurn == nil {
		t.Fatal("a harness turn is open")
	}
	if running.HarnessTurn.StartedAtMs <= 0 {
		t.Fatalf("turn started at %d", running.HarnessTurn.StartedAtMs)
	}
	rtEqual(t, running.LastHarnessTurnStartedOrdinal, rtUint64(1))
	observed := observations(t, relay)
	if len(observed) != 2 {
		t.Fatalf("%#v", observed)
	}
	_, started := observed[0].(core.RelayObservationHarnessTurnStarted)
	_, update := observed[1].(core.RelayObservationSessionUpdate)
	if !started || !update {
		t.Fatalf("%#v", observed)
	}

	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))

	settled := relay.OperationalState()
	rtEqual(t, settled.Execution, core.RelayExecutionStateIdle)
	if settled.HarnessTurn != nil {
		t.Fatal("the cycle result settles the turn")
	}
	// The confirmed turn boundary clears any tool status the adapter left open.
	if settled.ForegroundToolStartedAtMs != nil {
		t.Fatalf("a tool is still in the foreground at %d", *settled.ForegroundToolStartedAtMs)
	}
	// The started ordinal only moves forward, so a checkpoint can compare
	// against it.
	rtEqual(t, settled.LastHarnessTurnStartedOrdinal, rtUint64(1))
	last, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled)
	if !ok || last.Origin == nil || *last.Origin != "task-notification" {
		t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
	}
	rtNoErr(relay.preparePendingAssessment())
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("settled harness turn is classified")
	}
	rtEqual(t, pending.Evidence.Phase, core.TurnPhaseReplied)
	if _, again := relay.PendingRepliedVerdict(); again {
		t.Fatal("the verdict is taken once")
	}
}

// The adapter's origin marker is left out when a cycle produced no
// assistant usage, so it cannot be what ends a Claude turn. The result can:
// Claude Code sends one for every cycle.
//
// Hard-won: 6b6e684c: Claude cycles without an origin marker left sessions running until a later marker.
func TestAClaudeHarnessTurnSettlesOnItsResultAndNotOnTheOriginMarker(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)

	rtMust(relay.RecordSessionUpdate(agentTextChunk("The build finished.")))
	rtMust(relay.RecordSessionUpdate(originMarker("task-notification")))
	state := relay.OperationalState()
	if state.HarnessTurn == nil {
		t.Fatal("the marker alone settles nothing")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateRunning)

	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	state = relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("the result settles the turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)

	// A cycle that streams text but sends no marker still ends.
	rtMust(relay.RecordSessionUpdate(agentTextChunk("One more thing.")))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("output opens a turn")
	}
	human := cycleResult(t, "human")
	rtNoErr(relay.ClaudeTurnResult(&human))
	if relay.OperationalState().HarnessTurn != nil {
		t.Fatal("the result settles the turn")
	}
	last, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled)
	if !ok || last.PromptInFlight || last.Origin == nil || *last.Origin != "human" {
		t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
	}

	// A result with no turn open records nothing.
	before := relay.OperationalState().LatestOrdinal
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtEqual(t, relay.OperationalState().LatestOrdinal, before)
}

// Stop while Claude Code works on its own after a background task sends
// `session/cancel`; an unanswered Stop ends even an announced turn after
// the grace period. With nothing running, or during a Codex goal turn,
// Stop is still refused.
//
// Hard-won: 4c379acf: Stop was unavailable while Claude ran a cycle started by a completed background task.
// Hard-won: #1530: an announced Claude turn whose task never ended ignored Stop and blocked the queue for hours.
func TestUnansweredStopEndsAnAnnouncedClaudeTurnAndReleasesItsTask(t *testing.T) {
	refused := func(relay *DurableRelay, commandID string) string {
		response := relay.Handle(relayRequest("request-"+commandID, core.RelayRequestSubmit{
			CommandID: commandID,
			Command:   core.RelayCommandCancel{},
		}))
		failure, ok := response.Body.(core.RelayResponseBodyError)
		if !ok {
			t.Fatalf("Stop must be refused: %#v", response.Body)
		}
		return failure.Error.Message
	}
	unanswered := func(relay *DurableRelay) *uint64 {
		if ordinal, ok := relay.UnansweredHarnessTurnStop(); ok {
			return &ordinal
		}
		return nil
	}
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtEqual(t, refused(relay, "cancel-idle"), "there is no active prompt to cancel")

	rtNoErr(relay.applyClaudeTaskFixture("silent-turn", true, true, false, false))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("the announced task opens a turn")
	}
	submitRelay(relay, "queued-prompt", prompt("next"))
	submitRelay(relay, "cancel-harness-turn", core.RelayCommandCancel{})
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cancel-harness-turn")
	rtMust(relay.RecordCommandCompleted("cancel-harness-turn", core.RelayCommandOutcomeCancelled{}))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("the applied stop waits for its grace period")
	}
	ordinal := relay.snapshot.HarnessTurn.FirstOrdinal
	rtEqual(t, unanswered(relay), rtUint64(ordinal))
	if !rtMust(relay.EndUnansweredHarnessTurnStop(ordinal)) {
		t.Fatal("the unanswered stop ends the turn")
	}
	if _, ok := relay.turnTaskOrdinal("silent-turn"); ok {
		t.Fatal("the stop released the task")
	}
	if relay.snapshot.Assessment == nil {
		t.Fatal("queued work must not erase the autonomous turn assessment")
	}
	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("the turn ended")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtNoErr(relay.applyClaudeTaskFixture("silent-turn", false, false, false, false))
	if relay.snapshot.ActivePrompt == nil {
		t.Fatal("the queued prompt is running")
	}
	rtMust(relay.RecordCommandCompleted("queued-prompt", rtPromptOutcome("end_turn")))
	// A raw result can precede trailing output. The detached task must not
	// settle the new turn, but another Stop must still be able to end it.
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtMust(relay.RecordSessionUpdate(agentTextChunk("late output")))
	lateOrdinal := relay.snapshot.HarnessTurn.FirstOrdinal
	if lateOrdinal == ordinal {
		t.Fatal("late output opens a new turn")
	}
	rtNoErr(relay.applyClaudeTaskFixture("silent-turn", false, false, false, false))
	rtEqual(t, relay.snapshot.HarnessTurn.FirstOrdinal, lateOrdinal)
	submitRelay(relay, "stop-late-output", core.RelayCommandCancel{})
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	rtMust(relay.RecordCommandCompleted("stop-late-output", core.RelayCommandOutcomeCancelled{}))
	rtEqual(t, unanswered(relay), rtUint64(lateOrdinal))
	if !rtMust(relay.EndUnansweredHarnessTurnStop(lateOrdinal)) {
		t.Fatal("another stop ends the late turn")
	}

	// Announced ownership is process-local; replay retains the applied stop.
	replayTemp := t.TempDir()
	replay := relayTestsClaudeRelay(t, replayTemp)
	rtNoErr(replay.applyClaudeTaskFixture("lost-task", true, true, false, false))
	submitRelay(replay, "stop-before-restart", core.RelayCommandCancel{})
	rtMust(replay.ClaimPendingCommands(true))
	rtMust(replay.RecordCommandCompleted("stop-before-restart", core.RelayCommandOutcomeCancelled{}))
	stopped, ok := replay.UnansweredHarnessTurnStop()
	if !ok {
		t.Fatal("the applied stop is unanswered")
	}
	// A pre-revision-24 snapshot has only the legacy continuation field.
	rtRewriteRelayState(t, replayTemp, func(legacy map[string]any) {
		delete(legacy["harness_turn"].(map[string]any), "autonomous")
		legacy["format_version"] = 22
		// serde_json's IndexMut creates the object when it is missing.
		continuation, _ := legacy["continuation"].(map[string]any)
		if continuation == nil {
			continuation = map[string]any{}
			legacy["continuation"] = continuation
		}
		continuation["autonomous_turn_started"] = stopped
	})
	replay = relayTestsClaudeRelay(t, replayTemp)
	rtEqual(t, unanswered(replay), rtUint64(stopped))
	if !rtMust(replay.EndUnansweredHarnessTurnStop(stopped)) {
		t.Fatal("the replayed stop ends the turn")
	}
	replay = relayTestsClaudeRelay(t, replayTemp)
	if replay.snapshot.HarnessTurn != nil {
		t.Fatal("the ended turn stays ended")
	}

	codex := rtMust(DurableRelayOpen(t.TempDir(), session, "1.0.0"))
	codex.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
	rtMust(codex.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}))
	if codex.OperationalState().HarnessTurn == nil {
		t.Fatal("the Codex goal turn is open")
	}
	rtEqual(t, refused(codex, "cancel-codex"), "the agent is working on its own after a background task; there is no prompt to cancel")
}

// rtStringSet is a BTreeSet<String> of the given members.
func rtStringSet(members ...string) map[string]struct{} {
	set := make(map[string]struct{}, len(members))
	for _, member := range members {
		set[member] = struct{}{}
	}
	return set
}

// R4-2: the adapter's answer to a model change is not a model cycle, so no
// result ever follows it. A turn opened for that text stayed Running for
// minutes and nothing could stop it.
//
// Hard-won: c0f3babc: a model-change notice opened a phantom turn that no cycle result could settle.
func TestTextThatAnswersAModelChangeOpensNoHarnessTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	submitRelay(relay, "model-haiku", setConfig("model", "haiku"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "model-haiku")

	rtNoErr(relay.applyClaudeTaskFixture("config-turn", true, true, false, false))
	rtNoErr(relay.applyClaudeTaskFixture("config-turn", false, false, true, false))
	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeAutoModeFallbackText)))
	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("the answer to a configuration request is not a turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	// As a notice row of its own since R8-5.
	noticed := false
	for _, observation := range observations(t, relay) {
		if notice, ok := observation.(core.RelayObservationNotice); ok && notice.Message == strings.ReplaceAll(claudeAutoModeFallbackText, "**", "") {
			noticed = true
		}
	}
	if !noticed {
		t.Fatal("the notice still reaches the conversation")
	}

	rtMust(relay.RecordObservation(core.RelayObservationConfigurationUpdated{Key: "model", Value: "haiku"}))
	rtMust(relay.RecordCommandCompleted("model-haiku", core.RelayCommandOutcomeConfigured{}))
	state = relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("configuration opens no turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)

	// Text with no request in flight is still Claude Code working on its own.
	rtMust(relay.RecordSessionUpdate(agentTextChunk("The build finished.")))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("text at idle opens a turn")
	}
}

// R4-2: a stop of a turn Claude Code never ran is answered by nothing, so
// the relay ends that turn itself once the stop has had time to be answered.
// A real cycle's result still ends the turn first.
//
// Hard-won: c0f3babc: a no-op stop left a phantom Claude turn working because no result would arrive.
func TestAStopTheHarnessNeverAnswersEndsTheSelfStartedTurn(t *testing.T) {
	lastSettled := func(relay *DurableRelay) core.RelayObservationHarnessTurnSettled {
		t.Helper()
		settled, ok := rtLastObservation(t, relay).(core.RelayObservationHarnessTurnSettled)
		if !ok {
			t.Fatalf("unexpected last observation %#v", rtLastObservation(t, relay))
		}
		return settled
	}
	for _, command := range []core.RelayCommand{core.RelayCommandCancel{}, core.RelayCommandCancelTurn{}} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeAutoModeFallbackText)))
		turn := relay.OperationalState().HarnessTurn
		if turn == nil {
			t.Fatal("text at idle opens a turn")
		}
		if _, ok := relay.UnansweredHarnessTurnStop(); ok {
			t.Fatal("no stop was sent")
		}

		submitRelay(relay, "stop-phantom", command)
		rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
		rtMust(relay.RecordCommandCompleted("stop-phantom", core.RelayCommandOutcomeCancelled{}))
		firstOrdinal, ok := relay.UnansweredHarnessTurnStop()
		if !ok {
			t.Fatal("the applied stop waits for the harness to answer it")
		}
		// The turn stays open while the harness may still answer.
		rtEqual(t, relay.OperationalState().HarnessTurn, turn)

		if !rtMust(relay.EndUnansweredHarnessTurnStop(firstOrdinal)) {
			t.Fatalf("%#v", command)
		}
		state := relay.OperationalState()
		if state.HarnessTurn != nil {
			t.Fatal("the stop ended the turn")
		}
		rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
		if lastSettled(relay).PromptInFlight {
			t.Fatal("no prompt was in flight")
		}
		if _, ok := relay.UnansweredHarnessTurnStop(); ok {
			t.Fatal("the stop was answered")
		}
	}

	// The interrupted cycle's result arrives in time: nothing is left to end.
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(agentTextChunk("Summarizing")))
	submitRelay(relay, "stop-real", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("stop-real", core.RelayCommandOutcomeCancelled{}))
	firstOrdinal, ok := relay.UnansweredHarnessTurnStop()
	if !ok {
		t.Fatal("the applied stop waits for the harness to answer it")
	}
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	if settled := lastSettled(relay); settled.Autonomous == nil || settled.Autonomous.StopReason != "cancelled" || settled.Autonomous.StartOrdinal != firstOrdinal {
		t.Fatalf("unexpected settlement %#v", settled)
	}
	if _, ok := relay.UnansweredHarnessTurnStop(); ok {
		t.Fatal("the result answered the stop")
	}
	before := relay.OperationalState().LatestOrdinal
	if rtMust(relay.EndUnansweredHarnessTurnStop(firstOrdinal)) {
		t.Fatal("nothing is left to end")
	}
	rtEqual(t, relay.OperationalState().LatestOrdinal, before)

	// Hard-won: #1450 review: a result that beat the cancel's acknowledgement settled the stopped turn as finished
	temp = t.TempDir()
	relay = relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(agentTextChunk("Summarizing")))
	submitRelay(relay, "stop-fast", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtNoErr(relay.ClaudeTurnResult(&result))
	if settled := lastSettled(relay); settled.Autonomous == nil || settled.Autonomous.StopReason != "cancelled" {
		t.Fatalf("unexpected settlement %#v", settled)
	}
	rtMust(relay.RecordCommandCompleted("stop-fast", core.RelayCommandOutcomeCancelled{}))
	if relay.snapshot.CancellingHarnessTurn != nil {
		t.Fatal("the cancel completed")
	}
	if _, ok := relay.UnansweredHarnessTurnStop(); ok {
		t.Fatal("the stop was answered")
	}

	// A stop the runtime could not send leaves the turn to end as it does.
	rtMust(relay.RecordSessionUpdate(agentTextChunk("Again")))
	submitRelay(relay, "stop-refused", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandRejected("stop-refused", core.OutcomeReasonCommandFailed, "cancel ACP prompt: closed"))
	rtNoErr(relay.ClaudeTurnResult(&result))
	if settled := lastSettled(relay); settled.Autonomous == nil || settled.Autonomous.StopReason != "end_turn" {
		t.Fatalf("unexpected settlement %#v", settled)
	}

	// Hard-won: 3b2f0d538: a second stop downgraded an applied stop and its rejection cleared it
	temp = t.TempDir()
	relay = relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(agentTextChunk("Summarizing")))
	submitRelay(relay, "stop-first", core.RelayCommandCancelTurn{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("stop-first", core.RelayCommandOutcomeCancelled{}))
	submitRelay(relay, "stop-second", core.RelayCommandCancel{})
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandRejected("stop-second", core.OutcomeReasonCommandFailed, "cancel ACP prompt: closed"))
	firstOrdinal, ok = relay.UnansweredHarnessTurnStop()
	if !ok {
		t.Fatal("the acknowledged first stop outlives the refused second one")
	}
	// The target is journal state, so a restarted worker still has it.
	relay = relayTestsClaudeRelay(t, temp)
	if ordinal, ok := relay.UnansweredHarnessTurnStop(); !ok || ordinal != firstOrdinal {
		t.Fatalf("unanswered stop %d (%v), expected %d", ordinal, ok, firstOrdinal)
	}
	if !rtMust(relay.EndUnansweredHarnessTurnStop(firstOrdinal)) {
		t.Fatal("the replayed stop ends the turn")
	}
	if settled := lastSettled(relay); settled.Autonomous == nil || settled.Autonomous.StopReason != "cancelled" {
		t.Fatalf("unexpected settlement %#v", settled)
	}
}

// rtCommandStarted reports whether the journal holds a CommandStarted for one
// of the commands.
func rtCommandStarted(t *testing.T, relay *DurableRelay, commandIDs ...string) int {
	t.Helper()
	count := 0
	for _, observation := range observations(t, relay) {
		if started, ok := observation.(core.RelayObservationCommandStarted); ok && slices.Contains(commandIDs, started.CommandID) {
			count++
		}
	}
	return count
}

// Hard-won: 1389c545: Claude work without a prompt looked idle and allowed a recovery checkpoint mid-cycle.
func TestAHarnessTurnHoldsTheCheckpointBarrierUntilItSettles(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	submitRelay(relay, "typed-mid-turn", prompt("wait for the cycle"))

	submitRelay(relay, "barrier-command", core.RelayCommandBeginCheckpoint{Reason: nil})
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("a turn the harness started on its own must keep the barrier queued: %#v", claimed)
	}
	rtEqual(t, len(relay.OperationalState().QueuedPrompts), 1)
	if rtCommandStarted(t, relay, "barrier-command", "typed-mid-turn") != 0 {
		t.Fatal("the same open-turn state holds both checkpoint admission and prompt promotion")
	}

	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))

	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "barrier-command")
	rtEqual(t, len(relay.OperationalState().QueuedPrompts), 1)
}

// Hard-won: #1225: Claude queued a live prompt behind its autonomous turn while the relay marked it active.
func TestAPromptQueuedDuringAHarnessTurnWaitsThenDispatchesOnce(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("autonomous-turn", true, true, false, false))

	submitRelay(relay, "typed-mid-turn", prompt("answer this too"))
	rtNoClaims(t, relay)
	waiting := relay.OperationalState()
	rtEqual(t, len(waiting.QueuedPrompts), 1)
	rtEqual(t, waiting.QueuedPrompts[0].CommandID, "typed-mid-turn")
	if waiting.ActivePrompt != nil {
		t.Fatal("the prompt waits")
	}
	facts := relay.ActivityFacts()
	if state := core.Classify(&facts); state.Kind != core.ActivityStateTurn {
		t.Fatalf("expected a turn, got %#v", state)
	}
	if rtCommandStarted(t, relay, "typed-mid-turn") != 0 {
		t.Fatal("the queued prompt has no start event yet")
	}

	rtNoErr(relay.applyClaudeTaskFixture("autonomous-turn", false, false, true, false))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "typed-mid-turn")
	// The queued prompt starts exactly once.
	rtEqual(t, rtCommandStarted(t, relay, "typed-mid-turn"), 1)
	rtNoClaims(t, relay)

	// Barrier priority over queued prompts is an invariant: a pending
	// barrier still freezes a prompt typed after it.
	temp = t.TempDir()
	relay = relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	submitRelay(relay, "barrier-command", core.RelayCommandBeginCheckpoint{Reason: nil})
	submitRelay(relay, "typed-after-barrier", prompt("wait for me"))

	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("a pending barrier still outranks a prompt typed during a harness turn: %#v", claimed)
	}
}

func TestAPromptResultSettlesALingeringHarnessTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	submitRelay(relay, "next-prompt", prompt("carry on"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	// A previous relay version could dispatch this prompt behind an
	// autonomous Claude cycle; preserve the terminal cleanup for that state.
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}))

	rtMust(relay.RecordCommandCompleted("next-prompt", rtPromptOutcome("end_turn")))

	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("a prompt result means the SDK reached a turn boundary")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	if state.ForegroundToolStartedAtMs != nil {
		t.Fatal("the prompt result outranks a stale pending tool status")
	}
}

func TestARestartClearsAHarnessTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.applyClaudeTaskFixture("autonomous-turn", true, true, false, false))

	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))

	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("a restart clears the turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
}

func TestKimiTaskQueriesReconcileByNativeIdentityInEitherEventOrder(t *testing.T) {
	for _, nativeFirst := range []bool{false, true} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, "kimi-queries", "test"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		query := func(callID, title string) schema.SessionUpdate {
			status := schema.ToolCallStatusCompleted
			call := schema.ToolCall{
				ToolCallID: schema.ToolCallId(callID),
				Title:      title,
				Status:     &status,
				RawInput:   json.RawMessage(`{"task_id":"bash-tlqj0v63"}`),
				RawOutput:  json.RawMessage(`{"output":"retrieval_status: not_ready\ntask_id: bash-tlqj0v63\nstatus: running\nparent_tool_call_id: tool-launcher\n"}`),
			}
			return schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
		}
		native := KimiBackgroundTask{
			IsAgent:          false,
			TaskID:           "bash-tlqj0v63",
			Description:      "validation",
			StartedAtMs:      1_000,
			ParentToolCallID: rtString("tool-launcher"),
		}
		tools := rtStringSet("tool-launcher")
		tasks := rtStringSet("bash-tlqj0v63")
		if nativeFirst {
			rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{native}, tools, tasks))
		}
		for _, card := range []struct{ id, title string }{{"0:query-one", "TaskOutput"}, {"0:query-two", "WaitFor"}} {
			rtMust(relay.RecordSessionUpdate(query(card.id, card.title)))
			rtEqual(t, len(relay.OperationalState().BackgroundCommands), 1)
		}
		rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{native}, tools, tasks))
		rtEqual(t, len(relay.OperationalState().BackgroundCommands), 1)
		rtNoErr(relay.KimiBackgroundTasksChanged(nil, tools, tasks))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%#v", commands)
		}
		rtMust(relay.RecordSessionUpdate(query("0:late-query", "TaskOutput")))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%#v", commands)
		}
		if state := relay.OperationalState(); !state.IsQuiet() {
			t.Fatalf("%#v", state)
		}
	}
}

// rtBackgroundCommandIDs lists the published background command IDs.
func rtBackgroundCommandIDs(relay *DurableRelay) []string {
	var ids []string
	for _, command := range relay.OperationalState().BackgroundCommands {
		ids = append(ids, command.ID)
	}
	return ids
}

// Hard-won: 492a4bd: Kimi detached Bash work disappeared from the background task list.
func TestKimiDetachedShellIsListedOnceAsItsHostedTerminalDuringTheTurn(t *testing.T) {
	temp := t.TempDir()
	relay := kimiRelayWithPromptInFlight(t, temp)
	rtMust(relay.RecordSessionUpdate(kimiBackgroundShellCard("3:tool_bash", rtString("term-60"))))
	rtNoErr(relay.AgentTerminalStarted(core.ActiveAgentTerminal{
		TerminalID:  "term-60",
		Command:     "cargo build --release",
		StartedAtMs: 2_000,
	}))
	rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{kimiProcessTask("tool_bash")}, rtStringSet("tool_bash"), rtStringSet()))

	// The detached shell is one job, stoppable through its terminal.
	rtEqual(t, rtBackgroundCommandIDs(relay), []string{"terminal:term-60"})

	rtNoErr(relay.AgentTerminalClosed("term-60"))
	rtNoErr(relay.KimiBackgroundTasksChanged(nil, rtStringSet("tool_bash"), rtStringSet()))
	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("the shell exited, so nothing is left running: %#v", commands)
	}
}

// Hard-won: 492a4bd: Kimi detached Bash work disappeared when no terminal was bound.
func TestKimiDetachedShellWithoutABoundTerminalIsListedAsANativeTask(t *testing.T) {
	temp := t.TempDir()
	relay := kimiRelayWithPromptInFlight(t, temp)
	rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{kimiProcessTask("tool_bash")}, rtStringSet("tool_bash"), rtStringSet()))

	// Without ACP evidence the native record is all there is.
	rtEqual(t, rtBackgroundCommandIDs(relay), []string{"kimi:bash-r5ae"})
}

func TestKimiAgentAcpEvidenceTracksEachBackgroundAlternativeIndependently(t *testing.T) {
	for _, update := range []schema.SessionUpdate{
		kimiBackgroundAgentCardWithRunInBackgroundOnly(),
		kimiBackgroundAgentCardWithRunningTaskIDOnly(),
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
		rtMust(relay.RecordSessionUpdate(update))

		state := relay.OperationalState()
		rtEqual(t, len(state.BackgroundCommands), 1)
		rtEqual(t, state.BackgroundWorkKnown, rtBool(false))
		if state.IsQuiet() {
			t.Fatal("provisional work keeps the session busy")
		}
	}
}

func rtAgentDeadbeefTask() KimiBackgroundTask {
	return KimiBackgroundTask{
		IsAgent:          false,
		TaskID:           "agent-deadbeef",
		Description:      "Fix memory use",
		StartedAtMs:      1_000,
		ParentToolCallID: rtString("tool_agent"),
	}
}

func TestUnmatchedKimiProvisionalWorkSurvivesEmptyNativeScanUntilEvidenceOrTeardown(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	rtMust(relay.RecordSessionUpdate(kimiBackgroundAgentCard()))

	rtNoErr(relay.KimiBackgroundTasksChanged(nil, rtStringSet(), rtStringSet()))
	rtEqual(t, len(relay.OperationalState().BackgroundCommands), 1)

	rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{rtAgentDeadbeefTask()}, rtStringSet("tool_agent"), rtStringSet()))
	state := relay.OperationalState()
	rtEqual(t, len(state.BackgroundCommands), 1)
	rtEqual(t, state.BackgroundCommands[0].ID, "kimi:agent-deadbeef")

	for _, observation := range []core.RelayObservation{
		core.RelayObservationSessionRestarted{},
		core.RelayObservationClosing{},
		core.RelayObservationClosed{},
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
		rtMust(relay.RecordSessionUpdate(kimiBackgroundAgentCard()))
		rtNoErr(relay.KimiBackgroundTasksChanged(nil, rtStringSet(), rtStringSet()))
		rtMust(relay.RecordObservation(observation))
		state := relay.OperationalState()
		if len(state.BackgroundCommands) != 0 {
			t.Fatalf("%#v", state.BackgroundCommands)
		}
		rtEqual(t, state.BackgroundWorkKnown, rtBool(false))
	}
}

func TestKimiBackgroundAgentSurvivesItsParentPromptUntilNativeTermination(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	submitRelay(relay, "parent-command", prompt("delegate this"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)

	rtMust(relay.RecordSessionUpdate(kimiBackgroundAgentCard()))
	rtMust(relay.RecordCommandCompleted("parent-command", rtPromptOutcome("end_turn")))

	provisional := relay.OperationalState()
	rtEqual(t, len(provisional.BackgroundCommands), 1)
	rtEqual(t, provisional.BackgroundWorkKnown, rtBool(false))
	if provisional.IsQuiet() {
		t.Fatal("provisional work keeps the session busy")
	}

	rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{rtAgentDeadbeefTask()}, rtStringSet("tool_agent"), rtStringSet()))
	running := relay.OperationalState()
	rtEqual(t, len(running.BackgroundCommands), 1)
	rtEqual(t, running.BackgroundWorkKnown, rtBool(true))
	if running.IsQuiet() {
		t.Fatal("native work keeps the session busy")
	}

	rtNoErr(relay.KimiBackgroundTasksChanged(nil, rtStringSet("tool_agent"), rtStringSet()))
	terminated := relay.OperationalState()
	if len(terminated.BackgroundCommands) != 0 {
		t.Fatalf("%#v", terminated.BackgroundCommands)
	}
	rtEqual(t, terminated.BackgroundWorkKnown, rtBool(true))
	if !terminated.IsQuiet() {
		t.Fatal("terminated work leaves the session quiet")
	}
}

func TestKimiTrackerFailureRetainsWorkAndBlocksReplacement(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	task := rtAgentDeadbeefTask()
	task.ParentToolCallID = nil
	rtNoErr(relay.KimiBackgroundTasksChanged([]KimiBackgroundTask{task}, rtStringSet(), rtStringSet()))

	rtNoErr(relay.KimiBackgroundTasksUnavailable())

	state := relay.OperationalState()
	rtEqual(t, len(state.BackgroundCommands), 1)
	rtEqual(t, state.BackgroundWorkKnown, rtBool(false))
	if state.IsQuiet() {
		t.Fatal("unknown background work blocks replacement")
	}
}

// Hard-won: db2e99a5: sessions with a command left running were shown as idle on every surface.
func TestATerminalTheAgentLeftRunningIsBackgroundWorkOnceTheTurnEnds(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	rtNoErr(relay.AgentTerminalStarted(core.ActiveAgentTerminal{TerminalID: "terminal-1", Command: "cargo test", StartedAtMs: 4_000}))

	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("a terminal is the turn's own work while that turn is still open: %#v", commands)
	}

	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))

	// The command outlived the turn that started it.
	rtEqual(t, relay.OperationalState().BackgroundCommands, []core.BackgroundCommand{{
		ID:          "terminal:terminal-1",
		StartedAtMs: 4_000,
		Command:     "cargo test",
		CanStop:     true,
	}})

	rtNoErr(relay.AgentTerminalClosed("terminal-1"))
	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("the process exited, so there is nothing left running: %#v", commands)
	}
}

func TestAirTasksSurvivePromptBoundariesUntilTheTerminalUpdates(t *testing.T) {
	for _, outcome := range []string{"completed", "rejected", "interrupted"} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		submitRelay(relay, "review-prompt", prompt("start background reviews"))
		rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
		rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		rtNoErr(relay.setAirTasks([]AirTaskFixture{
			airTask("design", "Design review"),
			airTask("refuter", "Refute findings"),
		}))
		var err error
		switch outcome {
		case "completed":
			_, err = relay.RecordCommandCompleted("review-prompt", rtPromptOutcome("end_turn"))
		case "rejected":
			_, err = relay.RecordCommandRejected("review-prompt", core.OutcomeReasonAdmissionRejected, "adapter failed")
		default:
			_, err = relay.RecordCommandInterrupted("review-prompt", core.OutcomeReasonRuntimeFailure, "cancelled")
		}
		rtNoErr(err)

		state := relay.OperationalState()
		if state.Execution != core.RelayExecutionStateIdle {
			t.Fatalf("%s: execution %s", outcome, state.Execution)
		}
		if state.HarnessTurn != nil {
			t.Fatal("no harness turn is open")
		}
		if state.ForegroundToolStartedAtMs != nil {
			t.Fatal("no tool is in the foreground")
		}
		rtEqual(t, len(state.BackgroundCommands), 2)
		if state.IsQuiet() {
			t.Fatal("background agents must prevent worker replacement")
		}

		// Terminal edges clear finished tasks while repeated announcements keep
		// the clock of a task that remains live.
		entry := relay.airTasks["4:root:design"]
		if entry == nil || entry.state.Kind != AirTaskStateKindBackground {
			t.Fatalf("expected background task, got %#v", entry)
		}
		entry.state.Background.StartedAtMs = 123
		rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("design", "Design cleanup")}))
		rtEqual(t, relay.OperationalState().BackgroundCommands, []core.BackgroundCommand{{
			ID:          "air:4:root:design",
			StartedAtMs: 123,
			Command:     "Design cleanup",
			CanStop:     false,
		}})
		rtNoErr(relay.setAirTasks(nil))
		if state := relay.OperationalState(); !state.IsQuiet() {
			t.Fatalf("%#v", state)
		}
	}
}

func TestBackgroundTaskStopTargetsAreLiveCapabilityCheckedAndNamespaced(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.AgentTerminalStarted(core.ActiveAgentTerminal{TerminalID: "shared-id", Command: "cargo test", StartedAtMs: 10}))
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("shared-id", "Review tests")}))

	rtEqual(t, rtMust(relay.BackgroundTaskStopTarget("terminal:shared-id")), core.BackgroundTaskStopTarget(core.BackgroundTaskStopTargetHostedTerminal{TerminalID: "shared-id"}))
	if _, err := relay.BackgroundTaskStopTarget("air:4:root:shared-id"); err == nil {
		t.Fatal("Claude must not be stoppable until AIR grants the affordance")
	}

	rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateChanged{TaskID: "shared-id", Description: nil, CanStop: rtBool(true)}))
	rtEqual(t, rtMust(relay.BackgroundTaskStopTarget("air:4:root:shared-id")), core.BackgroundTaskStopTarget(core.BackgroundTaskStopTargetAirAsyncTask{SessionID: "root", TaskID: "shared-id"}))
	stoppable := false
	for _, command := range relay.OperationalState().BackgroundCommands {
		stoppable = stoppable || (command.ID == "air:4:root:shared-id" && command.CanStop)
	}
	if !stoppable {
		t.Fatal("the granted task is listed as stoppable")
	}

	rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateChanged{TaskID: "shared-id", Description: nil, CanStop: rtBool(false)}))
	if _, err := relay.BackgroundTaskStopTarget("air:4:root:shared-id"); err == nil {
		t.Fatal("a withdrawn affordance cannot be stopped")
	}
	rtNoErr(relay.AgentTerminalClosed("shared-id"))
	if _, err := relay.BackgroundTaskStopTarget("terminal:shared-id"); err == nil {
		t.Fatal("a stale UI id must never resolve after its task exits")
	}
}

// Hard-won: 460153f3: stopping a Claude task left an idle session marked Working with nothing running.
func TestARequestedStopAcknowledgementDoesNotOpenAHarnessTurn(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)
	rtEqual(t, rtMust(relay.BackgroundTaskStopTarget("air:4:root:sleeper")), core.BackgroundTaskStopTarget(core.BackgroundTaskStopTargetAirAsyncTask{SessionID: "root", TaskID: "sleeper"}))

	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeStopAcknowledgement("Sleep 600"))))

	state := relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	if state.HarnessTurn != nil {
		t.Fatal("no model turn follows a user-requested stop, so nothing would settle one")
	}
	observed := observations(t, relay)
	acknowledged := false
	if len(observed) == 1 {
		if update, ok := observed[0].(core.RelayObservationSessionUpdate); ok {
			text, isText := agentChunkText(&update.Update)
			acknowledged = isText && text == claudeStopAcknowledgement("Sleep 600")
		}
	}
	if !acknowledged {
		t.Fatalf("the acknowledgement still enters the transcript: %#v", observed)
	}

	// Genuine work after the acknowledgement opens a turn as before.
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	state = relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateRunning)
	if state.HarnessTurn == nil {
		t.Fatal("genuine work opens a turn")
	}
}

// Hard-won: review round 3: the adapter wraps the acknowledgement in a blockquote paragraph,
// so a stop requested on an idle session opened a harness turn that never settled.
func TestAnIdleStopAcknowledgementInTheAdaptersBlockquoteShapeOpensNoTurn(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)
	rtMust(relay.BackgroundTaskStopTarget("air:4:root:sleeper"))
	// As recorded from claude-agent-acp: noticeTranscriptText through transcriptBlock.
	recorded := "\n\n> **Task stopped by user:** python3 -c \"import time; time.sleep(100020)\".\n\n"
	rtMust(relay.RecordSessionUpdate(agentTextChunk(recorded)))
	state := relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	if state.HarnessTurn != nil {
		t.Fatal("no model cycle follows a requested stop, so no result would settle a turn")
	}
	// Without a pending stop the same text is ordinary agent output.
	other := relayTestsClaudeRelay(t, filepath.Join(temp, "other"))
	rtMust(other.RecordSessionUpdate(agentTextChunk(recorded)))
	if other.OperationalState().HarnessTurn == nil {
		t.Fatal("unrequested stop text is agent output")
	}
}

// Hard-won: 460153f3: Claude renamed tasks between the stop request and its stop acknowledgement.
func TestAStopAcknowledgementMatchesWhateverNameTheAdapterNowUses(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)
	rtMust(relay.BackgroundTaskStopTarget("air:4:root:sleeper"))

	// A child with the same provider id must consume only its own pending stop.
	rtNoErr(relay.AirTaskChanged("child", core.AirTaskUpdateSpawned{TaskID: "sleeper", Description: "child sleeper", CanStop: true}))
	rtMust(relay.BackgroundTaskStopTarget("air:5:child:sleeper"))
	rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventUpdate{
		SessionID: "child",
		Update:    agentTextChunk(claudeStopAcknowledgement("child sleeper")),
	}}))
	if _, ok := relay.claudePendingStops["child"]; ok {
		t.Fatal("the child's stop was acknowledged")
	}
	if _, ok := relay.claudePendingStops["root"]; !ok {
		t.Fatal("the root's stop is still pending")
	}

	// The spawn said "Sleep 600"; later progress renamed it to the
	// command text, which is what the adapter quotes.
	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeStopAcknowledgement("sleep 900"))))

	state := relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	if state.HarnessTurn != nil {
		t.Fatal("the acknowledgement opens no turn")
	}
	if len(relay.claudePendingStops) != 0 {
		t.Fatalf("%#v", relay.claudePendingStops)
	}
}

func TestAnUnrequestedStopTextStillOpensAHarnessTurn(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)

	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeStopAcknowledgement("Sleep 600"))))

	assertChunkOpenedAHarnessTurn(t, relay)
}

func TestAFailedStopClearsItsAcknowledgement(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)
	rtMust(relay.BackgroundTaskStopTarget("air:4:root:sleeper"))
	relay.ClaudeStopNotSent("root", "sleeper")

	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeStopAcknowledgement("Sleep 600"))))

	assertChunkOpenedAHarnessTurn(t, relay)
}

func TestStopAcknowledgementsClearOnRestart(t *testing.T) {
	temp := t.TempDir()
	relay := claudeRelayWithStoppableTask(t, temp)
	rtMust(relay.BackgroundTaskStopTarget("air:4:root:sleeper"))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	if len(relay.claudePendingStops) != 0 {
		t.Fatalf("%#v", relay.claudePendingStops)
	}

	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeStopAcknowledgement("Sleep 600"))))

	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("the chunk must open a turn")
	}
	observed := observations(t, relay)
	if len(observed) != 3 {
		t.Fatalf("%#v", observed)
	}
	_, restarted := observed[0].(core.RelayObservationSessionRestarted)
	_, started := observed[1].(core.RelayObservationHarnessTurnStarted)
	_, update := observed[2].(core.RelayObservationSessionUpdate)
	if !restarted || !started || !update {
		t.Fatalf("%#v", observed)
	}
}

func TestAirTasksAreProcessLocalAndClearOnTeardown(t *testing.T) {
	for _, observation := range []core.RelayObservation{
		core.RelayObservationSessionRestarted{},
		core.RelayObservationClosing{},
		core.RelayObservationClosed{},
	} {
		temp := t.TempDir()
		relay := relayTestsClaudeRelay(t, temp)
		rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("design", "Design review")}))
		rtMust(relay.RecordObservation(observation))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%#v", commands)
		}
	}
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("design", "Design review")}))
	rtNoErr(relay.retireHarnessProcessState())
	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("%#v", commands)
	}
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("design", "Design review")}))
	relay = relayTestsClaudeRelay(t, temp)
	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("%#v", commands)
	}
}

func TestAirTasksAreAuthoritativeAndReplayDoesNotAnticipateATurn(t *testing.T) {
	for _, policy := range []HarnessTurnPolicy{HarnessTurnPolicyCodexAdapter, HarnessTurnPolicyClaudeAdapter} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		rtMust(relay.RecordSessionUpdate(execCard("call-1", []string{"sleep", "600"}, nil)))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%#v", commands)
		}
		relay.SetHarnessTurnPolicy(policy)
		rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventReplayBegin{}}))
		rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("historical", "old"), airTask("live", "server")}))
		rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateFinished{TaskID: "historical", Settled: true, Failed: false}))
		rtMust(relay.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventReplayCommit{}}))
		if relay.ActivityFacts().TaskSettledAtMs != nil {
			t.Fatal("a replayed settle anticipates no turn")
		}
		rtEqual(t, len(relay.OperationalState().BackgroundCommands), 1)
		rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateFinished{TaskID: "live", Settled: true, Failed: false}))
		rtEqual(t, relay.ActivityFacts().TaskSettledAtMs != nil, policy == HarnessTurnPolicyClaudeAdapter)
		rtNoErr(relay.AirTaskChanged("root", core.AirTaskUpdateChanged{
			TaskID:      "live",
			Description: rtString("late progress"),
			CanStop:     rtBool(true),
		}))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("late progress cannot resurrect a terminal task: %#v", commands)
		}
	}
}

func TestCompletedCodexMcpExecuteCallsDoNotLeaveBackgroundWork(t *testing.T) {
	for _, partial := range []bool{false, true} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		submitRelay(relay, "memory-prompt", prompt("remember the result"))
		rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)

		kind := schema.ToolKindExecute
		call := schema.ToolCall{
			ToolCallID: "history",
			Title:      "mcp.mj-memory.search_sessions",
			Kind:       &kind,
			RawInput:   json.RawMessage(`{"server":"mj-memory","tool":"search_sessions","arguments":{"query":"remember the result"}}`),
		}
		output := json.RawMessage(`{"result":{"content":[{"type":"text","text":"saved"}]},"error":null}`)
		if partial {
			inProgress := schema.ToolCallStatusInProgress
			call.Status = &inProgress
			rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}))
			completed := schema.ToolCallStatusCompleted
			update := schema.ToolCallUpdate{ToolCallID: "history", Status: &completed, RawOutput: output}
			rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &update}))
		} else {
			completed := schema.ToolCallStatusCompleted
			call.Status = &completed
			call.RawOutput = output
			rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}))
		}
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%#v", commands)
		}
		rtMust(relay.RecordCommandCompleted("memory-prompt", rtPromptOutcome("end_turn")))
		if state := relay.OperationalState(); !state.IsQuiet() {
			t.Fatalf("%#v", state)
		}
	}
}

func TestPromptBoundariesClearStaleToolsButPreserveDetachedCommands(t *testing.T) {
	for _, outcome := range []string{"completed", "rejected", "interrupted"} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		submitRelay(relay, "boundary-prompt", prompt("run work"))
		rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
		rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("detached", "sleep 600")}))
		if relay.OperationalState().ForegroundToolStartedAtMs == nil {
			t.Fatal("the pending tool is foreground work")
		}

		var err error
		switch outcome {
		case "completed":
			_, err = relay.RecordCommandCompleted("boundary-prompt", rtPromptOutcome("end_turn"))
		case "rejected":
			_, err = relay.RecordCommandRejected("boundary-prompt", core.OutcomeReasonAdmissionRejected, "adapter failed")
		default:
			_, err = relay.RecordCommandInterrupted("boundary-prompt", core.OutcomeReasonRuntimeFailure, "cancelled")
		}
		rtNoErr(err)

		state := relay.OperationalState()
		if state.ForegroundToolStartedAtMs != nil {
			t.Fatalf("%s: a stale tool is still foreground work", outcome)
		}
		if len(state.BackgroundCommands) != 1 {
			t.Fatalf("%s: %#v", outcome, state.BackgroundCommands)
		}
		rtNoErr(relay.setAirTasks(nil))
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("%s: %#v", outcome, commands)
		}
	}
}

func TestAnUnsettledToolCallIsForegroundWorkEvenWithoutATurnMarker(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))

	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	state := relay.OperationalState()
	if state.ForegroundToolStartedAtMs == nil {
		t.Fatal("a pending tool is positive foreground-work evidence")
	}
	if state.IsQuiet() {
		t.Fatal("a pending foreground tool blocks replacement")
	}

	exitCode := int64(0)
	rtMust(relay.RecordSessionUpdate(execCard("call-1", []string{"true"}, &exitCode)))
	state = relay.OperationalState()
	if state.ForegroundToolStartedAtMs != nil {
		t.Fatal("a settled tool no longer overrides background work")
	}
	if !state.IsQuiet() {
		t.Fatal("a settled foreground tool permits replacement")
	}
}

func TestCompletedSubagentToolUpdateKeepsAParentPromptBusy(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	submitRelay(relay, "parent-prompt", prompt("keep working"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "parent-prompt")

	completed := schema.ToolCallStatusCompleted
	subagent := schema.ToolCall{ToolCallID: "m4_csharp", Title: "Start subagent m4_csharp", Status: &completed}
	rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &subagent}))

	state := relay.OperationalState()
	if state.ActivePrompt == nil || state.ActivePrompt.CommandID != "parent-prompt" {
		t.Fatalf("unexpected active prompt %#v", state.ActivePrompt)
	}
	if state.HarnessTurn != nil {
		t.Fatal("a prompt owns the turn")
	}
	if state.IsQuiet() {
		t.Fatal("the parent prompt is still in flight")
	}

	rtMust(relay.RecordCommandCompleted("parent-prompt", rtPromptOutcome("end_turn")))
	state = relay.OperationalState()
	if state.ActivePrompt != nil {
		t.Fatal("the prompt finished")
	}
	if state.HarnessTurn != nil {
		t.Fatal("no harness turn is open")
	}
	if !state.IsQuiet() {
		t.Fatal("prompt completion releases the busy guard")
	}
}

func TestARestartForgetsTheCommandsThePreviousHarnessLeftRunning(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	rtNoErr(relay.setAirTasks([]AirTaskFixture{airTask("call-1", "sleep 600")}))
	rtEqual(t, len(relay.OperationalState().BackgroundCommands), 1)

	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))

	if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
		t.Fatalf("the harness that owned those processes is gone: %#v", commands)
	}
}

func TestAnInFlightPromptBlocksCheckpointBarrierAdmission(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "stuck-prompt", prompt("keep running"))
	claimedPrompt := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimedPrompt), 1)
	rtEqual(t, claimedPrompt[0].CommandID, "stuck-prompt")

	submitRelay(relay, "barrier-command", core.RelayCommandBeginCheckpoint{Reason: nil})
	if claimed := rtMust(relay.ClaimPendingCommands(true)); len(claimed) != 0 {
		t.Fatalf("a live ACP turn must keep the checkpoint barrier queued: %#v", claimed)
	}

	rtMust(relay.RecordCommandInterrupted("stuck-prompt", core.OutcomeReasonRuntimeFailure, "worker restarted"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "barrier-command")
}

func TestCancelDispatchesWhileThePromptItTargetsIsInFlight(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "cancelled-prompt", prompt("keep running"))
	claimedPrompt := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimedPrompt), 1)
	rtEqual(t, claimedPrompt[0].CommandID, "cancelled-prompt")

	submitRelay(relay, "queued-correction", prompt("change direction"))
	submitRelay(relay, "cancel-command", core.RelayCommandCancel{})
	cancel := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(cancel), 1)
	rtEqual(t, cancel[0].CommandID, "cancel-command")
	if _, ok := cancel[0].Command.(core.RelayCommandCancel); !ok {
		t.Fatalf("%#v", cancel[0].Command)
	}
	steering := cancel[0].SteeringPrompt
	if steering == nil {
		t.Fatal("cancel carries the queued prompt head")
	}
	rtEqual(t, steering.QueuedCommandID, "queued-correction")
	rtEqual(t, steering.Prompt, []schema.ContentBlock{{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: "change direction"}}})

	rtMust(relay.RecordCommandCompleted("cancel-command", core.RelayCommandOutcomeSteered{QueuedCommandID: "queued-correction"}))
	state := relay.OperationalState()
	rtEqual(t, state.Execution, core.RelayExecutionStateRunning)
	rtEqual(t, state.ActivePrompt.CommandID, "cancelled-prompt")
	if len(state.QueuedPrompts) != 0 {
		t.Fatalf("%#v", state.QueuedPrompts)
	}

	materialized := rtProjectSession(relay)
	if _, ok := materialized.Execution.(core.MaterializedExecutionStateRunning); !ok {
		t.Fatalf("%#v", materialized.Execution)
	}
	if len(materialized.QueuedPrompts) != 0 {
		t.Fatalf("%#v", materialized.QueuedPrompts)
	}
	users := 0
	for _, item := range materialized.Transcript {
		if _, ok := item.Body.(core.TranscriptBodyUser); ok {
			users++
		}
	}
	rtEqual(t, users, 2)
}

func TestCancelTurnBypassesAPendingCheckpointWithoutSteeringTheQueue(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "active-prompt", prompt("keep running"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "active-prompt")
	submitRelay(relay, "pending-checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
	submitRelay(relay, "queued-prompt", prompt("leave queued"))
	submitRelay(relay, "cancel-turn", core.RelayCommandCancelTurn{})

	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cancel-turn")
	if _, ok := claimed[0].Command.(core.RelayCommandCancelTurn); !ok {
		t.Fatalf("%#v", claimed[0].Command)
	}
	if claimed[0].SteeringPrompt != nil {
		t.Fatal("cancel turn steers nothing")
	}
	rtEqual(t, queuedCommandIDs(relay), []string{"queued-prompt"})

	rtMust(relay.RecordCommandCompleted("cancel-turn", core.RelayCommandOutcomeCancelled{}))
	state := relay.OperationalState()
	rtEqual(t, state.ActivePrompt.CommandID, "active-prompt")
	if state.HarnessTurn != nil {
		t.Fatal("no harness turn is open")
	}
	rtEqual(t, queuedCommandIDs(relay), []string{"queued-prompt"})
	if state.CheckpointBarrier != nil {
		t.Fatal("the checkpoint is still pending")
	}
}

func TestCancelTurnBypassesAPendingCheckpointForAnAutonomousTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("output opens a turn")
	}
	submitRelay(relay, "pending-checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
	submitRelay(relay, "queued-prompt", prompt("leave queued"))
	submitRelay(relay, "cancel-turn", core.RelayCommandCancelTurn{})

	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "cancel-turn")
	if claimed[0].SteeringPrompt != nil {
		t.Fatal("cancel turn steers nothing")
	}
	rtEqual(t, queuedCommandIDs(relay), []string{"queued-prompt"})

	rtMust(relay.RecordCommandCompleted("cancel-turn", core.RelayCommandOutcomeCancelled{}))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("the turn waits for its result")
	}
	rtEqual(t, queuedCommandIDs(relay), []string{"queued-prompt"})

	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "pending-checkpoint")
	rtEqual(t, queuedCommandIDs(relay), []string{"queued-prompt"})
}

func TestCancelTurnNeverBypassesAnAdmittedCheckpoint(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "admitted-checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "admitted-checkpoint")
	rtMust(relay.RecordCheckpointReady("admitted-checkpoint"))
	before := relay.OperationalState()
	_, refusal, err := relay.submitCommand("cancel-turn", core.RelayCommandCancelTurn{})
	rtNoErr(err)
	if refusal == nil {
		t.Fatal("late cancellation must leave the checkpoint cursor intact")
	}
	rtEqual(t, refusal.Code, core.RelayErrorCodeInvalidState)
	rtNoClaims(t, relay)
	rtEqual(t, relay.OperationalState(), before)
	if _, ok := relay.snapshot.Dispatches["cancel-turn"]; ok {
		t.Fatal("the refused cancel was dispatched")
	}
}

func TestConfigAcceptedDuringAPromptWaitsWhileCancelBypassesIt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "active-prompt", prompt("keep running"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "active-prompt")

	submitRelay(relay, "config-after-prompt", setConfig("model", "later"))
	submitRelay(relay, "cancel-after-config", core.RelayCommandCancel{})

	cancel := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(cancel), 1)
	rtEqual(t, cancel[0].CommandID, "cancel-after-config")
	if _, ok := cancel[0].Command.(core.RelayCommandCancel); !ok {
		t.Fatalf("%#v", cancel[0].Command)
	}
	if cancel[0].SteeringPrompt != nil {
		t.Fatal("a configuration change is not a steer")
	}
	rtMust(relay.RecordCommandCompleted("cancel-after-config", core.RelayCommandOutcomeCancelled{}))
	rtNoClaims(t, relay)

	rtMust(relay.RecordCommandCompleted("active-prompt", rtPromptOutcome("cancelled")))
	config := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(config), 1)
	rtEqual(t, config[0].CommandID, "config-after-prompt")
}

func TestConfigAcceptedBeforeAPromptKeepsAcceptanceOrder(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "config-first", setConfig("model", "first"))
	submitRelay(relay, "prompt-second", prompt("then run"))

	// Queue entries run one at a time, so the prompt waits for the
	// configuration change accepted before it.
	config := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(config), 1)
	rtEqual(t, config[0].CommandID, "config-first")
	rtNoClaims(t, relay)

	rtMust(relay.RecordCommandCompleted("config-first", core.RelayCommandOutcomeConfigured{}))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-second")
}

func TestConfigQueuedBehindAPromptAppliesInQueueOrder(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "prompt-one", prompt("one"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "prompt-one")

	submitRelay(relay, "prompt-two", prompt("two"))
	submitRelay(relay, "config-third", setConfig("model", "sonnet"))
	submitRelay(relay, "prompt-four", prompt("four"))
	rtEqual(t, queuedCommandIDs(relay), []string{"prompt-two", "config-third", "prompt-four"})
	rtNoClaims(t, relay)

	finishPrompt(relay, "prompt-one")
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "prompt-two")
	finishPrompt(relay, "prompt-two")

	config := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(config), 1)
	rtEqual(t, config[0].CommandID, "config-third")
	if _, ok := config[0].Command.(core.RelayCommandSetConfig); !ok {
		t.Fatalf("%#v", config[0].Command)
	}
	// A configuration change applies between turns, so the relay stays
	// idle and the prompt behind it waits for the change to finish.
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateIdle)
	rtNoClaims(t, relay)

	rtMust(relay.RecordCommandCompleted("config-third", core.RelayCommandOutcomeConfigured{}))
	model, ok := relay.OperationalState().Config["model"]
	if !ok || model != "sonnet" {
		t.Fatalf("model is %q (%v)", model, ok)
	}
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "prompt-four")
}

func TestRemovingAQueuedConfigStopsItFromDispatching(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "active-prompt", prompt("running"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "active-prompt")
	submitRelay(relay, "config-queued", setConfig("effort", "high"))
	submitRelay(relay, "remove-config", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "config-queued"})

	if ids := queuedCommandIDs(relay); len(ids) != 0 {
		t.Fatalf("%v", ids)
	}
	if _, ok := relay.snapshot.Dispatches["config-queued"]; ok {
		t.Fatal("the removed configuration is no longer dispatched")
	}
	if relay.snapshot.HandledCommands["config-queued"].TerminalOrdinal == nil {
		t.Fatal("the removed configuration is terminal")
	}

	finishPrompt(relay, "active-prompt")
	rtNoClaims(t, relay)
	if config := relay.OperationalState().Config; len(config) != 0 {
		t.Fatalf("%#v", config)
	}
}

func TestClearingTheQueueDropsQueuedConfigurationChanges(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "active-prompt", prompt("running"))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "active-prompt")
	submitRelay(relay, "queued-prompt", prompt("later"))
	submitRelay(relay, "queued-config", setConfig("model", "later"))

	submitRelay(relay, "clear-queue", core.RelayCommandClearQueuedPrompts{})
	if ids := queuedCommandIDs(relay); len(ids) != 0 {
		t.Fatalf("%v", ids)
	}

	finishPrompt(relay, "active-prompt")
	rtNoClaims(t, relay)
}

func TestCloseRequiresExactCheckpointCutAndSurvivesControllerDisconnect(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	staleCut := readyCheckpoint(relay, "stale-close-barrier")
	// Output that changes no work waits behind the cut; drift comes from
	// something that ended that hold, such as the agent starting work.
	rtNoErr(relay.EndCheckpointHold())
	rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "post-cut drift"}))
	rejected := relay.Handle(relayRequest("reject-stale-close", core.RelayRequestSubmit{
		CommandID: "stale-close-command",
		Command:   core.RelayCommandClose{BarrierCommandID: "stale-close-barrier", Expected: staleCut},
	}))
	rtInvalidState(t, rejected)
	rtMust2(relay.CancelCheckpointBarrierOnDisconnect("stale-close-barrier"))

	exactCut := readyCheckpoint(relay, "exact-close-barrier")
	accepted := submitRelay(relay, "exact-close-command", core.RelayCommandClose{BarrierCommandID: "exact-close-barrier", Expected: exactCut})
	if accepted <= exactCut.Ordinal {
		t.Fatalf("close accepted at %d, not after the cut %d", accepted, exactCut.Ordinal)
	}
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateClosing)
	later := relay.Handle(relayRequest("post-close-command", core.RelayRequestSubmit{
		CommandID: "post-close-prompt",
		Command:   prompt("must not run"),
	}))
	rtInvalidState(t, later)

	if _, cancelled := rtMust2(relay.CancelCheckpointBarrierOnDisconnect("exact-close-barrier")); !cancelled {
		t.Fatal("the disconnect releases the barrier under an accepted close")
	}
	closing := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(closing), 1)
	rtEqual(t, closing[0].CommandID, "exact-close-command")
	if _, ok := closing[0].Command.(core.RelayCommandClose); !ok {
		t.Fatalf("%#v", closing[0].Command)
	}
}

func TestExactCloseAllowsCheckpointCompletionBeforeCloseDispatch(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	expected := readyCheckpoint(relay, "normal-close-barrier")
	submitRelay(relay, "normal-close-command", core.RelayCommandClose{BarrierCommandID: "normal-close-barrier", Expected: expected})
	submitRelay(relay, "normal-close-complete", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "normal-close-barrier"})

	closing := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(closing), 1)
	rtEqual(t, closing[0].CommandID, "normal-close-command")
	if _, ok := closing[0].Command.(core.RelayCommandClose); !ok {
		t.Fatalf("%#v", closing[0].Command)
	}
}

func TestCredentialRequestsCannotEnterDurableRelayState(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))

	for _, request := range []struct {
		id      string
		request core.RelayRequest
	}{
		{"cpu-usage", core.RelayRequestCpuUsage{}},
		{"credential-state", core.RelayRequestCredentialState{}},
		{"read-credentials", core.RelayRequestReadCredentials{}},
		{"install-credentials", core.RelayRequestInstallCredentials{Data: "e30="}},
	} {
		response := relay.Handle(relayRequest(request.id, request.request))
		failure, ok := response.Body.(core.RelayResponseBodyError)
		if !ok || failure.Error.Code != core.RelayErrorCodeInvalidState || failure.Error.Retryable {
			t.Fatalf("%s: %#v", request.id, response.Body)
		}
	}

	rtEqual(t, relay.LatestOrdinal(), uint64(0))
	if events := rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest)); len(events) != 0 {
		t.Fatalf("%#v", events)
	}
	persisted := string(rtMust(os.ReadFile(filepath.Join(temp, core.RelayStateFile))))
	if strings.Contains(persisted, "e30=") {
		t.Fatal("credential data reached the relay state")
	}
	if len(relay.snapshot.HandledCommands) != 0 {
		t.Fatalf("%#v", relay.snapshot.HandledCommands)
	}
}

func TestGoalControlsBypassWorkAndCheckpointsWithoutConsumingPrompts(t *testing.T) {
	for _, autonomous := range []bool{false, true} {
		temp := t.TempDir()
		var relay *DurableRelay
		if autonomous {
			relay = relayTestsClaudeRelay(t, temp)
		} else {
			relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
		}
		if autonomous {
			rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
		} else {
			submitRelay(relay, "active-work", prompt("keep running"))
			rtMust(relay.ClaimPendingCommands(true))
		}
		submitRelay(relay, "checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
		submitRelay(relay, "queued-work", prompt("leave queued"))
		for _, action := range core.GoalControlActionAll {
			commandID := "goal-" + action.AsStr()
			submitRelay(relay, commandID, core.RelayCommandGoalControl{Action: action})
			claimed := rtMust(relay.ClaimPendingCommands(true))
			rtEqual(t, len(claimed), 1)
			rtEqual(t, claimed[0].CommandID, commandID)
			if claimed[0].SteeringPrompt != nil {
				t.Fatal("a goal control steers nothing")
			}
			// Leave resume outstanding while sending subsequent clear.
			if action != core.GoalControlActionResume {
				rtMust(relay.RecordCommandCompleted(commandID, core.RelayCommandOutcomeGoalControlled{}))
			}
		}
		rtEqual(t, queuedCommandIDs(relay), []string{"queued-work"})
		if relay.OperationalState().CheckpointBarrier != nil {
			t.Fatal("the checkpoint is still pending")
		}
		rtEqual(t, relay.OperationalState().HarnessTurn != nil, autonomous)
		rtEqual(t, relay.OperationalState().ActivePrompt != nil, !autonomous)
		events := rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest))
		projection := core.MaterializedSessionEmpty(session)
		for index := range events {
			projected := rtMust(transcript.ProjectRelayEvent(&projection, &events[index]))
			rtNoErr(transcript.ApplyCommittedProjectionEvent(&projection, &events[index], projected.Mutation))
		}
		relay = rtMust(DurableRelayOpenForCheckpoint(temp, session, "1.0.0"))
		expected := []string{"queued-work"}
		if !autonomous {
			expected = []string{core.RestartContinuationID("active-work"), "queued-work"}
		}
		rtEqual(t, queuedCommandIDs(relay), expected)
		replay := rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest))
		if len(replay) < len(events) {
			t.Fatalf("the replay lost events: %d of %d", len(replay), len(events))
		}
		rtEqual(t, replay[:len(events)], events)
		if _, ok := relay.snapshot.Dispatches["goal-resume"]; ok {
			t.Fatal("the outstanding resume is not dispatched after a restart")
		}
		if relay.snapshot.HandledCommands["goal-resume"].Failure == nil {
			t.Fatal("the outstanding resume failed")
		}
	}
}

func TestContinuationExpectationIsProcessLocalAndClearsWhenAHarnessTurnOpens(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	generation := relay.TurnContext().Generation()
	rtNoErr(relay.ExpectContinuation(1_234, "waiting for work", generation))
	state := relay.OperationalState()
	activity := state.ActivityState()
	if activity.Kind != core.ActivityStateExpecting || activity.SinceMs == nil || *activity.SinceMs != 1_234 {
		t.Fatalf("expected an expectation since 1234, got %#v", activity)
	}
	rtEqual(t, relay.ActivityFacts(), state.Facts())
	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	if reopened.ActivityFacts().ExpectedContinuation != nil {
		t.Fatal("the expectation is process-local")
	}
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 2_000}))
	if relay.ActivityFacts().ExpectedContinuation != nil {
		t.Fatal("a harness turn answers the expectation")
	}
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Autonomous: nil, Origin: nil, PromptInFlight: false}))
	if relay.ActivityFacts().ExpectedContinuation != nil {
		t.Fatal("a settled turn does not restore the expectation")
	}
}

// Hard-won: d045ad87e: an older question was reused as an unanswered new turn's reply.
func TestNewPromptInvalidatesACompletedTurnVerdictAndResetsItsEvidence(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "prior-prompt", prompt("Explain the options"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Should I stop at option 1?"}}`)))
	rtMust(relay.RecordCommandCompleted("prior-prompt", rtPromptOutcome("EndTurn")))
	oldGeneration := relay.TurnContext().Generation()
	rtNoErr(relay.ExpectContinuation(1_234, "", oldGeneration))
	submitRelay(relay, "new-prompt", prompt("new instructions"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	if relay.ActivityFacts().ExpectedContinuation != nil {
		t.Fatal("a new prompt answers the expectation")
	}
	facts := relay.ActivityFacts()
	evidence := relay.TurnContext().Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &facts, 2_000)
	rtEqual(t, evidence.UserPromptTail, "new instructions")
	rtMust(relay.RecordCommandCompleted("new-prompt", rtPromptOutcome("EndTurn")))
	rtNoErr(relay.preparePendingAssessment())
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("the finished turn is classified")
	}
	assessment := pending.Evidence
	if assessment.AssistantTextTail != "" {
		t.Fatalf("the new turn has no reply, got %q", assessment.AssistantTextTail)
	}
	asked := false
	for _, message := range assessment.Authorization.Messages {
		asked = asked || message.Text == "Should I stop at option 1?"
	}
	if !asked {
		t.Fatal("the older question stays in authorization history")
	}
	messages := assessment.Authorization.Messages
	rtEqual(t, messages[len(messages)-1].Role, "user")
	// A restart must retain the empty current reply rather than reconstructing
	// it from the older question in authorization history.
	recovered := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	rtEqual(t, recovered.OperationalState().Assessment.Evidence, &assessment)
	rtNoErr(relay.ExpectContinuation(3_000, "", oldGeneration))
	if relay.ActivityFacts().ExpectedContinuation != nil {
		t.Fatal("a stale generation sets no expectation")
	}
}

func TestFinishedTurnQueuesOnlyOneRepliedClassification(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "prompt-1", prompt("instructions"))
	rtMust(relay.ClaimPendingCommands(true))
	if _, ok := relay.PendingRepliedVerdict(); ok {
		t.Fatal("a running turn has no replied verdict")
	}
	rtMust(relay.RecordCommandCompleted("prompt-1", rtPromptOutcome("EndTurn")))
	rtNoErr(relay.preparePendingAssessment())
	pending, ok := relay.PendingRepliedVerdict()
	if !ok {
		t.Fatal("the finished turn is classified")
	}
	rtEqual(t, pending.Evidence.Phase, core.TurnPhaseReplied)
	rtEqual(t, pending.Evidence.UserPromptTail, "instructions")
	if _, ok := relay.PendingRepliedVerdict(); ok {
		t.Fatal("the classification is queued once")
	}
}

func TestConfigurationNoticeIsRecordedOnlyAfterConfirmedCompletion(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	submitRelay(relay, "change-model", core.RelayCommandSetConfig{Key: "model", Value: "new-model"})
	rtMust(relay.ClaimPendingCommands(true))
	if rtCountObservations[core.RelayObservationNotice](observations(t, relay)) != 0 {
		t.Fatal("no notice before completion")
	}
	rtMust(relay.RecordCommandCompleted("change-model", core.RelayCommandOutcomeConfigured{}))
	reopened := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	notices := 0
	for _, event := range rtMust(reopened.EventsAfter(0, core.RelayEventGenesisDigest)) {
		if notice, ok := event.Observation.(core.RelayObservationNotice); ok && notice.Message == "model set to new-model" {
			notices++
		}
	}
	rtEqual(t, notices, 1)
}

func TestCompactPreservesLargePendingContextForTheNextPrompt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	context := strings.Repeat("shell output\n", 8192)
	rtNoErr(relay.InstallPromptContext(context))
	submitRelay(relay, "compact-command", prompt("/compact"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	if claimed[0].HiddenPromptContext != nil {
		t.Fatal("/compact does not consume the context")
	}
	rtMust(relay.RecordCommandCompleted("compact-command", rtPromptOutcome("EndTurn")))
	submitRelay(relay, "followup-command", prompt("continue"))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].HiddenPromptContext, rtString(context))
}

// The admission gate and the published capability must agree per harness:
// Kimi clears like Codex and Claude; every other harness still refuses.
func TestClearAdmissionMatchesThePublishedCapabilityPerHarness(t *testing.T) {
	for _, harness := range []struct {
		kind      core.HarnessKind
		supported bool
	}{
		{core.HarnessKindCodex, true},
		{core.HarnessKindClaude, true},
		{core.HarnessKindKimi, true},
		{core.HarnessKindGrok, false},
		{core.HarnessKindMuse, false},
		{core.HarnessKindOpenCode, false},
		{core.HarnessKindDsh, false},
		{core.HarnessKindAntigravity, false},
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		setVerdictHarness(t, relay, harness.kind)
		rtMust(relay.RecordObservation(rtOpenedSession("original")))
		rtNoErr(relay.MarkNativeSessionUsed())
		if relay.OperationalState().ClearContext != harness.supported {
			t.Fatalf("%s: published clear capability is %v", harness.kind, !harness.supported)
		}
		_, refusal, err := relay.submitCommand("clear-request", core.RelayCommandClearContext{})
		rtNoErr(err)
		if admitted := refusal == nil; admitted != harness.supported {
			t.Fatalf("%s: clear admitted %v", harness.kind, admitted)
		}
	}
}

// The conversation that held an open request is discarded by /clear, so the
// request must not reappear as a question in the fresh conversation.
func TestClearFencesAnOpenRequestAcrossReopen(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	relay.setAcpReady(true)
	submitRelay(relay, "ask-turn", prompt("Choose the format"))
	rtMust(relay.ClaimPendingCommands(true))
	finishPrompt(relay, "ask-turn")
	assessment := *relay.snapshot.Assessment
	assessment.Status = core.StatusAssessed
	action := core.AssessmentActionAwaitInput
	assessment.Action = &action
	rtMust(relay.RecordObservation(core.RelayObservationTurnAssessmentUpdated{Assessment: assessment}))
	if relay.snapshot.OpenRequest() == nil {
		t.Fatal("an awaited input is an open request")
	}
	submitRelay(relay, "clear-request", prompt("/clear"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("clear-request", core.RelayCommandOutcomeContextCleared{NativeSessionID: "replacement", Memory: nil}))
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("/clear fences the open request")
	}
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	if relay.snapshot.OpenRequest() != nil {
		t.Fatal("/clear fences the open request after a reopen")
	}
}

func TestClearChangesOnlyNativeContextAndIsIdempotentAcrossReopen(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	rtNoErr(relay.InstallPromptContext("old conversation handoff"))
	if relay.snapshot.Config == nil {
		relay.snapshot.Config = map[string]string{}
	}
	relay.snapshot.Config["model"] = "chosen-model"
	clear := prompt("/clear")
	_, refusal, err := relay.submitCommand("clear-request", clear)
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("/clear refused: %#v", refusal)
	}
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateRunning)
	rtRefused(t, relay, "later-prompt", prompt("must wait"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].Command, core.RelayCommand(core.RelayCommandClearContext{}))
	rtEqual(t, relay.OperationalState().NativeSessionID, rtString("original"))
	cut := relay.ReserveLiveBaseline()
	if _, received := cut.TryRecv(); received {
		t.Fatal("the cut waits for the clear")
	}
	rtMust(relay.RecordCommandCompleted("clear-request", core.RelayCommandOutcomeContextCleared{NativeSessionID: "replacement", Memory: nil}))
	if len(relay.pendingNativeResumes) != 0 {
		t.Fatalf("%#v", relay.pendingNativeResumes)
	}
	result, received := cut.TryRecv()
	if !received || result.Err != nil {
		t.Fatalf("the cut follows the clear: %v (%v)", result.Err, received)
	}
	ordinal := relay.OperationalState().LatestOrdinal
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("the replacement has no history")
	}
	if relay.snapshot.PendingPromptContext != nil {
		t.Fatal("the old handoff is gone")
	}
	model, ok := relay.snapshot.Config["model"]
	if !ok || model != "chosen-model" {
		t.Fatalf("model is %q (%v)", model, ok)
	}
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	setVerdictHarness(t, relay, core.HarnessKindCodex)
	_, refusal, err = relay.submitCommand("clear-request", clear)
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("a repeated /clear is idempotent: %#v", refusal)
	}
	rtEqual(t, relay.OperationalState().LatestOrdinal, ordinal)
	rtEqual(t, relay.OperationalState().NativeSessionID, rtString("replacement"))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("the replacement has no history")
	}
}

func TestInterruptedClearKeepsTheOriginalNativeConversation(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	_, refusal, err := relay.submitCommand("clear-request", core.RelayCommandClearContext{})
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("/clear refused: %#v", refusal)
	}
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, relay.OperationalState().NativeSessionID, rtString("original"))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("the original conversation is kept")
	}
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateIdle)
}

// Hard-won: 2b74434f9: /clear left pending and leased messages available to the fresh conversation.
func TestClearDiscardsMailboxOnlyOnSuccessAndSurvivesJournalReplay(t *testing.T) {
	for _, outcome := range []string{"success", "rejected", "interrupted"} {
		temp := t.TempDir()
		relay := clearableRelay(t, temp)
		leased := mailboxFixtureEvent("before-clear-lease", false)
		pending := mailboxFixtureEvent("before-clear-pending", false)
		submitRelay(relay, "lease-event", core.RelayCommandDeliverMailboxEvent{Event: leased})
		_, _, leaseID := requestMailboxDrainForTest(t, relay, "old-drain")
		if leaseID == nil {
			t.Fatal("the drain leases the event")
		}
		submitRelay(relay, "pending-event", core.RelayCommandDeliverMailboxEvent{Event: pending})
		submitRelay(relay, "clear-mailbox", core.RelayCommandClearContext{})
		rtMust(relay.ClaimPendingCommands(true))
		switch outcome {
		case "success":
			rtMust(relay.RecordCommandCompleted("clear-mailbox", core.RelayCommandOutcomeContextCleared{NativeSessionID: "fresh", Memory: nil}))
		case "rejected":
			rtMust(relay.RecordCommandRejected("clear-mailbox", core.OutcomeReasonAdmissionRejected, "reset failed"))
		}
		if outcome != "interrupted" {
			rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
		}
		relay = rtMust(DurableRelayOpen(temp, session, "test"))
		if outcome == "success" {
			stale := rtMust(relay.ackMailbox(*leaseID))
			if acknowledged, ok := stale.(core.RelayResponsePayloadMailboxAcknowledged); !ok || acknowledged.Acknowledged {
				t.Fatalf("a lease from before /clear must not acknowledge: %#v", stale)
			}
			for _, retry := range []struct {
				id    string
				event core.MailboxEvent
			}{
				{"lease-event", leased},
				{"lease-retry", leased},
				{"pending-retry", pending},
			} {
				submitRelay(relay, retry.id, core.RelayCommandDeliverMailboxEvent{Event: retry.event})
			}
			if _, count := drainMailboxForTest(t, relay, "after-clear"); count != 0 {
				t.Fatalf("cleared mail was delivered again (%d)", count)
			}
		} else if _, count := drainMailboxForTest(t, relay, "after-failed-clear"); count != 2 {
			t.Fatalf("a failed clear keeps both events, drained %d", count)
		}
		submitRelay(relay, "fresh-event", core.RelayCommandDeliverMailboxEvent{Event: mailboxFixtureEvent("after-clear-new", false)})
		if _, count := drainMailboxForTest(t, relay, "fresh-drain"); count != 1 {
			t.Fatalf("fresh mail is delivered, drained %d", count)
		}
	}
}

func TestClearRefusesPendingWorkAndInvalidInputWithoutChangingIdentity(t *testing.T) {
	for _, input := range []string{"/clear extra", "/compact ignored-instructions"} {
		temp := t.TempDir()
		relay := clearableRelay(t, temp)
		rtRefused(t, relay, "invalid-command", prompt(input))
		rtEqual(t, relay.OperationalState().NativeSessionID, rtString("original"))
	}
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	submitRelay(relay, "pending-work", prompt("do work"))
	rtRefused(t, relay, "clear-request", core.RelayCommandClearContext{})
	if _, ok := relay.snapshot.Dispatches["pending-work"]; !ok {
		t.Fatal("the pending work is still dispatched")
	}
}

// rtRunningEvidence is the judge evidence for a running Codex turn with
// default facts.
func rtRunningEvidence(relay *DurableRelay) core.TurnEvidence {
	facts := core.DefaultActivityFacts()
	return relay.TurnContext().Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 0)
}

// rtHasAssistantMessage reports whether an authorization history holds the
// assistant text.
func rtHasAssistantMessage(history *core.ContextHistory, text string) bool {
	if history == nil {
		return false
	}
	for _, message := range history.Messages {
		if message.Role == "assistant" && message.Text == text {
			return true
		}
	}
	return false
}

func TestJevMessagesSurviveReopenWithoutToolDetails(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "prompt-summary", prompt("Summarize this work"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Preserved answer"}}`)))
	for n := range 9 {
		kind := schema.ToolKindExecute
		status := schema.ToolCallStatusCompleted
		call := schema.ToolCall{
			ToolCallID: schema.ToolCallId(fmt.Sprintf("call-%d", n)),
			Title:      "Execute",
			Kind:       &kind,
			Status:     &status,
			RawInput:   rtMust(json.Marshal(map[string]string{"command": fmt.Sprintf("cargo test --marker=PRIVATE_%d", n)})),
		}
		rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}))
	}
	failed := schema.ToolCallStatusFailed
	update := schema.ToolCallUpdate{ToolCallID: "call-0", Status: &failed}
	rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &update}))
	replied := func(relay *DurableRelay) string {
		facts := core.DefaultActivityFacts()
		return relay.TurnContext().Evidence(core.HarnessKindCodex, core.TurnPhaseReplied, &facts, 0).TranscriptSummary
	}
	before := replied(relay)
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	after := replied(relay)
	rtEqual(t, before, after)
	for _, hidden := range []string{"PRIVATE_0", "PRIVATE_8", "<tool"} {
		if strings.Contains(after, hidden) {
			t.Fatalf("the summary contains %q: %s", hidden, after)
		}
	}
	for _, kept := range []string{"Summarize this work", "Preserved answer"} {
		if !strings.Contains(after, kept) {
			t.Fatalf("the summary lost %q: %s", kept, after)
		}
	}
}

func TestJevUserBoundaryMovesOnlyAfterConfirmedSteeringAndSurvivesReopen(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "active-prompt", prompt("ORIGINAL REQUEST"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "queued-prompt", prompt("DELIVERED CORRECTION"))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "ORIGINAL REQUEST")
	// Hard-won: 655be2b: a later removed queue receipt hid delivered text after ACK.
	submitRelay(relay, "removed-undelivered", prompt("NEVER DELIVER THIS"))
	submitRelay(relay, "remove-undelivered", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "removed-undelivered"})

	if strings.Contains(rtRunningEvidence(relay).TranscriptSummary, "DELIVERED CORRECTION") {
		t.Fatal("an undelivered prompt is not evidence")
	}
	submitRelay(relay, "cancel-command", core.RelayCommandCancel{})
	rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "ORIGINAL REQUEST")
	rtMust(relay.RecordCommandCompleted("cancel-command", core.RelayCommandOutcomeSteered{QueuedCommandID: "queued-prompt"}))
	before := rtRunningEvidence(relay)
	rtEqual(t, before.UserPromptTail, "DELIVERED CORRECTION")
	if !strings.Contains(before.TranscriptSummary, "DELIVERED CORRECTION") {
		t.Fatalf("the delivered correction is evidence: %s", before.TranscriptSummary)
	}
	if strings.Contains(before.TranscriptSummary, "ORIGINAL REQUEST") {
		t.Fatalf("the boundary moved past the original request: %s", before.TranscriptSummary)
	}
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	after := rtRunningEvidence(relay)
	rtEqual(t, after.UserPromptTail, before.UserPromptTail)
	rtEqual(t, after.TranscriptSummary, before.TranscriptSummary)
	// Hard-won: cc8c246: ACK pruning removed the only restart source for judge text tails.
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REPLY AFTER THE CORRECTION"}}`)))
	expected := rtRunningEvidence(relay)
	through := relay.LatestOrdinal()
	acknowledgeRelay(relay, "collect-judgment-history", through)
	relay = rtMust(DurableRelayOpen(temp, session, "after-ack"))
	recovered := rtRunningEvidence(relay)
	rtEqual(t, recovered.UserPromptTail, expected.UserPromptTail)
	if recovered.AssistantTextTail != "" {
		t.Fatalf("a recovered turn has no reply yet: %q", recovered.AssistantTextTail)
	}
	if recovered.UserPromptTail == "" {
		t.Fatal("the recovered turn keeps its instructions")
	}
	if !rtHasAssistantMessage(relay.snapshot.AuthorizationContext(), expected.AssistantTextTail) {
		t.Fatal("the reply survives in authorization history")
	}
	if history := relay.TurnContext().AuthorizationContext(); history == nil || len(history.Messages) == 0 {
		t.Fatal("the turn context restores authorization history")
	}
	// A newer delivered request must not inherit the prior request's reply.
	// Restart recovery now precedes new input. Settle the interrupted turn
	// before checking the evidence for the next delivered request.
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	for _, pending := range rtMust(relay.ClaimPendingCommands(true)) {
		rtMust(relay.RecordCommandCompleted(pending.CommandID, rtPromptOutcome("end_turn")))
	}
	submitRelay(relay, "next-person-request", prompt("NEXT REQUEST WITHOUT A REPLY"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("next-person-request", rtPromptOutcome("end_turn")))
	through = relay.LatestOrdinal()
	acknowledgeRelay(relay, "collect-next-request", through)
	relay = rtMust(DurableRelayOpen(temp, session, "next-request-after-ack"))
	recovered = rtRunningEvidence(relay)
	rtEqual(t, recovered.UserPromptTail, "NEXT REQUEST WITHOUT A REPLY")
	if recovered.AssistantTextTail != "" {
		t.Fatalf("the next request has no reply: %q", recovered.AssistantTextTail)
	}
	// Generated work continues under the latest human instructions, whether
	// or not its own delivery remains in the journal.
	through = relay.LatestOrdinal()
	acknowledgeRelay(relay, "collect-before-generated", through)
	submitRelay(relay, "auto-continue-no-human", prompt("Continue the authorized work"))
	rtMust(relay.ClaimPendingCommands(true))
	relay = rtMust(DurableRelayOpen(temp, session, "generated-with-retained-delivery"))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "NEXT REQUEST WITHOUT A REPLY")
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
	through = relay.LatestOrdinal()
	acknowledgeRelay(relay, "collect-generated-delivery", through)
	relay = rtMust(DurableRelayOpen(temp, session, "generated-with-pruned-delivery"))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "NEXT REQUEST WITHOUT A REPLY")
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	recovery := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(recovery), 1)
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "NEXT REQUEST WITHOUT A REPLY")
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
}

// Hard-won: ce467bc6f: a restarted mailbox wake inherited the prior turn's closed answer.
func TestMailboxReopenKeepsLatestInstructionsWithoutThePreviousReply(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "human-turn"))
	submitRelay(relay, "human-instructions", prompt("Keep working under these instructions"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"human-answer","content":{"type":"text","text":"human turn complete"}}`)))
	rtMust(relay.RecordCommandCompleted("human-instructions", rtPromptOutcome("end_turn")))
	relay = rtMust(DurableRelayOpen(temp, session, "idle-after-human"))
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "human turn complete")
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	submitRelay(relay, "mailbox-event-assessment", core.RelayCommandDeliverMailboxEvent{Event: core.MailboxEvent{
		Key:         "new-mailbox-work",
		Source:      "operator",
		Wake:        true,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyOperatorBack{Requests: []string{"continue the work"}},
	}})
	wake := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(wake), 1)
	if _, ok := wake[0].Command.(core.RelayCommandMailboxWake); !ok {
		t.Fatalf("%#v", wake[0].Command)
	}
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
	relay = rtMust(DurableRelayOpen(temp, session, "mailbox-before-output"))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "Keep working under these instructions")
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
	// Hard-won: 32f7cb9: reopening with recovery still queued revived the old answer.
	relay = rtMust(DurableRelayOpen(temp, session, "mailbox-recovery-queued"))
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"mailbox-answer","content":{"type":"text","text":"mailbox prefix"}}`)))
	relay = rtMust(DurableRelayOpen(temp, session, "mailbox-with-output"))
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "mailbox prefix")
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"mailbox-answer","content":{"type":"text","text":" plus suffix"}}`)))
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "mailbox prefix plus suffix")
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "Keep working under these instructions")
	// Hard-won: 32f7cb9: authorization history reopened narration closed by a thought.
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}`)))
	relay = rtMust(DurableRelayOpen(temp, session, "mailbox-after-thought"))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new reply"}}`)))
	rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "new reply")
}

// Hard-won: 31d6d4489: ACK and a second running restart lost the confirmed correction.
func TestRestartContinuationPreservesConfirmedSteeringContext(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "active-prompt", prompt("ORIGINAL REQUEST"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "queued-prompt", prompt("DELIVERED CORRECTION"))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "ORIGINAL REQUEST")
	if strings.Contains(rtRunningEvidence(relay).TranscriptSummary, "DELIVERED CORRECTION") {
		t.Fatal("a queued prompt is not evidence")
	}
	submitRelay(relay, "cancel-command", core.RelayCommandCancel{})
	rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "ORIGINAL REQUEST")
	rtMust(relay.RecordCommandCompleted("cancel-command", core.RelayCommandOutcomeSteered{QueuedCommandID: "queued-prompt"}))
	if relay.snapshot.HandledCommands["queued-prompt"].Command != nil {
		t.Fatal("the delivered prompt is compacted")
	}
	if _, ok := relay.snapshot.Dispatches["queued-prompt"]; ok {
		t.Fatal("the delivered prompt is no longer dispatched")
	}
	before := rtRunningEvidence(relay)
	rtEqual(t, before.UserPromptTail, "DELIVERED CORRECTION")
	if !strings.Contains(before.TranscriptSummary, "DELIVERED CORRECTION") || strings.Contains(before.TranscriptSummary, "ORIGINAL REQUEST") {
		t.Fatalf("unexpected summary %s", before.TranscriptSummary)
	}
	relay = rtMust(DurableRelayOpen(temp, session, "test"))
	after := rtRunningEvidence(relay)
	rtEqual(t, after.UserPromptTail, before.UserPromptTail)
	rtEqual(t, after.TranscriptSummary, before.TranscriptSummary)
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	recovery := rtMust(relay.ClaimPendingCommands(true))
	blocks, ok := core.RelayCommandPromptBlocksForCommand(recovery[0].Command, recovery[0].CommandID)
	if !ok {
		t.Fatal("the recovery carries a prompt")
	}
	if blocks[0].Text == nil || !strings.Contains(blocks[0].Text.Text, "take precedence") {
		t.Fatalf("unexpected recovery note %#v", blocks[0])
	}

	rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "DELIVERED CORRECTION")
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Work under the correction"}}`)))
	// Every new recovery is already running before its delivery is collected.
	// The latest user entry governs it, regardless of its executing command ID.
	for generation := range 3 {
		if generation == 0 {
			for index := 0; index <= core.RelayRetryIDRetention; index++ {
				submitRelay(relay, fmt.Sprintf("post-correction-control-%d", index), core.RelayCommandRecordNotice{Text: "later control"})
			}
		}
		through := relay.LatestOrdinal()
		acknowledgeRelay(relay, fmt.Sprintf("collect-recovery-%d", generation), through)
		rtEqual(t, relay.snapshot.RetainedThrough(), through)
		if _, ok := relay.snapshot.HandledCommands["queued-prompt"]; ok {
			t.Fatal("the delivered correction must outlive its retry receipt")
		}
		relay = rtMust(DurableRelayOpen(temp, session, "repeated-after-ack"))
		rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "DELIVERED CORRECTION")
		rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
		if !rtHasAssistantMessage(relay.snapshot.AuthorizationContext(), "Work under the correction") {
			t.Fatal("the reply survives in authorization history")
		}
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		recovery := rtMust(relay.ClaimPendingCommands(true))
		rtEqual(t, len(recovery), 1)
		if !core.IsRestartContinuation(recovery[0].CommandID) {
			t.Fatalf("unexpected recovery %s", recovery[0].CommandID)
		}
		rtEqual(t, rtRunningEvidence(relay).UserPromptTail, "DELIVERED CORRECTION")
		rtEqual(t, rtRunningEvidence(relay).AssistantTextTail, "")
		if !rtHasAssistantMessage(relay.snapshot.AuthorizationContext(), "Work under the correction") {
			t.Fatal("the reply survives in authorization history")
		}
	}
}

// I1-11: a finished turn leaves its compact receipt in the ledger until the
// daemon acknowledges its events. That record is history, not work, so it
// must not make `/clear` think the session is busy.
//
// Hard-won: 433c0a29: completed but unacknowledged dispatch records made every idle /clear refuse.
func TestClearIsAcceptedAfterAFinishedTurnWhoseEventsAreUnacknowledged(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	submitRelay(relay, "finished-turn", prompt("Remember PINEAPPLE"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("finished-turn", rtPromptOutcome("end_turn")))
	if _, ok := relay.snapshot.HandledCommands["finished-turn"]; !ok {
		t.Fatal("the finished turn keeps its receipt")
	}
	rtEqual(t, relay.OperationalState().Execution, core.RelayExecutionStateIdle)
	_, refusal, err := relay.submitCommand("clear-request", core.RelayCommandClearContext{})
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("%#v", refusal)
	}
}

// I1-13: a rejected `/clear` stays in the ledger until its events are
// acknowledged. The session must accept prompts again at once instead of
// saying the context is still being cleared.
//
// Hard-won: 2416c177: a failed /clear rollback left the resumed Claude runtime unusable for later prompts.
func TestARejectedClearDoesNotBlockLaterPrompts(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	_, refusal, err := relay.submitCommand("clear-request", core.RelayCommandClearContext{})
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("/clear refused: %#v", refusal)
	}
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandRejected("clear-request", core.OutcomeReasonAdmissionRejected, "restore model after clear failed"))
	if _, ok := relay.snapshot.HandledCommands["clear-request"]; !ok {
		t.Fatal("the rejected clear keeps its receipt")
	}
	if relay.OperationalState().ClearContextStartedAtMs != nil {
		t.Fatal("a rejected clear is not in progress")
	}
	_, refusal, err = relay.submitCommand("after-clear", prompt("still usable"))
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("%#v", refusal)
	}
}

// I1-11: the refusal still holds while a turn is running.
func TestClearIsRefusedWhileATurnIsRunning(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	submitRelay(relay, "running-turn", prompt("Write a story"))
	rtMust(relay.ClaimPendingCommands(true))
	rtRefused(t, relay, "clear-request", core.RelayCommandClearContext{})
}

// The daemon holds mailbox events while the published state reports a
// `/clear` in progress. A finished clear stays in the ledger, so a later turn
// must not republish it as running.
//
// Hard-won: 29a80b2d: a finished /clear was published as in progress during every later turn, so mailbox events waited until the session was fully idle.
func TestAFinishedClearIsNotPublishedAsInProgressDuringALaterTurn(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	_, refusal, err := relay.submitCommand("clear-request", core.RelayCommandClearContext{})
	rtNoErr(err)
	if refusal != nil {
		t.Fatalf("/clear refused: %#v", refusal)
	}
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("clear-request", core.RelayCommandOutcomeContextCleared{NativeSessionID: "replacement", Memory: nil}))
	submitRelay(relay, "later-turn", prompt("Write a story"))
	rtMust(relay.ClaimPendingCommands(true))
	if _, ok := relay.snapshot.HandledCommands["clear-request"]; !ok {
		t.Fatal("the finished clear keeps its receipt")
	}
	if relay.snapshot.ActivityTurnStartedAtMs == nil {
		t.Fatal("the later turn is running")
	}
	if relay.OperationalState().ClearContextStartedAtMs != nil {
		t.Fatal("a finished clear is not in progress")
	}
}

func rtAgentInitialized(runtime *core.RuntimeIdentity) core.RelayObservationAgentInitialized {
	return core.RelayObservationAgentInitialized{
		ProtocolVersion: schema.ProtocolVersion(1),
		Capabilities:    schema.AgentCapabilities{},
		AgentInfo:       nil,
		Runtime:         runtime,
	}
}

// R8-4: resuming a prompted Claude session on haiku opened a phantom turn.
// The adapter sends its "Auto mode unavailable" notice while the resumed
// session is set up (journal: `harness_turn_started` at 77, the notice
// chunk at 78, both before `session_opened` at 79), so the transcript said
// "Agent continued on its own" and the row stayed Working until Esc. Output
// before the session is open and configured is a notice row, never a turn.
//
// Hard-won: 0d653b12: a resumed Haiku session showed a phantom working turn for over three minutes.
func TestOutputWhileAResumedSessionIsSetUpIsANoticeAndOpensNoTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	// The recorded order: a restarted session, a new bridge, the notice,
	// then the opened and configured session.
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	rtMust(relay.RecordObservation(rtAgentInitialized(nil)))
	rtNoErr(relay.applyClaudeTaskFixture("setup-turn", true, true, false, false))
	if _, ok := relay.turnTaskOrdinal("setup-turn"); ok {
		t.Fatal("setup opens no turn")
	}
	rtNoErr(relay.applyClaudeTaskFixture("setup-turn", false, false, true, false))
	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeAutoModeFallbackText)))
	resumed := rtOpenedSession("native")
	resumed.Resumed = true
	rtMust(relay.RecordObservation(resumed))
	rtMust(relay.RecordObservation(rtSessionConfigured()))

	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("setup output is not a turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	recorded := observations(t, relay)
	if rtCountObservations[core.RelayObservationHarnessTurnStarted](recorded) != 0 {
		t.Fatalf("%#v", recorded)
	}
	notice := "Auto mode unavailable: the selected model does not support Auto mode; using " +
		"Accept edits instead."
	noticed := false
	for _, observation := range recorded {
		if message, ok := observation.(core.RelayObservationNotice); ok && message.Message == notice {
			noticed = true
		}
	}
	if !noticed {
		t.Fatalf("%#v", recorded)
	}
	transcriptItems := projectedTranscript(t, relay)
	shown := false
	for index := range transcriptItems {
		item := &transcriptItems[index]
		if system, ok := item.Body.(core.TranscriptBodySystem); ok && system.Text == notice {
			shown = true
		}
		if _, agent := item.Body.(core.TranscriptBodyAgent); agent || item.IsTurnStart() {
			t.Fatalf("%#v", transcriptItems)
		}
	}
	if !shown {
		t.Fatalf("%#v", transcriptItems)
	}

	// Once the session is configured, text with nothing in flight is Claude
	// Code working on its own again.
	rtMust(relay.RecordSessionUpdate(agentTextChunk("The build finished.")))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("text at idle opens a turn")
	}
}

// rtTurnStarts is the transcript items that start a turn.
func rtTurnStarts(items []core.TranscriptItem) []core.TranscriptItem {
	var starts []core.TranscriptItem
	for index := range items {
		if items[index].IsTurnStart() {
			starts = append(starts, items[index])
		}
	}
	return starts
}

// Hard-won: e8566ee4: a late Claude tool failure opened a turn that blocked mailbox wakes for 94 minutes.
func TestLateClaudeToolStatusPreservesTheTranscriptWithoutOpeningATurn(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	rtMust(relay.RecordSessionUpdate(agentTextChunk("Checking the background command.")))
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("text at idle opens a turn")
	}
	terminal := schema.ToolCall{ToolCallID: "background-terminal", Title: "Terminal"}
	rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &terminal}))
	result := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&result))
	rtMust(relay.RecordSessionUpdate(originMarker("task-notification")))
	settledTranscript := projectedTranscript(t, relay)
	settledOrdinal := relay.snapshot.LatestOrdinal

	failed := schema.ToolCallStatusFailed
	late := schema.ToolCallUpdate{
		ToolCallID: "background-terminal",
		Status:     &failed,
		Content: []schema.ToolCallContent{{
			Kind: schema.ToolCallContentKindContent,
			Content: &schema.Content{Content: schema.ContentBlock{
				Kind: schema.ContentBlockKindText,
				Text: &schema.TextContent{Text: "Claude stopped this tool call before it ran."},
			}},
		}},
	}
	rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &late}))

	if relay.OperationalState().HarnessTurn != nil {
		t.Fatal("a late tool status opens no turn")
	}
	for _, event := range rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest)) {
		if _, started := event.Observation.(core.RelayObservationHarnessTurnStarted); started && event.Ordinal > settledOrdinal {
			t.Fatalf("a turn started at %d", event.Ordinal)
		}
	}
	transcriptItems := projectedTranscript(t, relay)
	updated := false
	for _, item := range transcriptItems {
		if tool, ok := item.Body.(core.TranscriptBodyTool); ok {
			id, hasID := tool.Call.ID()
			status, hasStatus := tool.Call.RawStatus()
			updated = updated || (hasID && id == "background-terminal" && hasStatus && status == "failed")
		}
	}
	if !updated {
		t.Fatalf("%#v", transcriptItems)
	}
	// The legitimate first turn already has its marker; the late update must
	// add no second "Agent continued on its own" row.
	rtEqual(t, rtTurnStarts(transcriptItems), rtTurnStarts(settledTranscript))

	submitRelay(relay, "subagent-finished", core.RelayCommandDeliverMailboxEvent{Event: mailboxFixtureEvent("late-tool-wake", true)})
	woken := false
	for _, dispatch := range relay.snapshot.Dispatches {
		if _, wake := dispatch.Command.(core.RelayCommandMailboxWake); wake {
			woken = true
		}
	}
	if !woken {
		t.Fatal("the mailbox wake is dispatched")
	}
}

// R8-5: a live `/model haiku` on a session that already had a reply added
// the adapter's notice to that reply: item `agent:msg_011Cf…` went from
// "one two" to "one two**Auto mode unavailable:** …" (cli/038 vs 046). The
// notice carries no message id, and the projection joins an id-less chunk
// that arrives while idle to the last agent message. Text that answers a
// configuration request is a notice row of its own.
//
// Hard-won: 748622b7: a live model-change notice was appended to the previous agent reply.
func TestTextThatAnswersAModelChangeIsItsOwnNoticeAndLeavesTheReplyAlone(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	submitRelay(relay, "prompt-reply", prompt("Reply with one two."))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	messageID := schema.MessageId("msg_011Cf")
	chunk := schema.ContentChunk{
		Content:   schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: "one two"}},
		MessageID: &messageID,
	}
	rtMust(relay.RecordSessionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindAgentMessageChunk, AgentMessageChunk: &chunk}))
	finishPrompt(relay, "prompt-reply")

	submitRelay(relay, "model-haiku", setConfig("model", "haiku"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	rtMust(relay.RecordSessionUpdate(agentTextChunk(claudeAutoModeFallbackText)))
	rtMust(relay.RecordObservation(core.RelayObservationConfigurationUpdated{Key: "model", Value: "haiku"}))
	rtMust(relay.RecordCommandCompleted("model-haiku", core.RelayCommandOutcomeConfigured{}))

	state := relay.OperationalState()
	if state.HarnessTurn != nil {
		t.Fatal("configuration opens no turn")
	}
	rtEqual(t, state.Execution, core.RelayExecutionStateIdle)
	transcriptItems := projectedTranscript(t, relay)
	var reply *core.TranscriptItem
	for index := range transcriptItems {
		if transcriptItems[index].StableID == "agent:msg_011Cf" {
			reply = &transcriptItems[index]
			break
		}
	}
	if reply == nil {
		t.Fatal("the reply is in the transcript")
	}
	agent, ok := reply.Body.(core.TranscriptBodyAgent)
	if !ok {
		t.Fatalf("the reply is an agent message: %#v", reply)
	}
	var text strings.Builder
	for _, raw := range agent.Chunks {
		var chunk struct {
			Content struct {
				Text *string `json:"text"`
			} `json:"content"`
		}
		if json.Unmarshal(raw, &chunk) == nil && chunk.Content.Text != nil {
			text.WriteString(*chunk.Content.Text)
		}
	}
	rtEqual(t, text.String(), "one two")
	noticed := false
	for _, item := range transcriptItems {
		if system, ok := item.Body.(core.TranscriptBodySystem); ok && system.Text == "Auto mode unavailable: the selected model does not support Auto "+
			"mode; using Accept edits instead." {
			noticed = true
		}
	}
	if !noticed {
		t.Fatalf("%#v", transcriptItems)
	}
}

func TestLegacyRuntimeReceiptReplaysWithoutConstrainingNewPrompts(t *testing.T) {
	temp := t.TempDir()
	var identity core.RuntimeIdentity
	rtNoErr(json.Unmarshal([]byte(`{"id":"mj-runtime-v1:retired","harness":"codex","platform":"linux-x86_64","provenance":"managed_installation","components":[],"unavailable_reason":null}`), &identity))
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	rtMust(relay.RecordObservation(rtAgentInitialized(&identity)))
	receipt := relay.OperationalState().Runtime
	if receipt == nil {
		t.Fatal("the runtime identity is recorded")
	}
	relay = rtMust(DurableRelayOpen(temp, session, "new"))
	rtEqual(t, relay.OperationalState().Runtime, receipt)
	// A fresh harness emits no identity; the old receipt is historical only.
	rtMust(relay.RecordObservation(rtAgentInitialized(nil)))
	submitRelay(relay, "new-work", prompt("continue"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
}

// A Jev verdict leaves the completion identity a handback reminder is
// checked against at the prompt's own completion event, which is the ordinal
// the reminder's sender takes from the projected turn outcome. The identity
// survives a restart; the process-local inference does not.
//
// Hard-won: #1304: a Jev verdict rewrote the completion ordinal, so a parent's handback reminder was refused as stale.
func TestATurnVerdictKeepsTheCompletionIdentityAHandbackReminderNames(t *testing.T) {
	for _, verdict := range []struct {
		glide  core.Probabilities
		reason string
	}{
		{glideAnswer("wait", nil), "background_work_pending"},
		{glideAnswer("finished", nil), "work_finished"},
		{glideAnswer("await_input", nil), "user_input_required"},
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		setVerdictHarness(t, relay, core.HarnessKindClaude)
		submitRelay(relay, "first-command", prompt("work"))
		rtMust(relay.ClaimPendingCommands(true))
		completed := rtMust(relay.RecordCommandCompleted("first-command", rtPromptOutcome("end_turn")))
		rtNoErr(relay.preparePendingAssessment())
		pending, ok := relay.PendingRepliedVerdict()
		if !ok {
			t.Fatal("the finished turn is classified")
		}
		rtEqual(t, rtMust(relay.applyGlideAssessment(pending.Generation, verdict.glide)), verdict.reason)
		relay = rtMust(DurableRelayOpen(temp, session, "test"))
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		if relay.ActivityFacts().InferredIdleSinceMs != nil {
			t.Fatal("the inference is process-local")
		}
		if _, ok := relay.PendingRepliedVerdict(); ok {
			t.Fatal("a recorded assessment is not polled again after restart")
		}
		identity := rtCloneTurnCompletion(relay.OperationalState().TurnCompletion)
		reminder := core.RelayCommandHandbackReminder{
			CompletedCommandID: "first-command",
			CompletedOrdinal:   completed,
			Running:            []string{},
		}
		_, refusal, err := relay.submitCommand("handback-reminder-test", reminder)
		rtNoErr(err)
		if refusal != nil {
			t.Fatalf("%s: the reminder names the turn as the projection does: %#v", verdict.reason, refusal)
		}
		rtEqual(t, identity, &core.TurnCompletion{
			CommandID:        "first-command",
			CompletedOrdinal: completed,
			Decision:         core.DecisionKeepCurrent,
		})
	}
}

func TestDurableArchiveContextRetryAfterConsumptionDoesNotReinstallIt(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	install := func() core.RelayRequest {
		return core.RelayRequestSubmit{
			CommandID: "archive-context",
			Command:   core.RelayCommandInstallPromptContext{Text: "archived context"},
		}
	}
	accepted := relay.Handle(relayRequest("install", install()))
	rtOkPayload(t, accepted)
	submitRelay(relay, "first-prompt", prompt("start"))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].HiddenPromptContext, rtString("archived context"))
	rtMust(relay.RecordCommandCompleted("first-prompt", rtPromptOutcome("end_turn")))
	relay = rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	repeated := relay.Handle(relayRequest("retry-install", install()))
	rtOkPayload(t, repeated)
	submitRelay(relay, "second-prompt", prompt("continue"))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	if claimed[0].HiddenPromptContext != nil {
		t.Fatalf("retry reinstalled consumed context: %q", *claimed[0].HiddenPromptContext)
	}
}

// B-5: after the adapter restarts, the relay forgets the goal and execution
// state, so a Codex session is not replaceable until the adapter reports them
// again on the reopened session. The fake adapter of the reliability lab does.
func TestACodexSessionIsReplaceableAgainOnceTheRestartedAdapterReports(t *testing.T) {
	root := t.TempDir()
	relay := rtMust(DurableRelayOpen(root, session, "test"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
	report := func() {
		rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"goal":null,"execution":{"version":1,"status":"idle"}}`)))
	}
	configure := func() {
		rtMust(relay.RecordObservation(rtSessionConfigured()))
	}
	configure()
	report()
	if state := relay.OperationalState(); !state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("a reported idle session is replaceable")
	}

	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	state := relay.OperationalState()
	if state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("a restarted adapter has not reported")
	}
	facts := state.Facts()
	reported := false
	for _, reason := range core.ReplacementBlockers(&facts, core.HarnessKindCodex, core.WorkerReplacementAutomaticUpgrade) {
		reported = reported || strings.Contains(reason, "has not reported goal and execution state")
	}
	if !reported {
		t.Fatalf("%v", core.ReplacementBlockers(&facts, core.HarnessKindCodex, core.WorkerReplacementAutomaticUpgrade))
	}

	configure()
	report()
	if state := relay.OperationalState(); !state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("the reopened session reported again")
	}
}

// Hard-won: #1302: delivered hook input must release an intentional parent wait.
func TestMailboxDeliveryClaimsAreAtMostOnceAcrossHookPromptAndWake(t *testing.T) {
	hookRoot := t.TempDir()
	hookRelay := rtMust(DurableRelayOpen(hookRoot, session, "test"))
	rtMust(hookRelay.RecordObservation(rtSessionConfigured()))
	submitRelay(hookRelay, "hook-parent-turn", prompt("Delegate and await"))
	rtMust(hookRelay.ClaimPendingCommands(true))
	submitRelay(hookRelay, "hook-await-children", core.RelayCommandSetSubagentWait{ActivePromptID: "hook-parent-turn", Waiting: true})
	hookEvent := mailboxFixtureEvent("hook-event", false)
	submitRelay(hookRelay, "hook-event-command", core.RelayCommandDeliverMailboxEvent{Event: hookEvent})
	if hookRelay.snapshot.Continuation.AwaitingChildrenSinceMs != nil {
		t.Fatal("pending mail releases the wait")
	}
	hookText, hookCount := drainMailboxForTest(t, hookRelay, "hook-drain")
	if hookRelay.snapshot.Continuation.AwaitingChildrenSinceMs != nil {
		t.Fatal("delivered mail releases the wait")
	}
	rtEqual(t, hookCount, 1)
	rtEqual(t, hookText, rtString(core.RenderMailboxEvents([]core.MailboxEvent{hookEvent})))
	if _, ok := hookRelay.snapshot.DeliveredMailboxEventKeys[hookEvent.Key]; !ok {
		t.Fatal("the delivered key is remembered")
	}
	if _, count := drainMailboxForTest(t, hookRelay, "hook-drain-again"); count != 0 {
		t.Fatalf("mail was delivered twice (%d)", count)
	}
	if len(hookRelay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", hookRelay.snapshot.PendingMailboxEvents)
	}

	promptRoot := t.TempDir()
	promptRelay := rtMust(DurableRelayOpen(promptRoot, session, "test"))
	promptEvent := mailboxFixtureEvent("prompt-event", false)
	submitRelay(promptRelay, "prompt-event-command", core.RelayCommandDeliverMailboxEvent{Event: promptEvent})
	submitRelay(promptRelay, "next-user-prompt", prompt("continue"))
	claimed := rtMust(promptRelay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "next-user-prompt")
	rtEqual(t, claimed[0].HiddenPromptContext, rtString(core.RenderMailboxEvents([]core.MailboxEvent{promptEvent})))
	if len(promptRelay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", promptRelay.snapshot.PendingMailboxEvents)
	}
	if _, ok := promptRelay.snapshot.DeliveredMailboxEventKeys[promptEvent.Key]; !ok {
		t.Fatal("the delivered key is remembered")
	}
	if _, count := drainMailboxForTest(t, promptRelay, "prompt-hook-after-claim"); count != 0 {
		t.Fatalf("mail was delivered twice (%d)", count)
	}

	for _, waitingForChildren := range []bool{false, true} {
		wakeRoot := t.TempDir()
		wakeRelay := rtMust(DurableRelayOpen(wakeRoot, session, "test"))
		rtMust(wakeRelay.RecordObservation(rtSessionConfigured()))
		if waitingForChildren {
			submitRelay(wakeRelay, "waiting-parent", prompt("Delegate and await reports"))
			rtMust(wakeRelay.ClaimPendingCommands(true))
			submitRelay(wakeRelay, "await-reports", core.RelayCommandSetSubagentWait{ActivePromptID: "waiting-parent", Waiting: true})
			rtMust(wakeRelay.RecordCommandCompleted("waiting-parent", rtPromptOutcome("end_turn")))
		}
		wakeEvent := mailboxFixtureEvent("wake-event", true)
		submitRelay(wakeRelay, "wake-event-command", core.RelayCommandDeliverMailboxEvent{Event: wakeEvent})
		claimed := rtMust(wakeRelay.ClaimPendingCommands(true))
		rtEqual(t, len(claimed), 1)
		wake, ok := claimed[0].Command.(core.RelayCommandMailboxWake)
		if !ok {
			t.Fatalf("%#v", claimed[0].Command)
		}
		rtEqual(t, wake.Events, []core.MailboxEvent{wakeEvent})
		if claimed[0].HiddenPromptContext != nil {
			t.Fatal("a wake carries its events in its prompt")
		}
		if len(wakeRelay.snapshot.PendingMailboxEvents) != 0 {
			t.Fatalf("%#v", wakeRelay.snapshot.PendingMailboxEvents)
		}
		if _, ok := wakeRelay.snapshot.DeliveredMailboxEventKeys[wakeEvent.Key]; !ok {
			t.Fatal("the delivered key is remembered")
		}
		if _, count := drainMailboxForTest(t, wakeRelay, "wake-hook-after-claim"); count != 0 {
			t.Fatalf("mail was delivered twice (%d)", count)
		}
		if wakeRelay.snapshot.Continuation.AwaitingChildrenSinceMs != nil {
			t.Fatal("the wake releases the wait")
		}
	}
}

func TestPendingMailboxEventsAndDedupKeysSurviveJournalReplay(t *testing.T) {
	temp := t.TempDir()
	event := mailboxFixtureEvent("restart-event", false)
	{
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		submitRelay(relay, "restart-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})
	}

	rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
	replayed := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, replayed.snapshot.PendingMailboxEvents, []core.MailboxEvent{event})
	if len(replayed.snapshot.DeliveredMailboxEventKeys) != 0 {
		t.Fatalf("%#v", replayed.snapshot.DeliveredMailboxEventKeys)
	}
	structuredRetry := event
	var report core.SubagentFinishedReport
	rtNoErr(json.Unmarshal([]byte(`{"child_session_id":"child","task_name":"audit","state":"completed","finished":true,"output":"the report","report_dir":null}`), &report))
	structuredRetry.Body = core.MailboxEventBodySubagentFinished{Report: &report}
	submitRelay(replayed, "restart-event-command", core.RelayCommandDeliverMailboxEvent{Event: structuredRetry})
	rtEqual(t, replayed.snapshot.PendingMailboxEvents, []core.MailboxEvent{event})

	if _, count := drainMailboxForTest(t, replayed, "restart-drain"); count != 1 {
		t.Fatalf("drained %d", count)
	}
	if _, ok := replayed.snapshot.DeliveredMailboxEventKeys[event.Key]; !ok {
		t.Fatal("the delivered key is remembered")
	}
	rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
	replayed = rtMust(DurableRelayOpen(temp, session, "test"))
	if len(replayed.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", replayed.snapshot.PendingMailboxEvents)
	}
	frontier := replayed.LatestOrdinal()
	// Same-key resubmission after delivery is an accepted no-op.
	rtEqual(t, submitRelay(replayed, "same-key-new-command", core.RelayCommandDeliverMailboxEvent{Event: event}), frontier)
	rtEqual(t, replayed.LatestOrdinal(), frontier)
}

// Hard-won: #1277: leasing mailbox events to Codex's PostToolUse hook put a
// developer message between a tool call and its output in the rollout, and
// DeepSeek then rejected every later request, permanently killing the session.
func TestMailboxPostToolUseDrainsFollowTheResolvedDeliveryMode(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	event := mailboxFixtureEvent("codex-mid-turn", false)
	submitRelay(relay, "codex-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})
	postToolUseDrain := func() (*string, int, *string) {
		payload := rtMust(relay.DrainMailbox("PostToolUse"))
		drained, ok := payload.(core.RelayResponsePayloadMailboxDrained)
		if !ok {
			t.Fatalf("unexpected PostToolUse response: %#v", payload)
		}
		return drained.Text, drained.Count, drained.LeaseID
	}
	nothing := func() {
		t.Helper()
		if text, count, leaseID := postToolUseDrain(); text != nil || count != 0 || leaseID != nil {
			t.Fatalf("unexpected drain (%v, %d, %v)", text, count, leaseID)
		}
	}

	// Before the worker resolves the provider the mode is unknown, and an
	// unknown mode is never published or treated as tool-boundary delivery.
	if relay.OperationalState().MailboxDelivery != nil {
		t.Fatal("the delivery mode is unknown")
	}
	nothing()
	relay.SetMailboxDeliveryMode(core.MailboxDeliveryModeTurnBoundary)
	turnBoundary := core.MailboxDeliveryModeTurnBoundary
	rtEqual(t, relay.OperationalState().MailboxDelivery, &turnBoundary)
	nothing()
	if relay.snapshot.MailboxHookLease != nil {
		t.Fatal("a turn-boundary session takes no hook lease")
	}
	rtEqual(t, relay.snapshot.PendingMailboxEvents, []core.MailboxEvent{event})

	relay.SetMailboxDeliveryMode(core.MailboxDeliveryModeToolBoundary)
	toolBoundary := core.MailboxDeliveryModeToolBoundary
	rtEqual(t, relay.OperationalState().MailboxDelivery, &toolBoundary)
	text, count, leaseID := postToolUseDrain()
	rtEqual(t, count, 1)
	rtEqual(t, text, rtString(core.RenderMailboxEvents([]core.MailboxEvent{event})))
	if leaseID == nil {
		t.Fatal("tool-boundary drain creates a lease")
	}
	acknowledgeMailboxForTest(t, relay, "post-tool-use-ack", *leaseID)
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
}

func TestMailboxHookLeaseAcknowledgementIsTheDeliveryCommit(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	event := mailboxFixtureEvent("leased-event", false)
	submitRelay(relay, "leased-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})

	text, count, leaseID := requestMailboxDrainForTest(t, relay, "lease-drain")
	if leaseID == nil {
		t.Fatal("nonempty drain creates a lease")
	}
	rtEqual(t, count, 1)
	rtEqual(t, text, rtString(core.RenderMailboxEvents([]core.MailboxEvent{event})))
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
	if lease := relay.snapshot.MailboxHookLease; lease == nil || lease.LeaseID != *leaseID {
		t.Fatalf("unexpected lease %#v", lease)
	}
	if _, ok := relay.snapshot.DeliveredMailboxEventKeys[event.Key]; ok {
		t.Fatal("a lease is not a delivery")
	}

	acknowledgeMailboxForTest(t, relay, "lease-ack", *leaseID)
	if relay.snapshot.MailboxHookLease != nil {
		t.Fatal("the acknowledgement ends the lease")
	}
	if _, ok := relay.snapshot.DeliveredMailboxEventKeys[event.Key]; !ok {
		t.Fatal("the acknowledgement is the delivery")
	}
	committed := false
	for _, record := range retainedEvents(relay) {
		delivered, ok := record.Observation.(core.RelayObservationMailboxEventsDelivered)
		if ok && delivered.Path == core.MailboxDeliveryPathToolHook && delivered.LeaseID != nil && *delivered.LeaseID == *leaseID &&
			rtLooseEqual(reflect.ValueOf(delivered.EventKeys), reflect.ValueOf([]string{event.Key})) &&
			rtLooseEqual(reflect.ValueOf(delivered.Events), reflect.ValueOf([]core.MailboxEvent{event})) {
			committed = true
		}
	}
	if !committed {
		t.Fatal("the delivery is journaled with its lease")
	}
}

func TestMailboxHookLeaseTimeoutReturnsEventsBeforeNewerPendingEvents(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	leased := mailboxFixtureEvent("leased-first", false)
	newer := mailboxFixtureEvent("pending-second", false)
	submitRelay(relay, "leased-event-command", core.RelayCommandDeliverMailboxEvent{Event: leased})
	if _, _, leaseID := requestMailboxDrainForTest(t, relay, "timeout-drain"); leaseID == nil {
		t.Fatal("the drain leases the event")
	}
	submitRelay(relay, "newer-event-command", core.RelayCommandDeliverMailboxEvent{Event: newer})
	submitRelay(relay, "timeout-prompt", prompt("continue"))
	relay.snapshot.MailboxHookLease.ExpiresAtMs = 0

	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].HiddenPromptContext, rtString(core.RenderMailboxEvents([]core.MailboxEvent{leased, newer})))
	if relay.snapshot.MailboxHookLease != nil {
		t.Fatal("the expired lease is gone")
	}
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
}

func TestUnacknowledgedMailboxLeaseReturnsOnRestartAndReplayIsDeterministic(t *testing.T) {
	temp := t.TempDir()
	first := mailboxFixtureEvent("restart-lease-first", false)
	second := mailboxFixtureEvent("restart-lease-second", false)
	{
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		submitRelay(relay, "restart-lease-first-command", core.RelayCommandDeliverMailboxEvent{Event: first})
		_, count, leaseID := requestMailboxDrainForTest(t, relay, "restart-lease-drain")
		rtEqual(t, count, 1)
		if leaseID == nil {
			t.Fatal("the drain leases the event")
		}
		submitRelay(relay, "restart-lease-second-command", core.RelayCommandDeliverMailboxEvent{Event: second})
	}

	rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
	replayed := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, replayed.snapshot.PendingMailboxEvents, []core.MailboxEvent{first, second})
	if replayed.snapshot.MailboxHookLease != nil {
		t.Fatal("the lease returned on restart")
	}
	returned := false
	for _, record := range retainedEvents(replayed) {
		if lease, ok := record.Observation.(core.RelayObservationMailboxHookLeaseReturned); ok && lease.Reason == core.MailboxHookLeaseReturnReasonWorkerRestart {
			returned = true
		}
	}
	if !returned {
		t.Fatal("the return is journaled")
	}
	replayDigest := replayed.LatestDigest()

	rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
	replayedAgain := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, replayedAgain.snapshot.PendingMailboxEvents, []core.MailboxEvent{first, second})
	rtEqual(t, replayedAgain.LatestDigest(), replayDigest)
}

func TestALiveMailboxLeaseIsNotClaimedByAPromptOrIdleWake(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "active-prompt", prompt("running"))
	rtEqual(t, len(rtMust(relay.ClaimPendingCommands(true))), 1)
	event := mailboxFixtureEvent("leased-waking-event", true)
	submitRelay(relay, "leased-waking-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})
	_, count, leaseID := requestMailboxDrainForTest(t, relay, "live-lease-drain")
	rtEqual(t, count, 1)
	if leaseID == nil {
		t.Fatal("the drain leases the event")
	}

	submitRelay(relay, "prompt-during-lease", prompt("next"))
	finishPrompt(relay, "active-prompt")
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "prompt-during-lease")
	if claimed[0].HiddenPromptContext != nil {
		t.Fatal("a leased event is not prompt context")
	}
	finishPrompt(relay, "prompt-during-lease")
	if relay.snapshot.MailboxHookLease == nil {
		t.Fatal("the lease is still live")
	}
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
	for _, dispatch := range relay.snapshot.Dispatches {
		if _, wake := dispatch.Command.(core.RelayCommandMailboxWake); wake {
			t.Fatal("a leased event wakes nothing")
		}
	}
}

// rtContainsEvent is Vec::contains on mailbox events.
func rtContainsEvent(events []core.MailboxEvent, event core.MailboxEvent) bool {
	for _, candidate := range events {
		if rtLooseEqual(reflect.ValueOf(candidate), reflect.ValueOf(event)) {
			return true
		}
	}
	return false
}

// rtDispatchesMailboxWake reports whether any dispatch is a mailbox wake.
func rtDispatchesMailboxWake(relay *DurableRelay) bool {
	for _, dispatch := range relay.snapshot.Dispatches {
		if _, wake := dispatch.Command.(core.RelayCommandMailboxWake); wake {
			return true
		}
	}
	return false
}

func TestDeliveredMailboxKeysAreBoundedAndPrunedDeterministicallyOnReplay(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	for index := 0; index <= core.RelayRetryIDRetention; index++ {
		event := mailboxFixtureEvent(fmt.Sprintf("retained-event-%04d", index), false)
		submitRelay(relay, fmt.Sprintf("retained-command-%04d", index), core.RelayCommandDeliverMailboxEvent{Event: event})
		if index == 0 {
			if _, count := drainMailboxForTest(t, relay, "retained-drain-first"); count != 1 {
				t.Fatalf("drained %d", count)
			}
		}
	}
	rtEqual(t, len(relay.snapshot.PendingMailboxEvents), core.RelayRetryIDRetention)
	if _, count := drainMailboxForTest(t, relay, "retained-drain-batch"); count != core.RelayRetryIDRetention {
		t.Fatalf("drained %d", count)
	}

	pending := mailboxFixtureEvent("pending-beyond-retention", false)
	submitRelay(relay, "pending-beyond-retention-command", core.RelayCommandDeliverMailboxEvent{Event: pending})
	frontier := relay.LatestOrdinal()
	// Pending event keys remain deduplicated regardless of delivered-key
	// pruning.
	rtEqual(t, submitRelay(relay, "duplicate-pending-event-command", core.RelayCommandDeliverMailboxEvent{Event: pending}), frontier)

	deliveredKeys := maps.Clone(relay.snapshot.DeliveredMailboxEventKeys)
	rtEqual(t, len(deliveredKeys), core.RelayRetryIDRetention)
	if _, ok := deliveredKeys["retained-event-0000"]; ok {
		t.Fatal("the oldest delivered key is pruned")
	}
	for _, key := range []string{"retained-event-0001", "retained-event-0512"} {
		if _, ok := deliveredKeys[key]; !ok {
			t.Fatalf("%s is retained", key)
		}
	}
	rtEqual(t, relay.snapshot.PendingMailboxEvents, []core.MailboxEvent{pending})

	rtNoErr(os.Remove(filepath.Join(temp, core.RelayStateFile)))
	replayed := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, replayed.snapshot.DeliveredMailboxEventKeys, deliveredKeys)
	rtEqual(t, replayed.snapshot.PendingMailboxEvents, []core.MailboxEvent{pending})
}

func TestMailboxContextStaysPendingAcrossCompactionAndSlashPrompts(t *testing.T) {
	for index, promptText := range []string{"/compact", "/review"} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		event := mailboxFixtureEvent(fmt.Sprintf("context-event-%d", index), false)
		submitRelay(relay, "context-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})
		submitRelay(relay, "context-command", prompt(promptText))
		claimed := rtMust(relay.ClaimPendingCommands(true))
		rtEqual(t, len(claimed), 1)
		if claimed[0].HiddenPromptContext != nil {
			t.Fatalf("%s consumed the mail", promptText)
		}
		rtEqual(t, relay.snapshot.PendingMailboxEvents, []core.MailboxEvent{event})

		rtMust(relay.RecordCommandCompleted("context-command", rtPromptOutcome("end_turn")))
		submitRelay(relay, "eligible-user-prompt", prompt("continue"))
		claimed = rtMust(relay.ClaimPendingCommands(true))
		rtEqual(t, len(claimed), 1)
		rtEqual(t, claimed[0].HiddenPromptContext, rtString(core.RenderMailboxEvents([]core.MailboxEvent{event})))
	}
}

func TestMailboxWakeWaitsForCheckpointCloseAndCancellation(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "wake-checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
	event := mailboxFixtureEvent("barrier-wake-event", true)
	submitRelay(relay, "barrier-wake-event-command", core.RelayCommandDeliverMailboxEvent{Event: event})
	if !rtContainsEvent(relay.snapshot.PendingMailboxEvents, event) {
		t.Fatal("the event waits")
	}
	if rtDispatchesMailboxWake(relay) {
		t.Fatal("a pending checkpoint holds the wake")
	}

	cut := readyCheckpoint(relay, "wake-checkpoint")
	submitRelay(relay, "wake-close", core.RelayCommandClose{BarrierCommandID: "wake-checkpoint", Expected: cut})
	submitRelay(relay, "wake-checkpoint-complete", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "wake-checkpoint"})
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].CommandID, "wake-close")
	if _, ok := claimed[0].Command.(core.RelayCommandClose); !ok {
		t.Fatalf("%#v", claimed[0].Command)
	}
	if rtDispatchesMailboxWake(relay) {
		t.Fatal("a close holds the wake")
	}

	cancelRoot := t.TempDir()
	cancelRelay := rtMust(DurableRelayOpen(cancelRoot, session, "test"))
	submitRelay(cancelRelay, "running-prompt", prompt("work"))
	rtMust(cancelRelay.ClaimPendingCommands(true))
	submitRelay(cancelRelay, "cancel-running-prompt", core.RelayCommandCancelTurnFor{ActivePromptID: "running-prompt"})
	rtMust(cancelRelay.ClaimPendingCommands(true))
	rtEqual(t, cancelRelay.snapshot.CancellingPromptID, rtString("running-prompt"))
	cancelEvent := mailboxFixtureEvent("cancel-wake-event", true)
	submitRelay(cancelRelay, "cancel-wake-event-command", core.RelayCommandDeliverMailboxEvent{Event: cancelEvent})
	if !rtContainsEvent(cancelRelay.snapshot.PendingMailboxEvents, cancelEvent) {
		t.Fatal("the event waits")
	}
	if rtDispatchesMailboxWake(cancelRelay) {
		t.Fatal("a cancellation holds the wake")
	}
}

// Hard-won: 8ba3d377e: settings at the queue head made both automatic and manual steering unusable.
func TestPublishedApplyQueuedPreservesSettingsOrderAndSurvivesReconnect(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	submitRelay(relay, "model-change", setConfig("model", "fable"))
	submitRelay(relay, "effort-change", setConfig("effort", "high"))
	queuePrompt(t, relay, "user-question", "Compare Go with Rust.")
	rtNoClaims(t, relay)
	controls := relay.OperationalState().TurnControls
	action := controls.Primary
	rtEqual(t, action.Kind, core.TurnControlKindApplyQueued)
	if controls.Message == nil || !strings.Contains(*controls.Message, "model: fable, effort: high") {
		t.Fatalf("unexpected message %v", controls.Message)
	}
	command := action.Command
	rtEqual(t, command, core.RelayCommand(core.RelayCommandApplyQueuedFor{ActivePromptID: "prompt-first", QueuedCommandID: "model-change"}))
	submitRelay(relay, "apply-queued", command)
	rtEqual(t, relay.OperationalState().TurnControls.Status, core.TurnControlStatusPending)
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	rtEqual(t, claimed[0].Command, command)
	// Cancellation drains before either settings or the next prompt can start.
	finishPrompt(relay, "prompt-first")
	rtNoClaims(t, relay)
	rtMust(relay.RecordCommandCompleted("apply-queued", core.RelayCommandOutcomeCancelled{}))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].CommandID, "model-change")
	rtMust(relay.RecordCommandCompleted("model-change", core.RelayCommandOutcomeConfigured{}))
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, claimed[0].CommandID, "effort-change")
	rtMust(relay.RecordCommandCompleted("effort-change", core.RelayCommandOutcomeConfigured{}))
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "user-question")
	var reconnected core.RelayOperationalState
	rtNoErr(json.Unmarshal(rtMust(json.Marshal(relay.OperationalState())), &reconnected))
	rtEqual(t, reconnected.ActivePrompt.CommandID, "user-question")
	finishPrompt(relay, "user-question")
	reopened := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, reopened.snapshot.Config["model"], "fable")
	rtEqual(t, reopened.snapshot.Config["effort"], "high")
	if _, ok := reopened.snapshot.Dispatches["user-question"]; ok {
		t.Fatal("the finished prompt is no longer dispatched")
	}
	if reopened.snapshot.HandledCommands["user-question"].TerminalOrdinal == nil {
		t.Fatal("the finished prompt is terminal")
	}
	if len(reopened.snapshot.QueuedPrompts) != 0 {
		t.Fatalf("%#v", reopened.snapshot.QueuedPrompts)
	}
}

func TestStalePublishedControlsCannotFollowAReplacedQueueOrTurn(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	submitRelay(relay, "model-change", setConfig("model", "fable"))
	stale := relay.OperationalState().TurnControls.Primary.Command
	submitRelay(relay, "remove-model", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "model-change"})
	submitRelay(relay, "other-setting", setConfig("effort", "high"))
	rtRefused(t, relay, "stale-queue-click", stale)
	if relay.snapshot.CancellingPromptID != nil {
		t.Fatal("a refused click cancels nothing")
	}
	stale = relay.OperationalState().TurnControls.Primary.Command
	finishPrompt(relay, "prompt-first")
	rtEqual(t, rtMust(relay.ClaimPendingCommands(true))[0].CommandID, "other-setting")
	rtMust(relay.RecordCommandCompleted("other-setting", core.RelayCommandOutcomeConfigured{}))
	queuePrompt(t, relay, "later-turn", "A later request.")
	rtMust(relay.ClaimPendingCommands(true))
	rtRefused(t, relay, "stale-turn-click", stale)
	rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "later-turn")
	if relay.snapshot.CancellingPromptID != nil {
		t.Fatal("a refused click cancels nothing")
	}
}

func TestApplyQueuedRechecksItsHeadBeforeDispatch(t *testing.T) {
	temp := t.TempDir()
	relay := relaySteeringAutomatically(t, temp)
	submitRelay(relay, "model-change", setConfig("model", "fable"))
	command := relay.OperationalState().TurnControls.Primary.Command
	submitRelay(relay, "apply-queued", command)
	submitRelay(relay, "remove-model", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "model-change"})
	submitRelay(relay, "other-setting", setConfig("effort", "high"))
	rtNoClaims(t, relay)
	if _, ok := relay.snapshot.Dispatches["apply-queued"]; ok {
		t.Fatal("the stale apply is no longer dispatched")
	}
	if relay.snapshot.HandledCommands["apply-queued"].TerminalOrdinal == nil {
		t.Fatal("the stale apply is terminal")
	}
	if relay.snapshot.CancellingPromptID != nil {
		t.Fatal("the stale apply cancels nothing")
	}
	rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "prompt-first")
	rtEqual(t, queuedCommandIDs(relay), []string{"other-setting"})
}

func TestHooklessMidTurnMailboxEventStartsAWakeTurnAfterCompletion(t *testing.T) {
	root := t.TempDir()
	relay := rtMust(DurableRelayOpen(root, session, "test"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	submitRelay(relay, "parent-working", prompt("Do the independent work"))
	rtMust(relay.ClaimPendingCommands(true))
	event := mailboxFixtureEvent("child-report-without-a-hook", true)
	submitRelay(relay, "deliver-child-report", core.RelayCommandDeliverMailboxEvent{Event: event})
	rtNoClaims(t, relay)
	rtEqual(t, relay.snapshot.PendingMailboxEvents, []core.MailboxEvent{event})
	// No drain hook runs. Completion itself makes the pending wake eligible.
	rtMust(relay.RecordCommandCompleted("parent-working", rtPromptOutcome("end_turn")))
	claimed := rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	wake, ok := claimed[0].Command.(core.RelayCommandMailboxWake)
	if !ok {
		t.Fatalf("%#v", claimed[0].Command)
	}
	rtEqual(t, wake.Events, []core.MailboxEvent{event})
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
	wakeID := claimed[0].CommandID
	rtMust(relay.RecordCommandCompleted(wakeID, rtPromptOutcome("end_turn")))
	rtEqual(t, rtMust(relay.applyRepliedDecision(relay.turnContext.Generation(), core.DecisionExpectContinuation, 200)), "applied")
	if relay.activityIsIdle() {
		t.Fatal("an expected continuation is not idle")
	}
	later := mailboxFixtureEvent("later-child-report-without-a-hook", true)
	submitRelay(relay, "deliver-later-child-report", core.RelayCommandDeliverMailboxEvent{Event: later})
	claimed = rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, len(claimed), 1)
	wake, ok = claimed[0].Command.(core.RelayCommandMailboxWake)
	if !ok {
		t.Fatalf("%#v", claimed[0].Command)
	}
	rtEqual(t, wake.Events, []core.MailboxEvent{later})
	if len(relay.snapshot.PendingMailboxEvents) != 0 {
		t.Fatalf("%#v", relay.snapshot.PendingMailboxEvents)
	}
	if _, ok := relay.snapshot.DeliveredMailboxEventKeys[event.Key]; !ok {
		t.Fatal("the first report is delivered")
	}
}

func TestPushReviewRequestsRequireProtocol43AndOldTurnCapturesStillDecode(t *testing.T) {
	for _, request := range []core.ReviewerRequest{
		core.ReviewerRequestCaptureDelta{Repositories: nil, Baselines: map[string]string{}, Scope: core.CaptureScopePushableCommits},
		core.ReviewerRequestRecordReviewed{Trees: map[string][2]string{}},
	} {
		request := core.RelayRequestReviewer{Role: nil, Request: request}
		if core.RelayRequestSupportedAt(request, 42) {
			t.Fatalf("%#v is supported at 42", request)
		}
		if !core.RelayRequestSupportedAt(request, 43) {
			t.Fatalf("%#v is not supported at 43", request)
		}
	}
	scoped := core.RelayRequestReviewer{Role: nil, Request: core.ReviewerRequestCaptureDelta{
		Repositories: &[]string{"/worktree"},
		Baselines:    map[string]string{},
		Scope:        core.CaptureScopePushableCommits,
	}}
	if core.RelayRequestSupportedAt(scoped, 48) {
		t.Fatal("scoped captures are not supported at 48")
	}
	if !core.RelayRequestSupportedAt(scoped, 49) {
		t.Fatal("scoped captures are supported at 49")
	}
	legacy := core.ReviewerRequestCaptureDelta{Repositories: nil, Baselines: map[string]string{}, Scope: core.CaptureScopePushableCommits}
	if strings.Contains(string(rtMust(json.Marshal(legacy))), "repositories") {
		t.Fatal("an unscoped capture omits repositories")
	}
	old := rtMust(core.UnmarshalReviewerRequestJSON([]byte(`{"action":"capture_delta","params":{"baselines":{}}}`)))
	capture, ok := old.(core.ReviewerRequestCaptureDelta)
	if !ok || capture.Repositories != nil || capture.Scope != core.CaptureScopePushableCommits {
		t.Fatalf("unexpected decoded request %#v", old)
	}
	if !core.RelayRequestSupportedAt(core.RelayRequestReviewer{Role: nil, Request: old}, 43) {
		t.Fatal("an old capture is supported at 43")
	}
}

// Hard-won: #1450: a settled harness turn could not admit its handback reminder.
func TestSettledHarnessTurnAdmitsOnlyItsCurrentReminder(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	rtNoErr(relay.openHarnessTurnIfAllowed())
	id, ok := relay.originatingCommandID()
	if !ok {
		t.Fatal("the harness turn has an originating command")
	}
	rtNoErr(relay.settleHarnessTurn(nil, "end_turn"))
	completion := relay.OperationalState().TurnCompletion
	rtEqual(t, completion.CommandID, id)
	rtRefused(t, relay, "stale-reminder", core.RelayCommandHandbackReminder{
		CompletedCommandID: "harness-turn-0",
		CompletedOrdinal:   completion.CompletedOrdinal,
		Running:            []string{},
	})
	submitRelay(relay, "current-reminder", core.RelayCommandHandbackReminder{
		CompletedCommandID: id,
		CompletedOrdinal:   completion.CompletedOrdinal,
		Running:            []string{},
	})
}

// A follow-up for a completion the child has moved past is refused for good,
// so the daemon can tell it from a worker that merely cannot take a prompt yet.
func TestAHoldFollowUpForACompletionThatIsNoLongerCurrentIsRefusedForGood(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	rtNoErr(relay.openHarnessTurnIfAllowed())
	id, ok := relay.originatingCommandID()
	if !ok {
		t.Fatal("the harness turn has an originating command")
	}
	rtNoErr(relay.settleHarnessTurn(nil, "end_turn"))
	completion := relay.OperationalState().TurnCompletion
	followUp := func(completedCommandID string) core.RelayCommand {
		return core.RelayCommandHoldFollowUp{
			CompletedCommandID: completedCommandID,
			CompletedOrdinal:   completion.CompletedOrdinal,
			Kind:               core.HoldFollowUpKindProcessesExited,
			Running:            []string{},
		}
	}
	_, refused, err := relay.submitCommand("stale-hold", followUp("harness-turn-0"))
	rtNoErr(err)
	if refused == nil {
		t.Fatal("a stale follow-up is refused")
	}
	rtEqual(t, refused.Code, core.RelayErrorCodeInvalidState)
	if refused.Retryable {
		t.Fatal("a stale completion never succeeds later")
	}
	submitRelay(relay, "current-hold", followUp(id))
}

func TestReadinessAndTheInstantTheScanReadsCannotDisagree(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "1.0.0"))
	clock := relay.HarnessReadyClock()
	if relay.isAcpReady() {
		t.Fatal("a fresh relay is not ready")
	}
	rtEqual(t, clock.Load(), int64(0))
	relay.setAcpReady(true)
	first := clock.Load()
	if !relay.isAcpReady() || first == 0 {
		t.Fatalf("ready %v at %d", relay.isAcpReady(), first)
	}
	rtEqual(t, relay.OperationalState().ACPReady, rtBool(true))
	// A second configuration of an open session keeps the first instant.
	relay.setAcpReady(true)
	rtEqual(t, clock.Load(), first)
	relay.ClearAcpReadiness()
	if relay.isAcpReady() {
		t.Fatal("readiness was cleared")
	}
	rtEqual(t, clock.Load(), int64(0))
}

func TestLegacySnapshotOpenRequestUpgradeAndCheckpointRestore(t *testing.T) {
	for _, legacyCase := range []struct{ dismissed, laterPrompt, inputRequired bool }{
		{false, false, false},
		{true, false, false},
		{false, true, false},
		{false, false, true},
	} {
		root := t.TempDir()
		relay := rtMust(DurableRelayOpen(root, session, "test"))
		snapshot := relay.snapshot
		stopReason := "EndTurn"
		if legacyCase.inputRequired {
			stopReason = "AwaitingInput"
		}
		assessment := core.TurnAssessmentPending("legacy-turn", 10, 0, core.CompletionEvidenceBounded(stopReason, nil))
		if !legacyCase.inputRequired {
			action := core.AssessmentActionAwaitInput
			assessment.Action = &action
		} else {
			assessment.Action = nil
		}
		assessment.QuestionMessage = rtString("original-question")
		if legacyCase.dismissed {
			assessment.Status = core.StatusSuperseded
			assessment.Reason = "question_dismissed"
		}
		stored := assessment
		snapshot.Assessment = &stored
		snapshot.LatestPromptAcceptedOrdinal = nil
		if legacyCase.laterPrompt {
			snapshot.LatestPromptAcceptedOrdinal = rtUint64(11)
		}
		value := rtJsonObject(snapshot)
		value["format_version"] = 22
		for _, field := range []string{"request", "person_input_through"} {
			delete(value, field)
		}
		rtWriteRelayState(root, value)
		upgraded := rtMust(DurableRelayOpen(root, session, "test"))
		rtEqual(t, upgraded.snapshot.OpenRequest() != nil, !legacyCase.dismissed && !legacyCase.laterPrompt)
		expectedThrough := uint64(0)
		if legacyCase.laterPrompt {
			expectedThrough = 11
		}
		rtEqual(t, upgraded.snapshot.PersonInputThrough, expectedThrough)
		rtEqual(t, len(upgraded.snapshot.NotificationStack.Entries) != 0, upgraded.snapshot.OpenRequest() != nil)
		// Both modern exact-empty and exact-open checkpoint envelopes survive restoration.
		state := core.Checkpoint{
			Version:        1,
			Context:        nil,
			Assessment:     &assessment,
			Continuation:   core.ContinuationState{},
			CapacityRetry:  nil,
			TurnCompletion: nil,
			OpenRequest: &core.CheckpointOpenRequest{
				Request:            upgraded.snapshot.OpenRequest(),
				PersonInputThrough: upgraded.snapshot.PersonInputThrough,
			},
		}
		for _, legacy := range []bool{false, true} {
			destination := t.TempDir()
			encoded := rtJsonObject(state)
			if legacy {
				delete(encoded, "open_request")
			}
			stack := upgraded.snapshot.NotificationStack
			stack.Revision = 20
			stack.Entries = slices.Insert(slices.Clone(stack.Entries), 0, core.NotificationStackEntry{
				CompletedAtMs:    0,
				CompletedOrdinal: 5,
				Kind:             core.NotificationKindImportant,
				Message:          core.EntryMessageFound{Value: "agent:retained-important"},
			})
			seed := map[string]any{
				"event_frontier":        20,
				"event_frontier_digest": strings.Repeat("a", 64),
				"assessment_state":      encoded,
			}
			if !legacy {
				seed["notification_stack"] = rtJsonObject(stack)
			}
			rtNoErr(os.WriteFile(core.RestoredRelaySeedPath(destination), rtMust(json.Marshal(seed)), 0o644))
			restored := rtMust(DurableRelayOpen(destination, session, "test"))
			rtEqual(t, restored.snapshot.OpenRequest() != nil, !legacyCase.dismissed && (legacy || !legacyCase.laterPrompt))
			if !legacy {
				rtEqual(t, restored.snapshot.NotificationStack, stack)
				rtEqual(t, restored.snapshot.PersonInputThrough, state.OpenRequest.PersonInputThrough)
			}
		}
		// Current snapshots are exact even if their assessment still says AwaitInput.
		current := snapshot
		current.Request = nil
		rtWriteRelayState(root, current)
		currentRelay := rtMust(DurableRelayOpen(root, session, "test"))
		if currentRelay.snapshot.OpenRequest() != nil {
			t.Fatal("a current snapshot without a request has none")
		}
	}
}

// Hard-won: bd6363001 review: reopening a dead relay promoted work before Resume could discard it.
// Hard-won: 926625478 review: discard must apply to a seed left by an earlier recovery attempt.
func TestOfflineQueueDiscardPrecedesReopenPromotionAndRequiresExclusiveRootOwnership(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "old-active", prompt("old active turn"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "explicitly-discarded", prompt("must never execute"))
	if !slices.Contains(queuedCommandIDs(relay), "explicitly-discarded") {
		t.Fatal("the prompt is queued")
	}
	owner := rtMust(WorkerRootOwnerAcquire(temp))
	if DiscardQueuedPromptsBeforeStart(temp, session, "resume-discard") == nil {
		t.Fatal("a discard must not run while another process owns the root")
	}
	owner.Release()
	rtNoErr(DiscardQueuedPromptsBeforeStart(temp, session, "resume-discard"))
	reopened := rtMust(DurableRelayOpen(temp, session, "test"))
	for _, command := range rtMust(reopened.ClaimPendingCommands(true)) {
		if command.CommandID == "explicitly-discarded" {
			t.Fatal("a discarded prompt ran")
		}
	}
	if slices.Contains(queuedCommandIDs(reopened), "explicitly-discarded") {
		t.Fatal("a discarded prompt is still queued")
	}
	ordinal := reopened.snapshot.HandledCommands["resume-discard"].AcceptedOrdinal
	rtNoErr(DiscardQueuedPromptsBeforeStart(temp, session, "resume-discard"))
	repeated := rtMust(DurableRelayOpen(temp, session, "test"))
	rtEqual(t, repeated.snapshot.HandledCommands["resume-discard"].AcceptedOrdinal, ordinal)
	seedRoot := t.TempDir()
	seed := core.RestoredRelaySeed{
		CommandLedger:       nil,
		AssessmentState:     nil,
		NotificationStack:   nil,
		EventFrontier:       7,
		EventFrontierDigest: strings.Repeat("a", 64),
		QueuedPrompts: []core.CanonicalQueuedPrompt{{
			CommandID:  "seed-queued",
			Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
			Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"must not execute from an old seed"}`)},
			QueuedAtMs: 1,
		}},
		AcceptedConfig:      map[string]string{"model": "kept-model"},
		NativeSessionUnused: true,
		Mailbox:             nil,
	}
	rtNoErr(os.WriteFile(core.RestoredRelaySeedPath(seedRoot), rtMust(json.Marshal(seed)), 0o644))
	rtNoErr(DiscardQueuedPromptsBeforeStart(seedRoot, session, "discard-retried-seed"))
	recovered := rtMust(DurableRelayOpenForHarness(seedRoot, session, "test", core.HarnessKindCodex, nil))
	for _, command := range rtMust(recovered.ClaimPendingCommands(true)) {
		if command.CommandID == "seed-queued" {
			t.Fatal("a discarded seed prompt ran")
		}
	}
	if ids := queuedCommandIDs(recovered); len(ids) != 0 {
		t.Fatalf("%v", ids)
	}
	rtEqual(t, recovered.snapshot.RecoveryFloorOrdinal, uint64(7))
	model, ok := recovered.snapshot.Config["model"]
	if !ok || model != "kept-model" {
		t.Fatalf("model is %q (%v)", model, ok)
	}
}

// A turn Claude starts on its own ends a managed child's turn like a prompt
// does: its facts set `work_liveness` with the settlement's ordinal, the
// watcher can recount its processes, and the next prompt clears them.
func TestAnAutonomousHarnessTurnSettlingRecordsWorkFactsAndLiveness(t *testing.T) {
	temp := t.TempDir()
	relay := relayTestsClaudeRelay(t, temp)
	rtMust(relay.RecordSessionUpdate(toolCallUpdate()))
	result := core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: cycleResult(t, "task-notification")}
	if !relay.EventSettlesAutonomousTurn(result) {
		t.Fatal("the cycle result settles the autonomous turn")
	}
	identity := core.ProcessIdentity{Pid: 4242, Start: "boot-1"}
	facts := core.WorkFacts{
		ObservedAtMs: 77,
		Processes: core.ObservedKnown(core.ProcessFacts{
			Running:      1,
			Shown:        nil,
			Omitted:      1,
			Identities:   []core.ProcessIdentity{identity},
			Unattributed: 0,
		}),
		Worktree: core.ObservedUnavailable[core.WorktreeFacts]("not read"),
	}
	offered := facts
	relay.OfferSettleFacts(&offered)
	cycle := cycleResult(t, "task-notification")
	rtNoErr(relay.ClaudeTurnResult(&cycle))
	var settled *core.SettledHarnessTurn
	for _, observation := range observations(t, relay) {
		if turn, ok := observation.(core.RelayObservationHarnessTurnSettled); ok && turn.Autonomous != nil {
			settled = turn.Autonomous
			break
		}
	}
	if settled == nil {
		t.Fatal("an autonomous settlement")
	}
	rtEqual(t, settled.WorkFacts, &facts)
	completion := relay.OperationalState().TurnCompletion
	liveness := relay.OperationalState().WorkLiveness
	rtEqual(t, liveness.CompletedOrdinal, completion.CompletedOrdinal)
	rtEqual(t, liveness.Alive, uint32(1))
	rtEqual(t, liveness.CheckedAtMs, uint64(77))
	watched, identities, ok := relay.watchedWork()
	if !ok {
		t.Fatal("the settled work is watched")
	}
	rtEqual(t, watched, *liveness)
	rtEqual(t, identities, []core.ProcessIdentity{identity})
}

// rtCommandRetentionUpdated asserts the response to UpdateCommandRetention.
func rtCommandRetentionUpdated(t *testing.T, response core.RelayResponseEnvelope) {
	t.Helper()
	if _, ok := rtOkPayload(t, response).(core.RelayResponsePayloadCommandRetentionUpdated); !ok {
		t.Fatalf("%#v", response.Body)
	}
}

// Pinning is a durable operation boundary, independent of the journal and bounded retry tail.
//
// Hard-won: f30ad64: completed pin release also dropped an unrelated legacy Move hold.
func TestInterruptedMoveReceiptsSurviveAckPruningAndWorkerRestart(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	command := setConfig("model", "retained-model")
	accepted := submitRelay(relay, "move-queued-config", command)
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandCompleted("move-queued-config", core.RelayCommandOutcomeConfigured{}))
	for i := 0; i <= core.RelayRetryIDRetention; i++ {
		submitRelay(relay, fmt.Sprintf("later-command-%d", i), core.RelayCommandRecordNotice{Text: fmt.Sprintf("completed %d", i)})
	}
	// Model an old worker's acknowledged snapshot: its floor still retains
	// the journal, and its Move has not registered pins with this new protocol.
	relay.snapshot.AcknowledgedThrough = relay.LatestOrdinal()
	relay.snapshot.AcknowledgedDigest = relay.LatestDigest()
	rtNoErr(relay.persistSnapshot())
	rtRewriteRelayState(t, temp, func(old map[string]any) {
		old["format_version"] = 27
	})
	relay = rtMust(DurableRelayOpen(temp, session, "new"))
	if !relay.snapshot.LegacyCommandRetention {
		t.Fatal("an old snapshot holds legacy retention")
	}
	// Failed archive reconciliation cannot prevent releasing an unrelated
	// finished operation, but that release must keep the upgrade hold.
	for _, update := range []struct {
		requestID string
		ids       []string
	}{
		{"pin-finished-move", []string{"later-command-0"}},
		{"release-finished-move", []string{}},
	} {
		rtCommandRetentionUpdated(t, relay.Handle(relayRequest(update.requestID, core.RelayRequestUpdateCommandRetention{
			OperationID:     rtString("finished-move-operation"),
			CommandIds:      update.ids,
			ReconcileLegacy: false,
		})))
		if !relay.snapshot.LegacyCommandRetention {
			t.Fatal("the upgrade hold is kept")
		}
	}
	rtNoErr(relay.garbageCollectRelayHistory())
	relay = rtMust(DurableRelayOpen(temp, session, "still-unreconciled"))
	if !relay.snapshot.LegacyCommandRetention {
		t.Fatal("the upgrade hold survives a restart")
	}
	rtEqual(t, submitRelay(relay, "move-queued-config", command), accepted)
	update := func(ids []string, reconcileLegacy bool) core.RelayRequest {
		return core.RelayRequestUpdateCommandRetention{
			OperationID:     rtString("move-operation-1"),
			CommandIds:      ids,
			ReconcileLegacy: reconcileLegacy,
		}
	}
	rtCommandRetentionUpdated(t, relay.Handle(relayRequest("reconcile", update([]string{"move-queued-config"}, true))))
	ordinal := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	rtOkPayload(t, relay.Handle(relayRequest("acknowledge", core.RelayRequestAcknowledge{ThroughOrdinal: ordinal, ThroughDigest: digest})))
	rtEqual(t, relay.snapshot.RetainedThrough(), ordinal)
	rtEqual(t, relay.snapshot.RecoveryFloorOrdinal, uint64(0))
	rtEqual(t, relay.snapshot.HandledCommands["move-queued-config"].Outcome, core.RelayCommandOutcome(core.RelayCommandOutcomeConfigured{}))
	relay = rtMust(DurableRelayOpen(temp, session, "new-again"))
	// A retry after journal pruning returns the original acceptance, not a new dispatch.
	rtEqual(t, submitRelay(relay, "move-queued-config", command), accepted)
	if dispatch, ok := relay.snapshot.Dispatches["move-queued-config"]; ok && dispatch.State != core.RelayDispatchStateCompleted {
		t.Fatalf("the retry dispatched again: %#v", dispatch)
	}
	// A stale connector cannot undo operation pins after upgrade reconciliation.
	relay.Handle(relayRequest("stale-reconcile", core.RelayRequestUpdateCommandRetention{
		OperationID:     nil,
		CommandIds:      []string{},
		ReconcileLegacy: true,
	}))
	if _, ok := relay.snapshot.CommandRetention["move-operation-1"]; !ok {
		t.Fatal("the operation pin survives a stale reconcile")
	}
	// The daemon sends this only after queue_admission_finished is durable.
	relay.Handle(relayRequest("release-operation", update([]string{}, false)))
	rtNoErr(relay.garbageCollectRelayHistory())
	if _, ok := relay.snapshot.HandledCommands["move-queued-config"]; ok {
		t.Fatal("the released receipt is collected")
	}
}

func TestUnusedNativeEvidenceSurvivesAckPruningAndWorkerRestart(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	rtMust(relay.RecordObservation(rtOpenedSession("empty-native")))
	ordinal := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	relay.Handle(relayRequest("ack-empty", core.RelayRequestAcknowledge{ThroughOrdinal: ordinal, ThroughDigest: digest}))
	rtEqual(t, relay.snapshot.AcknowledgedThrough, ordinal)
	if _, err := relay.EventsAfter(0, core.RelayEventGenesisDigest); err == nil {
		t.Fatal("the acknowledged history is collected")
	}
	if active := rtMust(os.ReadFile(filepath.Join(temp, core.RelayJournalDir, core.RelayActiveSegment))); len(active) != 0 {
		t.Fatalf("the active segment holds %d bytes", len(active))
	}
	relay = rtMust(DurableRelayOpen(temp, session, "new"))
	if relay.NativeSessionMayHaveHistory() {
		t.Fatal("the unused evidence survives pruning")
	}
	rtEqual(t, relay.NativeSessionHistory(), NativeSessionHistoryCreatedHereUnused)

	evidence := relay.snapshot.NativeUseEvidence
	rtEqual(t, evidence.NativeSessionID, "empty-native")
	rtEqual(t, evidence.OpenedOrdinal, rtUint64(ordinal))
	rtEqual(t, evidence.Usage, core.NativeSessionUseUnused)
	rtEqual(t, evidence.Version, uint32(1))
}

func TestUncertainNativeUseSurvivesTerminalReceiptPruning(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	rtMust(relay.RecordObservation(rtOpenedSession("uncertain-native")))
	submitRelay(relay, "uncertain-prompt", prompt("possibly transmitted"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordCommandRejected("uncertain-prompt", core.OutcomeReasonCommandFailed, "lost ACP reply"))
	for i := 0; i <= core.RelayRetryIDRetention; i++ {
		submitRelay(relay, fmt.Sprintf("after-uncertain-%d", i), core.RelayCommandRecordNotice{Text: fmt.Sprintf("later %d", i)})
	}
	throughOrdinal := relay.LatestOrdinal()
	throughDigest := relay.LatestDigest()
	relay.Handle(relayRequest("ack-uncertain", core.RelayRequestAcknowledge{ThroughOrdinal: throughOrdinal, ThroughDigest: throughDigest}))
	if _, ok := relay.snapshot.Dispatches["uncertain-prompt"]; ok {
		t.Fatal("the rejected prompt is collected")
	}
	relay = rtMust(DurableRelayOpen(temp, session, "new"))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("uncertain use may have history")
	}
	rtEqual(t, relay.snapshot.NativeUseEvidence.Usage, core.NativeSessionUseUnknown)
	rtNoErr(relay.MarkNativeSessionUsed())
	rtEqual(t, relay.snapshot.NativeUseEvidence.Usage, core.NativeSessionUseUsed)
}

// rtCut is `cut.try_recv().unwrap().unwrap()`: the cut must have arrived
// without an error.
func rtCut(t *testing.T, cut *LiveBaselineReceiver) core.LiveSessionBaseline {
	t.Helper()
	result, received := cut.TryRecv()
	if !received {
		t.Fatal("the cut has not arrived")
	}
	if result.Err != nil {
		t.Fatalf("the cut was refused: %v", result.Err)
	}
	return result.Baseline
}

func rtNoCut(t *testing.T, cut *LiveBaselineReceiver) {
	t.Helper()
	if result, received := cut.TryRecv(); received {
		t.Fatalf("the cut arrived early: %#v", result)
	}
}

func rtWaitMessage(t *testing.T, relay *DurableRelay, want string) {
	t.Helper()
	message, ok := relay.liveBaselineWaitMessage()
	if !ok || !strings.Contains(message, want) {
		t.Fatalf("wait message %q (%v) does not mention %q", message, ok, want)
	}
}

func TestAdoptionCutPrecedesQueuedWorkAndDoesNotHoldAnnouncedNativeWork(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "draining-prompt", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	submitRelay(relay, "queued-followup", prompt("next"))
	// A native turn has already been announced. It must run to completion,
	// whereas the relay's own queued prompt must wait behind the cut.
	if relay.airTasks == nil {
		relay.airTasks = map[string]*AirTaskEntry{}
	}
	relay.airTasks["held-native"] = &AirTaskEntry{
		sessionID: "native",
		taskID:    "held-native",
		state:     AirTaskState{Kind: AirTaskStateKindHeldTurn},
	}
	cut := relay.ReserveLiveBaseline()
	rtNoCut(t, cut)
	finishPrompt(relay, "draining-prompt")
	rtNoCut(t, cut)
	if relay.OperationalState().HarnessTurn == nil {
		t.Fatal("the announced native turn is open")
	}
	clear(relay.airTasks)
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Autonomous: nil, Origin: rtString("agent"), PromptInFlight: false}))
	baseline := rtCut(t, cut)
	rtEqual(t, baseline.State.Execution, core.RelayExecutionStateIdle)
	rtEqual(t, baseline.Queue[0].CommandID, "queued-followup")
	frontier := baseline.State.LatestOrdinal
	rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, relay.OperationalState().ActivePrompt.CommandID, "queued-followup")
	if relay.LatestOrdinal() <= frontier {
		t.Fatal("the followup runs after the cut")
	}
}

func TestAdoptionCutHoldsQueuedShellsButAllowsCancellationAndReleasesOnDrop(t *testing.T) {
	for _, disconnect := range []bool{false, true} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		submitRelay(relay, "running-shell", core.RelayCommandRunUserShell{Command: "tail -f log"})
		rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))
		submitRelay(relay, "queued-shell", core.RelayCommandRunUserShell{Command: "printf next"})
		cut := relay.ReserveLiveBaseline()
		rtWaitMessage(t, relay, "tail -f log")
		if claimed := rtMust(relay.ClaimPendingUserShellCommandsUpTo(1)); len(claimed) != 0 {
			t.Fatalf("the cut holds queued shells: %#v", claimed)
		}
		if disconnect {
			cut.Close()
			rtNoErr(relay.resumeAfterAdoptionWait())
			if relay.liveBaselinePending() {
				t.Fatal("a dropped reservation releases the hold")
			}
			rtEqual(t, rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))[0].CommandID, "queued-shell")
		} else {
			submitRelay(relay, "cancel-running", core.RelayCommandCancelUserShell{ShellCommandID: "running-shell"})
			rtEqual(t, rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))[0].CommandID, "cancel-running")
			rtMust(relay.RecordCommandCompleted("running-shell", core.RelayCommandOutcomeUserShell{Result: successfulShell("tail -f log", "")}))
			baseline := rtCut(t, cut)
			rtEqual(t, len(baseline.State.ActiveUserShells), 1)
			rtEqual(t, baseline.State.ActiveUserShells[0].CommandID, "queued-shell")
			if baseline.State.ActiveUserShells[0].StartedAtMs != nil {
				t.Fatal("the queued shell has not started")
			}
			rtEqual(t, rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))[0].CommandID, "queued-shell")
		}
	}
}

func TestAdoptionCutWaitsForClearAndIgnoresDeadFormResponders(t *testing.T) {
	temp := t.TempDir()
	relay := clearableRelay(t, temp)
	submitRelay(relay, "clear-before-cut", core.RelayCommandClearContext{})
	cut := relay.ReserveLiveBaseline()
	rtNoCut(t, cut)
	rtWaitMessage(t, relay, "/clear")
	rtMust(relay.RecordCommandInterrupted("clear-before-cut", core.OutcomeReasonRuntimeFailure, "runtime restarted"))
	rtCut(t, cut)
	question := rtMust(core.ElicitationRequestFromACPParams("dead-question", json.RawMessage(
		`{"mode":"form","sessionId":"`+session+`","message":"Approve?","requestedSchema":{"type":"object","properties":{}}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	if questions := rtMust(relay.LiveSessionBaseline()).Questions; len(questions) != 0 {
		t.Fatalf("a dead responder's question is dropped: %#v", questions)
	}
}

func TestAdoptionCutWaitsForAnUnownedForegroundTool(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"tool_call","toolCallId":"unowned-build","title":"cargo test","status":"in_progress","kind":"execute"}`)))
	if relay.snapshot.ActivePrompt != nil || relay.snapshot.HarnessTurn != nil {
		t.Fatal("the tool has no owner")
	}
	cut := relay.ReserveLiveBaseline()
	rtNoCut(t, cut)
	rtWaitMessage(t, relay, "cargo test")
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"tool_call_update","toolCallId":"unowned-build","status":"completed"}`)))
	rtCut(t, cut)
}

func TestAdoptionWaitKeepsAutomaticSteeringQueuedUntilAfterTheCut(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	relay.SetSteeringSupported(rtBool(true))
	relay.SetAutomaticSteering(true)
	submitRelay(relay, "running-prompt", prompt("first"))
	rtMust(relay.ClaimPendingCommands(true))
	cut := relay.ReserveLiveBaseline()
	submitRelay(relay, "followup-prompt", prompt("next"))
	rtNoClaims(t, relay)
	if relay.OperationalState().Steering != nil {
		t.Fatal("the cut holds automatic steering")
	}
	finishPrompt(relay, "running-prompt")
	baseline := rtCut(t, cut)
	rtEqual(t, baseline.Queue[0].CommandID, "followup-prompt")
	rtMust(relay.ClaimPendingCommands(true))
	rtEqual(t, relay.OperationalState().ActivePrompt.CommandID, "followup-prompt")
	if relay.LatestOrdinal() <= baseline.State.LatestOrdinal {
		t.Fatal("the followup runs after the cut")
	}
}

func TestAdoptionCutWaitsForPromotedConfiguration(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "config-before-cut", setConfig("model", "chosen"))
	rtMust(relay.ClaimPendingCommands(true))
	cut := relay.ReserveLiveBaseline()
	rtNoCut(t, cut)
	rtWaitMessage(t, relay, "configuration")
	rtMust(relay.RecordCommandCompleted("config-before-cut", core.RelayCommandOutcomeConfigured{}))
	rtEqual(t, rtCut(t, cut).State.Config["model"], "chosen")
}

func TestAdoptionCutWaitsForAModeDispatchToFinish(t *testing.T) {
	for _, mode := range []struct {
		id      string
		command core.RelayCommand
		outcome core.RelayCommandOutcome
	}{
		{"mode-before-cut", core.RelayCommandSetSessionMode{ModeID: "agent"}, core.RelayCommandOutcomeSessionModeSet{}},
		{"policy-before-cut", core.RelayCommandRestoreExecutionMode{}, core.RelayCommandOutcomeConfigured{}},
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		submitRelay(relay, mode.id, mode.command)
		rtMust(relay.ClaimPendingCommands(true))
		cut := relay.ReserveLiveBaseline()
		rtNoCut(t, cut)
		rtWaitMessage(t, relay, "configuration")
		rtMust(relay.RecordCommandCompleted(mode.id, mode.outcome))
		rtCut(t, cut)
	}
}

func TestAdoptionCutWaitsForLiveTerminalsAndKeepsTheFollowupQueued(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	rtNoErr(relay.AgentTerminalStarted(core.ActiveAgentTerminal{TerminalID: "live-terminal", Command: "tail -f work.log", StartedAtMs: 1}))
	cut := relay.ReserveLiveBaseline()
	submitRelay(relay, "after-terminal", prompt("continue"))
	rtNoCut(t, cut)
	rtNoClaims(t, relay)
	rtWaitMessage(t, relay, "tail -f work.log")
	rtNoErr(relay.AgentTerminalClosed("live-terminal"))
	baseline := rtCut(t, cut)
	if len(baseline.State.ActiveAgentTerminals) != 0 {
		t.Fatalf("%#v", baseline.State.ActiveAgentTerminals)
	}
	rtEqual(t, baseline.Queue[0].CommandID, "after-terminal")
	rtMust(relay.ClaimPendingCommands(true))
	if relay.LatestOrdinal() <= baseline.State.LatestOrdinal {
		t.Fatal("the followup runs after the cut")
	}
}

func TestAdoptionCutWaitsForAGoalResumeBeforeItsTurnOpens(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "test"))
	submitRelay(relay, "resume-without-turn", core.RelayCommandGoalControl{Action: core.GoalControlActionResume})
	rtMust(relay.ClaimPendingCommands(true))
	if relay.snapshot.HarnessTurn != nil || relay.snapshot.ActivePrompt != nil {
		t.Fatal("the resume has not opened a turn")
	}
	cut := relay.ReserveLiveBaseline()
	rtNoCut(t, cut)
	if !slices.ContainsFunc(relay.adoptionBlockers(), func(blocker string) bool { return strings.Contains(blocker, "goal resume") }) {
		t.Fatalf("%v", relay.adoptionBlockers())
	}
	rtMust(relay.RecordCommandCompleted("resume-without-turn", core.RelayCommandOutcomeGoalControlled{}))
	rtCut(t, cut)
}

// rtWithRelayLock runs body under a Lock guard, as `relay.lock().unwrap()`.
func rtWithRelayLock(cell *RelayCell, body func(relay *DurableRelay)) {
	guard := cell.Lock()
	defer guard.Unlock()
	body(guard.DurableRelay)
}

// rtWithRelayObserve runs body under an Observe guard, as `relay.observe()`.
func rtWithRelayObserve(cell *RelayCell, body func(relay *DurableRelay)) {
	guard := cell.Observe()
	defer guard.Unlock()
	body(guard.DurableRelay)
}

func rtGoalSnapshotJSON(text string) *core.GoalSnapshot {
	var goal core.GoalSnapshot
	rtNoErr(json.Unmarshal([]byte(text), &goal))
	return &goal
}

// Automatic reopen and answered native resumes have no command dispatch to block a cut.
//
// Hard-won: d5c0c25: an unrelated shell cancellation released the native resume reservation.
func TestAdoptionCutWaitsForUnledgeredNativeResumeUntilItsTurnEnds(t *testing.T) {
	temp := t.TempDir()
	cell := NewRelayCell(rtMust(DurableRelayOpen(temp, session, "test")))
	pendingResumes := func() int {
		count := 0
		rtWithRelayObserve(cell, func(relay *DurableRelay) { count = len(relay.pendingNativeResumes) })
		return count
	}
	reserve := func() *LiveBaselineReceiver {
		var cut *LiveBaselineReceiver
		rtWithRelayLock(cell, func(relay *DurableRelay) { cut = relay.ReserveLiveBaseline() })
		return cut
	}
	update := func(text string) {
		rtWithRelayLock(cell, func(relay *DurableRelay) { rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(text))) })
	}
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		submitRelay(relay, "unrelated-shell", core.RelayCommandRunUserShell{Command: "tail -f previous.log"})
		rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))
	})
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		relay.nativeGoalResumeSent(1, rtGoalSnapshotJSON(`{"objective":"resume this goal","createdAt":1,"status":"paused"}`))
	})
	cut := reserve()
	rtNoCut(t, cut)
	rtWithRelayObserve(cell, func(relay *DurableRelay) {
		if !slices.ContainsFunc(relay.adoptionBlockers(), func(blocker string) bool { return strings.Contains(blocker, "goal resume") }) {
			t.Fatalf("%v", relay.adoptionBlockers())
		}
	})
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		submitRelay(relay, "unrelated-shell-cancel", core.RelayCommandCancelUserShell{ShellCommandID: "unrelated-shell"})
		rtMust(relay.ClaimPendingUserShellCommandsUpTo(1))
		rtMust(relay.RecordCommandCompleted("unrelated-shell-cancel", core.RelayCommandOutcomeUserShellCancelled{}))
		rtMust(relay.RecordCommandCompleted("unrelated-shell", core.RelayCommandOutcomeUserShell{Result: successfulShell("tail -f previous.log", "")}))
		if len(relay.pendingNativeResumes) == 0 {
			t.Fatal("an unrelated cancellation keeps the resume reservation")
		}
	})
	// Native acknowledgement does not settle the resumed turn.
	rtNoCut(t, cut)
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}))
		rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Autonomous: nil, Origin: nil, PromptInFlight: false}))
		if len(relay.pendingNativeResumes) == 0 {
			t.Fatal("an unrelated turn keeps the resume reservation")
		}
		rtNoCut(t, cut)
	})
	// Hard-won: 0c505b8: repeated paused metadata was mistaken for resume settlement.
	for _, tokensUsed := range []int{0, 10} {
		update(fmt.Sprintf(`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"resume this goal","createdAt":1,"status":"paused","tokensUsed":%d}}}`, tokensUsed))
		if pendingResumes() == 0 {
			t.Fatal("paused metadata does not settle the resume")
		}
		rtNoCut(t, cut)
	}
	update(`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"resume this goal","createdAt":1,"status":"active"},"execution":{"version":1,"status":"running","turnId":"resumed-goal"}}}`)
	rtNoCut(t, cut)
	rtEqual(t, pendingResumes(), 0)
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{PromptInFlight: false, Origin: nil, Autonomous: nil}))
	})
	rtCut(t, cut)
	// A local request error/timeout is not proof of rejection. A later goal
	// clear is authoritative and supersedes the already-sent resume.
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeSent(2, nil) })
	rejectedCut := reserve()
	rtNoCut(t, rejectedCut)
	// Hard-won: 1f0ee89: arrival after a marker did not tie idle to that resume.
	update(`{"sessionUpdate":"session_info_update","_meta":{"execution":{"version":1,"status":"idle","turnId":"resumed-goal"}}}`)
	rtWithRelayObserve(cell, func(relay *DurableRelay) {
		rtEqual(t, relay.snapshot.Goal.Execution.Status, "idle")
	})
	rtNoCut(t, rejectedCut)
	if pendingResumes() == 0 {
		t.Fatal("idle execution of the old turn does not settle the resume")
	}

	update(`{"sessionUpdate":"session_info_update","_meta":{"goal":null}}`)
	rtCut(t, rejectedCut)
	// Hard-won: 282a657: an explicit clear could not retire a resume with no known goal.
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeSent(20, nil) })
	rtWithRelayObserve(cell, func(relay *DurableRelay) {
		if relay.pendingNativeResumes[0].goal != nil {
			t.Fatal("the resume has no known goal")
		}
	})
	unknownCut := reserve()
	update(`{"sessionUpdate":"session_info_update","_meta":{"execution":{"version":1,"status":"idle"}}}`)
	rtNoCut(t, unknownCut)
	update(`{"sessionUpdate":"session_info_update","_meta":{"goal":null}}`)
	rtCut(t, unknownCut)
	// Hard-won: 655be2b: a definitive refusal must not retire another request.
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeSent(3, nil) })
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeSent(4, nil) })
	refusedCut := reserve()
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeRejected(3) })
	rtNoCut(t, refusedCut)
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeRejected(4) })
	rtCut(t, refusedCut)
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		relay.nativeGoalResumeSent(6, rtGoalSnapshotJSON(`{"objective":"new goal","createdAt":2,"status":"paused"}`))
	})
	update(`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"new goal","createdAt":2,"status":"paused"}}}`)
	deactivatedCut := reserve()
	rtNoCut(t, deactivatedCut)
	update(`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"new goal","createdAt":2,"status":"complete"}}}`)
	rtCut(t, deactivatedCut)
	rtWithRelayLock(cell, func(relay *DurableRelay) { relay.nativeGoalResumeSent(5, nil) })
	exitedCut := reserve()
	rtNoCut(t, exitedCut)
	rtWithRelayLock(cell, func(relay *DurableRelay) {
		rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	})
	rtCut(t, exitedCut)
}

// Hard-won: 64493cb: a durable ACK with an uncleared full active file prevented the first restart append.
func TestStartupCollectsAnAcknowledgedFullActiveSegmentBeforeAppending(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "old"))
	active := filepath.Join(temp, core.RelayJournalDir, core.RelayActiveSegment)
	payload := strings.Repeat("x", 64*1024)
	activeLength := func() uint64 {
		info, err := os.Stat(active)
		if err != nil {
			return 0
		}
		return uint64(info.Size())
	}
	for activeLength() < core.RelaySegmentByteLimit {
		rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: payload}))
	}
	frontier := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	// Crash between the durable ACK and physical active-file replacement.
	relay.snapshot.AcknowledgedThrough = frontier
	relay.snapshot.AcknowledgedDigest = digest
	rtNoErr(relay.persistSnapshot())

	relay = rtMust(DurableRelayOpen(temp, session, "new"))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	rtEqual(t, relay.LatestOrdinal(), frontier+1)
	relay = rtMust(DurableRelayOpen(temp, session, "newer"))
	events := rtMust(relay.EventsAfter(frontier, digest))
	rtEqual(t, len(events), 1)
	if _, ok := events[0].Observation.(core.RelayObservationSessionRestarted); !ok {
		t.Fatalf("%#v", events[0].Observation)
	}
}

// Hard-won: 7a9a7ec: legacy duplicate sealed files lost cleanup eligibility before a later ACK covered them.
func TestLaterAcknowledgementsCollectLegacyRewriteDuplicatesWithoutRestarting(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "legacy"))
	recordPagedHistory(t, relay, 80)
	spans := slices.Clone(relay.journalSpans)
	var sealed []string
	for _, span := range spans {
		if filepath.Ext(span.path) == ".gz" {
			sealed = append(sealed, span.path)
		}
	}
	if len(sealed) < 2 {
		t.Fatalf("only %d sealed segments", len(sealed))
	}
	events := rtMust(relay.EventsAfter(0, core.RelayEventGenesisDigest))
	partial := spans[0].fileLastOrdinal - 1
	partialDigest := relayTestDigestAt(relay, partial)
	frontier := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	relay.snapshot.AcknowledgedThrough = partial
	relay.snapshot.AcknowledgedDigest = partialDigest
	rtNoErr(relay.persistSnapshot())
	// A legacy rewrite installed its complete replacement active file and
	// crashed before deleting the sealed originals it had copied.
	var replacement []byte
	for _, event := range events {
		replacement = append(replacement, rtMust(json.Marshal(event))...)
		replacement = append(replacement, '\n')
	}
	rtNoErr(os.WriteFile(filepath.Join(temp, core.RelayJournalDir, core.RelayActiveSegment), replacement, 0o600))
	relay = rtMust(DurableRelayOpen(temp, session, "current"))
	rtEqual(t, len(relay.journalSpans), 1)
	if !relay.journalCleanupPending {
		t.Fatal("the duplicates await cleanup")
	}
	for _, path := range sealed {
		if _, err := os.Stat(path); err != nil {
			t.Fatalf("a duplicate was removed before an ACK covered it: %v", err)
		}
	}
	firstEnd := spans[0].fileLastOrdinal
	acknowledgeRelay(relay, "first-duplicate-covered", firstEnd)
	if _, err := os.Stat(sealed[0]); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("the covered duplicate is collected: %v", err)
	}
	if !relay.journalCleanupPending {
		t.Fatal("later duplicates still await cleanup")
	}
	acknowledgeRelay(relay, "all-duplicates-covered", frontier)
	for _, path := range sealed {
		if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("a covered duplicate is collected: %v", err)
		}
	}
	if relay.journalCleanupPending {
		t.Fatal("no cleanup is left")
	}
	if events := rtMust(relay.EventsAfter(frontier, digest)); len(events) != 0 {
		t.Fatalf("%#v", events)
	}
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	relay = rtMust(DurableRelayOpen(temp, session, "reopened"))
	rtEqual(t, len(rtMust(relay.EventsAfter(frontier, digest))), 1)
}

// rtJudgeEvidence is the turn evidence a judge reads, with default facts.
func rtJudgeEvidence(relay *DurableRelay, harness core.HarnessKind, phase core.TurnPhase) core.TurnEvidence {
	facts := core.DefaultActivityFacts()
	return relay.TurnContext().Evidence(harness, phase, &facts, 0)
}

func rtFormQuestion(id, message string) core.ElicitationRequest {
	return rtMust(core.ElicitationRequestFromACPParams(id, json.RawMessage(
		`{"mode":"form","sessionId":"`+session+`","message":"`+message+`","requestedSchema":{"type":"object","properties":{}}}`)))
}

// Hard-won: b4985a8: SessionOpened discarded a new bridge's still-live startup form.
func TestFormsOpenedDuringBridgeStartupSurviveSessionOpenAndAckPruning(t *testing.T) {
	for _, resumed := range []bool{false, true} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "test"))
		rtMust(relay.RecordObservation(core.RelayObservationElicitationRequested{Request: rtFormQuestion("old-runtime", "Approve?")}))
		rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
		live := rtFormQuestion("new-opening-runtime", "Approve?")
		rtMust(relay.RecordObservation(core.RelayObservationElicitationRequested{Request: live}))
		opened := rtOpenedSession("new-native")
		opened.Resumed = resumed
		rtMust(relay.RecordObservation(opened))
		projection := rtProjectSession(relay)
		rtEqual(t, projection.PendingElicitations, []core.ElicitationRequest{live})
		pending, ok := relay.snapshot.PendingElicitations[live.ID]
		if !ok || pending == nil {
			t.Fatalf("the live form is pending: %#v", relay.snapshot.PendingElicitations)
		}
		rtEqual(t, *pending, live)
		frontier := relay.LatestOrdinal()
		acknowledgeRelay(relay, "collect-startup", frontier)
		relay = rtMust(DurableRelayOpen(temp, session, "reopened"))
		pending, ok = relay.snapshot.PendingElicitations[live.ID]
		if !ok || pending == nil {
			t.Fatalf("the live form survives pruning: %#v", relay.snapshot.PendingElicitations)
		}
		rtEqual(t, *pending, live)
		// Reopening does not resurrect the old bridge's responder.
		if questions := rtMust(relay.LiveSessionBaseline()).Questions; len(questions) != 0 {
			t.Fatalf("%#v", questions)
		}
		rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
		if questions := rtMust(relay.LiveSessionBaseline()).Questions; len(questions) != 0 {
			t.Fatalf("%#v", questions)
		}
	}
}

// Hard-won: bd33e3913: pruning a queued prompt lost its later delivery boundary on replay.
func TestReplayedDeliveryWithoutQueueEventResetsThePreviousReply(t *testing.T) {
	for _, replay := range []struct{ steered, finished, replied bool }{
		{false, false, false},
		{true, false, false},
		{false, true, false},
		{false, true, true},
	} {
		temp := t.TempDir()
		relay := rtMust(DurableRelayOpen(temp, session, "first-turn"))
		submitRelay(relay, "first-prompt", prompt("first request"))
		rtMust(relay.ClaimPendingCommands(true))
		submitRelay(relay, "second-prompt", prompt("second request"))
		through := relay.LatestOrdinal()
		acknowledgeRelay(relay, "collect-queue", through)
		rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first answer"}}`)))
		if replay.steered {
			submitRelay(relay, "confirm-correction", core.RelayCommandCancel{})
			rtMust(relay.ClaimPendingCommands(true))
			rtMust(relay.RecordCommandCompleted("confirm-correction", core.RelayCommandOutcomeSteered{QueuedCommandID: "second-prompt"}))
			if _, ok := relay.snapshot.Dispatches["second-prompt"]; ok {
				t.Fatal("the steered prompt is delivered")
			}
		} else {
			finishPrompt(relay, "first-prompt")
			rtMust(relay.ClaimPendingCommands(true))
			rtEqual(t, relay.snapshot.ActivePrompt.CommandID, "second-prompt")
		}
		// Hard-won: c624e4aba: a completed prompt lost its boundary with its dispatch.
		if replay.finished {
			if replay.replied {
				rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second answer"}}`)))
			}
			finishPrompt(relay, "second-prompt")
			if _, ok := relay.snapshot.Dispatches["second-prompt"]; ok {
				t.Fatal("the finished prompt is collected")
			}
		}
		relay = rtMust(DurableRelayOpen(temp, session, "second-turn"))
		expectedTail := ""
		if replay.replied {
			expectedTail = "second answer"
		}
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).UserPromptTail, "second request")
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, expectedTail)
		rtMust(relay.RecordObservation(rtSessionConfigured()))
		rtMust(relay.ClaimPendingCommands(true))
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, expectedTail)
	}
}

// rtFindMessage is the authorization message with the ID.
func rtFindMessage(t *testing.T, history *core.ContextHistory, id string) core.EvidenceMessage {
	t.Helper()
	for _, message := range history.Messages {
		if message.ID == id {
			return message
		}
	}
	t.Fatalf("no message %s in %#v", id, history.Messages)
	return core.EvidenceMessage{}
}

// Hard-won: bd33e3913: a recovery question inherited the interrupted reply's explicit ID.
func TestRestartContinuationStartsANewAuthorizationMessageForAnUnlabelledQuestion(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "interrupted"))
	submitRelay(relay, "original", prompt("do the work"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"interrupted-reply","content":{"type":"text","text":"working"}}`)))
	relay = rtMust(DurableRelayOpen(temp, session, "recovery"))
	rtMust(relay.RecordObservation(rtSessionConfigured()))
	recovery := rtMust(relay.ClaimPendingCommands(true))
	if !core.IsRestartContinuation(recovery[0].CommandID) {
		t.Fatalf("unexpected recovery %s", recovery[0].CommandID)
	}
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Which option?"}}`)))
	// Hard-won: 798a9157c: the auxiliary tail still inherited the old explicit message ID.
	rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, "Which option?")
	history := relay.snapshot.AuthorizationContext()
	question := history.Messages[len(history.Messages)-1]
	rtEqual(t, question.Text, "Which option?")
	if question.ID == "interrupted-reply" {
		t.Fatal("the question has its own message")
	}
	rtEqual(t, rtFindMessage(t, history, "interrupted-reply").Text, "working")
	// Hard-won: c624e4aba: a completed continuation has no queue event or dispatch to replay.
	finishPrompt(relay, recovery[0].CommandID)
	relay = rtMust(DurableRelayOpen(temp, session, "completed-recovery"))
	rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseReplied).AssistantTextTail, "Which option?")
}

// Hard-won: 7211dfd7b: an autonomous unlabelled reply inherited a completed prompt's explicit ID.
func TestAutonomousReplyAfterCompletedPromptHasItsOwnJudgmentTail(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "human-reply"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	submitRelay(relay, "human-prompt", prompt("do the work"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"human-reply","content":{"type":"text","text":"finished human work"}}`)))
	finishPrompt(relay, "human-prompt")
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"autonomous reply"}}`)))
	if relay.snapshot.HarnessTurn == nil {
		t.Fatal("the reply opens a harness turn")
	}
	check := func(relay *DurableRelay) {
		t.Helper()
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindClaude, core.TurnPhaseRunning).AssistantTextTail, "autonomous reply")
	}
	check(relay)
	relay = rtMust(DurableRelayOpen(temp, session, "autonomous-reopen"))
	check(relay)
}

// Hard-won: c82699f02: Codex native markers split the reply of a still-running ACP prompt.
func TestPromptedReplySurvivesNativeStartAndSettleMarkers(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "prompted-native"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
	submitRelay(relay, "prompt-native-turn", prompt("answer this"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"prompt-reply","content":{"type":"text","text":"before start; "}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 100}))
	if relay.snapshot.HarnessTurn.Autonomous {
		t.Fatal("a prompt owns the native turn")
	}
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after start; "}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Autonomous: nil, Origin: nil, PromptInFlight: true}))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after settle"}}`)))
	check := func(relay *DurableRelay) {
		t.Helper()
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, "before start; after start; after settle")
	}
	check(relay)
	relay = rtMust(DurableRelayOpen(temp, session, "prompted-native-reopen"))
	check(relay)
}

// Hard-won: e06c07a69: restarted autonomous turns lost their start proofs and merged on replay.
func TestBridgeRestartsKeepInterruptedAutonomousRepliesSeparateOnReopen(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "autonomous-restarts"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"first-autonomous-reply","content":{"type":"text","text":"first reply"}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second reply"}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationSessionRestarted{}))
	if relay.snapshot.HarnessTurn != nil {
		t.Fatal("a restart ends the turn")
	}
	if rtCountObservations[core.RelayObservationHarnessTurnSettled](observations(t, relay)) != 0 {
		t.Fatal("a restart settles nothing")
	}
	check := func(relay *DurableRelay) {
		t.Helper()
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindClaude, core.TurnPhaseReplied).AssistantTextTail, "second reply")
	}
	check(relay)
	history := relay.snapshot.AuthorizationContext()
	rtEqual(t, history.Messages[len(history.Messages)-1].Text, "second reply")
	rtEqual(t, rtFindMessage(t, history, "first-autonomous-reply").Text, "first reply")
	relay = rtMust(DurableRelayOpen(temp, session, "autonomous-restarts-reopen"))
	check(relay)
}

// Hard-won: a72c7b7fd: ACP completion split a reply whose native goal was still running.
func TestNativeGoalReplySpansAcpCompletionAndReopen(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "native-acp-completion"))
	relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
	submitRelay(relay, "native-goal-prompt", prompt("keep working"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionInfoMeta(`{"execution":{"version":1,"status":"running","turnId":"native-goal"}}`)))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"native-goal-reply","content":{"type":"text","text":"before completion; "}}`)))
	finishPrompt(relay, "native-goal-prompt")
	if !relay.snapshot.Goal.Running() {
		t.Fatal("the native goal is still running")
	}
	rtEqual(t, relay.snapshot.Execution, core.RelayExecutionStateRunning)
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after completion"}}`)))
	check := func(relay *DurableRelay) {
		t.Helper()
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, "before completion; after completion")
		history := relay.snapshot.AuthorizationContext()
		reply := history.Messages[len(history.Messages)-1]
		rtEqual(t, reply.ID, "native-goal-reply")
		rtEqual(t, reply.Text, "before completion; after completion")
	}
	check(relay)
	relay = rtMust(DurableRelayOpen(temp, session, "native-acp-completion-reopen"))
	check(relay)
}

// Hard-won: ddbed432e: an unlabelled response appended to the elicitation's answered question.
func TestAnsweredElicitationStartsANewAuxiliaryReplyBeforeUnlabelledChunks(t *testing.T) {
	temp := t.TempDir()
	relay := rtMust(DurableRelayOpen(temp, session, "elicitation-answer"))
	submitRelay(relay, "elicitation-prompt", prompt("continue the task"))
	rtMust(relay.ClaimPendingCommands(true))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","messageId":"question-message","content":{"type":"text","text":"Which option?"}}`)))
	rtMust(relay.RecordObservation(core.RelayObservationElicitationRequested{Request: rtFormQuestion("option-question", "Which option?")}))
	rtMust(relay.RecordObservation(core.RelayObservationElicitationResolved{
		ElicitationID: "option-question",
		Action:        "accept",
		Reply:         rtString("implement"),
	}))
	rtMust(relay.RecordSessionUpdate(rtSessionUpdateJSON(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Implementing the selected option."}}`)))
	check := func(relay *DurableRelay) {
		t.Helper()
		rtEqual(t, rtJudgeEvidence(relay, core.HarnessKindCodex, core.TurnPhaseRunning).AssistantTextTail, "Implementing the selected option.")
	}
	check(relay)
	relay = rtMust(DurableRelayOpen(temp, session, "elicitation-answer-reopen"))
	check(relay)
}

// rtCloneTurnCompletion copies a published turn completion, as Rust's
// clone(), so a later in-place change cannot reach the copy.
func rtCloneTurnCompletion(completion *core.TurnCompletion) *core.TurnCompletion {
	if completion == nil {
		return nil
	}
	copied := *completion
	return &copied
}
