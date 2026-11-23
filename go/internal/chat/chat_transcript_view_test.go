// Tests of mj-chat/src/chat/transcript.rs (chat/transcript/tests.rs),
// continued: terminal output, browser projection, selection, scrollbar and
// link routing.

package chat

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func TestMaterializedTerminalContentRendersOutputAndExitSummary(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-terminal")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:bash",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(
				`{"toolCallId":"bash","title":"Bash","status":"completed","content":[{"type":"terminal","terminalId":"term-1"}]}`,
			)),
			TerminalOutputs: []core.TerminalOutputRecord{{
				TerminalID: "term-1",
				// Colored output from a real build tool: the escape must not
				// survive into the terminal hel is drawing on.
				Output:   core.NewPackedText("\x1b[32mtests passed\x1b[0m"),
				ExitCode: transcriptPtr(uint32(0)),
			}},
			TerminalRefs: []string{"term-1"},
		},
	}}

	entries := client.MaterializedChatEntries(&session)
	assertStrings(t, entries[0].ToolContent, "tests passed\nexited 0")

	chat := ChatStateFromMaterialized(&session, nil, nil)
	chat.renderMode = client.TranscriptRenderModeRaw
	rendered := transcriptText(chat, 80)
	if !shows(rendered, "tests passed") {
		t.Fatalf("raw rows show the captured output: %#v", rendered)
	}
	if !shows(rendered, "exited 0") {
		t.Fatalf("raw rows show how the terminal ended: %#v", rendered)
	}
	if shows(rendered, "terminal term-1") {
		t.Fatalf("the id placeholder is replaced once output exists: %#v", rendered)
	}
	if shows(rendered, "\x1b") {
		t.Fatalf("escape sequences are sanitized out: %#v", rendered)
	}

	browser := TranscriptSnapshotFromMaterialized(&session).BrowserTranscript(nil)
	if !slices.Equal(browser.Entries[0].Lines, []string{"Bash"}) {
		t.Fatalf("the remote viewer shows the decluttered title, not the output: %#v", browser.Entries[0].Lines)
	}
}

func TestKimiTextAndCapturedTerminalOutputRenderOnceAndOnlyInRawMode(t *testing.T) {
	const output = "toolchain inventory"
	session := core.MaterializedSessionEmpty("session-kimi-terminal")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:kimi-shell",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(
				`{"toolCallId":"kimi-shell","title":"Execute ` + "`inspect toolchain`" + `","status":"completed","content":[{"type":"content","content":{"type":"text","text":` +
					transcriptJSONString(output) + `}}],"rawOutput":{"type":"Bash","output":` + transcriptJSONBytes(output) +
					`,"exit_code":1,"command":"inspect toolchain"}}`,
			)),
			TerminalOutputs: []core.TerminalOutputRecord{{
				TerminalID: "term-1",
				Output:     core.NewPackedText(output),
				ExitCode:   transcriptPtr(uint32(1)),
			}},
			TerminalRefs: []string{"term-1"},
		},
	}}

	entries := client.MaterializedChatEntries(&session)
	assertStrings(t, entries[0].ToolContent, output, "exited 1")

	chat := ChatStateFromMaterialized(&session, nil, nil)
	rich := transcriptText(chat, 80)
	if shows(rich, output) {
		t.Fatalf("Rich mode shows the tool call, not its duplicate output: %#v", rich)
	}

	chat.renderMode = client.TranscriptRenderModeRaw
	raw := transcriptText(chat, 80)
	if countContaining(raw, output) != 1 {
		t.Fatalf("Raw mode keeps one copy of the output: %#v", raw)
	}
	if !shows(raw, "exited 1") {
		t.Fatalf("%#v", raw)
	}

	if lines := browserLines(&session); shows(lines, output) {
		t.Fatalf("the remote Rich feed suppresses the duplicate output: %#v", lines)
	}
}

func TestLegacyKimiDuplicateIsSuppressedWithoutReprojectingHistory(t *testing.T) {
	const output = "legacy failed output"
	session := core.MaterializedSessionEmpty("session-legacy-kimi-terminal")
	session.AppliedEventOrdinal = 2
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{
		{
			StableID:        "tool:kimi-shell",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 2,
			Body: core.TranscriptBodyTool{
				Call: core.OpaqueToolCallFromValue(json.RawMessage(
					`{"toolCallId":"kimi-shell","title":"Execute ` + "`inspect toolchain`" + `","status":"completed","content":[{"type":"content","content":{"type":"text","text":` +
						transcriptJSONString(output) + `}}],"rawOutput":{"type":"Bash","output":` + transcriptJSONBytes(output) +
						`,"exit_code":1,"command":"inspect toolchain"}}`,
				)),
			},
		},
		{
			StableID:        "terminal:term-1",
			Position:        2,
			CreatedAtMs:     2,
			LastChangedAtMs: 2,
			Body: core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
				TerminalID: "term-1",
				Output:     core.NewPackedText(output),
				ExitCode:   transcriptPtr(uint32(1)),
			}),
		},
	}

	entries := client.MaterializedChatEntries(&session)
	if !entries[1].RawOnly {
		t.Fatal("the legacy duplicate is raw-only")
	}
	chat := ChatStateFromMaterialized(&session, nil, nil)
	rich := transcriptText(chat, 80)
	if shows(rich, output) {
		t.Fatalf("an existing duplicate becomes quiet after upgrading: %#v", rich)
	}
	if lines := browserLines(&session); shows(lines, output) {
		t.Fatalf("%#v", lines)
	}
}

const standaloneOutput = "cargo build finished"

func terminalRecord(exitCode *uint32, signal *string) core.TerminalOutputRecord {
	return core.TerminalOutputRecord{
		TerminalID: "term-1",
		Output:     core.NewPackedText(standaloneOutput),
		ExitCode:   exitCode,
		Signal:     signal,
	}
}

func terminalOutputItem(position uint64, record core.TerminalOutputRecord) *core.TranscriptItem {
	return &core.TranscriptItem{
		StableID:        "terminal:" + record.TerminalID,
		Position:        position,
		CreatedAtMs:     int64(position),
		LastChangedAtMs: int64(position),
		Body:            core.TranscriptBodyTerminalOutputOf(record),
	}
}

// standaloneTerminalSession is a hel-hosted command whose output no tool call
// refers to, after an agent message so the feed has something else to show.
func standaloneTerminalSession(record core.TerminalOutputRecord) *core.MaterializedSession {
	session := core.MaterializedSessionEmpty("session-standalone-terminal")
	session.AppliedEventOrdinal = 2
	session.Transcript = []*core.TranscriptItem{
		agentMessageItem("agent:1", 1, "running the build"),
		terminalOutputItem(2, record),
	}
	return &session
}

