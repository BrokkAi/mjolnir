// Port of mj-transcript/src/projection/tests.rs.

package transcript

import (
	"bytes"
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func projectionEvent(t testing.TB, previous *core.MaterializedSession, observation core.RelayObservation) core.RelayEvent {
	t.Helper()
	event := core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        previous.AppliedEventOrdinal + 1,
		PreviousDigest: previous.AppliedEventDigest,
		Observation:    observation,
	}
	event.RecordedAtMs = int64(event.Ordinal) * 100
	projectionRedigest(t, &event)
	return event
}

func projectionRedigest(t testing.TB, event *core.RelayEvent) {
	t.Helper()
	digest, err := core.RelayEventDigest(event)
	if err != nil {
		t.Fatalf("relay event digest: %v", err)
	}
	event.Digest = digest
}

func projectionApply(t testing.TB, session *core.MaterializedSession, event core.RelayEvent) {
	t.Helper()
	projected, err := ProjectRelayEvent(session, &event)
	if err != nil {
		t.Fatalf("project relay event %d: %v", event.Ordinal, err)
	}
	if err := ApplyCommittedProjectionEvent(session, &event, projected.Mutation); err != nil {
		t.Fatalf("apply relay event %d: %v", event.Ordinal, err)
	}
}

func projectionApplyObservation(t testing.TB, session *core.MaterializedSession, observation core.RelayObservation) {
	t.Helper()
	projectionApply(t, session, projectionEvent(t, session, observation))
}

func projectionApplyIndexedObservation(t testing.TB, session *core.MaterializedSession, index *ProjectionIndex, observation core.RelayObservation) {
	t.Helper()
	next := projectionEvent(t, session, observation)
	projected, err := ProjectRelayEventIndexed(session, index, &next)
	if err != nil {
		t.Fatalf("project relay event %d: %v", next.Ordinal, err)
	}
	if err := ApplyCommittedProjectionEventIndexed(session, index, &next, projected.Mutation); err != nil {
		t.Fatalf("apply relay event %d: %v", next.Ordinal, err)
	}
}

// projectionClone is MaterializedSession::clone: the copy can be projected
// without disturbing the original.
func projectionClone(session *core.MaterializedSession) core.MaterializedSession {
	clone := *session
	clone.Transcript = slices.Clone(session.Transcript)
	clone.QueuedPrompts = slices.Clone(session.QueuedPrompts)
	clone.PendingElicitations = slices.Clone(session.PendingElicitations)
	clone.Configuration = cloneSessionConfiguration(session.Configuration)
	return clone
}

// projectionJSONText is a value's JSON, for comparing values as Rust's
// PartialEq compares them.
func projectionJSONText(t testing.TB, value any) string {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("marshal %T: %v", value, err)
	}
	return string(encoded)
}

func projectionAssertJSONEqual(t testing.TB, got, want any, what string) {
	t.Helper()
	if left, right := projectionJSONText(t, got), projectionJSONText(t, want); left != right {
		t.Fatalf("%s differs:\n got: %s\nwant: %s", what, left, right)
	}
}

func projectionParse(t testing.TB, data []byte) any {
	t.Helper()
	value, err := jsonvalue.Parse(data)
	if err != nil {
		t.Fatalf("parse %s: %v", data, err)
	}
	return value
}

// projectionMember reads a member of a parsed JSON object.
func projectionMember(value any, key string) any {
	object, _ := value.(jsonvalue.Object)
	member, _ := object.Get(key)
	return member
}

func projectionPrompt(text string) core.RelayCommandPrompt {
	return core.RelayCommandPrompt{Prompt: []schema.ContentBlock{textBlock(text)}}
}

func projectionQueued(commandID string, command core.RelayCommand, createdAtMs int64) core.RelayObservationCommandQueued {
	return core.RelayObservationCommandQueued{CommandID: commandID, Command: command, CreatedAtMs: createdAtMs}
}

func projectionStarted(commandID string, startedAtMs int64) core.RelayObservationCommandStarted {
	return core.RelayObservationCommandStarted{CommandID: commandID, StartedAtMs: startedAtMs}
}

func projectionPromptCompleted(commandID, stopReason string) core.RelayObservationCommandCompleted {
	return core.RelayObservationCommandCompleted{
		CommandID: commandID,
		Outcome:   core.RelayCommandOutcomePrompt{StopReason: stopReason},
	}
}

func projectionUpdate(update schema.SessionUpdate) core.RelayObservationSessionUpdate {
	return core.RelayObservationSessionUpdate{Update: update}
}

func projectionUpdateJSON(t testing.TB, text string) core.RelayObservationSessionUpdate {
	t.Helper()
	var update schema.SessionUpdate
	if err := json.Unmarshal([]byte(text), &update); err != nil {
		t.Fatalf("decode session update %s: %v", text, err)
	}
	return core.RelayObservationSessionUpdate{Update: update}
}

func projectionChunk(text string, messageID *string) *schema.ContentChunk {
	chunk := &schema.ContentChunk{Content: textBlock(text)}
	if messageID != nil {
		id := schema.MessageId(*messageID)
		chunk.MessageID = &id
	}
	return chunk
}

// projectionUntaggedAgentChunk is an agent message chunk with no message_id,
// as Grok Build's goal mode streams them.
func projectionUntaggedAgentChunk(text string) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{
		Kind:              schema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: projectionChunk(text, nil),
	})
}

// projectionUntaggedThoughtChunk is an agent thought chunk with no
// message_id, mirroring projectionUntaggedAgentChunk.
func projectionUntaggedThoughtChunk(text string) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{
		Kind:              schema.SessionUpdateKindAgentThoughtChunk,
		AgentThoughtChunk: projectionChunk(text, nil),
	})
}

func projectionAgentChunk(text, messageID string) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{
		Kind:              schema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: projectionChunk(text, &messageID),
	})
}

func projectionThoughtChunk(text, messageID string) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{
		Kind:              schema.SessionUpdateKindAgentThoughtChunk,
		AgentThoughtChunk: projectionChunk(text, &messageID),
	})
}

func projectionToolCall(call schema.ToolCall) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCall, ToolCall: &call})
}

func projectionToolCallUpdate(update schema.ToolCallUpdate) core.RelayObservation {
	return projectionUpdate(schema.SessionUpdate{Kind: schema.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &update})
}

func projectionStatus(status schema.ToolCallStatus) *schema.ToolCallStatus {
	return &status
}

// projectionTerminalToolCall is a tool call whose only content is a terminal
// reference, the shape kimi-code sends for every Bash call.
func projectionTerminalToolCall(callID, terminalID string) core.RelayObservation {
	return projectionToolCall(schema.ToolCall{
		ToolCallID: schema.ToolCallId(callID),
		Title:      "shell",
		Content: []schema.ToolCallContent{{
			Kind:     schema.ToolCallContentKindTerminal,
			Terminal: &schema.Terminal{TerminalID: schema.TerminalId(terminalID)},
		}},
	})
}

func projectionTerminalOutput(terminalID string) core.RelayObservation {
	exitCode := uint32(0)
	return core.RelayObservationTerminalOutput{
		TerminalID: terminalID,
		Output:     "build finished\n",
		ExitCode:   &exitCode,
	}
}

func projectionFallbackTerminalTool(terminalID, command string) core.RelayObservation {
	return projectionToolCall(core.FallbackTerminalToolCall(terminalID, command))
}

func projectionAttachedTerminalOutputs(t testing.TB, item *core.TranscriptItem) []core.TerminalOutputRecord {
	t.Helper()
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatalf("expected a tool item, got %#v", item.Body)
	}
	return body.TerminalOutputs
}

// projectionBytes is a string's bytes as a JSON array, as serde writes
// &[u8].
func projectionBytes(text string) []any {
	values := make([]any, len(text))
	for i := 0; i < len(text); i++ {
		values[i] = int64(text[i])
	}
	return values
}

func projectionRawJSON(t testing.TB, value any) json.RawMessage {
	t.Helper()
	encoded, err := projectionJSON(value)
	if err != nil {
		t.Fatalf("marshal %T: %v", value, err)
	}
	return encoded
}

func projectionKimiRawToolUpdate(t testing.TB, callID, output string) core.RelayObservation {
	return projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: schema.ToolCallId(callID),
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
		RawOutput: projectionRawJSON(t, jsonvalue.Object{
			{Key: "type", Value: "Bash"},
			{Key: "output", Value: projectionBytes(output)},
			{Key: "exit_code", Value: int64(0)},
			{Key: "command", Value: "cargo test"},
		}),
	})
}

func projectionCallMember(t testing.TB, item *core.TranscriptItem, key string) any {
	t.Helper()
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatalf("expected a tool item, got %#v", item.Body)
	}
	return projectionMember(projectionParse(t, body.Call.Value()), key)
}

func projectionFindItem(session *core.MaterializedSession, stableID string) *core.TranscriptItem {
	for _, item := range session.Transcript {
		if item.StableID == stableID {
			return item
		}
	}
	return nil
}

func projectionStreamingAgent(session *core.MaterializedSession) bool {
	for _, item := range session.Transcript {
		if body, ok := item.Body.(core.TranscriptBodyAgent); ok && body.Streaming {
			return true
		}
	}
	return false
}

func projectionIsRunning(session *core.MaterializedSession) bool {
	_, running := session.Execution.(core.MaterializedExecutionStateRunning)
	return running
}

func projectionIsIdle(session *core.MaterializedSession) bool {
	_, idle := session.Execution.(core.MaterializedExecutionStateIdle)
	return idle
}

func projectionSystemText(item *core.TranscriptItem) (string, bool) {
	body, ok := item.Body.(core.TranscriptBodySystem)
	return body.Text, ok
}

func uint64Pointer(value uint64) *uint64 { return &value }

func TestHiddenHandoffStateSurvivesPromptDeliveryAndCanonicalExport(t *testing.T) {
	session := core.MaterializedSessionEmpty("handoff-projection")
	source, err := CanonicalSessionFromMaterialized(&session)
	if err != nil {
		t.Fatal(err)
	}
	text := ModelHandoffOpening + "verified state\n\n<retained_recent_context>\nrecent work</retained_recent_context>"
	marked, err := EncodeHandoff(&source, text)
	if err != nil {
		t.Fatal(err)
	}
	projectionApplyObservation(t, &session, projectionQueued("install-context",
		core.RelayCommandInstallPromptContext{Text: marked}, 100))
	if len(session.Transcript) != 0 {
		t.Fatalf("hidden context entered the transcript: %#v", session.Transcript)
	}
	if _, ok := session.Configuration.Values[HandoffStateKey]; !ok {
		t.Fatal("handoff state was not projected")
	}
	projectionApplyObservation(t, &session, projectionQueued("first-prompt", projectionPrompt("continue work"), 200))
	projectionApplyObservation(t, &session, projectionStarted("first-prompt", 300))
	projectionApplyObservation(t, &session, core.RelayObservationSessionConfigured{ConfigOptions: []schema.SessionConfigOption{}})
	canonical, err := CanonicalSessionFromMaterialized(&session)
	if err != nil {
		t.Fatal(err)
	}
	state, ok := ReusableHandoff(&canonical)
	if !ok {
		t.Fatal("no reusable handoff after canonical export")
	}
	if state.Summary != "verified state\n\n<retained_recent_context>\nrecent work</retained_recent_context>" {
		t.Fatalf("summary = %q", state.Summary)
	}
	for _, item := range canonical.Transcript {
		if body, ok := item.Body.(core.CanonicalTranscriptBodyUser); ok &&
			strings.Contains(core.MaterializedContentText(body.Content), "verified state") {
			t.Fatalf("the hidden handoff became a user row: %#v", item)
		}
	}

	pasted := core.MaterializedSessionEmpty("pasted-handoff")
	projectionApplyObservation(t, &pasted, projectionQueued("pasted", projectionPrompt(marked), 100))
	if _, ok := pasted.Configuration.Values[HandoffStateKey]; ok {
		t.Fatal("a pasted handoff installed hidden state")
	}
}

func TestHandoffProvenanceRejectsALaterUpdateToAnOldToolAndClear(t *testing.T) {
	session := core.MaterializedSessionEmpty("handoff-freshness")
	source, err := CanonicalSessionFromMaterialized(&session)
	if err != nil {
		t.Fatal(err)
	}
	source.EventFrontier = 1
	source.Transcript = append(source.Transcript, core.CanonicalTranscriptItem{
		StableID: "tool:old",
		Position: 1,
		Body:     core.CanonicalTranscriptBodyTool{Call: json.RawMessage(`{"toolCallId":"old","status":"in_progress"}`)},
	})
	state, err := HandoffStateCapture(&source, "old state")
	if err != nil {
		t.Fatal(err)
	}
	if valid, err := state.Validate(&source); err != nil || !valid {
		t.Fatalf("validate source = %v, %v", valid, err)
	}
	updated := source
	updated.Transcript = slices.Clone(source.Transcript)
	updated.Transcript[0].Body = core.CanonicalTranscriptBodyTool{Call: json.RawMessage(`{"toolCallId":"old","status":"completed"}`)}
	updated.EventFrontier = 3
	if valid, err := state.Validate(&updated); err != nil || valid {
		t.Fatalf("validate updated = %v, %v", valid, err)
	}

	cleared := source
	cleared.Transcript = slices.Clone(source.Transcript)
	cleared.EventFrontier = 2
	cleared.Transcript = append(cleared.Transcript, core.CanonicalTranscriptItem{
		StableID: "context-cleared:reset",
		Position: 2,
		Body:     core.CanonicalTranscriptBodySystem{Text: "clear"},
	})
	if valid, err := state.Validate(&cleared); err != nil || valid {
		t.Fatalf("validate cleared = %v, %v", valid, err)
	}
}

func projectionRenderedTranscript(session *core.MaterializedSession) string {
	title, ok := session.ResolvedTitle()
	if !ok {
		title = session.SessionID
	}
	rows := make([]string, 0, len(session.Transcript))
	for _, item := range session.Transcript {
		rows = append(rows, fmt.Sprintf("%d: %s", item.Position, TranscriptItemText(item)))
	}
	return fmt.Sprintf("Session: %s\n%s", title, strings.Join(rows, "\n"))
}

func projectionAppendTranscriptState(output *strings.Builder, label string, session *core.MaterializedSession) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (session transcript) ===\n", label)
	output.WriteString(projectionRenderedTranscript(session))
	output.WriteByte('\n')
}

