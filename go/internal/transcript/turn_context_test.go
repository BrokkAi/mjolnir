// Port of the tests in mj-transcript/src/turn_context.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func turnContextMessage(t *testing.T, id, text string) *schema.SessionUpdate {
	t.Helper()
	return summaryDecodeUpdate(t, string(transcriptTestJSON(map[string]any{
		"sessionUpdate": "agent_message_chunk",
		"messageId":     id,
		"content":       map[string]any{"type": "text", "text": text},
	})))
}

func turnContextTool(t *testing.T, title string) *schema.SessionUpdate {
	t.Helper()
	return summaryDecodeUpdate(t, string(transcriptTestJSON(map[string]any{
		"sessionUpdate": "tool_call",
		"toolCallId":    title,
		"title":         title,
		"status":        "in_progress",
	})))
}

func turnContextEvidenceJSON(t *testing.T, evidence core.TurnEvidence) string {
	t.Helper()
	encoded, err := json.Marshal(evidence)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func turnContextStart(id string) core.RelayObservation {
	return core.RelayObservationCommandStarted{CommandID: id}
}

func turnContextText(text string) *string { return &text }

func TestDiagnosticLoggingDoesNotChangeEvidenceOrGenerationAndSurvivesReset(t *testing.T) {
	context := &TurnContext{}
	context.Reset(nil, "Implement the parser")
	generation := context.Generation()
	before := turnContextEvidenceJSON(t, context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &core.ActivityFacts{}, 0))
	log, err := core.DecisionLogOpen(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer log.Close()
	context.SetDecisionLog(log)
	attempt := log.Start("s", "activity", "Who acts?", "Current request")
	attempt.Finish("unchanged", "Kept runtime facts")
	if context.Generation() != generation {
		t.Fatalf("generation moved from %d to %d", generation, context.Generation())
	}
	if after := turnContextEvidenceJSON(t, context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &core.ActivityFacts{}, 0)); after != before {
		t.Fatalf("evidence changed:\n%s\n%s", before, after)
	}
	context.Reset(nil, "New work")
	if _, ok := context.DecisionLog(); !ok {
		t.Fatal("the decision log did not survive the reset")
	}
}

func TestVerdictUsesLatestDeliveredUserWithoutToolHistoryButKeepsLiveFacts(t *testing.T) {
	context := &TurnContext{}
	context.ObserveRelay(turnContextStart("old"), turnContextText("OLD REQUEST"))
	context.Observe(turnContextMessage(t, "old", "OLD ANSWER"))
	context.Observe(turnContextTool(t, "active-build"))
	context.ObserveRelay(turnContextStart("new"), turnContextText("CURRENT REQUEST"))
	noisy := summaryDecodeUpdate(t, string(transcriptTestJSON(map[string]any{
		"sessionUpdate": "tool_call", "toolCallId": "noisy", "title": "noisy", "status": "completed",
		"rawInput": map[string]any{"command": strings.Repeat("TOOL_BODY", 20_000)},
	})))
	context.Observe(noisy)
	context.Observe(turnContextMessage(t, "new", "CURRENT ANSWER"))
	facts := core.ActivityFacts{
		BackgroundCommands: 2,
		QueuedCommands:     3,
		ToolsInFlight: []core.InFlightToolCall{{
			ToolCallID:  "active-build",
			Title:       turnContextText("active-build"),
			Status:      schema.ToolCallStatusInProgress,
			StartedAtMs: 1_000,
		}},
	}
	evidence := context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 5_000)
	if !strings.Contains(evidence.TranscriptSummary, "CURRENT REQUEST") || !strings.Contains(evidence.TranscriptSummary, "CURRENT ANSWER") {
		t.Fatalf("summary:\n%s", evidence.TranscriptSummary)
	}
	for _, excluded := range []string{"OLD REQUEST", "OLD ANSWER", "TOOL_BODY", "<tool", "bytes omitted"} {
		if strings.Contains(evidence.TranscriptSummary, excluded) {
			t.Fatalf("%s:\n%s", excluded, evidence.TranscriptSummary)
		}
	}
	if evidence.UserPromptTail != "CURRENT REQUEST" || evidence.AssistantTextTail != "CURRENT ANSWER" {
		t.Fatalf("tails %q %q", evidence.UserPromptTail, evidence.AssistantTextTail)
	}
	if evidence.BackgroundCommands != 2 || evidence.QueuedCommands != 3 {
		t.Fatalf("counts %d %d", evidence.BackgroundCommands, evidence.QueuedCommands)
	}
	if len(evidence.ToolsInFlight) != 1 || !strings.Contains(evidence.ToolsInFlight[0].Title, "active-build") ||
		evidence.ToolsInFlight[0].RunningS != 4 {
		t.Fatalf("tools in flight %+v", evidence.ToolsInFlight)
	}
	full := context.RenderTranscript(256 * 1024)
	if !strings.Contains(full, "OLD REQUEST") || !strings.Contains(full, "TOOL_BODY") {
		t.Fatalf("full transcript:\n%s", full)
	}
}