func fallbackTerminalSession(t *testing.T, record core.TerminalOutputRecord) *core.MaterializedSession {
	t.Helper()
	call := core.FallbackTerminalToolCall(record.TerminalID, "cargo build")
	status := schema.ToolCallStatusFailed
	if record.ExitedCleanly() {
		status = schema.ToolCallStatusCompleted
	}
	call.Status = &status
	encoded, err := json.Marshal(call)
	if err != nil {
		t.Fatal(err)
	}
	session := core.MaterializedSessionEmpty("session-fallback-terminal")
	session.AppliedEventOrdinal = 1
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:" + string(call.ToolCallID),
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 2,
		Body: core.TranscriptBodyTool{
			Call:            core.OpaqueToolCallFromValue(encoded),
			TerminalRefs:    []string{record.TerminalID},
			TerminalOutputs: []core.TerminalOutputRecord{record},
		},
	}}
	return &session
}

func browserLines(session *core.MaterializedSession) []string {
	var lines []string
	for _, entry := range TranscriptSnapshotFromMaterialized(session).BrowserTranscript(nil).Entries {
		lines = append(lines, entry.Lines...)
	}
	return lines
}

func TestACleanlyExitedStandaloneTerminalItemRendersOnlyInRawMode(t *testing.T) {
	session := standaloneTerminalSession(terminalRecord(transcriptPtr(uint32(0)), nil))

	chat := ChatStateFromMaterialized(session, nil, nil)
	rich := transcriptText(chat, 80)
	if !shows(rich, "running the build") {
		t.Fatalf("the rest of the conversation still renders: %#v", rich)
	}
	if shows(rich, standaloneOutput) {
		t.Fatalf("a clean command's output is left out of the rich feed: %#v", rich)
	}
	if shows(rich, "exited 0") {
		t.Fatalf("and so is its exit summary: %#v", rich)
	}

	browser := browserLines(session)
	if !shows(browser, "running the build") {
		t.Fatalf("the rest of the conversation still reaches the remote viewer: %#v", browser)
	}
	if shows(browser, standaloneOutput) {
		t.Fatalf("the remote viewer mirrors the rich feed: %#v", browser)
	}

	chat.renderMode = client.TranscriptRenderModeRaw
	raw := transcriptText(chat, 80)
	if !shows(raw, standaloneOutput) {
		t.Fatalf("raw rows keep the captured output: %#v", raw)
	}
	if !shows(raw, "exited 0") {
		t.Fatalf("raw rows keep how the terminal ended: %#v", raw)
	}
}

func TestAFailedFallbackTerminalToolRemainsVisible(t *testing.T) {
	session := fallbackTerminalSession(t, terminalRecord(transcriptPtr(uint32(3)), nil))
	entries := client.MaterializedChatEntries(session)
	if entries[0].RawOnly {
		t.Fatal("the failed fallback is not raw-only")
	}

	chat := ChatStateFromMaterialized(session, nil, nil)
	rich := transcriptText(chat, 80)
	if !shows(rich, "cargo build") {
		t.Fatalf("%#v", rich)
	}
	if !shows(rich, standaloneOutput) {
		t.Fatalf("%#v", rich)
	}
}

func TestAnAbnormallyEndedStandaloneTerminalItemRendersEverywhere(t *testing.T) {
	for _, testCase := range []struct {
		record  core.TerminalOutputRecord
		summary string
	}{
		{terminalRecord(transcriptPtr(uint32(3)), nil), "exited 3"},
		{terminalRecord(nil, transcriptPtr("SIGKILL")), "killed by SIGKILL"},
		{terminalRecord(transcriptPtr(uint32(0)), transcriptPtr("SIGKILL")), "killed by SIGKILL"},
		{terminalRecord(nil, nil), "exit status unavailable"},
	} {
		session := standaloneTerminalSession(testCase.record)

		chat := ChatStateFromMaterialized(session, nil, nil)
		rich := transcriptText(chat, 80)
		if !shows(rich, standaloneOutput) {
			t.Fatalf("%s: the rich feed keeps the output: %#v", testCase.summary, rich)
		}
		if !shows(rich, testCase.summary) {
			t.Fatalf("%s: the rich feed says how it ended: %#v", testCase.summary, rich)
		}

		browser := browserLines(session)
		if !shows(browser, standaloneOutput) {
			t.Fatalf("%s: the remote viewer keeps the output: %#v", testCase.summary, browser)
		}
		if !shows(browser, testCase.summary) {
			t.Fatalf("%s: the remote viewer says how it ended: %#v", testCase.summary, browser)
		}
	}
}

func TestACleanStandaloneTerminalItemBetweenCompletedToolsKeepsOneRun(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-terminal-between-tools")
	session.AppliedEventOrdinal = 3
	session.Transcript = []*core.TranscriptItem{
		fixtureToolItem(1),
		terminalOutputItem(2, terminalRecord(transcriptPtr(uint32(0)), nil)),
		fixtureToolItem(3),
	}
	chat := ChatStateFromMaterialized(&session, nil, nil)
	chat.entries = append(chat.entries, core.ChatEntryPlain(4, core.ChatRoleUser, "now ship it"))

	text := transcriptText(chat, 80)

	if !slices.Equal(text, []string{
		"✓ Tool · done",
		"│ read, read",
		"",
		"❯ You",
		"│ now ship it",
		"",
	}) {
		t.Fatalf("the omitted entry neither renders nor splits the run: %#v", text)
	}
}

// Grok Build's final update replaces `content` with plain text, so the output
// hel captured is attached to the item with nothing in the call pointing at
// it. It is still the only copy of what the command printed.
// Hard-won: b364ed85: Grok replacing a call terminal reference lost its captured output.
func TestAttachedTerminalOutputRendersWhenTheCallNoLongerRefersToIt(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-dropped-terminal")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:bash",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(
				`{"toolCallId":"bash","title":"Bash","status":"completed","content":[{"type":"content","content":{"type":"text","text":"ran the build"}}]}`,
			)),
			TerminalOutputs: []core.TerminalOutputRecord{{
				TerminalID: "term-1",
				Output:     core.NewPackedText("build finished"),
				ExitCode:   transcriptPtr(uint32(0)),
			}},
			TerminalRefs: []string{"term-1"},
		},
	}}

	entries := client.MaterializedChatEntries(&session)
	if !slices.Equal(entries[0].ToolContent, []string{"ran the build", "build finished\nexited 0"}) {
		t.Fatalf("the captured output follows the content the call still carries: %#v", entries[0].ToolContent)
	}
}