func TestGoldenPlanProposalTranscript(t *testing.T) {
	var output strings.Builder
	session := core.MaterializedSessionEmpty("session-1")
	request := core.NormalizedPlanReview("plan-review-3",
		projectionParse(t, []byte(`{"plan":"1. Read the code\n2. Change it"}`)))
	projectionApplyObservation(t, &session, core.RelayObservationElicitationRequested{Request: request})
	projectionAppendTranscriptState(&output, "decision pending", &session)
	fmt.Fprintf(&output, "pending decisions: %d\n", len(session.PendingElicitations))

	accepted := request.ReplyText(&core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"action": core.ElicitationValueString("implement")},
	})
	projectionApplyObservation(t, &session, core.RelayObservationElicitationResolved{
		ElicitationID: "plan-review-3",
		Action:        "accept",
		Reply:         &accepted,
	})
	projectionAppendTranscriptState(&output, "proposal remains after accepting", &session)
	fmt.Fprintf(&output, "pending decisions: %d\n", len(session.PendingElicitations))

	conversation := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &conversation, projectionUntaggedAgentChunk("here is my plan"))
	projectionApplyObservation(t, &conversation, core.RelayObservationElicitationRequested{
		Request: core.NormalizedPlanReview("plan-review-1", projectionParse(t, []byte(`{"plan":"do the work"}`))),
	})
	projectionApplyObservation(t, &conversation, projectionUntaggedAgentChunk("starting now"))
	projectionAppendTranscriptState(&output, "proposal stays between its conversation", &conversation)

	question, err := core.ElicitationRequestFromACPParams("question-1", json.RawMessage(`{
		"mode": "form", "sessionId": "session-1", "message": "How should we build this?",
		"requestedSchema": {"type": "object", "properties": {
			"architecture": {"type": "string", "title": "Architecture", "oneOf": [
				{"const": "thin", "title": "Thin callers"}, {"const": "matrix", "title": "Dynamic matrix"}
			]},
			"architecture_note": {"type": "string", "title": "Other", "_meta": {
				"codex": {"role": "user_note", "questionId": "architecture"}
			}},
			"features": {"type": "array", "title": "Features", "items": {"type": "string", "anyOf": [
				{"const": "tui", "title": "Terminal"}, {"const": "web", "title": "Browser"}
			]}},
			"ratio": {"type": "number", "title": "Ratio"},
			"token": {"type": "string", "title": "Access token", "_meta": {"codex": {"isSecret": true}}},
			"token_note": {"type": "string", "title": "Other token", "_meta": {
				"codex": {"role": "user_note", "questionId": "token"}
			}},
			"verify": {"type": "boolean", "title": "Verify"},
			"workers": {"type": "integer", "title": "Workers"}
		}}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	projectionApplyObservation(t, &conversation, core.RelayObservationElicitationRequested{Request: question})
	response := core.ElicitationResponse{
		Action: core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{
			"architecture_note": core.ElicitationValueString("Keep one owner.\n支持 Unicode"),
			"features":          core.ElicitationValueStringArray{"tui", "web"},
			"ratio":             core.ElicitationValueNumber(1.5),
			"token_note":        core.ElicitationValueString("private-token"),
			"verify":            core.ElicitationValueBoolean(true),
			"workers":           core.ElicitationValueInteger(4),
		},
	}
	if err := question.ValidateResponse(&response); err != nil {
		t.Fatal(err)
	}
	reply := question.ReplyText(&response)
	resolution := core.RelayObservationElicitationResolved{
		ElicitationID: question.ID,
		Action:        "accept",
		Reply:         &reply,
	}
	encoded, err := resolution.MarshalJSON()
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(encoded, []byte("private-token")) {
		t.Fatalf("a secret answer reached the journal: %s", encoded)
	}
	decoded, err := core.UnmarshalRelayObservationJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	projectionApplyObservation(t, &conversation, decoded)
	last := conversation.Transcript[len(conversation.Transcript)-1]
	if _, ok := last.Body.(core.TranscriptBodyUser); !ok {
		t.Fatalf("the answer is not a user row: %#v", last)
	}
	if last.IsTurnStart() {
		t.Fatal("an answer within a turn started a turn")
	}
	projectionApplyObservation(t, &conversation, projectionUntaggedAgentChunk("I will use one owner."))
	projectionAppendTranscriptState(&output, "custom answers and masked secrets between assistant messages", &conversation)

	var restored core.MaterializedSession
	if err := json.Unmarshal([]byte(projectionJSONText(t, conversation)), &restored); err != nil {
		t.Fatal(err)
	}
	projectionAssertJSONEqual(t, restored, conversation, "restored session")
	if projectionRenderedTranscript(&restored) != projectionRenderedTranscript(&conversation) {
		t.Fatal("the restored session renders differently")
	}

	// Harness IDs can repeat after restart; the journal ordinal owns identity.
	for _, response := range []core.ElicitationResponse{
		{Action: core.ElicitationResponseDecline},
		{Action: core.ElicitationResponseCancel},
	} {
		projectionApplyObservation(t, &conversation, core.RelayObservationElicitationRequested{Request: question})
		text := question.ReplyText(&response)
		projectionApplyObservation(t, &conversation, core.RelayObservationElicitationResolved{
			ElicitationID: question.ID,
			Action:        response.ActionName(),
			Reply:         &text,
		})
	}
	projectionApplyObservation(t, &conversation, core.RelayObservationElicitationRequested{Request: question})
	projectionApplyObservation(t, &conversation, core.RelayObservationElicitationResolved{
		ElicitationID: question.ID,
		Action:        "closed",
	})
	projectionAppendTranscriptState(&output, "explicit skip and cancel survive automatic withdrawal", &conversation)

	var reviewerEntries []core.ChatEntry
	recordedAt := int64(100)
	ApplyRuntimeEventToEntries(&reviewerEntries, 1, &recordedAt, core.RuntimeEventElicitationResolved{
		ElicitationID: "review-question",
		Action:        "accept",
		Reply:         &reply,
	})
	fmt.Fprintf(&output, "reviewer reply (%s):\n%s\n", reviewerEntries[0].Role, reviewerEntries[0].Text)

	core.AssertGolden(t, "testdata", "plan-proposal-transcript", output.String())
}

func TestGoldenSessionTitleProjection(t *testing.T) {
	var output strings.Builder

	provisional := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &provisional, projectionQueued("prompt-1", projectionPrompt("  fix the flaky\nresume test  "), 100))
	projectionAppendTranscriptState(&output, "first prompt supplies provisional title", &provisional)

	titled := core.MaterializedSessionEmpty("session-1")
	for _, prompt := range []struct {
		commandID, text string
		createdAtMs     int64
	}{{"prompt-1", "first prompt", 100}, {"prompt-2", "second prompt", 200}} {
		projectionApplyObservation(t, &titled, projectionQueued(prompt.commandID, projectionPrompt(prompt.text), prompt.createdAtMs))
	}
	projectionApplyObservation(t, &titled, projectionUpdate(schema.SessionUpdate{
		Kind: schema.SessionUpdateKindSessionInfoUpdate,
		SessionInfoUpdate: &schema.SessionInfoUpdate{
			Title: schema.Nullable[string]{Set: true, Value: "Agent-generated title"},
		},
	}))
	projectionAppendTranscriptState(&output, "harness title replaces provisional title", &titled)

	backfilled := core.MaterializedSessionEmpty("session-1")
	backfilled.Transcript = append(backfilled.Transcript, &core.TranscriptItem{
		StableID:        "user:prompt-1",
		Position:        1,
		CreatedAtMs:     100,
		LastChangedAtMs: 100,
		Body:            core.TranscriptBodyUser{Content: []json.RawMessage{projectionRawJSON(t, textBlock("original task"))}},
	})
	projectionApplyObservation(t, &backfilled, projectionQueued("prompt-2", projectionPrompt("follow-up task"), 200))
	projectionAppendTranscriptState(&output, "first existing prompt backfills title", &backfilled)

	exitCode := int32(7)
	signal := "SIGTERM"
	for _, shell := range []struct {
		commandID, command string
		outcome            core.UserShellResult
	}{
		{"shell-exit", "cargo test -p mj-transcript", core.UserShellResult{
			Command:    "cargo test -p mj-transcript",
			Stdout:     "2 passed",
			Stderr:     "one expected warning",
			ExitCode:   &exitCode,
			DurationMs: 1_234,
			Status:     core.UserShellStatusExited,
		}},
		{"shell-signal", "long-running generator", core.UserShellResult{
			Command:    "long-running generator",
			Stdout:     "partial output",
			Signal:     &signal,
			DurationMs: 2_345,
			Status:     core.UserShellStatusSignaled,
		}},
	} {
		session := core.MaterializedSessionEmpty("session-1")
		projectionApplyObservation(t, &session, projectionQueued(shell.commandID, core.RelayCommandRunUserShell{Command: shell.command}, 300))
		kind := core.RelayCommandKindRunUserShell
		projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
			Command:   &kind,
			CommandID: shell.commandID,
			Outcome:   core.RelayCommandOutcomeUserShell{Result: shell.outcome},
		})
		projectionAppendTranscriptState(&output, shell.commandID, &session)
	}

	core.AssertGolden(t, "testdata", "session-title-projection", output.String())
}

func projectionMailboxEvent(key, source string, wake bool, createdAtMs uint64, body core.MailboxEventBody) core.MailboxEvent {
	return core.MailboxEvent{Key: key, Source: source, Wake: wake, CreatedAtMs: createdAtMs, Body: body}
}

func projectionHookDelivery(events []core.MailboxEvent, hookEvent, leaseID string) core.RelayObservationMailboxEventsDelivered {
	keys := make([]string, 0, len(events))
	for _, event := range events {
		keys = append(keys, event.Key)
	}
	return core.RelayObservationMailboxEventsDelivered{
		EventKeys: keys,
		Path:      core.MailboxDeliveryPathToolHook,
		HookEvent: &hookEvent,
		Events:    events,
		LeaseID:   &leaseID,
	}
}

// Hard-won: #4625: an external-events row must omit the by-clause when a merge actor is unavailable.
func TestGoldenMailboxDeliveryTranscript(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-mailbox")
	githubEvent := projectionMailboxEvent("github:owner/repo#12:comment:345", "github", false, 100,
		core.MailboxEventBodyGithubComment{
			ItemKind: core.GithubItemKindPullRequest,
			Number:   12,
			Title:    "Mailbox delivery",
			Author:   "alice",
			Body:     "The first comment line.\nThe second line is omitted from the notice.",
		})
	projectionApplyObservation(t, &session, projectionQueued("github-event", core.RelayCommandDeliverMailboxEvent{Event: githubEvent}, 100))
	projectionApplyObservation(t, &session, projectionHookDelivery([]core.MailboxEvent{githubEvent}, "PostToolUse", "hook-lease"))

	parentEvent := projectionMailboxEvent("parent:message:1", "parent", false, 300,
		core.MailboxEventBodyParentMessage{Text: "Please check this detail.\nThe whole second line must survive delivery."})
	projectionApplyObservation(t, &session, projectionQueued("parent-event", core.RelayCommandDeliverMailboxEvent{Event: parentEvent}, 300))
	promptCommandID := "user-prompt"
	projectionApplyObservation(t, &session, core.RelayObservationMailboxEventsDelivered{
		EventKeys:       []string{parentEvent.Key},
		Path:            core.MailboxDeliveryPathPrompt,
		PromptCommandID: &promptCommandID,
		Events:          []core.MailboxEvent{parentEvent},
	})

	const pullTitle = "Model Rust built-in macro values for CQ07"
	changesRequested := "changes_requested"
	var report core.SubagentFinishedReport
	if err := json.Unmarshal([]byte(`{
		"child_session_id": "child-1", "task_name": "Check parser",
		"state": "failed", "finished": true,
		"output": "Fixed <parser>.\nTests passed: \"parser\".",
		"report_dir": "/reports/child-1", "report_source": "handback",
		"failure": {"summary": "verification timed out"}, "truncated": true
	}`), &report); err != nil {
		t.Fatal(err)
	}
	events := []core.MailboxEvent{
		projectionMailboxEvent("github:repo#4623:interest:session", "github", false, 500,
			core.MailboxEventBodyNewGithubItem{Kind: core.GithubItemKindPullRequest, Number: 4623, Title: pullTitle}),
		projectionMailboxEvent("github:repo#4623:comment:1", "github", true, 502,
			core.MailboxEventBodyGithubComment{
				ItemKind: core.GithubItemKindPullRequest, Number: 4623, Title: pullTitle,
				Author: "alice", Body: "Please check the <macro> case.\nMore detail.",
			}),
		projectionMailboxEvent("github:repo#4624:interest:session", "github", false, 501,
			core.MailboxEventBodyNewGithubItem{
				Kind: core.GithubItemKindIssue, Number: 4624,
				Title: "Per-unit resolution summary: shared tables, driver and Go producer (Milestone 1)",
			}),
		projectionMailboxEvent("github:repo#4623:review:2", "github", true, 503,
			core.MailboxEventBodyGithubReview{
				ItemKind: core.GithubItemKindPullRequest, Number: 4623, Title: pullTitle,
				Author: "bob", Body: "Could you revise the error path?", ReviewState: &changesRequested,
			}),
		projectionMailboxEvent("github:repo#4623:review-comment:3", "github", true, 504,
			core.MailboxEventBodyGithubReviewComment{
				ItemKind: core.GithubItemKindPullRequest, Number: 4623, Title: pullTitle,
				Author: "carol", Body: "This assertion needs a case.",
			}),
		projectionMailboxEvent("github:repo#4623:merged", "github", true, 505,
			core.MailboxEventBodyGithubPullRequestLifecycle{
				Change: core.MailboxPullRequestChangeMerged, Number: 4623, Title: pullTitle,
				Actor: "dana", Recipient: core.LifecycleRecipientCreator,
			}),
		projectionMailboxEvent("github:repo#4623:closed", "github", true, 506,
			core.MailboxEventBodyGithubPullRequestLifecycle{
				Change: core.MailboxPullRequestChangeClosedWithoutMerging, Number: 4623, Title: pullTitle,
				Actor: "erin", Recipient: core.LifecycleRecipientCreator,
			}),
		projectionMailboxEvent("github:repo#4624:merged-unknown", "github", true, 506,
			core.MailboxEventBodyGithubPullRequestLifecycle{
				Change: core.MailboxPullRequestChangeMerged, Number: 4624, Title: "PR without a known merger",
				Recipient: core.LifecycleRecipientCreator,
			}),
		projectionMailboxEvent("github:repo#4623:reopened", "github", true, 507,
			core.MailboxEventBodyGithubPullRequestLifecycle{
				Change: core.MailboxPullRequestChangeReopened, Number: 4623, Title: pullTitle,
				Actor: "frank", Recipient: core.LifecycleRecipientCreator,
			}),
		projectionMailboxEvent("parent:message:2", "parent", true, 508,
			core.MailboxEventBodyParentMessage{Text: "Please verify the edge case."}),
		projectionMailboxEvent("session-message:peer", "session_message", true, 509,
			core.MailboxEventBodySessionMessage{
				From: core.Sender{Type: core.SenderSession, ID: "b6932a80-1234-5678-9abc-def012345678", Title: "Fix cache race"},
				Text: "The lock is fixed; please verify it.",
			}),
		projectionMailboxEvent("session-message:user", "session_message", true, 510,
			core.MailboxEventBodySessionMessage{From: core.Sender{Type: core.SenderUser}, Text: "Please check the final result."}),
		projectionMailboxEvent("api:plain-1", "api", true, 511,
			core.MailboxEventBodyPlainText{Text: "A plain event from the CLI."}),
		projectionMailboxEvent("subagent-finished:child-1", "subagent", false, 512,
			core.MailboxEventBodySubagentFinished{Report: &report}),
		projectionMailboxEvent("github:repo#4623:merged:interested", "github", false, 513,
			core.MailboxEventBodyGithubPullRequestLifecycle{
				Change: core.MailboxPullRequestChangeMerged, Number: 4623, Title: pullTitle,
				Actor: "dana", Recipient: core.LifecycleRecipientInterested,
			}),
	}
	for index, event := range events {
		projectionApplyObservation(t, &session, projectionQueued(fmt.Sprintf("mixed-event-%d", index),
			core.RelayCommandDeliverMailboxEvent{Event: event}, int64(event.CreatedAtMs)))
	}
	projectionApplyObservation(t, &session, projectionQueued("mixed-event-wake", core.RelayCommandMailboxWake{Events: events}, 600))

	// A wake starts an autonomous turn, with no invented local user prompt.
	projectionApplyObservation(t, &session, projectionStarted("mixed-event-wake", 700))
	projectionApplyObservation(t, &session, projectionUntaggedAgentChunk("Working on the delivered messages."))
	// Hook acknowledgments can carry a batch, and a replay must retain each
	// original position rather than append another copy.
	hookMessages := []core.MailboxEvent{
		projectionMailboxEvent("hook:one", "session_message", false, 710,
			core.MailboxEventBodySessionMessage{
				From: core.Sender{Type: core.SenderSession, ID: "review-session", Title: "Review agent"},
				Text: "The first review item.\n支持 Unicode and **markdown**.",
			}),
		projectionMailboxEvent("hook:two", "session_message", false, 711,
			core.MailboxEventBodySessionMessage{
				From: core.Sender{Type: core.SenderSession, ID: "untitled-session"},
				Text: "The second item arrives in the same acknowledgment.",
			}),
		projectionMailboxEvent("github:repo#4623:review-comment:9", "github", false, 712,
			core.MailboxEventBodyGithubReviewComment{
				ItemKind: core.GithubItemKindPullRequest, Number: 4623, Title: pullTitle,
				Author: "gina", Body: "A late review note arrives mid-turn.",
			}),
	}
	delivered := projectionHookDelivery(hookMessages, "PostToolUse", "batch-lease")
	projectionApplyObservation(t, &session, delivered)
	projectionApplyObservation(t, &session, delivered)
	projectionApplyObservation(t, &session, projectionUntaggedAgentChunk("Responding after delivery."))

	question, err := core.ElicitationRequestFromACPParams("away-question", json.RawMessage(`{
		"mode":"form", "sessionId":"session", "message":"Claude Code requests permission:\ntouch afk-created.txt",
		"requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	projectionApplyObservation(t, &session, core.RelayObservationElicitationRequested{Request: question})
	projectionApplyObservation(t, &session, core.RelayObservationElicitationResolved{
		ElicitationID: question.ID,
		Action:        core.OperatorAwayAction,
	})
	notices := []core.MailboxEvent{
		projectionMailboxEvent("operator-away", "operator_away", true, 1000,
			core.MailboxEventBodyOperatorAway{Request: "Claude Code requests permission:\ntouch afk-created.txt"}),
		projectionMailboxEvent("operator-back", "operator_away", true, 1001,
			core.MailboxEventBodyOperatorBack{Requests: []string{"Claude Code requests permission:\ntouch afk-created.txt"}}),
	}
	projectionApplyObservation(t, &session, projectionHookDelivery(notices, "PostToolUse", "operator-notices"))

	core.AssertGolden(t, "testdata", "mailbox-delivery-transcript", projectionRenderedTranscript(&session))
}

// Session 3c3f7e29: a week of GitHub events accumulated in one row at the
// first event's position, so each wake read "Agent continued on its own" with
// no visible cause. A replayed delivery must also find its row when the row
// is older than the live window, or the projector adds a second copy.
func TestMailboxDeliveriesReferenceTheirExternalEventsRow(t *testing.T) {
	commented := "commented"
	github := core.MailboxEvent{
		Key:         "github:repo#7:review:1",
		Source:      "github",
		Wake:        true,
		CreatedAtMs: 1,
		Body: core.MailboxEventBodyGithubReview{
			ItemKind:    core.GithubItemKindPullRequest,
			Number:      7,
			Title:       "Delivery rows",
			Author:      "coderabbitai[bot]",
			ReviewState: &commented,
		},
	}
	message := core.MailboxEvent{
		Key:         "parent:message:1",
		Source:      "parent",
		Wake:        true,
		CreatedAtMs: 2,
		Body:        core.MailboxEventBodyParentMessage{Text: "Check the review."},
	}
	const rowID = "system:mailbox:delivered:github:repo#7:review:1"
	session := core.MaterializedSessionEmpty("references")
	for _, check := range []struct {
		observation core.RelayObservation
		expected    bool
	}{
		{projectionQueued("mailbox-wake-1", core.RelayCommandMailboxWake{Events: []core.MailboxEvent{message, github}}, 3), true},
		{projectionHookDelivery([]core.MailboxEvent{github}, "PostToolBatch", "lease"), true},
		{projectionHookDelivery([]core.MailboxEvent{message}, "PostToolBatch", "lease"), false},
		// A queued event has not reached the agent and makes no row.
		{projectionQueued("queued", core.RelayCommandDeliverMailboxEvent{Event: github}, 4), false},
	} {
		references, err := HistoricalReferencesOf([]core.RelayEvent{projectionEvent(t, &session, check.observation)})
		if err != nil {
			t.Fatal(err)
		}
		if got := slices.Contains(references.IDs, rowID); got != check.expected {
			t.Fatalf("%#v: references row = %v, want %v (%v)", check.observation, got, check.expected, references.IDs)
		}
		if slices.Contains(references.IDs, "system:mailbox:external-events") {
			t.Fatalf("%#v: references the legacy accumulated row", check.observation)
		}
	}

	queuedOnly := core.MaterializedSessionEmpty("queued-only")
	projectionApplyObservation(t, &queuedOnly, projectionQueued("queued", core.RelayCommandDeliverMailboxEvent{Event: github}, 4))
	if len(queuedOnly.Transcript) != 0 {
		t.Fatalf("a queued event made a row: %#v", queuedOnly.Transcript)
	}
}

// Hard-won: 60145fdb: leaving Claude Plan mode selected default Manual permissions instead of the saved policy.
func TestExecutionModeRestorationReportsDurableSuccessAndFailureToClients(t *testing.T) {
	session := core.MaterializedSessionEmpty("restore-mode")
	refused := "auto mode refused"
	kind := core.RelayCommandKindRestoreExecutionMode
	for _, check := range []struct {
		observation core.RelayObservation
		expected    *string
	}{
		{core.RelayObservationCommandCompleted{Command: &kind, CommandID: "restore-mode", Outcome: core.RelayCommandOutcomeConfigured{}}, nil},
		{core.RelayObservationCommandRejected{CommandID: "restore-mode", Command: kind, Message: refused}, &refused},
	} {
		event := projectionEvent(t, &session, check.observation)
		projected, err := ProjectRelayEvent(&session, &event)
		if err != nil {
			t.Fatal(err)
		}
		want := []core.MaterializedConfigResult{{ConfigID: "restore-mode", Value: check.expected}}
		if !reflect.DeepEqual(projected.Mutation.ConfigResults, want) {
			t.Fatalf("config results = %#v, want %#v", projected.Mutation.ConfigResults, want)
		}
	}
}

// Hard-won: 61a1cfaf: session loading discarded Codex goal and command-catalogue state across config replacement.
func TestConfigurationReplacementPreservesGoalsAndRemovesObsoleteAdapterSettings(t *testing.T) {
	session := core.MaterializedSessionEmpty("goal-config")
	projectionApplyObservation(t, &session, projectionUpdateJSON(t, `{
		"sessionUpdate": "session_info_update", "_meta": {
			"mjGoalCapability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
			"goal": {"objective": "finish", "status": "paused", "createdAt": 1, "tokensUsed": 42, "tokenBudget": 1000},
			"execution": {"version": 1, "revision": 2, "status": "idle"},
			"mjGoalResumeAnswered": "answered"
		}
	}`))
	if session.Configuration.Goal == nil {
		t.Fatal("the goal was not projected")
	}
	goal := *session.Configuration.Goal
	pending := "pending"
	goal.PendingResume = &pending
	goal.Decision = &core.GoalDecision{Goal: *goal.Snapshot, Resume: true}
	installed := goal
	session.Configuration.Goal = &installed
	for _, viaNotification := range []bool{false, true} {
		session.Configuration.Values["obsolete"] = true
		const options = `[{
			"id": "model", "name": "Model", "type": "select", "currentValue": "astra",
			"options": [{"value": "astra", "name": "Astra"}]
		}]`
		var observation core.RelayObservation
		if viaNotification {
			observation = projectionUpdateJSON(t, `{"sessionUpdate": "config_option_update", "configOptions": `+options+`}`)
		} else {
			var configOptions []schema.SessionConfigOption
			if err := json.Unmarshal([]byte(options), &configOptions); err != nil {
				t.Fatal(err)
			}
			observation = core.RelayObservationSessionConfigured{ConfigOptions: configOptions}
		}
		projectionApplyObservation(t, &session, observation)
		projectionAssertJSONEqual(t, session.Configuration.Goal, &goal, "goal")
		if !reflect.DeepEqual(session.Configuration.Values, map[string]any{"model": "astra"}) {
			t.Fatalf("values = %#v", session.Configuration.Values)
		}

		canonical, err := CanonicalSessionFromMaterialized(&session)
		if err != nil {
			t.Fatal(err)
		}
		archivedText := []byte(projectionJSONText(t, canonical))
		archived := projectionParse(t, archivedText)
		archivedGoal := projectionMember(projectionMember(projectionMember(archived, "session"), "configuration"), "mj_goal_state")
		if !jsonvalue.Equal(archivedGoal, projectionParse(t, []byte(projectionJSONText(t, goal)))) {
			t.Fatalf("archived goal = %#v", archivedGoal)
		}
		var decoded core.CanonicalSessionSnapshot
		if err := json.Unmarshal(archivedText, &decoded); err != nil {
			t.Fatal(err)
		}
		restored, err := MaterializedSessionFromCanonical("goal-config", &decoded)
		if err != nil {
			t.Fatal(err)
		}
		projectionAssertJSONEqual(t, restored.Configuration, session.Configuration, "restored configuration")
	}
	cleared := projectionClone(&session)
	projectionApplyObservation(t, &cleared, core.RelayObservationCommandCompleted{
		CommandID: "clear",
		Outcome:   core.RelayCommandOutcomeContextCleared{NativeSessionID: "new"},
	})
	projectionAssertJSONEqual(t, cleared.Configuration.Goal, &core.GoalState{Capability: goal.Capability, Known: true}, "cleared goal")
	projectionAssertJSONEqual(t, cleared.Configuration.Values, session.Configuration.Values, "cleared values")
	projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
	goal.Restart()
	projectionAssertJSONEqual(t, session.Configuration.Goal, &goal, "restarted goal")
	if session.Configuration.Values["model"] != "astra" {
		t.Fatalf("model = %#v", session.Configuration.Values["model"])
	}
}

func TestResumeOpenUpdatesOperationalStateWithoutAddingTranscriptNoise(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	resumed := projectionEvent(t, &session, core.RelayObservationSessionOpened{NativeSessionID: "native", Resumed: true})
	projected, err := ProjectRelayEvent(&session, &resumed)
	if err != nil {
		t.Fatal(err)
	}
	if len(projected.Mutation.Transcript) != 0 {
		t.Fatalf("transcript = %#v", projected.Mutation.Transcript)
	}
	// Forms the new bridge issued before the open stay live (2a0aae9d8).
	if projected.Mutation.PendingElicitations != nil {
		t.Fatal("a resumed open cleared pending forms")
	}

	started := projectionEvent(t, &session, core.RelayObservationSessionOpened{NativeSessionID: "native"})
	projected, err = ProjectRelayEvent(&session, &started)
	if err != nil {
		t.Fatal(err)
	}
	if len(projected.Mutation.Transcript) != 1 {
		t.Fatalf("transcript = %#v", projected.Mutation.Transcript)
	}
}

// projectionStartPromptAndQueueASteer starts prompt-1, queues prompt-2
// behind it, and admits a steer of prompt-2 into the running turn. It
// returns the two acceptance ordinals.
func projectionStartPromptAndQueueASteer(t testing.TB, session *core.MaterializedSession) (uint64, uint64) {
	t.Helper()
	queue := func(id string) uint64 {
		projectionApplyObservation(t, session, projectionQueued(id, projectionPrompt(id), 10))
		return session.AppliedEventOrdinal
	}
	first := queue("prompt-1")
	projectionApplyObservation(t, session, projectionStarted("prompt-1", 20))
	second := queue("prompt-2")
	projectionApplyObservation(t, session, projectionQueued("steer-1",
		core.RelayCommandSteer{ActivePromptID: "prompt-1", QueuedPromptID: "prompt-2"}, 30))
	return first, second
}

func projectionSteerPrompt2(t testing.TB, session *core.MaterializedSession) {
	t.Helper()
	projectionApplyObservation(t, session, core.RelayObservationCommandCompleted{
		CommandID: "steer-1",
		Outcome:   core.RelayCommandOutcomeSteered{QueuedCommandID: "prompt-2"},
	})
}

func projectionSettled(start uint64, stopReason string) core.RelayObservationHarnessTurnSettled {
	return core.RelayObservationHarnessTurnSettled{
		Autonomous: &core.SettledHarnessTurn{StartOrdinal: start, StopReason: stopReason},
	}
}

func TestASteeredPromptFinishesWithTheTurnItJoined(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	first, second := projectionStartPromptAndQueueASteer(t, &session)
	projectionSteerPrompt2(t, &session)
	if session.ActiveTurn == nil {
		t.Fatal("the running turn is missing")
	}
	turn := *session.ActiveTurn
	if turn.CommandID != "prompt-2" || turn.SteeredInto == nil || *turn.SteeredInto != "prompt-1" {
		t.Fatalf("turn = %#v", turn)
	}

	// Hard-won: #1450 review: a queued prompt rejected under a steered turn marked the running prompt answered
	projectionApplyObservation(t, &session, projectionQueued("prompt-3", projectionPrompt("third"), 31))
	third := session.AppliedEventOrdinal
	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "prompt-3",
		Command:   core.RelayCommandKindPrompt,
		Message:   "refused",
	})
	rejected := session.LastTurnOutcome
	if rejected == nil {
		t.Fatal("the rejection has no outcome")
	}
	if !rejected.Answers(third) || rejected.Answers(first) || rejected.Answers(second) {
		t.Fatalf("rejection answers: third %v first %v second %v", rejected.Answers(third), rejected.Answers(first), rejected.Answers(second))
	}

	projectionApplyObservation(t, &session, projectionPromptCompleted("prompt-1", "EndTurn"))
	if session.ActiveTurn != nil {
		t.Fatal("the turn kept running")
	}
	outcome := session.LastTurnOutcome
	if outcome == nil || outcome.CommandID != "prompt-1" {
		t.Fatalf("outcome = %#v", outcome)
	}
	// `mj wait --turn` finishes once an outcome's acceptance ordinal reaches
	// its target, so a waiter for either prompt now finishes.
	if outcome.AcceptedOrdinal == nil || *outcome.AcceptedOrdinal != second || second <= first {
		t.Fatalf("accepted ordinal = %v, second %d, first %d", outcome.AcceptedOrdinal, second, first)
	}
	if outcome.TurnStartPosition == nil || *outcome.TurnStartPosition != turn.TurnStartPosition {
		t.Fatalf("turn start = %v", outcome.TurnStartPosition)
	}
	if !outcome.Answers(first) || !outcome.Answers(second) {
		t.Fatal("the outcome does not answer both prompts")
	}

	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 35})
	firstHarness := session.AppliedEventOrdinal
	projectionApplyObservation(t, &session, projectionSettled(firstHarness, "end_turn"))
	if !session.LastTurnOutcome.Answers(second) {
		t.Fatal("a harness turn forgot the answered prompt")
	}

	// Hard-won: #1450 review: a prompt removed from the queue counted as answered by a later harness turn
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 40})
	start := session.AppliedEventOrdinal
	// Hard-won: #1450 review: a queued configuration change held back the answer for a removed prompt
	projectionApplyObservation(t, &session, projectionQueued("set-config", core.RelayCommandSetConfig{Key: "model", Value: "gpt-9"}, 40))
	projectionApplyObservation(t, &session, projectionQueued("removed", projectionPrompt("removed"), 41))
	removed := session.AppliedEventOrdinal
	removeKind := core.RelayCommandKindRemoveQueuedPrompt
	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		Command:   &removeKind,
		CommandID: "remove",
		Outcome:   core.RelayCommandOutcomeQueueChanged{RemovedCommandIds: []string{"removed"}},
	})
	if session.LastTurnOutcome.Answers(removed) {
		t.Fatal("the queue change answered the removed prompt")
	}
	projectionApplyObservation(t, &session, projectionSettled(start, "end_turn"))
	outcome = session.LastTurnOutcome
	if !outcome.Answers(first) || !outcome.Answers(second) || !outcome.Answers(removed) {
		t.Fatalf("outcome %#v does not answer every settled prompt", outcome)
	}
}