func TestToolCallsAfterTheLastAssistantTextAreNamedInTheEvidence(t *testing.T) {
	context := &TurnContext{}
	context.ObserveRelay(turnContextStart("audit"), turnContextText("Audit the cache and hand back."))
	context.Observe(turnContextMessage(t, "m1", "I'm checking the update paths now."))
	handback := summaryDecodeUpdate(t, `{"sessionUpdate":"tool_call", "toolCallId":"hb", "title":"mcp__mj-agents__handback", "status":"completed", "rawInput":{"report":"done"}}`)
	context.Observe(handback)
	facts := core.ActivityFacts{}
	evidence := context.Evidence(core.HarnessKindCodex, core.TurnPhaseReplied, &facts, 5_000)
	if len(evidence.FinalToolCalls) != 1 || !strings.Contains(evidence.FinalToolCalls[0].Name, "handback") ||
		evidence.FinalToolCalls[0].Status != "completed" {
		t.Fatalf("final tool calls %+v", evidence.FinalToolCalls)
	}
	// Text after the call closes the list again.
	context.Observe(turnContextMessage(t, "m2", "Handed back."))
	evidence = context.Evidence(core.HarnessKindCodex, core.TurnPhaseReplied, &facts, 5_000)
	if len(evidence.FinalToolCalls) != 0 {
		t.Fatalf("final tool calls %+v", evidence.FinalToolCalls)
	}
}

func TestEvidenceCapsUtf8TextToolsAndDurations(t *testing.T) {
	context := &TurnContext{}
	context.Reset(nil, strings.Repeat("é", 20_000))
	context.Observe(turnContextMessage(t, "first", strings.Repeat("🦀", 20_000)))
	first := context.Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &core.ActivityFacts{}, 0)
	if len(first.UserPromptTail) != core.UserPromptBytes || len(first.AssistantTextTail) != core.AssistantTextBytes {
		t.Fatalf("tails of %d and %d bytes", len(first.UserPromptTail), len(first.AssistantTextTail))
	}
	for n := range 20 {
		context.Observe(turnContextTool(t, fmt.Sprintf("%d%s", n, strings.Repeat("é", 300))))
	}
	lastActivity := int64(1_000)
	facts := core.ActivityFacts{
		LastAcpActivityAtMs: &lastActivity,
		BackgroundCommands:  3,
		QueuedCommands:      2,
	}
	for n := range 100 {
		facts.ToolsInFlight = append(facts.ToolsInFlight, core.InFlightToolCall{
			ToolCallID:  strconv.Itoa(n),
			Title:       turnContextText(strings.Repeat("🦀", 200)),
			Status:      schema.ToolCallStatusInProgress,
			StartedAtMs: 2_000,
		})
	}
	evidence := context.Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &facts, 95_000)
	if len(evidence.ToolsInFlight) != core.InFlightTools {
		t.Fatalf("%d tools in flight", len(evidence.ToolsInFlight))
	}
	for _, tool := range evidence.ToolsInFlight {
		if len(tool.Title) > core.ToolTitleBytes || tool.RunningS != 93 {
			t.Fatalf("tool %+v", tool)
		}
	}
	if len(evidence.TranscriptSummary) > 48*1024 {
		t.Fatalf("summary of %d bytes", len(evidence.TranscriptSummary))
	}
	if evidence.AssistantTextTail != first.AssistantTextTail {
		t.Fatal("the assistant tail changed")
	}
	if evidence.SilentForS != 94 {
		t.Fatalf("silent for %d", evidence.SilentForS)
	}
	encoded := turnContextEvidenceJSON(t, evidence)
	var fields map[string]any
	if err := json.Unmarshal([]byte(encoded), &fields); err != nil {
		t.Fatal(err)
	}
	if fields["phase"] != "running" || fields["harness"] != "claude" ||
		fields["background_commands"] != float64(3) || fields["queued_commands"] != float64(2) {
		t.Fatalf("evidence JSON %v", fields)
	}
	if len(encoded) >= 64*1024 {
		t.Fatalf("evidence JSON of %d bytes", len(encoded))
	}
}

