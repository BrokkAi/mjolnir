// Port of the tests in mj-core/src/acp.rs.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"testing"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/schema"
)

// acpReloadError is a reload failure as the worker sees it: the adapter's
// JSON-RPC error under the worker's own context.
func acpReloadError(id string, code int, message, data string) error {
	return fmt.Errorf("resume ACP session %s: %w", id, &acp.RPCError{Code: code, Message: message, Data: json.RawMessage(data)})
}

func TestCodexReportsAMissingThreadThroughTheWrappedAdapterMessage(t *testing.T) {
	// What codex-acp actually sends back for a thread with no rollout.
	missing := acpReloadError("0199f0ba", -32603, "Internal error", `{"details": "no rollout found for thread id 0199f0ba"}`)
	if !ErrorReportsMissingNativeSession(HarnessKindCodex, "0199f0ba", missing) {
		t.Fatal("the missing thread is not recognized")
	}
	// Anything else keeps failing the resume loudly.
	if ErrorReportsMissingNativeSession(HarnessKindCodex, "0199f0ba",
		errors.New("resume ACP session 0199f0ba: Internal error: session store is locked")) {
		t.Fatal("an unrelated failure reads as a missing thread")
	}
}

// Hard-won: 710ffb6: an unused Codex thread was misclassified as used and restart recovery failed.
func TestCodexReportsAThreadItNeverWroteAsThreadNotFound(t *testing.T) {
	// What codex-acp sent when a same-build worker reloaded a thread that
	// was created and never prompted (launch finding I2-7).
	err := errors.New(`load ACP session 01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f: Internal error: {"details": "thread not found: 01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f"}`)
	if !ErrorReportsMissingNativeSession(HarnessKindCodex, "01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f", err) {
		t.Fatal("the never-written thread is not recognized")
	}
	// A missing thread other than the one being resumed is not ours.
	if ErrorReportsMissingNativeSession(HarnessKindCodex, "0f0f0f0f-0000-4000-8000-000000000000", err) {
		t.Fatal("another thread reads as ours")
	}
}

func TestClaudeReportsAMissingSessionOnlyForTheSessionBeingResumed(t *testing.T) {
	id := "7ee4c940-f82c-4c6a-847f-47e21675e585"
	// What claude-agent-acp sends back for a session with no transcript on
	// disk: ACP SDK `sessionNotFound` from 0.99, `resourceNotFound` before.
	missing := []error{
		acpReloadError(id, -32002, "Resource not found", fmt.Sprintf(`{"sessionId": %q}`, id)),
		acpReloadError(id, -32002, "Resource not found: "+id, fmt.Sprintf(`{"uri": %q}`, id)),
	}
	for _, missing := range missing {
		if !ErrorReportsMissingNativeSession(HarnessKindClaude, id, missing) {
			t.Fatalf("%v is not recognized", missing)
		}
		// A resource error about anything else must never replace the session.
		if ErrorReportsMissingNativeSession(HarnessKindClaude, "0f0f0f0f-0000-4000-8000-000000000000", missing) {
			t.Fatalf("%v reads as another session", missing)
		}
		// Each harness only recognizes its own answer.
		if ErrorReportsMissingNativeSession(HarnessKindCodex, id, missing) {
			t.Fatalf("%v reads as a Codex thread", missing)
		}
	}
	// The same session named under another error code is a different failure.
	if ErrorReportsMissingNativeSession(HarnessKindClaude, id,
		acpReloadError(id, -32603, "Internal error", fmt.Sprintf(`{"sessionId": %q}`, id))) {
		t.Fatal("an internal error reads as a missing session")
	}
	if ErrorReportsMissingNativeSession(HarnessKindClaude, id, fmt.Errorf("resume ACP session %s: connection closed", id)) {
		t.Fatal("a closed connection reads as a missing session")
	}
}