func TestAPostToolBatchSteerHasTheSameTurnProjectionAsACPSteering(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionStartPromptAndQueueASteer(t, &session)
	hookEvent, leaseID := "PostToolBatch", "hook-lease"
	delivery := projectionEvent(t, &session, core.RelayObservationMailboxEventsDelivered{
		EventKeys: []string{},
		Path:      core.MailboxDeliveryPathToolHook,
		HookEvent: &hookEvent,
		Events:    []core.MailboxEvent{},
		LeaseID:   &leaseID,
		Steering: &core.MailboxHookSteering{
			CommandID:      "steer-1",
			ActivePromptID: "prompt-1",
			QueuedPromptID: "prompt-2",
		},
	})
	projected, err := ProjectRelayEvent(&session, &delivery)
	if err != nil {
		t.Fatal(err)
	}
	events := projected.Mutation.APIEvents
	if len(events) < 2 {
		t.Fatalf("api events = %#v", events)
	}
	started, ok := events[0].(core.ApiEventDataTurnStarted)
	if !ok || started.Turn.CommandID != "prompt-2" || started.Turn.SteeredInto == nil || *started.Turn.SteeredInto != "prompt-1" {
		t.Fatalf("first api event = %#v", events[0])
	}
	ended, ok := events[1].(core.ApiEventDataCommandEnded)
	if !ok || ended.Result.CommandID != "steer-1" || ended.Result.Outcome != core.CommandResultKindSucceeded {
		t.Fatalf("second api event = %#v", events[1])
	}
	projectionApply(t, &session, delivery)
	if len(session.QueuedPrompts) != 0 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}
	if session.ActiveTurn == nil || session.ActiveTurn.CommandID != "prompt-2" ||
		session.ActiveTurn.SteeredInto == nil || *session.ActiveTurn.SteeredInto != "prompt-1" {
		t.Fatalf("active turn = %#v", session.ActiveTurn)
	}
	found := false
	for _, item := range session.Transcript {
		if body, ok := item.Body.(core.TranscriptBodyUser); ok && item.StableID == "user:prompt-2" &&
			core.MaterializedContentText(body.Content) == "prompt-2" {
			found = true
		}
	}
	if !found {
		t.Fatalf("the steered prompt has no user row: %#v", session.Transcript)
	}
}