func TestMessageBoundariesAndPromptResetDoNotMixAnswers(t *testing.T) {
	context := &TurnContext{}
	context.Reset(nil, "First prompt")
	context.Observe(turnContextMessage(t, "one", "Old answer"))
	context.Observe(turnContextMessage(t, "two", "New "))
	context.Observe(turnContextMessage(t, "two", "answer?"))
	evidence := context.Evidence(core.HarnessKindClaude, core.TurnPhaseReplied, &core.ActivityFacts{}, 0)
	if evidence.AssistantTextTail != "New answer?" {
		t.Fatalf("assistant tail %q", evidence.AssistantTextTail)
	}
	generation := context.Generation()
	context.SetCounts(1, 2)
	if context.Generation() == generation {
		t.Fatal("a count change kept the generation")
	}
	generation = context.Generation()
	context.SetCounts(1, 2)
	if context.Generation() != generation {
		t.Fatal("unchanged counts moved the generation")
	}
	if background, queued := context.Counts(); background != 1 || queued != 2 {
		t.Fatalf("counts %d %d", background, queued)
	}
	context.Invalidate()
	if context.Generation() == generation {
		t.Fatal("invalidate kept the generation")
	}
	if background, queued := context.Counts(); background != 1 || queued != 2 {
		t.Fatalf("counts %d %d", background, queued)
	}
	generation = context.Generation()
	context.Reset(nil, "Second prompt")
	if background, queued := context.Counts(); background != 1 || queued != 2 {
		t.Fatalf("counts %d %d", background, queued)
	}
	if context.Generation() == generation {
		t.Fatal("reset kept the generation")
	}
	evidence = context.Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &core.ActivityFacts{}, 0)
	if evidence.UserPromptTail != "Second prompt" || evidence.AssistantTextTail != "" ||
		strings.Contains(evidence.TranscriptSummary, "<user>") {
		t.Fatalf("evidence %+v", evidence)
	}
	// A prompt Mjolnir submitted is not the person's latest request.
	context.Reset(turnContextText("mailbox-wake-1"), "A child finished its task.")
	evidence = context.Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &core.ActivityFacts{}, 0)
	if evidence.UserPromptTail != "" {
		t.Fatalf("user prompt tail %q", evidence.UserPromptTail)
	}
}

// Hard-won: 753b1f960: recovery lost its request when the original start left the hot journal.
func TestRecoveryInitializesMissingRequestAndPreservesRestoredCorrections(t *testing.T) {
	for _, restored := range []bool{false, true} {
		context := &TurnContext{}
		if restored {
			context.ObserveRelay(core.RelayObservationCommandStarted{CommandID: "correction", StartedAtMs: 1},
				turnContextText("Use the corrected approach"))
			context.Observe(turnContextMessage(t, "partial", "Partial work"))
		} else {
			context.MarkEarlierHistoryOmitted()
			context.Observe(turnContextMessage(t, "suffix", "Only the tail survived"))
		}
		recovery := core.RestartContinuationID("original")
		context.ObserveRelay(core.RelayObservationCommandStarted{CommandID: recovery, StartedAtMs: 2},
			turnContextText("Original request"))
		evidence := context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &core.ActivityFacts{}, 0)
		request, answer := "Original request", "Only the tail survived"
		if restored {
			request, answer = "Use the corrected approach", "Partial work"
		}
		if evidence.UserPromptTail != request || !strings.Contains(evidence.TranscriptSummary, request) {
			t.Fatalf("restored %t: user prompt tail %q, summary:\n%s", restored, evidence.UserPromptTail, evidence.TranscriptSummary)
		}
		if evidence.AssistantTextTail != answer || !strings.Contains(evidence.TranscriptSummary, answer) {
			t.Fatalf("restored %t: assistant tail %q, summary:\n%s", restored, evidence.AssistantTextTail, evidence.TranscriptSummary)
		}
		before := evidence.TranscriptSummary
		context.ObserveRelay(core.RelayObservationCommandStarted{CommandID: core.RestartContinuationID(recovery), StartedAtMs: 3},
			turnContextText("Original request"))
		if after := context.Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &core.ActivityFacts{}, 0).TranscriptSummary; after != before {
			t.Fatalf("restored %t: summary changed:\n%s\n%s", restored, before, after)
		}
	}
}