// Codex runs the command in its own terminal, which hel never opened, and
// reports the text in `rawOutput` beside the reference.
func TestCodexRawOutputRendersForATerminalHelHasNoRecordFor(t *testing.T) {
	details := func(rawOutput string) []string {
		var call acp.ToolCall
		if err := json.Unmarshal([]byte(
			`{"toolCallId":"exec","title":"Shell","status":"completed","content":[{"type":"terminal","terminalId":"exec-1"}],"rawOutput":`+rawOutput+`}`,
		), &call); err != nil {
			t.Fatalf("valid ACP tool call: %v", err)
		}
		status := acp.ToolCallStatusPending
		if call.Status != nil {
			status = *call.Status
		}
		return transcript.ToolContentDetails(
			call.Content,
			nil,
			call.RawOutput,
			core.ToolStatusOf(status),
		)
	}

	assertStrings(t, details(`{"formatted_output":"tests passed","exit_code":0}`), "tests passed\nexited 0")
	if got := details(`{"formatted_output":"still running"}`); !slices.Equal(got, []string{"still running"}) {
		t.Fatalf("an exit line needs an exit code to report: %#v", got)
	}
	if got := details(`{"exit_code":0}`); !slices.Equal(got, []string{"terminal exec-1"}) {
		t.Fatalf("without output there is nothing to show but the id: %#v", got)
	}
}

// executeToolCallValue is ToolCall::new(id, "Bash").kind(Execute)
// .status(Completed).raw_input({"command": command}) as JSON.
func executeToolCallValue(t *testing.T, id string, command string) json.RawMessage {
	t.Helper()
	kind := acp.ToolKindExecute
	status := acp.ToolCallStatusCompleted
	call := acp.ToolCall{
		ToolCallID: acp.ToolCallId(id),
		Title:      "Bash",
		Kind:       &kind,
		Status:     &status,
		RawInput:   json.RawMessage(`{"command":` + transcriptJSONString(command) + `}`),
	}
	encoded, err := json.Marshal(call)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func TestCurrentStoredToolPresentationIsPreserved(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-stored-tool-summary")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:git",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(executeToolCallValue(t, "git", "git add src/lib.rs")),
			Presentation: &core.ToolCallPresentation{
				Summary:        "git",
				Source:         core.NewPackedText("git add src/lib.rs"),
				SourceKind:     core.ToolSummarySourceKindRawInput,
				ToolKind:       acp.ToolKindExecute,
				SummaryVersion: transcript.ToolSummaryVersion,
			},
		},
	}}

	browser := TranscriptSnapshotFromMaterialized(&session).BrowserTranscript(nil)
	assertStrings(t, browser.Entries[0].Lines, "git")
}

func TestStaleStoredToolPresentationIsReparsedForRichSurfaces(t *testing.T) {
	source := "python3 <<'PYEOF'\nprint('x')\nPYEOF\ngrep -n x file | head"
	session := core.MaterializedSessionEmpty("session-stale-tool-summary")
	session.AppliedEventOrdinal = 1
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:heredoc",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(executeToolCallValue(t, "heredoc", source)),
			Presentation: &core.ToolCallPresentation{
				Summary:        "python3 grep | head",
				Source:         core.NewPackedText(source),
				SourceKind:     core.ToolSummarySourceKindRawInput,
				ToolKind:       acp.ToolKindExecute,
				SummaryVersion: 0,
			},
		},
	}}

	browser := TranscriptSnapshotFromMaterialized(&session).BrowserTranscript(nil)
	assertStrings(t, browser.Entries[0].Lines, "python3 ; grep | head")
}

func TestBrowserUsesRichGroupOrderAndChangesItsTopologyKey(t *testing.T) {
	first := completedExecuteTool(1, "Running: git add src/lib.rs")
	initial := TranscriptSnapshotFromEntries([]core.ChatEntry{first}).BrowserTranscript(nil)
	if len(initial.Entries) != 1 {
		t.Fatalf("entries %d", len(initial.Entries))
	}
	assertStrings(t, initial.Entries[0].Lines, "git add")

	grouped := TranscriptSnapshotFromEntries([]core.ChatEntry{
		first,
		thought(2, "checking commands"),
		completedExecuteTool(3, "Running: gh pr create --draft"),
	}).BrowserTranscript(nil)

	if grouped.PresentationKey == initial.PresentationKey {
		t.Fatal("the topology key changes")
	}
	if len(grouped.Entries) != 2 {
		t.Fatalf("entries %d", len(grouped.Entries))
	}
	if grouped.Entries[0].Role != "thought" {
		t.Fatalf("role %q", grouped.Entries[0].Role)
	}
	assertStrings(t, grouped.Entries[0].Lines, "checking commands")
	if grouped.Entries[1].Role != "tool" {
		t.Fatalf("role %q", grouped.Entries[1].Role)
	}
	if grouped.Entries[1].ID != 1 {
		t.Fatalf("id %d", grouped.Entries[1].ID)
	}
	if grouped.Entries[1].UpdatedSeq != 3 {
		t.Fatalf("updated seq %d", grouped.Entries[1].UpdatedSeq)
	}
	assertStrings(t, grouped.Entries[1].Lines, "git add, gh pr create")
}

// transcriptPane is the transcript pane the last frame registered.
func transcriptPane(t *testing.T, chat *ChatState) SurfaceFrame {
	t.Helper()
	pane := chat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if pane == nil {
		t.Fatal("the transcript is registered")
	}
	return *pane
}

// autoscrollTick is one auto-scroll tick: scroll the transcript, redraw so
// the registry describes the rows now on screen, and re-resolve the still
// pointer against it. This is the sequence the dashboard loop runs on its
// interval.
func autoscrollTick(chat *ChatState, selection *SelectionState, direction int8) {
	if direction < 0 {
		chat.scrollHistoryUp(3)
	} else {
		chat.scrollHistoryDown(3)
	}
	drawnTranscriptSelecting(chat, 40, 12, true)
	selection.Retrack(chat.FrameSurfaces())
}

func selectionRange(t *testing.T, selection *SelectionState, why string) SelectionRange {
	t.Helper()
	selected, ok := selection.Range()
	if !ok {
		t.Fatal(why)
	}
	return selected
}

// A drag held at the transcript's top edge keeps pulling older rows into the
// selection, and the copied text is the cached rows for the whole span —
// including the rows the viewport has scrolled past.
func TestAutoscrollingATranscriptDragSelectsRowsTheViewportScrolledPast(t *testing.T) {
	chat := numberedChat(60)
	drawnTranscript(chat, 40, 12)
	rows := transcriptText(chat, 40)
	pane := transcriptPane(t, chat)
	height := pane.Rect.Dy()
	selection := NewSelectionState()

	// Press on the last visible row, then drag onto the top edge, which is
	// where a held pointer asks for auto-scroll.
	selection.OnMouseDown(pane.Rect.Max.X-1, pane.Rect.Max.Y-1, chat.FrameSurfaces())
	selection.OnMouseDrag(pane.Rect.Min.X, pane.Rect.Min.Y, chat.FrameSurfaces())
	dragging := selectionRange(t, &selection, "dragging")
	span := dragging.End.Row - dragging.Start.Row
	if span+1 != height {
		t.Fatalf("the drag starts covering the viewport: span %d, height %d", span, height)
	}
	surface, direction, ok := selection.AutoscrollRequest(chat.FrameSurfaces())
	if !ok || surface != (SurfaceIdTranscript{}) || direction != -1 {
		t.Fatalf("autoscroll %#v %d %v", surface, direction, ok)
	}

	for range 4 {
		autoscrollTick(chat, &selection, -1)
		grownRange := selectionRange(t, &selection, "still dragging")
		grown := grownRange.End.Row - grownRange.Start.Row
		if grown != span+3 {
			t.Fatalf("each tick pulls three more rows in: %d, want %d", grown, span+3)
		}
		span = grown
	}

	selected := selectionRange(t, &selection, "still dragging")
	if span+1 <= height {
		t.Fatal("the selection outgrew the viewport it started in")
	}
	copied, ok := chat.TranscriptSelectionText(&selected)
	if !ok {
		t.Fatal("the selection has text")
	}
	start := len(rows) - (span + 1)
	// Copying leaves out the role gutter, which only decorates a row.
	expected := make([]string, 0, len(rows)-start)
	for _, row := range rows[start:] {
		expected = append(expected, strings.TrimRightFunc(strings.TrimPrefix(row, roleGutter()), unicode.IsSpace))
	}
	assertStrings(t, strings.Split(copied, "\n"), expected...)
}