func TestAnInterruptedSteeredTurnClearsTheRunningTurn(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	_, second := projectionStartPromptAndQueueASteer(t, &session)
	projectionSteerPrompt2(t, &session)

	projectionApplyObservation(t, &session, core.RelayObservationCommandInterrupted{
		CommandID: "prompt-1",
		Command:   core.RelayCommandKindPrompt,
		Message:   "stopped",
	})
	if session.ActiveTurn != nil {
		t.Fatal("the steered turn kept running")
	}
	outcome := session.LastTurnOutcome
	if outcome == nil || outcome.AcceptedOrdinal == nil || *outcome.AcceptedOrdinal != second {
		t.Fatalf("outcome = %#v", outcome)
	}
	if !reflect.DeepEqual(outcome.Outcome, core.TurnOutcomeKindInterrupted{Message: "stopped"}) {
		t.Fatalf("outcome kind = %#v", outcome.Outcome)
	}
}

func TestAReturnedSteerLeavesThePromptQueuedAndTheTurnRunning(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionStartPromptAndQueueASteer(t, &session)
	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		CommandID: "steer-1",
		Outcome:   core.RelayCommandOutcomeSteeringReturned{QueuedCommandID: "prompt-2"},
	})
	if session.ActiveTurn == nil || session.ActiveTurn.CommandID != "prompt-1" {
		t.Fatalf("active turn = %#v", session.ActiveTurn)
	}
	if len(session.QueuedPrompts) != 1 || session.QueuedPrompts[0].CommandID != "prompt-2" {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}
}

func TestARejectedQueuedPromptRecordsItsAcceptanceOrdinalWithoutATurnStart(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, projectionQueued("running", projectionPrompt("run"), 1))
	running := session.AppliedEventOrdinal
	projectionApplyObservation(t, &session, projectionStarted("running", 2))

	projectionApplyObservation(t, &session, projectionQueued("prompt-1", projectionPrompt("go"), 10))
	accepted := session.AppliedEventOrdinal

	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "prompt-1",
		Command:   core.RelayCommandKindPrompt,
		Message:   "transport failed",
	})

	if session.ActiveTurn == nil {
		t.Fatal("the running turn ended")
	}
	if len(session.QueuedPrompts) != 0 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}
	outcome := session.LastTurnOutcome
	if outcome == nil {
		t.Fatal("no outcome")
	}
	if !outcome.Answers(accepted) || outcome.Answers(running) {
		t.Fatal("the rejection answers the wrong prompts")
	}
	if outcome.AcceptedOrdinal == nil || *outcome.AcceptedOrdinal != accepted {
		t.Fatalf("accepted ordinal = %v", outcome.AcceptedOrdinal)
	}
	if outcome.TurnStartPosition != nil {
		t.Fatal("a prompt that never started has no turn in the transcript")
	}
	if !reflect.DeepEqual(outcome.Outcome, core.TurnOutcomeKindRejected{Message: "transport failed"}) {
		t.Fatalf("outcome kind = %#v", outcome.Outcome)
	}

	projectionApplyObservation(t, &session, projectionAgentChunk("partial output", "answer"))
	if !projectionIsRunning(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if !projectionStreamingAgent(&session) {
		t.Fatal("no open agent stream")
	}
	// A store projected before revision 28 has a user row but no active turn.
	session.ActiveTurn = nil
	if projectionFindItem(&session, "user:running") == nil {
		t.Fatal("no user row for the running prompt")
	}

	// Hard-won: 566f76395: rejecting a prompt started before upgrade left execution running.
	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "running",
		Command:   core.RelayCommandKindPrompt,
		Message:   "transport failed",
	})
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if projectionStreamingAgent(&session) {
		t.Fatal("an agent stream stayed open")
	}
}

func TestQueuedPromptsKeepTheirOwnAcceptanceOrdinalsThroughTheirTurns(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	var accepted []uint64
	for _, commandID := range []string{"prompt-a", "prompt-b"} {
		projectionApplyObservation(t, &session, projectionQueued(commandID, projectionPrompt("go"), 10))
		accepted = append(accepted, session.AppliedEventOrdinal)
	}
	// The second prompt is accepted before the first one starts, which is
	// exactly the ordering that makes "newest turn" the wrong answer.
	if accepted[1] <= accepted[0] {
		t.Fatalf("accepted = %v", accepted)
	}

	for index, commandID := range []string{"prompt-a", "prompt-b"} {
		projectionApplyObservation(t, &session, projectionStarted(commandID, 20))
		projectionApplyObservation(t, &session, projectionPromptCompleted(commandID, "EndTurn"))
		outcome := session.LastTurnOutcome
		if outcome == nil || outcome.AcceptedOrdinal == nil || *outcome.AcceptedOrdinal != accepted[index] {
			t.Fatalf("%s must report the ordinal its own submission returned: %#v", commandID, outcome)
		}
	}
}

// Hard-won: 1389c545: autonomous Claude turns were treated as idle, allowing mid-turn checkpoints and losing recovery work.
func TestAHarnessTurnRunsTheSessionAndMarksWhereItBegan(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")

	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4_200})

	if !reflect.DeepEqual(session.Execution, core.MaterializedExecutionStateRunning{StartedAtMs: 4_200}) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	marker := session.Transcript[len(session.Transcript)-1]
	if marker.StableID != core.HarnessTurnItemPrefix+"1" {
		t.Fatalf("marker = %q", marker.StableID)
	}
	if !marker.IsTurnStart() {
		t.Fatal("the marker does not start a turn")
	}
	if text, ok := projectionSystemText(marker); !ok || text != core.HarnessTurnText {
		t.Fatalf("marker body = %#v", marker.Body)
	}

	projectionApplyObservation(t, &session, projectionAgentChunk("picking this back up", "answer-1"))
	if !projectionStreamingAgent(&session) {
		t.Fatal("output inside the turn streams into a fresh item")
	}

	origin := "task-notification"
	settled := projectionEvent(t, &session, core.RelayObservationHarnessTurnSettled{
		Autonomous: &core.SettledHarnessTurn{StartOrdinal: 1, StopReason: "end_turn"},
		Origin:     &origin,
	})
	projected, err := ProjectRelayEvent(&session, &settled)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(projected.Mutation.APIEvents, func(event core.ApiEventData) bool {
		_, ended := event.(core.ApiEventDataTurnEnded)
		return ended
	}) {
		t.Fatalf("api events = %#v", projected.Mutation.APIEvents)
	}
	if err := ApplyCommittedProjectionEvent(&session, &settled, projected.Mutation); err != nil {
		t.Fatal(err)
	}

	outcome := session.LastTurnOutcome
	if outcome.CommandID != "harness-turn-1" {
		t.Fatalf("command id = %q", outcome.CommandID)
	}
	if outcome.TurnStartPosition == nil || *outcome.TurnStartPosition != 1 {
		t.Fatalf("turn start = %v", outcome.TurnStartPosition)
	}
	if !reflect.DeepEqual(outcome.Outcome, core.TurnOutcomeKindCompleted{StopReason: "end_turn"}) {
		t.Fatalf("outcome kind = %#v", outcome.Outcome)
	}
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if projectionStreamingAgent(&session) {
		t.Fatal("settling closes the streams a canonical export refuses to hold open")
	}
	if ordinal, ok := core.LatestCompletedTurnOrdinal(&session); !ok || ordinal != 1 {
		t.Fatalf("the finished turn is covered from the marker that began it: %d, %v", ordinal, ok)
	}
	window := core.ProjectionWindowOf(&session)
	if window.LatestTurnStartPosition == nil || *window.LatestTurnStartPosition != 1 {
		t.Fatalf("latest turn start = %v", window.LatestTurnStartPosition)
	}
}

// Hard-won: #1450 review: a Codex tail reused an older autonomous marker.
func TestNativeTailPreservesPromptOutcomeAndAutonomousErrorIsRetained(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 1})
	projectionApplyObservation(t, &session, projectionSettled(1, "error"))
	if !reflect.DeepEqual(session.LastTurnOutcome.Outcome, core.TurnOutcomeKindCompleted{StopReason: "error"}) {
		t.Fatalf("outcome = %#v", session.LastTurnOutcome.Outcome)
	}
	prompt := *session.LastTurnOutcome
	prompt.CommandID = "prompt"
	prompt.AcceptedOrdinal = uint64Pointer(3)
	session.LastTurnOutcome = &prompt
	completed := projectionJSONText(t, session.LastTurnOutcome)
	origin := "codex"
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnSettled{Origin: &origin})
	if got := projectionJSONText(t, session.LastTurnOutcome); got != completed {
		t.Fatalf("outcome changed:\n got: %s\nwant: %s", got, completed)
	}
}

func TestAHarnessTurnWithoutOutputKeepsItsDividerAndStopsItsClock(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4200})
	if !reflect.DeepEqual(session.Execution, core.MaterializedExecutionStateRunning{StartedAtMs: 4200}) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	origin := "autonomous_task_ended"
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnSettled{Origin: &origin})
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	marker := session.Transcript[0]
	if marker.StableID != "harness-turn:1" {
		t.Fatalf("marker = %q", marker.StableID)
	}
	if text, ok := projectionSystemText(marker); !ok || text != core.HarnessTurnText {
		t.Fatalf("marker body = %#v", marker.Body)
	}
	if ordinal, ok := core.LatestCompletedTurnOrdinal(&session); !ok || ordinal != 1 {
		t.Fatalf("latest completed turn = %d, %v", ordinal, ok)
	}
}

// Hard-won: db2e99a5: a native turn settling beneath an accepted prompt incorrectly made the session idle.
func TestATurnThatSettlesUnderAnInFlightPromptKeepsTheSessionRunning(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4_200})
	// A prompt typed mid-turn dispatches at once, so it is still running
	// when the harness reaches the boundary of the turn it started.
	projectionApplyObservation(t, &session, projectionQueued("prompt-1", projectionPrompt("go"), 10))
	projectionApplyObservation(t, &session, projectionStarted("prompt-1", 20))
	projectionApplyObservation(t, &session, projectionAgentChunk("still writing", "answer-1"))

	origin := "task-notification"
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnSettled{Origin: &origin, PromptInFlight: true})

	if !projectionIsRunning(&session) {
		t.Fatal("the prompt is still running, so the session is not idle")
	}
	if !projectionStreamingAgent(&session) {
		t.Fatal("the prompt's own answer keeps streaming into its item")
	}

	projectionApplyObservation(t, &session, projectionPromptCompleted("prompt-1", "end_turn"))

	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if projectionStreamingAgent(&session) {
		t.Fatal("an agent stream stayed open")
	}
}

func TestFinishingAnACPPromptPreservesALaterNativeGoalStream(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4200})
	projectionApplyObservation(t, &session, projectionUpdateJSON(t,
		`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"finish","status":"active"},"execution":{"version":1,"status":"running","turnId":"later"}}}`))
	projectionApplyObservation(t, &session, projectionAgentChunk("autonomous work", "later-answer"))
	projectionApplyObservation(t, &session, projectionPromptCompleted("initial-prompt", "end_turn"))
	if !projectionIsRunning(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if !projectionStreamingAgent(&session) {
		t.Fatal("the native goal stream was closed")
	}
	origin := "codex"
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnSettled{Origin: &origin})
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
}

