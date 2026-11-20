// Port of mj-worker/src/relay/tests.rs.

package worker

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func initialFifoCommands() []core.InitialWorkerCommand {
	panic("unported: mj-worker/src/relay/tests.rs initial_fifo_commands")
}

func legacyStartupWithPrunedReceipt(t *testing.T, clear, omitWatermark, removed bool) {
	panic("unported: mj-worker/src/relay/tests.rs legacy_startup_with_pruned_receipt")
}

// relaySteeringAutomatically is a relay whose bridge advertises steering,
// with `prompt-first` running.
func relaySteeringAutomatically(t *testing.T, root string) *DurableRelay {
	panic("unported: mj-worker/src/relay/tests.rs relay_steering_automatically")
}

func runningClaudeTurn(t *testing.T, root string, hookAvailable bool) *DurableRelay {
	panic("unported: mj-worker/src/relay/tests.rs running_claude_turn")
}

func queuePrompt(t *testing.T, relay *DurableRelay, commandID, text string) {
	panic("unported: mj-worker/src/relay/tests.rs queue_prompt")
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
	panic("unported: mj-worker/src/relay/tests.rs record_paged_history")
}

func attachEnvelope(t *testing.T, relay *DurableRelay, requestID string, afterOrdinal uint64) core.RelayRequestEnvelope {
	panic("unported: mj-worker/src/relay/tests.rs attach_envelope")
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

// relayTestsClaudeRelay builds a relay that models the turns Claude Code
// starts on its own. Rust's claude_relay; prefixed because
// relay/verdict.rs's settled_task_tests has its own claude_relay.
func relayTestsClaudeRelay(t *testing.T, root string) *DurableRelay {
	panic("unported: mj-worker/src/relay/tests.rs claude_relay")
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
	panic("unported: mj-worker/src/relay/tests.rs observations")
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
	panic("unported: mj-worker/src/relay/tests.rs kimi_relay_with_prompt_in_flight")
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
	panic("unported: mj-worker/src/relay/tests.rs claude_relay_with_stoppable_task")
}

func assertChunkOpenedAHarnessTurn(t *testing.T, relay *DurableRelay) {
	panic("unported: mj-worker/src/relay/tests.rs assert_chunk_opened_a_harness_turn")
}

func clearableRelay(t *testing.T, root string) *DurableRelay {
	panic("unported: mj-worker/src/relay/tests.rs clearable_relay")
}

// projectedTranscript is the transcript a relay's events project to.
func projectedTranscript(t *testing.T, relay *DurableRelay) []core.TranscriptItem {
	panic("unported: mj-worker/src/relay/tests.rs projected_transcript")
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
	panic("unported: mj-worker/src/relay/tests.rs request_mailbox_drain_for_test")
}

// drainPostToolBatch returns the drained text, the leased count and the
// lease ID.
func drainPostToolBatch(t *testing.T, relay *DurableRelay) (*string, int, *string) {
	panic("unported: mj-worker/src/relay/tests.rs drain_post_tool_batch")
}

func acknowledgeMailboxForTest(t *testing.T, relay *DurableRelay, requestID, leaseID string) {
	panic("unported: mj-worker/src/relay/tests.rs acknowledge_mailbox_for_test")
}

func drainMailboxForTest(t *testing.T, relay *DurableRelay, requestID string) (*string, int) {
	t.Helper()
	text, count, leaseID := requestMailboxDrainForTest(t, relay, requestID)
	if leaseID != nil {
		acknowledgeMailboxForTest(t, relay, requestID+"-ack", *leaseID)
	}
	return text, count
}