func TestScrollingUnderAFrozenBaseMovesTheRegisteredTopRowByTheRowsCrossed(t *testing.T) {
	chat := numberedChat(60)
	drawnTranscript(chat, 40, 12)
	pinned := transcriptPane(t, chat).TopRow

	chat.scrollHistoryUp(3)
	drawnTranscriptSelecting(chat, 40, 12, true)
	if got := pinned - transcriptPane(t, chat).TopRow; got != 3 {
		t.Fatalf("moved %d", got)
	}

	chat.scrollHistoryUp(7)
	drawnTranscriptSelecting(chat, 40, 12, true)
	if got := pinned - transcriptPane(t, chat).TopRow; got != 10 {
		t.Fatalf("moved %d", got)
	}

	chat.scrollHistoryDown(4)
	drawnTranscriptSelecting(chat, 40, 12, true)
	if got := pinned - transcriptPane(t, chat).TopRow; got != 6 {
		t.Fatalf("moved %d", got)
	}

	// With no selection to hold it, the base re-pins to whatever is on screen.
	drawnTranscript(chat, 40, 12)
	if got := transcriptPane(t, chat).TopRow; got != pinned {
		t.Fatalf("top row %d, want %d", got, pinned)
	}
}

func TestAWidthChangeOrARebuiltCacheInvalidatesAFrozenRowSpace(t *testing.T) {
	chat := numberedChat(60)
	drawnTranscript(chat, 40, 12)
	drawnTranscriptSelecting(chat, 40, 12, true)
	if chat.TranscriptSelectionInvalidated() {
		t.Fatal("a steady layout keeps the row space")
	}

	drawnTranscriptSelecting(chat, 60, 12, true)
	if !chat.TranscriptSelectionInvalidated() {
		t.Fatal("rewrapped rows are not the rows the selection was measured in")
	}

	drawnTranscriptSelecting(chat, 60, 12, true)
	if chat.TranscriptSelectionInvalidated() {
		t.Fatal("the re-pinned space holds")
	}
	chat.invalidateRenderCache()
	drawnTranscriptSelecting(chat, 60, 12, true)
	if !chat.TranscriptSelectionInvalidated() {
		t.Fatal("a rebuilt cache invalidates the row space")
	}
}

func TestAJumpAcrossTheDeepPastInvalidatesInsteadOfWalkingIt(t *testing.T) {
	chat := newTranscriptChat()
	chat.renderMode = client.TranscriptRenderModeRaw
	chat.entries = make([]core.ChatEntry, 0, 40)
	for index := range uint64(40) {
		rows := make([]string, 0, 700)
		for row := range 700 {
			rows = append(rows, fmt.Sprintf("entry %d row %d", index, row))
		}
		chat.entries = append(chat.entries, core.ChatEntryPlain(index, core.ChatRoleUser, strings.Join(rows, "\n")))
	}
	drawnTranscript(chat, 40, 12)
	drawnTranscriptSelecting(chat, 40, 12, true)
	if chat.TranscriptSelectionInvalidated() {
		t.Fatal("a steady layout keeps the row space")
	}

	chat.HandleKey(ctrlKey(tea.KeyHome))
	drawnTranscriptSelecting(chat, 40, 12, true)

	if !chat.TranscriptSelectionInvalidated() {
		t.Fatal("a jump past the walk budget drops the selection instead of rendering the history")
	}
}

// A range that stops mid-row is cut on the cells the row occupies, so a wide
// grapheme is never split into half a character.
func TestATranscriptEndpointRowIsCutOnTheCellsItOccupies(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, "世界 wide row"))
	drawnTranscript(chat, 40, 24)
	// The body row follows the entry's header; its gutter takes two columns
	// and each of the wide graphemes after it takes two more.
	body := transcriptPane(t, chat).TopRow + 1

	if text, ok := chat.TranscriptSelectionText(&SelectionRange{
		Start: NewContentPos(body, 2),
		End:   NewContentPos(body, 5),
	}); !ok || text != "世界" {
		t.Fatalf("got %q, %v", text, ok)
	}
	if text, ok := chat.TranscriptSelectionText(&SelectionRange{
		Start: NewContentPos(body, 3),
		End:   NewContentPos(body, 8),
	}); !ok || text != "界 wi" {
		t.Fatalf("got %q, %v", text, ok)
	}
}

func scrollbarChat() *ChatState {
	chat := newTranscriptChat()
	var reply strings.Builder
	for line := range 200 {
		fmt.Fprintf(&reply, "line %d\n", line)
	}
	chat.entries = []core.ChatEntry{core.ChatEntryPlain(1, core.ChatRoleAgent, reply.String())}
	drawnTranscript(chat, 60, 24)
	return chat
}

// scrollbarMouse sends a left-button mouse event: kind is a
// tea.MouseClickMsg (Down), tea.MouseMotionMsg (Drag) or tea.MouseReleaseMsg
// (Up) whose position is replaced by column, row.
func scrollbarMouse(chat *ChatState, kind tea.MouseMsg, column int, row int) {
	mouse := kind.Mouse()
	mouse.X, mouse.Y = column, row
	mouse.Button = tea.MouseLeft
	switch kind.(type) {
	case tea.MouseClickMsg:
		chat.HandleMouse(tea.MouseClickMsg(mouse))
	case tea.MouseReleaseMsg:
		chat.HandleMouse(tea.MouseReleaseMsg(mouse))
	default:
		chat.HandleMouse(tea.MouseMotionMsg(mouse))
	}
}

func longReply(seq uint64) core.ChatEntry {
	var reply strings.Builder
	for line := range 100 {
		fmt.Fprintf(&reply, "line %d\n", line)
	}
	return core.ChatEntryPlain(seq, core.ChatRoleAgent, reply.String())
}