// Hard-won: #1450 follow-up: a cancel during a Codex native tail ended an older harness turn
func TestCancellingACodexNativeTailPreservesThePromptOutcome(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 1})
	projectionApplyObservation(t, &session, projectionSettled(1, "end_turn"))
	projectionApplyObservation(t, &session, projectionQueued("prompt", projectionPrompt("go"), 2))
	projectionApplyObservation(t, &session, projectionStarted("prompt", 3))
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4})
	projectionApplyObservation(t, &session, projectionPromptCompleted("prompt", "end_turn"))
	outcome := projectionJSONText(t, session.LastTurnOutcome)
	cancelKind := core.RelayCommandKindCancelTurn
	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		Command:   &cancelKind,
		CommandID: "cancel",
		Outcome:   core.RelayCommandOutcomeCancelled{},
	})
	origin := "codex"
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnSettled{Origin: &origin})
	if got := projectionJSONText(t, session.LastTurnOutcome); got != outcome {
		t.Fatalf("outcome changed:\n got: %s\nwant: %s", got, outcome)
	}
	for _, item := range session.Transcript {
		if strings.HasPrefix(item.StableID, core.TurnInterruptedItemPrefix) {
			t.Fatalf("an interrupted row was added: %#v", item)
		}
	}
}

// Hard-won: 1389c545: worker restart left an open autonomous turn that canonical checkpoint export rejected.
func TestARestartDuringAHarnessTurnLeavesAnIdleSessionWithNoOpenStreams(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 4_200})
	projectionApplyObservation(t, &session, projectionAgentChunk("half a sentence", "answer-1"))

	projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})

	if got := session.UnreadInterruptionsAfter(0); got != 1 {
		t.Fatalf("unread interruptions = %d", got)
	}
	if got := session.UnreadInterruptionsAfter(session.AppliedEventOrdinal); got != 0 {
		t.Fatalf("unread interruptions after the frontier = %d", got)
	}

	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	for _, item := range session.Transcript {
		switch body := item.Body.(type) {
		case core.TranscriptBodyAgent:
			if body.Streaming {
				t.Fatalf("open agent stream %q", item.StableID)
			}
		case core.TranscriptBodyThought:
			if body.Streaming {
				t.Fatalf("open thought stream %q", item.StableID)
			}
		}
	}
	if _, err := CanonicalSessionFromMaterialized(&session); err != nil {
		t.Fatalf("a restarted session exports without open streams: %v", err)
	}
}

// Hard-won: 1389c545: a plan from a self-started turn overwrote the previous turn's plan.
func TestAPlanFromAHarnessTurnDoesNotOverwriteThePreviousTurnsPlan(t *testing.T) {
	plan := func(content string) core.RelayObservation {
		return projectionUpdate(schema.SessionUpdate{
			Kind: schema.SessionUpdateKindPlan,
			Plan: &schema.Plan{Entries: []schema.PlanEntry{{
				Content:  content,
				Priority: schema.PlanEntryPriorityHigh,
				Status:   schema.PlanEntryStatusInProgress,
			}}},
		})
	}
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, projectionQueued("prompt-1", projectionPrompt("go"), 10))
	projectionApplyObservation(t, &session, projectionStarted("prompt-1", 20))
	projectionApplyObservation(t, &session, plan("first turn plan"))
	projectionApplyObservation(t, &session, projectionPromptCompleted("prompt-1", "end_turn"))

	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 30})
	projectionApplyObservation(t, &session, plan("second turn plan"))

	plans := 0
	for _, item := range session.Transcript {
		if _, ok := item.Body.(core.TranscriptBodyPlan); ok {
			plans++
		}
	}
	if plans != 2 {
		t.Fatalf("the self-started turn keeps its own plan instead of rewriting the last one: %d plans", plans)
	}
}

// Hard-won: d2f26b4c6: a recovery note broke slash-command presentation.
func TestInterruptedPromptBeforeRestartProducesOneUnreadInterruption(t *testing.T) {
	for _, restartFirst := range []bool{false, true} {
		for _, variant := range []struct {
			original      string
			nativeRunning bool
		}{{"go", false}, {"/compact", false}, {"go", true}, {"/compact", true}} {
			recoveryID := core.RestartContinuationID("prompt")
			session := core.MaterializedSessionEmpty("session")
			projectionApplyObservation(t, &session, projectionQueued("prompt", projectionPrompt(variant.original), 10))
			projectionApplyObservation(t, &session, projectionStarted("prompt", 20))
			// Hard-won: 243c9ecff: a native running update kept an interrupted prompt running.
			if variant.nativeRunning {
				projectionApplyObservation(t, &session, projectionUpdateJSON(t,
					`{"sessionUpdate":"session_info_update","_meta":{"execution":{"version":1,"status":"running","turnId":"prompt"}}}`))
				projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 21})
				if !session.Configuration.Goal.Running() {
					t.Fatal("the native execution is not running")
				}
			}
			if restartFirst {
				// Hard-won: b8acf76: a restart before its prompt interruption emitted two rows.
				projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
			}
			restarted := core.OutcomeReasonWorkerRestarted
			projectionApplyObservation(t, &session, core.RelayObservationCommandInterrupted{
				ContinuationID: &recoveryID,
				Reason:         &restarted,
				CommandID:      "prompt",
				Command:        core.RelayCommandKindPrompt,
				Message:        "worker restarted",
			})
			if accepted := session.QueuedPrompts[0].AcceptedOrdinal; accepted == nil || *accepted != 1 {
				t.Fatalf("recovery accepted ordinal = %v", accepted)
			}
			interruptedAt := session.AppliedEventOrdinal
			if !projectionIsIdle(&session) {
				t.Fatalf("execution = %#v", session.Execution)
			}
			projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
			if got := session.InterruptionEventOrdinals(); !reflect.DeepEqual(got, []uint64{interruptedAt}) {
				t.Fatalf("interruptions = %v, want [%d]", got, interruptedAt)
			}
			if got := session.UnreadInterruptionsAfter(interruptedAt); got != 0 {
				t.Fatalf("unread interruptions = %d", got)
			}
			projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
			if got := session.InterruptionEventOrdinals(); !reflect.DeepEqual(got, []uint64{interruptedAt}) {
				t.Fatalf("interruptions = %v, want [%d]", got, interruptedAt)
			}
			projectionApplyObservation(t, &session, projectionStarted(recoveryID, 30))
			if accepted := session.ActiveTurn.AcceptedOrdinal; accepted == nil || *accepted != 1 {
				t.Fatalf("active accepted ordinal = %v", accepted)
			}
			item := projectionFindItem(&session, "user:"+recoveryID)
			if item == nil {
				t.Fatal("missing recovery prompt")
			}
			body, ok := item.Body.(core.TranscriptBodyUser)
			if !ok {
				t.Fatal("missing recovery prompt")
			}
			text := func(index int) any { return projectionMember(projectionParse(t, body.Content[index]), "text") }
			if strings.HasPrefix(variant.original, "/") {
				if len(body.Content) != 1 || text(0) != variant.original {
					t.Fatalf("content = %s", body.Content)
				}
			} else {
				if len(body.Content) != 2 || text(0) != core.RestartRecoveryNote || text(1) != variant.original {
					t.Fatalf("content = %s", body.Content)
				}
			}
		}
	}
}

// Hard-won: d8947861e: interrupting a mailbox wake left the projected session running.
func TestInterruptedMailboxWakeWithContinuationReturnsToIdle(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	const commandID = "mailbox-wake"
	continuationID := core.RestartContinuationID(commandID)
	projectionApplyObservation(t, &session, projectionQueued(commandID, core.RelayCommandMailboxWake{Events: []core.MailboxEvent{{
		Key:         "parent:message:1",
		Source:      "parent",
		Wake:        true,
		CreatedAtMs: 10,
		Body:        core.MailboxEventBodyParentMessage{Text: "Check the review."},
	}}}, 10))
	projectionApplyObservation(t, &session, projectionStarted(commandID, 20))
	if !projectionIsRunning(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	restarted := core.OutcomeReasonWorkerRestarted
	projectionApplyObservation(t, &session, core.RelayObservationCommandInterrupted{
		ContinuationID: &continuationID,
		Reason:         &restarted,
		CommandID:      commandID,
		Command:        core.RelayCommandKindPrompt,
		Message:        "worker restarted",
	})
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if session.ActiveTurn != nil {
		t.Fatal("the wake kept running")
	}
	if session.QueuedPrompts[0].CommandID != continuationID {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}
}

// I1-17: a turn the user cancelled ends with an "Interrupted" row, so a
// reader can tell a cut-off reply from a finished one. A finished turn gets
// no such row.
// Hard-won: a1dfcb6b: cancelled prompt output looked complete because the transcript had no Interrupted row.
func TestACancelledTurnEndsWithAnInterruptedRow(t *testing.T) {
	for _, check := range []struct {
		stopReason string
		marked     bool
	}{{"cancelled", true}, {"end_turn", false}} {
		session := core.MaterializedSessionEmpty("session")
		projectionApplyObservation(t, &session, projectionQueued("prompt", projectionPrompt("write a story"), 10))
		projectionApplyObservation(t, &session, projectionStarted("prompt", 20))
		projectionApplyObservation(t, &session, projectionPromptCompleted("prompt", check.stopReason))
		last := session.Transcript[len(session.Transcript)-1]
		text, system := projectionSystemText(last)
		if isMarker := system && text == core.TurnInterruptedText; isMarker != check.marked {
			t.Fatalf("%s: %#v", check.stopReason, session.Transcript)
		}
	}
}

// I2-8: Esc on a turn the harness started on its own (a goal continuation)
// cancelled it but left no "Interrupted" row and no cancelled outcome, which
// an ordinary cancelled turn gets. A stop with nothing running, or one that
// lands on a prompt of ours, adds neither: the prompt's own completion does.
// Hard-won: c8e52abc: cancelling an autonomous harness turn omitted its Interrupted row and cancelled outcome.
func TestACancelOfATurnTheHarnessStartedEndsItAsInterrupted(t *testing.T) {
	cancel := func(session *core.MaterializedSession, commandID string) {
		projectionApplyObservation(t, session, projectionQueued(commandID, core.RelayCommandCancelTurn{}, 40))
		kind := core.RelayCommandKindCancelTurn
		projectionApplyObservation(t, session, core.RelayObservationCommandCompleted{
			Command:   &kind,
			CommandID: commandID,
			Outcome:   core.RelayCommandOutcomeCancelled{},
		})
	}
	interruptedRows := func(session *core.MaterializedSession) int {
		count := 0
		for _, item := range session.Transcript {
			if text, ok := projectionSystemText(item); ok && text == core.TurnInterruptedText {
				count++
			}
		}
		return count
	}

	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 30})
	var startedAt uint64
	for _, item := range session.Transcript {
		if strings.HasPrefix(item.StableID, core.HarnessTurnItemPrefix) {
			startedAt = item.Position
			break
		}
	}
	if startedAt == 0 {
		t.Fatal("the harness turn marker is missing")
	}
	cancel(&session, "cancel-1")
	if interruptedRows(&session) != 0 || session.LastTurnOutcome != nil {
		t.Fatal("a cancel before the settlement ended the turn")
	}
	settle := projectionSettled(startedAt, "cancelled")
	projectionApplyObservation(t, &session, settle)
	if interruptedRows(&session) != 1 {
		t.Fatalf("interrupted rows: %#v", session.Transcript)
	}
	outcome := session.LastTurnOutcome
	if outcome == nil {
		t.Fatal("a turn outcome is missing")
	}
	if outcome.Result().Kind != core.TurnResultKindCancelled {
		t.Fatalf("result = %#v", outcome.Result())
	}
	if outcome.TurnStartPosition == nil || *outcome.TurnStartPosition != startedAt {
		t.Fatalf("turn start = %v", outcome.TurnStartPosition)
	}
	if outcome.CommandID != core.HarnessTurnID(startedAt) {
		t.Fatalf("command id = %q", outcome.CommandID)
	}

	if outcome.AcceptedOrdinal != nil {
		t.Fatalf("accepted ordinal = %v", outcome.AcceptedOrdinal)
	}
	cancelled := projectionJSONText(t, session.LastTurnOutcome)
	projectionApplyObservation(t, &session, settle)
	if got := projectionJSONText(t, session.LastTurnOutcome); got != cancelled {
		t.Fatalf("a replayed settlement changed the outcome: %s", got)
	}
	if interruptedRows(&session) != 1 {
		t.Fatalf("interrupted rows: %#v", session.Transcript)
	}

	// Nothing running: nothing to interrupt.
	idle := core.MaterializedSessionEmpty("session")
	cancel(&idle, "cancel-idle")
	if interruptedRows(&idle) != 0 || idle.LastTurnOutcome != nil {
		t.Fatal("a cancel with nothing running interrupted something")
	}

	// A prompt of ours is the running turn: its completion writes the row.
	prompted := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &prompted, projectionQueued("prompt", projectionPrompt("write a story"), 10))
	projectionApplyObservation(t, &prompted, projectionStarted("prompt", 20))
	cancel(&prompted, "cancel-prompt")
	if interruptedRows(&prompted) != 0 || prompted.LastTurnOutcome != nil {
		t.Fatal("a cancel of our prompt wrote its row before the prompt completed")
	}
}

func TestStreamedChunksAreOneUnreadLogicalAgentMessage(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionAgentChunk("hel", "answer-1"))
	if latest := session.Transcript[0].LatestContentEventOrdinal; latest == nil || *latest != 1 {
		t.Fatalf("latest content ordinal = %v", latest)
	}
	if session.UnreadAgentMessagesAfter(0) != 1 || session.UnreadAgentMessagesAfter(1) != 0 {
		t.Fatal("unread agent messages after the first chunk")
	}

	projectionApplyObservation(t, &session, projectionAgentChunk("lo", "answer-1"))
	if session.UnreadAgentMessagesAfter(0) != 1 || session.UnreadAgentMessagesAfter(1) != 1 {
		t.Fatal("unread agent messages after the second chunk")
	}
	body, ok := session.Transcript[0].Body.(core.TranscriptBodyAgent)
	if !ok || core.MaterializedChunksText(body.Chunks) != "hello" {
		t.Fatalf("body = %#v", session.Transcript[0].Body)
	}
	if session.Transcript[0].Position != 1 {
		t.Fatalf("position = %d", session.Transcript[0].Position)
	}
	if latest := session.Transcript[0].LatestContentEventOrdinal; latest == nil || *latest != 2 {
		t.Fatalf("latest content ordinal = %v", latest)
	}

	projectionApplyObservation(t, &session, projectionPromptCompleted("prompt-1", "end_turn"))
	if latest := session.Transcript[0].LatestContentEventOrdinal; latest == nil || *latest != 2 {
		t.Fatalf("latest content ordinal = %v", latest)
	}
	if session.UnreadAgentMessagesAfter(2) != 0 {
		t.Fatal("the completion added unread content")
	}
}

