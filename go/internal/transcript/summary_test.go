// Port of the tests in mj-transcript/src/summary.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func summaryTestCall(n int) schema.ToolCall {
	kind := schema.ToolKindExecute
	status := schema.ToolCallStatusCompleted
	return schema.ToolCall{
		ToolCallID: schema.ToolCallId(fmt.Sprintf("call-%d", n)),
		Title:      "Run command",
		Kind:       &kind,
		Status:     &status,
		RawInput:   transcriptTestJSON(map[string]any{"command": fmt.Sprintf("cargo test --marker=ARGUMENT_%d", n)}),
		RawOutput:  transcriptTestJSON(map[string]any{"exit_code": n, "formatted_output": fmt.Sprintf("OUTPUT_%d", n)}),
	}
}

func summaryToolCallUpdate(call schema.ToolCall) *schema.SessionUpdate {
	return &schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call}
}

func summaryDecodeUpdate(t *testing.T, text string) *schema.SessionUpdate {
	t.Helper()
	var update schema.SessionUpdate
	if err := json.Unmarshal([]byte(text), &update); err != nil {
		t.Fatalf("decode %s: %v", text, err)
	}
	return &update
}

func TestLegacyPersistedIndexTitleUsesItsStableFallbackID(t *testing.T) {
	fixture, err := os.ReadFile(filepath.Join("testdata", "tests", "fixtures", "legacy-indexed-tool-call.txt"))
	if err != nil {
		t.Fatal(err)
	}
	legacyTitle := strings.TrimRightFunc(string(fixture), unicode.IsSpace)

	call, terminals := IndexedToolCall(legacyTitle, "legacy-tool-17")

	want, _ := jsonvalue.Parse([]byte(`{"toolCallId": "legacy-tool-17", "title": "Terminal", "status": "completed"}`))
	got, err := jsonvalue.Parse(call)
	if err != nil || !jsonValueEqual(got, want) {
		t.Fatalf("call %s", call)
	}
	if len(terminals) != 0 {
		t.Fatalf("terminals %v", terminals)
	}
}

func TestNinthCallDemotesFirstAndLateUpdatesDoNotPromoteIt(t *testing.T) {
	var summary TranscriptSummary
	summary.PushUser("test the change")
	for n := range 9 {
		summary.Observe(summaryToolCallUpdate(summaryTestCall(n)))
	}
	failed := schema.ToolCallStatusFailed
	summary.Observe(&schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &schema.ToolCallUpdate{
		ToolCallID: "call-0",
		Status:     &failed,
		RawOutput:  json.RawMessage(`{"exit_code": 42,"output":"LATE_PAYLOAD"}`),
	}})
	output := summary.Render(DefaultSummaryBytes)
	for _, absent := range []string{"ARGUMENT_0", "OUTPUT_0", "LATE_PAYLOAD"} {
		if strings.Contains(output, absent) {
			t.Fatalf("output contains %s:\n%s", absent, output)
		}
	}
	if !strings.Contains(output, "cargo test [failed] exit=42") {
		t.Fatalf("output lacks the late outcome:\n%s", output)
	}
	for n := 1; n < 9; n++ {
		if !strings.Contains(output, fmt.Sprintf("ARGUMENT_%d", n)) || !strings.Contains(output, fmt.Sprintf("OUTPUT_%d", n)) {
			t.Fatalf("output lacks call %d:\n%s", n, output)
		}
	}
	full := 0
	for _, entry := range summary.Entries {
		if _, ok := entry.Tool.Get("call"); ok {
			full++
		}
	}
	if full != FullToolCalls {
		t.Fatalf("%d full calls", full)
	}
}