// markedScrollbarChat is the user's messages with long replies, and an
// automatic continuation between them that is a user-role prompt the user
// did not write.
func markedScrollbarChat() *ChatState {
	generated := core.ChatEntryPlain(3, core.ChatRoleUser, "Continue.")
	item := *userTranscriptItem(3, "Continue.")
	item.StableID = "user:auto-continue-1-1"
	generated.Source = core.TranscriptSource{Item: &item}
	chat := newTranscriptChat()
	chat.entries = []core.ChatEntry{
		core.ChatEntryPlain(1, core.ChatRoleUser, "first request"),
		longReply(2),
		generated,
		longReply(4),
		core.ChatEntryPlain(5, core.ChatRoleUser, "second request"),
		longReply(6),
		longReply(7),
	}
	return chat
}

func trackSymbol(rows []string, geometry ScrollbarGeometry, row int) string {
	characters := []rune(rows[row])
	if geometry.Track.Min.X < len(characters) {
		return string(characters[geometry.Track.Min.X])
	}
	return ""
}

func transcriptScrollbarGeometry(t *testing.T, chat *ChatState) ScrollbarGeometry {
	t.Helper()
	geometry, ok := chat.transcriptScrollbar.pointer.Geometry()
	if !ok {
		t.Fatal("the scrollbar is drawn")
	}
	return geometry
}

func TestScrollbarMarksMessagesTheUserWroteButNotGeneratedPrompts(t *testing.T) {
	chat := markedScrollbarChat()
	rows := drawnTranscript(chat, 60, 24)
	var entries []int
	for _, message := range chat.transcriptScrollbar.userMessages {
		entries = append(entries, message.entry)
	}
	if !slices.Equal(entries, []int{0, 4}) {
		t.Fatalf("entries %v", entries)
	}
	geometry := transcriptScrollbarGeometry(t, chat)
	marks := slices.Clone(chat.transcriptScrollbar.marks)
	if len(marks) == 0 {
		t.Fatal("the user messages are marked")
	}
	for row := geometry.Track.Min.Y; row < geometry.Track.Max.Y; row++ {
		expected := ThemeGlyphs().ScrollTrack
		switch {
		case row >= geometry.Thumb.Min.Y && row < geometry.Thumb.Max.Y:
			expected = ThemeGlyphs().ScrollThumb
		case slices.ContainsFunc(marks, func(mark UserMessageMark) bool { return mark.trackRow == row }):
			expected = ThemeGlyphs().ScrollMark
		}
		if got := trackSymbol(rows, geometry, row); got != expected {
			t.Fatalf("track row %d: %q, want %q", row, got, expected)
		}
	}
}

func TestClickingAScrollbarMarkOpensItsMessage(t *testing.T) {
	chat := markedScrollbarChat()
	drawnTranscript(chat, 60, 24)
	// Neither message has been drawn yet, so both marks sit at estimated rows.
	marks := slices.Clone(chat.transcriptScrollbar.marks)
	var entries []int
	for _, mark := range marks {
		entries = append(entries, mark.entry)
	}
	if !slices.Equal(entries, []int{0, 4}) {
		t.Fatalf("entries %v", entries)
	}
	for index, text := range []string{"first request", "second request"} {
		mark := marks[index]
		if chat.renderCache.entries[mark.entry] != nil {
			t.Fatalf("entry %d is not drawn yet", mark.entry)
		}
		geometry := transcriptScrollbarGeometry(t, chat)
		scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Track.Min.X, mark.trackRow)
		opened := TranscriptAnchorRow{Entry: mark.entry, Row: 0}
		if chat.anchor != TranscriptAnchor(opened) {
			t.Fatalf("anchor %#v, want %#v", chat.anchor, opened)
		}
		if chat.TranscriptScrollbarDragging() {
			t.Fatal("a mark click starts no drag")
		}
		rows := drawnTranscript(chat, 60, 24)
		if !strings.Contains(rows[2], text) {
			t.Fatalf("%#v", rows)
		}
		// Return to the opening view for the next mark.
		chat.setAnchor(TranscriptAnchorRow{Entry: 6, Row: 0})
		drawnTranscript(chat, 60, 24)
	}
}

func TestTextEstimatesMatchTheDrawnRowsOfCommonEntries(t *testing.T) {
	tool := core.ChatEntryPlain(5, core.ChatRoleTool, "Run cargo test")
	tool.ToolStatus = transcriptPtr(core.ToolStatusCompleted)
	tool.ToolSummary = transcriptPtr("cargo test")
	var output strings.Builder
	for line := range 30 {
		fmt.Fprintf(&output, "out %d\n", line)
	}
	tool.ToolContent = []string{output.String()}
	entries := []core.ChatEntry{
		core.ChatEntryPlain(1, core.ChatRoleUser, "fix the build"),
		core.ChatEntryPlain(2, core.ChatRoleAgent, "one\ntwo\nthree"),
		core.ChatEntryPlain(3, core.ChatRoleAgent, strings.Repeat("word ", 60)),
		core.ChatEntryPlan(4, []core.PlanLine{
			{Text: "first step", Status: core.PlanStatusCompleted},
			{Text: "second step", Status: core.PlanStatusPending},
		}),
		tool,
	}
	inline := ToolViewInline{OutputLines: 6}
	for _, width := range []int{40, 80} {
		for _, mode := range []client.TranscriptRenderMode{client.TranscriptRenderModeRich, client.TranscriptRenderModeRaw} {
			for _, view := range []ToolView{ToolViewSummary{}, inline} {
				for index := range entries {
					entry := &entries[index]
					drawn := len(renderTranscriptEntryWithOptions(entry, width, mode, view))
					estimated := estimatedEntryRows(entry, width, mode, view)
					if estimated != drawn {
						t.Fatalf("%v %q at width %d in %v with %#v: estimated %d, drawn %d",
							entry.Role, entry.Text, width, mode, view, estimated, drawn)
					}
				}
			}
		}
	}
}

// altKey is a press of a named key with Alt held.
func altKey(code rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: code, Mod: tea.ModAlt}
}