// Hard-won: 89ee2de3: late ACP agent chunks reopened a stream that could never close before checkpoint export.
func TestAgentChunkWhileIdleIsRecordedClosed(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.Execution = core.MaterializedExecutionStateIdle{}
	projectionApplyObservation(t, &session, projectionAgentChunk("trailing", "msg-1"))
	item := projectionFindItem(&session, "agent:msg-1")
	if item == nil {
		t.Fatal("trailing chunk recorded")
	}
	body, ok := item.Body.(core.TranscriptBodyAgent)
	if !ok || body.Streaming || core.MaterializedChunksText(body.Chunks) != "trailing" {
		t.Fatalf("body = %#v", item.Body)
	}
}

// Hard-won: 89ee2de3: late ACP thought chunks reopened a stream that could never close before checkpoint export.
func TestThoughtChunkWhileIdleIsRecordedClosed(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.Execution = core.MaterializedExecutionStateIdle{}
	projectionApplyObservation(t, &session, projectionThoughtChunk("late thought", "msg-1"))
	item := projectionFindItem(&session, "thought:msg-1")
	if item == nil {
		t.Fatal("trailing thought recorded")
	}
	if body, ok := item.Body.(core.TranscriptBodyThought); !ok || body.Streaming {
		t.Fatalf("body = %#v", item.Body)
	}
}

// Hard-won: 94d3f379: real Grok goal output without message IDs produced thousands of one-token transcript bubbles.
func TestIdleUntaggedAgentChunksCoalesceIntoOneClosedItem(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.Execution = core.MaterializedExecutionStateIdle{}
	for _, word := range []string{"Grok ", "streams ", "one ", "word ", "at ", "a ", "time"} {
		projectionApplyObservation(t, &session, projectionUntaggedAgentChunk(word))
	}
	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	body, ok := session.Transcript[0].Body.(core.TranscriptBodyAgent)
	if !ok || body.Streaming || core.MaterializedChunksText(body.Chunks) != "Grok streams one word at a time" {
		t.Fatalf("body = %#v", session.Transcript[0].Body)
	}
}

// Hard-won: 94d3f379: real Grok goal output without message IDs produced thousands of one-token transcript bubbles.
func TestIdleUntaggedThoughtChunksCoalesceIntoOneClosedItem(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.Execution = core.MaterializedExecutionStateIdle{}
	for _, word := range []string{"thinking ", "in ", "small ", "pieces"} {
		projectionApplyObservation(t, &session, projectionUntaggedThoughtChunk(word))
	}
	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	body, ok := session.Transcript[0].Body.(core.TranscriptBodyThought)
	if !ok || body.Streaming || core.MaterializedChunksText(body.Chunks) != "thinking in small pieces" {
		t.Fatalf("body = %#v", session.Transcript[0].Body)
	}
}

func TestBackwardRelayClockNeverRegressesTranscriptChangeTimes(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	first := projectionEvent(t, &session, projectionAgentChunk("first", "answer-1"))
	first.RecordedAtMs = 1_000
	projectionRedigest(t, &first)
	projectionApply(t, &session, first)

	backward := projectionEvent(t, &session, projectionAgentChunk(" second", "answer-1"))
	backward.RecordedAtMs = 500
	projectionRedigest(t, &backward)
	projectionApply(t, &session, backward)
	if session.Transcript[0].LastChangedAtMs != 1_000 {
		t.Fatalf("last changed = %d", session.Transcript[0].LastChangedAtMs)
	}

	completion := projectionEvent(t, &session, projectionPromptCompleted("prompt-1", "end_turn"))
	completion.RecordedAtMs = 250
	projectionRedigest(t, &completion)
	projectionApply(t, &session, completion)
	if session.Transcript[0].LastChangedAtMs != 1_000 {
		t.Fatalf("last changed = %d", session.Transcript[0].LastChangedAtMs)
	}
	if activity := session.GetLastActivityAtMs(); activity == nil || *activity != 1_000 {
		t.Fatalf("last activity = %v", activity)
	}
}

func TestToolUpdateWithoutAnInitialCallIsIgnoredAndAdvancesTheFrontier(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	title := "updated"
	relayEvent := projectionEvent(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "missing-tool",
		Title:      &title,
	}))

	projected, err := ProjectRelayEvent(&session, &relayEvent)
	if err != nil {
		t.Fatalf("a delayed pre-resume tool update is an observable no-op: %v", err)
	}
	if err := ApplyCommittedProjectionEvent(&session, &relayEvent, projected.Mutation); err != nil {
		t.Fatalf("the no-op still advances the committed relay frontier: %v", err)
	}

	if len(session.Transcript) != 0 || session.AppliedEventOrdinal != 1 {
		t.Fatalf("transcript %#v at %d", session.Transcript, session.AppliedEventOrdinal)
	}
}

func TestMetadataOnlyToolUpdateWithoutAnInitialCallIsIgnored(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	relayEvent := projectionEvent(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "pre-resume-tool",
		Meta: jsonvalue.Object{{
			Key:   "terminal_output_delta",
			Value: jsonvalue.Object{{Key: "data", Value: "late output"}},
		}},
	}))

	projected, err := ProjectRelayEvent(&session, &relayEvent)
	if err != nil {
		t.Fatalf("private metadata cannot change the transcript projection: %v", err)
	}
	if err := ApplyCommittedProjectionEvent(&session, &relayEvent, projected.Mutation); err != nil {
		t.Fatalf("the no-op still advances the committed relay frontier: %v", err)
	}

	if len(session.Transcript) != 0 || session.AppliedEventOrdinal != 1 {
		t.Fatalf("transcript %#v at %d", session.Transcript, session.AppliedEventOrdinal)
	}
}

// Hard-won: c823ba502: ACP terminal metadata vanished instead of reaching persisted tool details.
func TestTerminalMetadataRetainsBoundedUnicodeOutputAcrossRestoreAndLateExit(t *testing.T) {
	session := core.MaterializedSessionEmpty("terminal-metadata")
	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))
	update := func(session *core.MaterializedSession, meta jsonvalue.Object) {
		encoded := projectionRawJSON(t, jsonvalue.Object{
			{Key: "sessionUpdate", Value: "tool_call_update"},
			{Key: "toolCallId", Value: "call-1"},
			{Key: "_meta", Value: meta},
		})
		projectionApplyObservation(t, session, projectionUpdateJSON(t, string(encoded)))
	}
	// More than a pipe buffer and the retained-output budget, ending partway
	// through a multibyte character if the byte cap ignores UTF-8 boundaries.
	output := strings.Repeat("🦀", 1_048_577) + "\nfirst failure\n"
	update(&session, jsonvalue.Object{{Key: "terminal_output_delta", Value: jsonvalue.Object{
		{Key: "terminal_id", Value: "term-1"},
		{Key: "data", Value: output},
	}}})
	record := projectionAttachedTerminalOutputs(t, session.Transcript[0])[0]
	if !record.Truncated || record.Output.Len() > 4*1024*1024 || !strings.HasSuffix(output, record.Output.Text()) {
		t.Fatalf("record truncated %v, length %d", record.Truncated, record.Output.Len())
	}

	// A daemon rebuilds projection ownership solely from the serialized session.
	var restored core.MaterializedSession
	if err := json.Unmarshal([]byte(projectionJSONText(t, session)), &restored); err != nil {
		t.Fatal(err)
	}
	session = restored
	update(&session, jsonvalue.Object{{Key: "terminal_output_delta", Value: jsonvalue.Object{
		{Key: "terminal_id", Value: "term-1"},
		{Key: "data", Value: "late failure\n"},
	}}})
	update(&session, jsonvalue.Object{{Key: "terminal_exit", Value: jsonvalue.Object{
		{Key: "terminal_id", Value: "term-1"},
		{Key: "exit_code", Value: int64(128)},
		{Key: "signal", Value: nil},
	}}})
	record = projectionAttachedTerminalOutputs(t, session.Transcript[0])[0]
	if !strings.HasSuffix(record.Output.Text(), "first failure\nlate failure\n") {
		t.Fatal("late output was not appended")
	}
	if record.ExitCode == nil || *record.ExitCode != 128 || !record.Truncated {
		t.Fatalf("exit %v, truncated %v", record.ExitCode, record.Truncated)
	}

	// A final snapshot replaces earlier chunks rather than repeating them.
	update(&session, jsonvalue.Object{
		{Key: "terminal_output", Value: jsonvalue.Object{
			{Key: "terminal_id", Value: "term-1"},
			{Key: "data", Value: "complete failure\n"},
		}},
		{Key: "terminal_exit", Value: jsonvalue.Object{
			{Key: "terminal_id", Value: "term-1"},
			{Key: "exit_code", Value: nil},
			{Key: "signal", Value: "SIGTERM"},
		}},
	})
	record = projectionAttachedTerminalOutputs(t, session.Transcript[0])[0]
	if record.Output.Text() != "complete failure\n" || record.ExitCode != nil ||
		record.Signal == nil || *record.Signal != "SIGTERM" || record.Truncated {
		t.Fatalf("record = %q exit %v signal %v truncated %v", record.Output.Text(), record.ExitCode, record.Signal, record.Truncated)
	}
}

// Hard-won: 111f6dcd: Codex resent a revised tool call as a new item, changing identity and failing every recovery retry.
func TestResentToolCallKeepsIdentityAndReplacesTheCallPayload(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "read file"}))
	created := *session.Transcript[0]
	if created.Position != 1 || created.CreatedAtMs != 100 {
		t.Fatalf("created = %#v", created)
	}

	resend := projectionEvent(t, &session, projectionToolCall(schema.ToolCall{
		ToolCallID: "call-1",
		Title:      "read file again",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
	}))
	projected, err := ProjectRelayEvent(&session, &resend)
	if err != nil {
		t.Fatal(err)
	}
	var item *core.TranscriptItem
	for _, change := range projected.Mutation.Transcript {
		if upsert, ok := change.(core.TranscriptMutationUpsert); ok && upsert.Item.StableID == "tool:call-1" {
			item = &upsert.Item
			break
		}
	}
	if item == nil {
		t.Fatal("the re-sent tool call upserts its existing item")
	}
	if item.Position != created.Position || item.CreatedAtMs != created.CreatedAtMs ||
		item.LastChangedAtMs != resend.RecordedAtMs ||
		!reflect.DeepEqual(item.LatestContentEventOrdinal, created.LatestContentEventOrdinal) {
		t.Fatalf("item = %#v, created = %#v", item, created)
	}
	if title := projectionCallMember(t, item, "title"); title != "read file again" {
		t.Fatalf("title = %#v", title)
	}

	if err := ApplyCommittedProjectionEvent(&session, &resend, projected.Mutation); err != nil {
		t.Fatalf("the merged item passes the projection integrity checks: %v", err)
	}
	if len(session.Transcript) != 1 || session.Transcript[0].Position != created.Position {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
}

// Hard-won: 111f6dcd: a revised full call after an incremental update created a second transcript item.
func TestToolCallUpdateThenResentToolCallSurvivesTheProjection(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "shell"}))
	projectionApplyObservation(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "call-1",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
	}))
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "shell (retried)"}))

	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	item := session.Transcript[0]
	if item.Position != 1 || item.CreatedAtMs != 100 || item.LastChangedAtMs != 300 {
		t.Fatalf("item = %#v", item)
	}
	if title := projectionCallMember(t, item, "title"); title != "shell (retried)" {
		t.Fatalf("title = %#v", title)
	}
}

func TestRealToolCallReplacesFallbackAndKeepsItsStartOrder(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionFallbackTerminalTool("term-1", "cargo test"))
	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))

	if len(session.Transcript) != 1 {
		t.Fatalf("the fallback was consumed: %#v", session.Transcript)
	}
	item := session.Transcript[0]
	if item.StableID != "tool:call-1" || item.Position != 1 || len(projectionAttachedTerminalOutputs(t, item)) != 1 {
		t.Fatalf("item = %#v", item)
	}
}

func TestFallbackIsSuppressedWhenRealToolAlreadyClaimsTerminal(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyObservation(t, &session, projectionFallbackTerminalTool("term-1", "cargo test"))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))

	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "tool:call-1" ||
		len(projectionAttachedTerminalOutputs(t, session.Transcript[0])) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
}

func TestRawResultBeforeTerminalCloseClaimsTheFallback(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionFallbackTerminalTool("term-1", "cargo test"))
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "Execute `cargo test`"}))
	projectionApplyObservation(t, &session, projectionKimiRawToolUpdate(t, "call-1", "build finished\n"))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))

	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "tool:call-1" ||
		session.Transcript[0].Position != 2 || len(projectionAttachedTerminalOutputs(t, session.Transcript[0])) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
}

func TestRawResultAfterTerminalCloseClaimsTheFallback(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionFallbackTerminalTool("term-1", "cargo test"))
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "Execute `cargo test`"}))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))
	projectionApplyObservation(t, &session, projectionKimiRawToolUpdate(t, "call-1", "build finished\n"))

	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "tool:call-1" ||
		session.Transcript[0].Position != 2 || len(projectionAttachedTerminalOutputs(t, session.Transcript[0])) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
}

func TestLateFallbackClaimsOutputFromAFastTerminal(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))
	projectionApplyObservation(t, &session, projectionFallbackTerminalTool("term-1", "true"))

	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "tool:hel-terminal:term-1" {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	body, ok := session.Transcript[0].Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatal("the parked output became a fallback tool")
	}
	call, err := decodeToolCall(body.Call)
	if err != nil {
		t.Fatal(err)
	}
	if call.Status == nil || *call.Status != schema.ToolCallStatusCompleted {
		t.Fatalf("status = %v", call.Status)
	}
}

func TestIndexedPageProjectionTracksTerminalAndToolReplacements(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	index := NewProjectionIndex(&session)
	projectionApplyIndexedObservation(t, &session, index, projectionTerminalOutput("term-1"))
	projectionApplyIndexedObservation(t, &session, index, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyIndexedObservation(t, &session, index, projectionTerminalOutput("term-1"))

	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "tool:call-1" {
		t.Fatalf("parked output was consumed: %#v", session.Transcript)
	}
	outputs := projectionAttachedTerminalOutputs(t, session.Transcript[0])
	if len(outputs) != 1 || outputs[0].TerminalID != "term-1" {
		t.Fatalf("outputs = %#v", outputs)
	}
}

func TestTerminalOutputBeforeTheToolCallAttachesWhenTheCallArrives(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))

	// Output nobody refers to yet is parked in its own item rather than
	// dropped, so a terminal a call never names still reaches the reader.
	if len(session.Transcript) != 1 || session.Transcript[0].StableID != "terminal:term-1" {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	if body, ok := session.Transcript[0].Body.(core.TranscriptBodyTerminalOutput); !ok || body.Record.TerminalID != "term-1" {
		t.Fatalf("body = %#v", session.Transcript[0].Body)
	}

	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))

	if len(session.Transcript) != 1 {
		t.Fatalf("the tool call consumes the parked item: %#v", session.Transcript)
	}
	if session.Transcript[0].StableID != "tool:call-1" {
		t.Fatalf("stable id = %q", session.Transcript[0].StableID)
	}
	outputs := projectionAttachedTerminalOutputs(t, session.Transcript[0])
	if len(outputs) != 1 || outputs[0].Output.Text() != "build finished\n" {
		t.Fatalf("outputs = %#v", outputs)
	}

	// Both orderings converge on the same tool body.
	reversed := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &reversed, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyObservation(t, &reversed, projectionTerminalOutput("term-1"))
	projectionAssertJSONEqual(t, projectionAttachedTerminalOutputs(t, reversed.Transcript[0]), outputs,
		"output arriving before or after the call must read the same")
	if reversed.Transcript[0].LastChangedAtMs != 200 {
		t.Fatalf("last changed = %d", reversed.Transcript[0].LastChangedAtMs)
	}
}