func TestHugeUnicodeBodiesAreMarkedAndPreserveRecentOutcomes(t *testing.T) {
	var summary TranscriptSummary
	summary.PushUser("latest user requirement")
	for n := range 8 {
		call := summaryTestCall(n)
		call.RawOutput = transcriptTestJSON(map[string]any{"exit_code": n, "output": strings.Repeat("🦀\"\\\n", 70_000)})
		summary.Observe(summaryToolCallUpdate(call))
	}
	output := summary.Render(16 * 1024)
	if len(output) > 16*1024 {
		t.Fatalf("output is %d bytes", len(output))
	}
	if !strings.Contains(output, "bytes omitted") || !strings.Contains(output, "latest user requirement") {
		t.Fatalf("output:\n%s", output)
	}
	for n := range 8 {
		if !strings.Contains(output, fmt.Sprintf("exit=%d", n)) {
			t.Fatalf("output lacks exit=%d", n)
		}
	}
	for limit := range 100 {
		if excerpt := Excerpt(strings.Repeat("🦀", 100), limit); len(excerpt) > limit {
			t.Fatalf("excerpt of %d bytes for limit %d", len(excerpt), limit)
		}
	}
}

func TestContextBoundaryDiscardsPreviousConversation(t *testing.T) {
	session := core.MaterializedSessionEmpty("summary-test")
	for _, row := range []struct {
		position uint64
		id       string
		body     core.TranscriptBody
	}{
		{1, "user:old", core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"OLD_CONTEXT"}`)}}},
		{2, "context-clear:test", core.TranscriptBodySystem{Text: "cleared"}},
		{3, "user:new", core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"NEW_CONTEXT"}`)}}},
	} {
		id := row.id
		if row.position == 2 {
			id = core.ContextBoundaryPrefix + "test"
		}
		session.Transcript = append(session.Transcript, &core.TranscriptItem{
			StableID: id,
			Position: row.position,
			Body:     row.body,
		})
	}
	summary := TranscriptSummaryFromMaterialized(&session)
	output := summary.Render(4096)
	if strings.Contains(output, "OLD_CONTEXT") || !strings.Contains(output, "NEW_CONTEXT") {
		t.Fatalf("output:\n%s", output)
	}
}

func TestTerminalOutputAndExitAreAttachedAndPartialUpdatesKeepName(t *testing.T) {
	var summary TranscriptSummary
	summary.Observe(summaryDecodeUpdate(t, `{"sessionUpdate":"tool_call","toolCallId":"terminal","title":"Run command","kind":"execute","status":"in_progress","rawInput":{"command":"cargo test --secret argument"},"content":[{"type":"terminal","terminalId":"term-1"}]}`))
	exit := uint32(17)
	summary.ObserveTerminal(&core.TerminalOutputRecord{
		TerminalID: "term-1",
		Output:     core.NewPackedText("terminal result"),
		ExitCode:   &exit,
	})
	title := "Done"
	failed := schema.ToolCallStatusFailed
	summary.Observe(&schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &schema.ToolCallUpdate{
		ToolCallID: "terminal",
		Title:      &title,
		Status:     &failed,
	}})
	output := summary.Render(4096)
	if !strings.Contains(output, "cargo test [failed] exit=17") || !strings.Contains(output, "terminal result") {
		t.Fatalf("output:\n%s", output)
	}
}

func TestDemotedTerminalKeepsLateOutcomeWithoutItsBody(t *testing.T) {
	var summary TranscriptSummary
	summary.Observe(summaryDecodeUpdate(t, `{"sessionUpdate":"tool_call","toolCallId":"terminal","title":"cargo test","kind":"execute","status":"in_progress","content":[{"type":"terminal","terminalId":"term"}]}`))
	for n := range 8 {
		summary.Observe(summaryToolCallUpdate(summaryTestCall(n)))
	}
	exit := uint32(23)
	summary.ObserveTerminal(&core.TerminalOutputRecord{
		TerminalID: "term",
		Output:     core.NewPackedText("OLD_TERMINAL_PAYLOAD"),
		ExitCode:   &exit,
	})
	output := summary.Render(DefaultSummaryBytes)
	if !strings.Contains(output, "exit=23") || strings.Contains(output, "OLD_TERMINAL_PAYLOAD") {
		t.Fatalf("output:\n%s", output)
	}
}

func TestMalformedOlderCallsDoNotLeakTheirTitlesOrPayloads(t *testing.T) {
	invalid := json.RawMessage(`{"title":"SECRET_TITLE", "rawInput":"SECRET_INPUT", "status":"completed"}`)
	compact := string(mustMarshalJSONValue(toolValue(invalid, nil, false)))
	if !strings.Contains(compact, "invalid tool call") || strings.Contains(compact, "SECRET") {
		t.Fatalf("compact %s", compact)
	}
}