func TestAltPageKeysJumpBetweenTheMessagesTheUserWrote(t *testing.T) {
	chat := markedScrollbarChat()
	drawnTranscript(chat, 60, 24)
	assertAnchor := func(want TranscriptAnchor) {
		t.Helper()
		if chat.anchor != want {
			t.Fatalf("anchor %#v, want %#v", chat.anchor, want)
		}
	}
	// The opening reveal parks a new view on the latest reply.
	assertAnchor(TranscriptAnchorRow{Entry: 6, Row: 0})

	chat.HandleKey(altKey(tea.KeyPgUp))
	assertAnchor(TranscriptAnchorRow{Entry: 4, Row: 0})
	rows := drawnTranscript(chat, 60, 24)
	if !strings.Contains(rows[2], "second request") {
		t.Fatalf("%#v", rows)
	}

	// The automatic continuation at entry 2 is not a stop.
	chat.HandleKey(altKey(tea.KeyPgUp))
	assertAnchor(TranscriptAnchorRow{Entry: 0, Row: 0})
	chat.HandleKey(altKey(tea.KeyPgUp))
	assertAnchor(TranscriptAnchorRow{Entry: 0, Row: 0})

	// Part way into a reply, the previous stop is the message above it.
	drawnTranscript(chat, 60, 24)
	chat.HandleKey(key(tea.KeyPgDown))
	if anchor, ok := chat.anchor.(TranscriptAnchorRow); !ok || anchor.Entry != 1 {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	chat.HandleKey(altKey(tea.KeyPgUp))
	assertAnchor(TranscriptAnchorRow{Entry: 0, Row: 0})

	chat.HandleKey(altKey(tea.KeyPgDown))
	assertAnchor(TranscriptAnchorRow{Entry: 4, Row: 0})
	chat.HandleKey(altKey(tea.KeyPgDown))
	assertAnchor(TranscriptAnchorBottom{})
}

func TestAConversationThatFitsHasNoScrollbarMarks(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = []core.ChatEntry{
		core.ChatEntryPlain(1, core.ChatRoleUser, "first request"),
		core.ChatEntryPlain(2, core.ChatRoleAgent, "done"),
		core.ChatEntryPlain(3, core.ChatRoleUser, "second request"),
	}
	rows := drawnTranscript(chat, 60, 24)
	geometry := transcriptScrollbarGeometry(t, chat)
	if geometry.MaxScroll != 0 {
		t.Fatalf("max scroll %d", geometry.MaxScroll)
	}
	if len(chat.transcriptScrollbar.marks) != 0 {
		t.Fatalf("marks %#v", chat.transcriptScrollbar.marks)
	}
	for row := geometry.Track.Min.Y; row < geometry.Track.Max.Y; row++ {
		if trackSymbol(rows, geometry, row) == ThemeGlyphs().ScrollMark {
			t.Fatalf("track row %d is marked", row)
		}
	}
}

func TestScrollbarMarksSkipTheThumbAndMergeRunsIntoTheOldestMessage(t *testing.T) {
	// A 20-row track over 200 content rows maps 10 content rows to each track
	// row. The thumb covers track rows 8 and 9.
	track := uv.Rect(0, 0, 1, 20)
	geometry, ok := NewScrollbarGeometry(track, 200, 80, 20)
	if !ok {
		t.Fatal("the track has geometry")
	}
	if geometry.Thumb.Min.Y != 8 || geometry.Thumb.Dy() != 2 {
		t.Fatalf("thumb %v", geometry.Thumb)
	}
	messages := func(rows []int) []UserMessageRow {
		result := make([]UserMessageRow, 0, len(rows))
		for entry, row := range rows {
			result = append(result, UserMessageRow{entry: entry, row: row})
		}
		return result
	}
	marks := func(rows ...int) [][2]int {
		var result [][2]int
		for _, mark := range userMessageMarks(geometry, 200, messages(rows)) {
			result = append(result, [2]int{mark.trackRow, mark.entry})
		}
		return result
	}
	// Separate messages each get a mark.
	if got := marks(0, 30, 150); !reflect.DeepEqual(got, [][2]int{{0, 0}, {3, 1}, {15, 2}}) {
		t.Fatalf("marks %v", got)
	}
	// A run of messages on adjacent or equal rows shows only its oldest.
	if got := marks(30, 35, 40, 50, 70); !reflect.DeepEqual(got, [][2]int{{3, 0}, {7, 4}}) {
		t.Fatalf("rows 3, 3, 4, 5 merge; row 7 is two rows below the run: %v", got)
	}
	// Messages under the thumb are on screen and get no mark. The thumb
	// separates the marks on either side of it.
	if got := marks(70, 80, 95, 100); !reflect.DeepEqual(got, [][2]int{{7, 0}, {10, 3}}) {
		t.Fatalf("marks %v", got)
	}
	// Nothing is marked when all the content fits.
	fitting, ok := NewScrollbarGeometry(track, 20, 0, 20)
	if !ok {
		t.Fatal("the track has geometry")
	}
	if got := userMessageMarks(fitting, 20, messages([]int{0, 5})); len(got) != 0 {
		t.Fatalf("marks %v", got)
	}
}

func TestScrollbarDragReachesBothEndsAndReleaseStopsCapture(t *testing.T) {
	chat := scrollbarChat()
	geometry := transcriptScrollbarGeometry(t, chat)
	if geometry.MaxScroll == 0 {
		t.Fatal("the reply overflows")
	}
	scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Thumb.Min.X, geometry.Thumb.Min.Y)
	if !chat.TranscriptScrollbarDragging() {
		t.Fatal("the thumb is held")
	}
	scrollbarMouse(chat, tea.MouseMotionMsg{}, 0, 0)
	if chat.anchor != TranscriptAnchor(TranscriptAnchorRow{Entry: 0, Row: 0}) {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	drawnTranscript(chat, 60, 24)
	scrollbarMouse(chat, tea.MouseMotionMsg{}, 0, 65535)
	if !anchorIsBottom(chat.anchor) {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	scrollbarMouse(chat, tea.MouseReleaseMsg{}, 0, 65535)
	if chat.TranscriptScrollbarDragging() {
		t.Fatal("release stops capture")
	}
	scrollbarMouse(chat, tea.MouseMotionMsg{}, 0, 0)
	if !anchorIsBottom(chat.anchor) {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	chat.entries = append(chat.entries, core.ChatEntryPlain(2, core.ChatRoleAgent, "new output"))
	rows := drawnTranscript(chat, 60, 24)
	if !shows(rows, "new output") {
		t.Fatalf("%#v", rows)
	}
}

func TestGrabbingScrollbarThumbDoesNotJumpAndPreservesGrabOffset(t *testing.T) {
	chat := scrollbarChat()
	var reply strings.Builder
	for line := range 40 {
		fmt.Fprintf(&reply, "line %d\n", line)
	}
	chat.entries[0] = core.ChatEntryPlain(1, core.ChatRoleAgent, reply.String())
	chat.invalidateRenderCache()
	chat.anchor = TranscriptAnchorBottom{}
	drawnTranscript(chat, 60, 24)
	geometry := transcriptScrollbarGeometry(t, chat)
	if geometry.Thumb.Dy() <= 1 {
		t.Fatalf("thumb %v", geometry.Thumb)
	}
	grabRow := geometry.Thumb.Max.Y - 1
	scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Track.Min.X, grabRow)
	if !anchorIsBottom(chat.anchor) {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	scrollbarMouse(chat, tea.MouseMotionMsg{}, geometry.Track.Min.X, grabRow-1)
	if _, ok := chat.anchor.(TranscriptAnchorRow); !ok {
		t.Fatalf("anchor %#v", chat.anchor)
	}
	if got := chat.transcriptScrollbar.pointer.GrabOffset(); got != geometry.Thumb.Dy()-1 {
		t.Fatalf("grab offset %d", got)
	}
}

func TestScrollbarKeepsUnseenHistoryLazyAndCancelsDragOnResize(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = make([]core.ChatEntry, 0, 1000)
	for index := range uint64(1000) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(index, core.ChatRoleUser, "message"))
	}
	drawnTranscript(chat, 60, 24)
	drawn := 0
	for _, entry := range chat.renderCache.entries {
		if entry != nil {
			drawn++
		}
	}
	if drawn >= 30 {
		t.Fatalf("%d entries were drawn", drawn)
	}
	geometry := transcriptScrollbarGeometry(t, chat)
	scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Thumb.Min.X, geometry.Thumb.Min.Y)
	drawnTranscript(chat, 40, 20)
	if chat.TranscriptScrollbarDragging() {
		t.Fatal("a resize cancels the drag")
	}
}