func TestMismatchedRawResultDoesNotHideAGenuineOrphanFailure(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "Execute"}))
	exitCode := uint32(1)
	projectionApplyObservation(t, &session, core.RelayObservationTerminalOutput{
		TerminalID: "term-1",
		Output:     "orphan failure\n",
		ExitCode:   &exitCode,
	})
	projectionApplyObservation(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "call-1",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
		RawOutput: projectionRawJSON(t, jsonvalue.Object{
			{Key: "output", Value: projectionBytes("different output")},
			{Key: "exit_code", Value: int64(1)},
		}),
	}))

	if len(session.Transcript) != 2 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	if !slices.ContainsFunc(session.Transcript, func(item *core.TranscriptItem) bool {
		body, ok := item.Body.(core.TranscriptBodyTerminalOutput)
		return ok && body.Record.Output.Text() == "orphan failure\n"
	}) {
		t.Fatalf("the orphan failure is hidden: %#v", session.Transcript)
	}
}

func TestIdenticalOrphanResultsAreNotAssignedArbitrarily(t *testing.T) {
	const output = "same output\n"
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionToolCall(schema.ToolCall{ToolCallID: "call-1", Title: "Execute"}))
	for _, terminalID := range []string{"term-1", "term-2"} {
		exitCode := uint32(1)
		projectionApplyObservation(t, &session, core.RelayObservationTerminalOutput{
			TerminalID: terminalID,
			Output:     output,
			ExitCode:   &exitCode,
		})
	}
	projectionApplyObservation(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "call-1",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
		RawOutput: projectionRawJSON(t, jsonvalue.Object{
			{Key: "output", Value: projectionBytes(output)},
			{Key: "exit_code", Value: int64(1)},
		}),
	}))

	parked := 0
	for _, item := range session.Transcript {
		if _, ok := item.Body.(core.TranscriptBodyTerminalOutput); ok {
			parked++
		}
	}
	if parked != 2 {
		t.Fatalf("identical concurrent results need an explicit reference: %d parked", parked)
	}
	if outputs := projectionAttachedTerminalOutputs(t, session.Transcript[0]); len(outputs) != 0 {
		t.Fatalf("outputs = %#v", outputs)
	}
}

func TestWholesaleToolCallUpdateKeepsTheAttachedTerminalOutput(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))
	// ToolCall::update replaces content wholesale, which is why the output
	// lives beside the call rather than inside it.
	projectionApplyObservation(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "call-1",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
		Content: []schema.ToolCallContent{{
			Kind:     schema.ToolCallContentKindTerminal,
			Terminal: &schema.Terminal{TerminalID: "term-1"},
		}},
	}))

	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
	outputs := projectionAttachedTerminalOutputs(t, session.Transcript[0])
	if len(outputs) != 1 || outputs[0].Output.Text() != "build finished\n" {
		t.Fatalf("outputs = %#v", outputs)
	}
	if status := projectionCallMember(t, session.Transcript[0], "status"); status != "completed" {
		t.Fatalf("status = %#v", status)
	}
}

// Grok Build names the terminal on a mid-flight update and then replaces
// content wholesale with plain text before the terminal is reaped, so the
// close event arrives with nothing in the call pointing at it.
// Hard-won: b364ed85: live Grok updates dropped a prior terminal reference before its output arrived.
func TestAToolCallThatDroppedItsTerminalReferenceStillAttachesTheOutput(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionTerminalToolCall("call-1", "term-1"))
	projectionApplyObservation(t, &session, projectionToolCallUpdate(schema.ToolCallUpdate{
		ToolCallID: "call-1",
		Status:     projectionStatus(schema.ToolCallStatusCompleted),
		Content: []schema.ToolCallContent{{
			Kind:    schema.ToolCallContentKindContent,
			Content: &schema.Content{Content: textBlock("ran the build")},
		}},
	}))
	projectionApplyObservation(t, &session, projectionTerminalOutput("term-1"))

	if len(session.Transcript) != 1 {
		t.Fatalf("the output attaches instead of parking in its own item: %#v", session.Transcript)
	}
	if session.Transcript[0].StableID != "tool:call-1" {
		t.Fatalf("stable id = %q", session.Transcript[0].StableID)
	}
	outputs := projectionAttachedTerminalOutputs(t, session.Transcript[0])
	if len(outputs) != 1 || outputs[0].Output.Text() != "build finished\n" {
		t.Fatalf("outputs = %#v", outputs)
	}
	body := session.Transcript[0].Body.(core.TranscriptBodyTool)
	if !reflect.DeepEqual(body.TerminalRefs, []string{"term-1"}) {
		t.Fatalf("terminal refs = %v", body.TerminalRefs)
	}
	if len(body.Call.TerminalIDs()) != 0 {
		t.Fatalf("the final call really did drop the reference: %v", body.Call.TerminalIDs())
	}
}

func TestSessionInfoUpdateCapsLargeTitlesInThePublishedProjection(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionUpdate(schema.SessionUpdate{
		Kind: schema.SessionUpdateKindSessionInfoUpdate,
		SessionInfoUpdate: &schema.SessionInfoUpdate{
			Title: schema.Nullable[string]{Set: true, Value: strings.Repeat("word ", 20_000)},
		},
	}))

	expected := strings.Repeat("word ", 50) + "word…"
	if session.SessionTitle == nil || *session.SessionTitle != expected {
		t.Fatalf("title = %v", session.SessionTitle)
	}
	if title, ok := session.ResolvedTitle(); !ok || title != expected {
		t.Fatalf("resolved title = %q", title)
	}
	if size := len(projectionJSONText(t, session)); size >= 10_000 {
		t.Fatalf("serialized session is %d bytes", size)
	}
}

func TestSessionInfoUpdateWithoutTitlePreservesTheProvisionalTitle(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionQueued("prompt-1", projectionPrompt("first prompt"), 100))

	projectionApplyObservation(t, &session, projectionUpdate(schema.SessionUpdate{
		Kind: schema.SessionUpdateKindSessionInfoUpdate,
		SessionInfoUpdate: &schema.SessionInfoUpdate{
			UpdatedAt: schema.Nullable[string]{Set: true, Value: "2026-08-31T12:00:00Z"},
		},
	}))

	if session.SessionTitle == nil || *session.SessionTitle != "first prompt" {
		t.Fatalf("title = %v", session.SessionTitle)
	}
}

func TestExplicitSessionTitleClearRestoresThePromptFallback(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionQueued("prompt-1", projectionPrompt("first prompt"), 100))

	projectionApplyObservation(t, &session, projectionUpdate(schema.SessionUpdate{
		Kind: schema.SessionUpdateKindSessionInfoUpdate,
		SessionInfoUpdate: &schema.SessionInfoUpdate{
			Title: schema.Nullable[string]{Set: true, Null: true},
		},
	}))

	if session.SessionTitle != nil {
		t.Fatalf("title = %q", *session.SessionTitle)
	}
	if title, ok := session.ResolvedTitle(); !ok || title != "first prompt" {
		t.Fatalf("resolved title = %q", title)
	}
}

func projectionQueuedEntry(commandID, text string, queuedAtMs int64) core.MaterializedQueuedPrompt {
	return core.MaterializedQueuedPrompt{
		CommandID:  commandID,
		Kind:       core.QueuedCommandKindPrompt{},
		Content:    []any{json.RawMessage(`{"type":"text","text":"` + text + `"}`)},
		QueuedAtMs: queuedAtMs,
	}
}

func TestQueueChangesProjectOnlyFromTheirCompletionEvents(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.QueuedPrompts = append(session.QueuedPrompts, projectionQueuedEntry("queued-1", "later", 10))

	projectionApplyObservation(t, &session, projectionQueued("remove-1", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "queued-1"}, 100))
	if len(session.QueuedPrompts) != 1 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}

	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		CommandID: "remove-1",
		Outcome:   core.RelayCommandOutcomeQueueChanged{RemovedCommandIds: []string{"queued-1"}},
	})
	if len(session.QueuedPrompts) != 0 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}

	session.QueuedPrompts = append(session.QueuedPrompts,
		projectionQueuedEntry("queued-2", "two", 20),
		projectionQueuedEntry("queued-3", "three", 30))
	projectionApplyObservation(t, &session, projectionQueued("clear-1", core.RelayCommandClearQueuedPrompts{}, 200))
	if len(session.QueuedPrompts) != 2 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}

	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		CommandID: "clear-1",
		Outcome:   core.RelayCommandOutcomeQueueChanged{RemovedCommandIds: []string{"queued-2", "queued-3"}},
	})
	if len(session.QueuedPrompts) != 0 {
		t.Fatalf("queue = %#v", session.QueuedPrompts)
	}
}

func TestRejectedCloseRollsClosingProjectionBackToIdle(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, projectionQueued("close-1", core.RelayCommandClose{
		BarrierCommandID: "barrier-1",
		Expected:         core.RelayCursor{Ordinal: 0, Digest: strings.Repeat("0", 64)},
	}, 100))
	if _, closing := session.Execution.(core.MaterializedExecutionStateClosing); !closing {
		t.Fatalf("execution = %#v", session.Execution)
	}

	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "close-1",
		Command:   core.RelayCommandKindClose,
		Message:   "ACP close failed",
	})
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
}

// Hard-won: 0d9df1dd: rejected command notices exposed internal command IDs in the conversation.
func TestARejectedCommandNoticeDoesNotShowTheCommandID(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "set-config-0123abcd",
		Command:   core.RelayCommandKindSetConfig,
		Message:   `"gpt-9" is not an available model value`,
	})
	var notices []string
	for _, item := range session.Transcript {
		if text, ok := projectionSystemText(item); ok {
			notices = append(notices, text)
		}
	}
	if !reflect.DeepEqual(notices, []string{`"gpt-9" is not an available model value`}) {
		t.Fatalf("notices = %q", notices)
	}
}