func TestHarnessesThatAlwaysMaterializeASessionNeverReportOneMissing(t *testing.T) {
	for _, harness := range []HarnessKind{HarnessKindKimi, HarnessKindGrok, HarnessKindMuse, HarnessKindOpenCode} {
		if ErrorReportsMissingNativeSession(harness, "native",
			acpReloadError("native", -32002, "Resource not found", `{"sessionId": "native"}`)) {
			t.Fatalf("%v reports a missing session", harness)
		}
		if ErrorReportsMissingNativeSession(harness, "native", errors.New("no rollout found for thread id native")) {
			t.Fatalf("%v reports a missing thread", harness)
		}
	}
}

func acpUpdate(t *testing.T, text string) *schema.SessionUpdate {
	t.Helper()
	var update schema.SessionUpdate
	if err := json.Unmarshal([]byte(text), &update); err != nil {
		t.Fatalf("session update fixture %s: %v", text, err)
	}
	return &update
}

// What counts as the harness answering a prompt (#970).

// Hard-won: f549b59: harness-owned ACP updates masked swallowed prompts and made the worker treat them as answered.
func TestTrafficTheHarnessEmitsOnItsOwnIsNotAnAnswer(t *testing.T) {
	for _, value := range []string{
		`{"sessionUpdate": "available_commands_update", "availableCommands": []}`,
		`{"sessionUpdate": "current_mode_update", "currentModeId": "default"}`,
		`{"sessionUpdate": "config_option_update", "configOptions": []}`,
		`{"sessionUpdate": "session_info_update"}`,
		`{"sessionUpdate": "usage_update", "used": 12, "size": 100}`,
	} {
		if SessionUpdateIsAgentOutput(acpUpdate(t, value)) {
			t.Fatalf("%s must not count as the agent answering", value)
		}
	}
}

// A turn that only ran tools answered the prompt. Text is not the test: an
// agent that edits a file and says nothing has still done the work.
func TestToolCallsAndTextBothCountAsAnAnswer(t *testing.T) {
	for _, value := range []string{
		`{"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "Edit README.md"}`,
		`{"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed"}`,
		`{"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done"}}`,
		`{"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "thinking"}}`,
	} {
		if !SessionUpdateIsAgentOutput(acpUpdate(t, value)) {
			t.Fatalf("%s must count as the agent answering", value)
		}
	}
}

// The exact shape issue #970 reported: the bridge streams its compaction
// progress into the swallowed prompt's turn, so counting every message would
// hide the loss.
// Hard-won: a1d7b48: Claude compaction progress banners masked swallowed prompts as completed answers.
func TestCompactionProgressTextIsNotAnAnswer(t *testing.T) {
	for _, text := range []string{
		"Compacting...",
		"\n\nCompacting completed.",
		"Compacting failed: out of memory.",
	} {
		encoded, _ := json.Marshal(text)
		banner := acpUpdate(t, `{"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": `+string(encoded)+`}}`)
		if !SessionUpdateIsCompactionBanner(banner) || SessionUpdateIsAgentOutput(banner) {
			t.Fatalf("%q", text)
		}
	}
	answer := acpUpdate(t, `{"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Compacting the loop saves two allocations."}}`)
	if SessionUpdateIsCompactionBanner(answer) || !SessionUpdateIsAgentOutput(answer) {
		t.Fatal("an answer about compacting reads as a banner")
	}
}

func TestMaintenanceParserMatchesWholeCommandNames(t *testing.T) {
	if command, args, ok := ContextCommandText(" /CLEAR \n"); !ok || command != ContextCommandClear || args != "" {
		t.Fatalf("clear = (%v, %q, %v)", command, args, ok)
	}
	if command, args, ok := ContextCommandText("/compact preserve decisions"); !ok || command != ContextCommandCompact || args != "preserve decisions" {
		t.Fatalf("compact = (%v, %q, %v)", command, args, ok)
	}
	if _, _, ok := ContextCommandText("/compactfoo"); ok {
		t.Fatal("/compactfoo matched")
	}
	if _, _, ok := ContextCommandText("please /clear"); ok {
		t.Fatal("please /clear matched")
	}
}