func TestScrollbarDragKeepsItsMappingWhenHistoryRendersOrOutputArrives(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = make([]core.ChatEntry, 0, 100)
	for index := range uint64(100) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(index, core.ChatRoleUser, strings.Repeat("long message\n", 20)))
	}
	drawnTranscript(chat, 60, 24)
	geometry := transcriptScrollbarGeometry(t, chat)
	scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Thumb.Min.X, geometry.Thumb.Min.Y)
	estimates := slices.Clone(chat.transcriptScrollbar.estimates)
	row := geometry.Track.Min.Y + geometry.Track.Dy()/2
	scrollbarMouse(chat, tea.MouseMotionMsg{}, geometry.Track.Min.X, row)
	drawnTranscript(chat, 60, 24)
	chat.entries = append(chat.entries, core.ChatEntryPlain(100, core.ChatRoleAgent, "new output"))
	drawnTranscript(chat, 60, 24)
	if !chat.TranscriptScrollbarDragging() {
		t.Fatal("the drag survives")
	}
	if !slices.Equal(chat.transcriptScrollbar.estimates, estimates) {
		t.Fatal("the estimates stay frozen during the drag")
	}
	anchor := chat.anchor
	scrollbarMouse(chat, tea.MouseMotionMsg{}, geometry.Track.Min.X, row)
	drawnTranscript(chat, 60, 24)
	if chat.anchor != anchor {
		t.Fatalf("anchor %#v, want %#v", chat.anchor, anchor)
	}
}

func isASCII(text string) bool {
	for index := 0; index < len(text); index++ {
		if text[index] >= 0x80 {
			return false
		}
	}
	return true
}

// `symbols = "ascii"` is for a Linux console or a locale without UTF-8, so
// the conversation has to reach it as well as the borders and status marks.
// The role marks, the header's timestamp separator and the system rule were
// the last Unicode a session with one exchange in it still drew.
func TestTheASCIISymbolSetReachesTheTranscriptRoleMarksAndTimestamps(t *testing.T) {
	chat := newTranscriptChat()
	timed := func(seq uint64, role core.ChatRole, text string) core.ChatEntry {
		entry := core.ChatEntryPlain(seq, role, text)
		entry.RecordedAtMs = transcriptPtr(int64(75_600_000))
		return entry
	}
	chat.entries = []core.ChatEntry{
		timed(1, core.ChatRoleUser, "ask"),
		timed(2, core.ChatRoleAgent, "answer"),
		core.ChatEntryPlain(3, core.ChatRoleThought, "consider"),
		timed(4, core.ChatRoleSystem, "harness session started"),
		core.ChatEntryTool(5, "Edit", nil, core.ToolStatusCompleted),
		core.ChatEntryPlain(6, core.ChatRolePlan, "step one"),
		core.ChatEntryPlain(7, core.ChatRolePlanProposal, "step two"),
	}
	chat.latestSeq = 7

	unicodeRows := transcriptText(chat, 60)
	if !slices.ContainsFunc(unicodeRows, func(line string) bool { return strings.HasPrefix(line, "● Agent · ") }) {
		t.Fatalf("the default set is unchanged: %#v", unicodeRows)
	}

	asciiChat := newTranscriptChat()
	asciiChat.entries = slices.Clone(chat.entries)
	asciiChat.latestSeq = 7
	var ascii []string
	WithSymbols(core.SymbolSetAscii, func() {
		ascii = transcriptText(asciiChat, 60)
	})
	for _, line := range ascii {
		if !isASCII(line) {
			t.Fatalf("%q in %#v", line, ascii)
		}
	}
	for _, expected := range []string{"> You - ", "* Agent - ", "o Thinking", "- Mjolnir - "} {
		if !slices.ContainsFunc(ascii, func(line string) bool { return strings.HasPrefix(line, expected) }) {
			t.Fatalf("%q in %#v", expected, ascii)
		}
	}
}

// sessionDItem is one item of R10's session D, as its store held it.
func sessionDItem(t *testing.T, value string) *core.TranscriptItem {
	t.Helper()
	var item core.TranscriptItem
	if err := json.Unmarshal([]byte(value), &item); err != nil {
		t.Fatal(err)
	}
	return &item
}

// sessionDSleep is session D's `sleep 60` tool call with status, last
// changed at lastChangedAtMs.
func sessionDSleep(t *testing.T, status string, lastChangedAtMs int64) *core.TranscriptItem {
	t.Helper()
	return sessionDItem(t, fmt.Sprintf(`{
		"stable_id": "tool:exec-7ee05740-cd4d-46f7-acda-c5154d550799",
		"position": 27,
		"created_at_ms": 1790343723040,
		"last_changed_at_ms": %d,
		"body": {"kind": "tool", "call": {
			"toolCallId": "exec-7ee05740-cd4d-46f7-acda-c5154d550799",
			"title": "sleep 60", "kind": "execute", "status": %s,
			"rawInput": {"command": "sleep 60"}
		}}
	}`, lastChangedAtMs, transcriptJSONString(status)))
}