func TestControlCommandOutcomesDoNotEndAnActivePrompt(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.AppliedEventOrdinal = 2
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: 100}
	session.Transcript = append(session.Transcript, &core.TranscriptItem{
		StableID:                  "agent:answer-1",
		Position:                  2,
		LatestContentEventOrdinal: uint64Pointer(2),
		CreatedAtMs:               200,
		LastChangedAtMs:           200,
		Body: core.TranscriptBodyAgent{
			Chunks:    []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"working"}}`)},
			Streaming: true,
		},
	})
	streaming := func() bool {
		body, ok := session.Transcript[0].Body.(core.TranscriptBodyAgent)
		return ok && body.Streaming
	}

	projectionApplyObservation(t, &session, core.RelayObservationCommandCompleted{
		CommandID: "config-1",
		Outcome:   core.RelayCommandOutcomeConfigured{},
	})
	if !projectionIsRunning(&session) || !streaming() {
		t.Fatalf("a configuration outcome ended the prompt: %#v", session.Execution)
	}

	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "cancel-1",
		Command:   core.RelayCommandKindCancel,
		Message:   "not cancellable",
	})
	if !projectionIsRunning(&session) || !streaming() {
		t.Fatalf("a rejected cancel ended the prompt: %#v", session.Execution)
	}
}

// Hard-won: ff088d42: a real 15,000-chunk DeepSeek thought caused quadratic projection work and multi-gigabyte RSS.
func TestStreamedTextForOneMessageIDBecomesASingleChunk(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: 1}
	for _, token := range []string{"think", "ing ", "it ", "through"} {
		projectionApplyObservation(t, &session, projectionThoughtChunk(token, "msg-1"))
	}
	for _, token := range []string{"here ", "is ", "the ", "answer"} {
		projectionApplyObservation(t, &session, projectionAgentChunk(token, "msg-1"))
	}

	thought := projectionFindItem(&session, "thought:msg-1")
	if thought == nil {
		t.Fatal("thought recorded")
	}
	thoughtBody, ok := thought.Body.(core.TranscriptBodyThought)
	if !ok {
		t.Fatalf("expected a thought body: %#v", thought.Body)
	}
	if len(thoughtBody.Chunks) != 1 || core.MaterializedChunksText(thoughtBody.Chunks) != "thinking it through" {
		t.Fatalf("chunks: %s", thoughtBody.Chunks)
	}

	agent := projectionFindItem(&session, "agent:msg-1")
	if agent == nil {
		t.Fatal("agent message recorded")
	}
	agentBody, ok := agent.Body.(core.TranscriptBodyAgent)
	if !ok {
		t.Fatalf("expected an agent body: %#v", agent.Body)
	}
	if len(agentBody.Chunks) != 1 || core.MaterializedChunksText(agentBody.Chunks) != "here is the answer" {
		t.Fatalf("chunks: %s", agentBody.Chunks)
	}
}

func TestClearPreservesHistoryAndAddsOneDurableContextBoundary(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationWarning{Message: "earlier history"})
	previous := projectionJSONText(t, session.Transcript[0])
	projectionApplyObservation(t, &session, projectionQueued("clear-request", core.RelayCommandClearContext{}, 200))
	if !projectionIsRunning(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	complete := projectionEvent(t, &session, core.RelayObservationCommandCompleted{
		CommandID: "clear-request",
		Outcome:   core.RelayCommandOutcomeContextCleared{NativeSessionID: "new"},
	})
	projectionApply(t, &session, complete)
	if !projectionIsIdle(&session) {
		t.Fatalf("execution = %#v", session.Execution)
	}
	if got := projectionJSONText(t, session.Transcript[0]); got != previous {
		t.Fatalf("history changed: %s", got)
	}
	boundaries := func() int {
		count := 0
		for _, item := range session.Transcript {
			if core.IsContextBoundary(item.StableID) {
				count++
			}
		}
		return count
	}
	if boundaries() != 1 {
		t.Fatalf("boundaries = %d", boundaries())
	}
	if session.LastTurnOutcome != nil {
		t.Fatal("the clear kept the last turn outcome")
	}
	// The projection refuses duplicate ordinals; command retry deduplication
	// happens in the durable relay before a second event can be emitted.
	if _, err := ProjectRelayEvent(&session, &complete); err == nil {
		t.Fatal("a duplicate ordinal projected")
	}
	if boundaries() != 1 {
		t.Fatalf("boundaries = %d", boundaries())
	}
}

func TestWindowedCurrentTurnKeepsStreamingAndToolUpdatesIdenticalToFullHistory(t *testing.T) {
	full := core.MaterializedSessionEmpty("window-stream")
	// Multiple settled turns, with enough text to exceed a pipe buffer too.
	for turn := 0; turn < 12; turn++ {
		commandID := fmt.Sprintf("prompt-%d", turn)
		projectionApplyObservation(t, &full, projectionQueued(commandID, projectionPrompt("continue"), 0))
		projectionApplyObservation(t, &full, projectionStarted(commandID, 0))
		for range 100 {
			projectionApplyObservation(t, &full, core.RelayObservationNotice{Message: strings.Repeat("recorded context ", 20)})
		}
	}
	projectionApplyObservation(t, &full, projectionToolCall(schema.ToolCall{
		ToolCallID: "current-tool",
		Title:      "read file",
		Status:     projectionStatus(schema.ToolCallStatusInProgress),
	}))
	projectionApplyObservation(t, &full, projectionUntaggedAgentChunk("first "))
	live := projectionClone(&full)
	window := core.ProjectionWindowOf(&live)
	window.Trim(&live, 1024)
	if window.OmittedItems == 0 || len(live.Transcript) >= len(full.Transcript) {
		t.Fatalf("the window omitted %d items", window.OmittedItems)
	}
	for _, observation := range []core.RelayObservation{
		projectionUntaggedAgentChunk("second"),
		projectionToolCallUpdate(schema.ToolCallUpdate{
			ToolCallID: "current-tool",
			Status:     projectionStatus(schema.ToolCallStatusCompleted),
		}),
	} {
		next := projectionEvent(t, &full, observation)
		projectionApply(t, &full, next)
		projectionApply(t, &live, next)
		window.Trim(&live, 1024)
		for _, item := range live.Transcript {
			durable := projectionFindItem(&full, item.StableID)
			if durable == nil {
				t.Fatalf("%q is not in the full history", item.StableID)
			}
			projectionAssertJSONEqual(t, item, durable, item.StableID)
		}
		if len(live.Transcript)+window.OmittedItems != len(full.Transcript) {
			t.Fatalf("live %d + omitted %d != full %d", len(live.Transcript), window.OmittedItems, len(full.Transcript))
		}
	}
	body, ok := live.Transcript[len(live.Transcript)-1].Body.(core.TranscriptBodyAgent)
	if !ok {
		t.Fatal("agent stream")
	}
	if text := core.MaterializedChunksText(body.Chunks); text != "first second" {
		t.Fatalf("text = %q", text)
	}
	tool := projectionFindItem(&live, "tool:current-tool")
	if tool == nil {
		t.Fatal("tool")
	}
	if status := projectionCallMember(t, tool, "status"); status != "completed" {
		t.Fatalf("status = %#v", status)
	}
}

// Hard-won: 32f1541e: checkpoint recovery failures exposed daemon bookkeeping IDs as transcript messages.
func TestAnInterruptedCheckpointBarrierAddsNothingToTheTranscript(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	for _, command := range []core.RelayCommandKind{core.RelayCommandKindBeginCheckpoint, core.RelayCommandKindReleaseCheckpoint} {
		projectionApplyObservation(t, &session, core.RelayObservationCommandInterrupted{
			CommandID: "worker-upgrade-0123abcd",
			Command:   command,
			Message:   "relay restarted without the controller that owned the checkpoint barrier",
		})
	}
	if len(session.Transcript) != 0 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}

	// A person's own command still reports its failure.
	projectionApplyObservation(t, &session, core.RelayObservationCommandRejected{
		CommandID: "clear",
		Command:   core.RelayCommandKindClearContext,
		Message:   "busy",
	})
	if len(session.Transcript) != 1 {
		t.Fatalf("transcript = %#v", session.Transcript)
	}
}

// Issue 1217: the reminder ran as "Agent continued on its own", so the
// transcript hid the prompt the child was obeying.
// Hard-won: e97c0266: a child obeyed an invisible handback reminder and stopped before finishing its task.
func TestAHandbackReminderIsShownAsThePromptThatStartedItsTurn(t *testing.T) {
	session := core.MaterializedSessionEmpty("child")
	projectionApplyObservation(t, &session, projectionQueued("handback-reminder-36", core.RelayCommandHandbackReminder{
		CompletedCommandID: "task",
		CompletedOrdinal:   36,
	}, 10))
	projectionApplyObservation(t, &session, projectionStarted("handback-reminder-36", 20))
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 21})

	if session.ActiveTurn == nil || session.ActiveTurn.CommandID != "handback-reminder-36" {
		t.Fatalf("the reminder's turn = %#v", session.ActiveTurn)
	}
	if !slices.ContainsFunc(session.Transcript, func(item *core.TranscriptItem) bool {
		body, ok := item.Body.(core.TranscriptBodyUser)
		return ok && core.MaterializedContentText(body.Content) == core.SubagentHandbackReminderText
	}) {
		t.Fatalf("the reminder is not shown: %#v", session.Transcript)
	}
	if slices.ContainsFunc(session.Transcript, func(item *core.TranscriptItem) bool {
		text, ok := projectionSystemText(item)
		return ok && text == core.HarnessTurnText
	}) {
		t.Fatal("the reminder's turn reads as the agent continuing on its own")
	}
}

func TestCanonicalRoundTripPreservesCursorAndLogicalPositions(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-1")
	session.AppliedEventOrdinal = 4
	session.AppliedEventDigest = strings.Repeat("a", 64)
	activity := int64(40)
	session.LastActivityAtMs = &activity
	title := "Build it"
	session.SessionTitle = &title
	exitCode := uint32(0)
	signal := "SIGKILL"
	session.Transcript = append(session.Transcript,
		&core.TranscriptItem{
			StableID:                  "agent:a",
			Position:                  2,
			LatestContentEventOrdinal: uint64Pointer(4),
			CreatedAtMs:               20,
			LastChangedAtMs:           40,
			Body: core.TranscriptBodyAgent{Chunks: []json.RawMessage{json.RawMessage(
				`{"content":{"type":"text","text":"done"},"messageId":"a","_meta":{"provider":"test"}}`)}},
		},
		&core.TranscriptItem{
			StableID:        "thought:t",
			Position:        3,
			CreatedAtMs:     30,
			LastChangedAtMs: 30,
			Body: core.TranscriptBodyThought{Chunks: []json.RawMessage{json.RawMessage(
				`{"content":{"type":"text","text":"reasoning","_meta":{"contentProvider":"test"}},"messageId":"t","_meta":{"chunkProvider":"test"}}`)}},
		},
		&core.TranscriptItem{
			StableID:        "tool:call-1",
			Position:        4,
			CreatedAtMs:     40,
			LastChangedAtMs: 40,
			Body: core.TranscriptBodyTool{
				Call: core.OpaqueToolCallFromValue(json.RawMessage(`{"toolCallId":"call-1","title":"Read file","kind":"read","status":"completed","content":[{"type":"terminal","terminalId":"term-1"}],"rawInput":{"path":"README.md"},"rawOutput":{"bytes":42},"_meta":{"provider":"test"}}`)),
				TerminalOutputs: []core.TerminalOutputRecord{{
					TerminalID: "term-1",
					Output:     core.NewPackedText("ok\n"),
					Truncated:  true,
					ExitCode:   &exitCode,
				}},
				// "term-3" is a reference the call no longer carries, so only
				// the remembered list can survive the archive round trip.
				TerminalRefs: []string{"term-1", "term-3"},
				Presentation: &core.ToolCallPresentation{
					Summary:        "Read",
					Source:         core.NewPackedText("Read file"),
					SourceKind:     core.ToolSummarySourceKindTitle,
					ToolKind:       acp.ToolKindRead,
					SummaryVersion: ToolSummaryVersion,
				},
			},
		},
		&core.TranscriptItem{
			StableID:        "terminal:term-2",
			Position:        4,
			CreatedAtMs:     40,
			LastChangedAtMs: 40,
			Body: core.TranscriptBodyTerminalOutput{Record: core.TerminalOutputRecord{
				TerminalID: "term-2",
				Output:     core.NewPackedText("orphaned output\n"),
				Signal:     &signal,
			}},
		},
		&core.TranscriptItem{
			StableID:        "plan:4",
			Position:        4,
			CreatedAtMs:     40,
			LastChangedAtMs: 40,
			Body: core.TranscriptBodyPlan{Plan: json.RawMessage(
				`{"entries":[{"content":"finish","priority":"high","status":"in_progress","_meta":{"entryProvider":"test"}}],"_meta":{"planProvider":"test"}}`)},
		},
		&core.TranscriptItem{
			StableID:        PlanProposalItemID(4),
			Position:        4,
			CreatedAtMs:     40,
			LastChangedAtMs: 40,
			Body:            core.TranscriptBodyPlanProposal{ProposalID: "plan-review-1", Plan: "1. Read the code\n2. Change it"},
		},
	)
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		CommandID:  "queued-config",
		Kind:       core.QueuedCommandKindSetConfig{Key: "model", Value: "sonnet"},
		Content:    []any{json.RawMessage(`{"type":"text","text":"/model sonnet"}`)},
		QueuedAtMs: 50,
	})
	canonical, err := CanonicalSessionFromMaterialized(&session)
	if err != nil {
		t.Fatal(err)
	}
	if err := canonical.Validate(); err != nil {
		t.Fatal(err)
	}
	want := core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindSetConfig, Key: "model", Value: "sonnet"}
	if canonical.QueuedPrompts[0].Kind != want {
		t.Fatalf("queued kind = %#v", canonical.QueuedPrompts[0].Kind)
	}
	restored, err := MaterializedSessionFromCanonical("session-1", &canonical)
	if err != nil {
		t.Fatal(err)
	}
	if restored.AppliedEventOrdinal != 4 || restored.Transcript[0].Position != 2 || restored.UnreadAgentMessagesAfter(1) != 1 {
		t.Fatalf("restored = %#v", restored)
	}
	projectionAssertJSONEqual(t, restored, session, "restored session")
}

// The controller reads an autonomous turn's work facts from the outcome this
// projection builds; the settlement is their only source.
func TestAnAutonomousTurnSettlementCarriesItsWorkFactsIntoTheOutcome(t *testing.T) {
	session := core.MaterializedSessionEmpty("session")
	projectionApplyObservation(t, &session, core.RelayObservationHarnessTurnStarted{StartedAtMs: 35})
	start := session.AppliedEventOrdinal
	facts := core.WorkFactsUnavailable("under test")
	settled := projectionSettled(start, "end_turn")
	settled.Autonomous.WorkFacts = &facts
	projectionApplyObservation(t, &session, settled)
	outcome := session.LastTurnOutcome
	if outcome == nil || outcome.WorkFacts == nil {
		t.Fatalf("outcome = %#v", outcome)
	}
	projectionAssertJSONEqual(t, outcome.WorkFacts, &facts, "work facts")
}

// Hard-won: 5c9e20e: live recovery replaced a provider's selectable replacement with a withdrawn accepted model.
func TestLiveRecoveryPrefersASelectableProviderModelOverThePreviousSelection(t *testing.T) {
	for _, check := range []struct {
		current  string
		offered  []string
		expected string
	}{
		{"replacement", []string{"replacement"}, "replacement"},
		{"replacement", []string{"previous", "replacement"}, "replacement"},
		{"unselectable-transcript-id", []string{"previous"}, "previous"},
	} {
		replay := core.MaterializedSessionEmpty("model-recovery")
		replay.Configuration.Values = map[string]any{"model": "previous"}
		worker := core.NewRelaySnapshot("model-recovery")
		worker.Config["model"] = "previous"
		options := make([]jsonvalue.Object, 0, len(check.offered))
		for _, value := range check.offered {
			options = append(options, jsonvalue.Object{{Key: "value", Value: value}, {Key: "name", Value: value}})
		}
		update := projectionRawJSON(t, jsonvalue.Object{
			{Key: "sessionUpdate", Value: "config_option_update"},
			{Key: "configOptions", Value: []any{jsonvalue.Object{
				{Key: "id", Value: "model"},
				{Key: "name", Value: "Model"},
				{Key: "type", Value: "select"},
				{Key: "currentValue", Value: check.current},
				{Key: "options", Value: options},
			}}},
		})
		next := projectionEvent(t, &replay, projectionUpdateJSON(t, string(update)))
		if err := core.ApplyRelayEvent(&worker, &next); err != nil {
			t.Fatal(err)
		}
		projectionApply(t, &replay, next)
		baseline := core.LiveSessionBaseline{State: worker.OperationalState()}
		recovered, err := MaterializedSessionFromLiveBaseline(&baseline)
		if err != nil {
			t.Fatal(err)
		}
		if recovered.Configuration.Values["model"] != check.expected {
			t.Fatalf("%s: recovered model = %#v", check.current, recovered.Configuration.Values["model"])
		}
		if check.current == "replacement" && recovered.Configuration.Values["model"] != replay.Configuration.Values["model"] {
			t.Fatalf("recovered %#v, replayed %#v", recovered.Configuration.Values["model"], replay.Configuration.Values["model"])
		}
		canonical, err := CanonicalSessionFromMaterialized(&recovered)
		if err != nil {
			t.Fatal(err)
		}
		restored, err := MaterializedSessionFromCanonical("model-recovery", &canonical)
		if err != nil {
			t.Fatal(err)
		}
		if restored.Configuration.Values["model"] != check.expected {
			t.Fatalf("restored model = %#v", restored.Configuration.Values["model"])
		}
	}
}

// Hard-won: e06c07a69: mailbox wakes have an active owner but no user row to mark their interruption.
func TestInterruptedMailboxWakeBeforeRestartProducesOneUnreadInterruption(t *testing.T) {
	for _, restartFirst := range []bool{false, true} {
		for _, nativeRunning := range []bool{false, true} {
			session := core.MaterializedSessionEmpty("session")
			projectionApplyObservation(t, &session, projectionQueued("mailbox-wake", core.RelayCommandMailboxWake{Events: []core.MailboxEvent{{
				Key:         "parent-message",
				Source:      "parent",
				Wake:        true,
				CreatedAtMs: 1,
				Body:        core.MailboxEventBodyParentMessage{Text: "continue"},
			}}}, 10))
			projectionApplyObservation(t, &session, projectionStarted("mailbox-wake", 20))
			if session.ActiveTurn == nil || !session.ActiveTurn.BelongsTo("mailbox-wake") {
				t.Fatalf("active turn = %#v", session.ActiveTurn)
			}
			if projectionFindItem(&session, "user:mailbox-wake") != nil {
				t.Fatal("the wake has a user row")
			}
			if nativeRunning {
				projectionApplyObservation(t, &session, projectionUpdateJSON(t,
					`{"sessionUpdate":"session_info_update", "_meta":{"execution":{"version":1,"status":"running","turnId":"wake"}}}`))
			}
			if restartFirst {
				projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
			}
			continuationID := core.RestartContinuationID("mailbox-wake")
			restarted := core.OutcomeReasonWorkerRestarted
			projectionApplyObservation(t, &session, core.RelayObservationCommandInterrupted{
				ContinuationID: &continuationID,
				Reason:         &restarted,
				CommandID:      "mailbox-wake",
				Command:        core.RelayCommandKindPrompt,
				Message:        "bridge restarted",
			})
			interruptedAt := session.AppliedEventOrdinal
			if !projectionIsIdle(&session) {
				t.Fatalf("execution = %#v", session.Execution)
			}
			projectionApplyObservation(t, &session, core.RelayObservationSessionRestarted{})
			if got := session.InterruptionEventOrdinals(); !reflect.DeepEqual(got, []uint64{interruptedAt}) {
				t.Fatalf("interruptions = %v, want [%d]", got, interruptedAt)
			}
		}
	}
}