// R10-1, replayed in the order session D's journal recorded it. Esc ended
// the turn (the "Interrupted" marker, ordinal 35) while the `sleep 60` Codex
// had started kept running, because Codex owns that process; 48 s later the
// command's completion arrived (ordinal 45) and the row turned from
// "running" into "✓ Tool · done", as if the interrupted turn had finished
// the work.
// Hard-won: 5ff5884d: a tool finishing after interruption was rendered as done.
func TestAToolThatEndsAfterItsTurnWasInterruptedIsNotShownAsDone(t *testing.T) {
	prompt := sessionDItem(t, `{
		"stable_id": "user:prompt-7b3aba1e44a67b21c81f588fb67efd01",
		"position": 9,
		"created_at_ms": 1790343719503,
		"last_changed_at_ms": 1790343719503,
		"body": {"kind": "user", "content": [
			{"type": "text", "text": "Run the shell command sleep 60 and then say done."}
		]}
	}`)
	listing := sessionDItem(t, `{
		"stable_id": "tool:exec-listing",
		"position": 20,
		"created_at_ms": 1790343721000,
		"last_changed_at_ms": 1790343721500,
		"body": {"kind": "tool", "call": {
			"toolCallId": "exec-listing", "title": "ls", "kind": "execute",
			"status": "completed", "rawInput": {"command": "ls"}
		}}
	}`)
	interrupted := sessionDItem(t, `{
		"stable_id": "system:turn-interrupted:prompt-7b3aba1e44a67b21c81f588fb67efd01",
		"position": 35,
		"created_at_ms": 1790343735122,
		"last_changed_at_ms": 1790343735122,
		"body": {"kind": "system", "text": "Interrupted"}
	}`)
	toolRows := func(chat *ChatState) []string {
		var rows []string
		for _, line := range transcriptText(chat, 80) {
			if strings.Contains(line, "Tool") {
				rows = append(rows, line)
			}
		}
		return rows
	}

	// Ordinal 44: the turn has ended and the command is still running.
	session := core.MaterializedSessionEmpty("session-d")
	session.AppliedEventOrdinal = 44
	session.Transcript = []*core.TranscriptItem{
		prompt,
		listing,
		sessionDSleep(t, "in_progress", 1_790_343_723_040),
		interrupted,
	}
	chat := ChatStateFromMaterialized(&session, nil, nil)
	rows := toolRows(chat)
	if !shows(rows, "Tool · running") {
		t.Fatalf("%#v", rows)
	}

	// Ordinal 45: the command ends on its own.
	session.AppliedEventOrdinal = 45
	session.Transcript[2] = sessionDSleep(t, "completed", 1_790_343_782_961)
	chat.ApplyMaterialized(&session, nil, nil)
	rows = toolRows(chat)
	if !shows(rows, "Tool · ended after interrupt") {
		t.Fatalf("%#v", rows)
	}
	// The command that finished before the interruption is still done.
	if countContaining(rows, "Tool · done") != 1 {
		t.Fatalf("%#v", rows)
	}
	// The web viewer's rows come from the same entries.
	browser := TranscriptSnapshotFromMaterialized(&session).BrowserTranscript(nil)
	var labels []string
	for _, entry := range browser.Entries {
		labels = append(labels, entry.Label)
	}
	if !slices.Contains(labels, "Tool · ended after interrupt") || !slices.Contains(labels, "Tool · done") {
		t.Fatalf("%#v", labels)
	}
}

func clickTextAction(t *testing.T, chat *ChatState, rows []string, text string) ChatAction {
	t.Helper()
	column, row := renderedTextPosition(t, rows, text)
	return chat.HandleMouse(leftClick(column, row))
}

func assertAction(t *testing.T, got ChatAction, want ChatAction) {
	t.Helper()
	// The Debug forms carry every field of the actions these tests expect,
	// and read a nil action as ChatAction::None.
	if debugChatAction(got) != debugChatAction(want) {
		t.Fatalf("action %s, want %s", debugChatAction(got), debugChatAction(want))
	}
}

// Hard-won: 743ae5d4: underlined transcript links dropped their destinations and did nothing on click.
func TestClickingLinkTextOpensThatLinksDestination(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"See [the report](https://example.com/one) and [the notes](https://example.com/two).",
	))
	rows := drawnTranscript(chat, 80, 12)

	assertAction(t, clickTextAction(t, chat, rows, "notes"), ChatActionOpenLink{URL: "https://example.com/two"})
	assertAction(t, clickTextAction(t, chat, rows, "report"), ChatActionOpenLink{URL: "https://example.com/one"})
	assertAction(t, clickTextAction(t, chat, rows, "See"), ChatActionNone{})
}

func TestLinksWithTheSameTextOpenTheirOwnDestinations(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"First [issue](https://example.com/a)\n\nSecond [issue](https://example.com/b)",
	))
	rows := drawnTranscript(chat, 80, 12)
	second := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "Second") })
	if second < 0 {
		t.Fatal("second paragraph is drawn")
	}
	line := rows[second]
	offset := strings.Index(line, "issue")
	action := chat.HandleMouse(leftClick(displayWidth(line[:offset]), second))
	assertAction(t, action, ChatActionOpenLink{URL: "https://example.com/b"})
	assertAction(t, clickTextAction(t, chat, rows, "issue"), ChatActionOpenLink{URL: "https://example.com/a"})
}

func TestClickingANonWebLinkCopiesItInsteadOfOpeningIt(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"Run [this helper](file:///tmp/helper.sh) now.",
	))
	rows := drawnTranscript(chat, 80, 12)

	assertAction(t, clickTextAction(t, chat, rows, "helper"), ChatActionCopyLink{URL: "file:///tmp/helper.sh"})
}

// Hard-won: 743ae5d4: underlined transcript links did nothing on click, including wrapped spans.
func TestALinkWrappedAcrossRowsOpensFromItsContinuationRow(t *testing.T) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"Please read [the open upstream report about integer division](https://example.com/issue) today.",
	))
	rows := drawnTranscript(chat, 28, 16)
	divisionRow := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "division") })
	if divisionRow < 0 {
		t.Fatal("link text is drawn")
	}
	if strings.Contains(rows[divisionRow], "Please") {
		t.Fatalf("the link must wrap for this test: %#v", rows)
	}
	assertAction(t, clickTextAction(t, chat, rows, "division"), ChatActionOpenLink{URL: "https://example.com/issue"})
	assertAction(t, clickTextAction(t, chat, rows, "today"), ChatActionNone{})

	standalone := newTranscriptChat()
	standalone.entries = append(standalone.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"[alpha bravo charlie delta echo foxtrot golf](https://example.com/only)",
	))
	standaloneRows := drawnTranscript(standalone, 20, 16)
	expected := ChatActionOpenLink{URL: "https://example.com/only"}

	for _, word := range []string{"alpha", "echo", "golf"} {
		got := clickTextAction(t, standalone, standaloneRows, word)
		if !reflect.DeepEqual(got, ChatAction(expected)) {
			t.Fatalf("%s: action %s in %#v", word, debugChatAction(got), standaloneRows)
		}
	}
}

func agentRows(text string, width int) (*ChatState, []string) {
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, text))
	rows := drawnTranscript(chat, width, 24)
	return chat, rows
}

func opens(url string) ChatAction {
	return ChatActionOpenLink{URL: url}
}

func TestAPlainURLOpensWithoutItsTrailingPunctuation(t *testing.T) {
	chat, rows := agentRows(
		"The report is at https://example.com/some_path/report. Also (see https://example.com/x).",
		100,
	)

	assertAction(t, clickTextAction(t, chat, rows, "some_path"), opens("https://example.com/some_path/report"))
	assertAction(t, clickTextAction(t, chat, rows, "example.com/x"), opens("https://example.com/x"))
	assertAction(t, clickTextAction(t, chat, rows, "report is"), ChatActionNone{})
}

func TestAURLInsideALinkLabelOpensTheLinksDestination(t *testing.T) {
	chat, rows := agentRows("[https://example.com/label](https://example.com/dest)", 80)

	assertAction(t, clickTextAction(t, chat, rows, "label"), opens("https://example.com/dest"))
}
