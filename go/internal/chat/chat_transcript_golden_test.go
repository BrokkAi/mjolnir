// Tests of mj-chat/src/chat/transcript.rs (chat/transcript/tests.rs),
// continued: the golden surfaces and copying wrapped rows.

package chat

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"slices"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func goldenChatBuffer(chat *ChatState, width int, height int) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	renderFullFrame(NewFrame(buffer), chat, false)
	return buffer
}

func appendTranscriptGoldenState(output *strings.Builder, label string, width int, height int, rows []string, details []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(rows, "\n"))
	output.WriteByte('\n')
	for _, detail := range details {
		output.WriteString(detail)
		output.WriteByte('\n')
	}
}

// debugSurface is Rust's Debug of an Option<&SurfaceFrame>.
func debugSurface(surface *SurfaceFrame) string {
	if surface == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%#v)", *surface)
}

// pinLocalZoneToUTC makes time.Local UTC for the rest of one test. Event
// headers render in the local zone (chrono::Local in Rust), and goldens
// recorded with epoch-relative timestamps were recorded in UTC, where Rust CI
// runs. time.Local is process-wide, so a test that calls this must not call
// t.Parallel.
func pinLocalZoneToUTC(t *testing.T) {
	t.Helper()
	saved := time.Local
	time.Local = time.UTC
	t.Cleanup(func() { time.Local = saved })
}

// Hard-won: 0d4b1c9a1: submissions and rebuilds overrode deliberate reader navigation.
func TestGoldenTranscriptNavigation(t *testing.T) {
	// The transcript items below carry timestamps near the epoch, so their
	// "You · 00:00" headers read 00:00 only in UTC.
	pinLocalZoneToUTC(t)
	var output strings.Builder

	chat := numberedChat(40)
	rows := BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "tail before wheel", 40, 24, rows, nil)
	chat.HandleMouse(wheel(tea.MouseWheelUp))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "wheel up", 40, 24, rows, nil)
	chat.HandleMouse(wheel(tea.MouseWheelDown))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "wheel down follows", 40, 24, rows, nil)

	chat = numberedChat(40)
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "page navigation tail", 60, 24, rows, nil)
	chat.HandleKey(key(tea.KeyPgUp))
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "page up", 60, 24, rows, nil)
	chat.HandleKey(key(tea.KeyPgDown))
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "page down follows", 60, 24, rows, nil)
	chat.HandleKey(key(tea.KeyPgUp))
	chat.HandleKey(ctrlKey(tea.KeyEnd))
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "ctrl end follows", 60, 24, rows, nil)

	chat = numberedChat(200)
	_ = goldenChatBuffer(chat, 40, 24)
	chat.HandleKey(ctrlKey(tea.KeyHome))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "ctrl home", 40, 24, rows, nil)
	chat.HandleKey(ctrlKey(tea.KeyEnd))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "ctrl end", 40, 24, rows, nil)

	chat = numberedChat(200)
	chat.setInput("draft prompt")
	_ = goldenChatBuffer(chat, 40, 24)
	chat.HandleKey(keypadKey(tea.KeyKpHome))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "keypad home leaves draft cursor", 40, 24, rows,
		[]string{fmt.Sprintf("input cursor: %d", chat.inputCursor)})
	chat.HandleKey(keypadKey(tea.KeyKpEnd))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "keypad end leaves draft cursor", 40, 24, rows,
		[]string{fmt.Sprintf("input cursor: %d", chat.inputCursor)})
	chat.HandleKey(key(tea.KeyHome))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "plain home edits draft", 40, 24, rows,
		[]string{fmt.Sprintf("input cursor: %d", chat.inputCursor)})
	chat.HandleKey(key(tea.KeyEnd))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "plain end edits draft", 40, 24, rows,
		[]string{fmt.Sprintf("input cursor: %d", chat.inputCursor)})

	chat = newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		"response advertised on the dashboard",
	))
	for index := range uint64(8) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(
			index+2,
			core.ChatRoleSystem,
			fmt.Sprintf("terminal failure %d\n%s", index, strings.Repeat("output\n", 12)),
		))
	}
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "opening reveal", 60, 24, rows, nil)
	chat.HandleKey(ctrlKey(tea.KeyEnd))
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "revealed transcript tail", 60, 24, rows, nil)

	saved := chat.TranscriptPosition()
	reopened := newTranscriptChat()
	reopened.entries = slices.Clone(chat.entries)
	reopened.RestoreTranscriptPosition(saved)
	rows = BufferLines(goldenChatBuffer(reopened, 60, 24))
	appendTranscriptGoldenState(&output, "reopened transcript tail", 60, 24, rows, nil)

	chat = numberedChat(2)
	before := BufferLines(goldenChatBuffer(chat, 40, 24))
	chat.HandleMouse(wheel(tea.MouseWheelUp))
	chat.HandleKey(key(tea.KeyPgUp))
	rows = BufferLines(goldenChatBuffer(chat, 40, 24))
	appendTranscriptGoldenState(&output, "short transcript ignores scrolling", 40, 24, rows,
		[]string{fmt.Sprintf("viewport unchanged: %v", slices.Equal(before, rows))})

	chat = scrollbarChat()
	geometry := transcriptScrollbarGeometry(t, chat)
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	surface := chat.FrameSurfaces().SurfaceAt(geometry.Track.Min.X, geometry.Track.Min.Y)
	appendTranscriptGoldenState(&output, "scrollbar before track click", 60, 24, rows,
		[]string{"track content surface: " + debugSurface(surface)})
	scrollbarMouse(chat, tea.MouseClickMsg{}, geometry.Track.Min.X, geometry.Track.Min.Y+geometry.Track.Dy()/2)
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "scrollbar track seeks", 60, 24, rows,
		[]string{"anchor: " + debugAnchor(chat.anchor)})
	beforeRelease := chat.anchor
	scrollbarMouse(chat, tea.MouseReleaseMsg{}, geometry.Track.Min.X, geometry.Track.Min.Y)
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "scrollbar release preserves seek", 60, 24, rows,
		[]string{fmt.Sprintf("anchor unchanged: %v", chat.anchor == beforeRelease)})

	chat = newTranscriptChat()
	for _, size := range [][2]int{{60, 24}, {4, 24}, {3, 24}, {2, 24}, {1, 1}, {0, 0}} {
		width, height := size[0], size[1]
		_ = goldenChatBuffer(chat, width, height)
		scrollbarMouse(chat, tea.MouseClickMsg{}, max(width-1, 0), 0)
		rows = BufferLines(goldenChatBuffer(chat, width, height))
		appendTranscriptGoldenState(&output, "empty transcript drag", width, height, rows,
			[]string{fmt.Sprintf("scrollbar dragging: %v", chat.TranscriptScrollbarDragging())})
	}

	// Exercise the live materialized path, not just appending ChatEntry rows.
	scrollSession := core.MaterializedSessionEmpty("scroll-regression")
	for position := uint64(1); position <= 40; position++ {
		scrollSession.Transcript = append(scrollSession.Transcript, userTranscriptItem(position, fmt.Sprintf("message %d", position)))
	}
	scrollSession.AppliedEventOrdinal = 40
	for _, shell := range []bool{false, true} {
		session := cloneMaterializedSession(&scrollSession)
		chat := ChatStateFromMaterialized(session, nil, nil)
		goldenChatBuffer(chat, 60, 24)
		chat.HandleKey(key(tea.KeyPgUp))
		before := drawnTranscript(chat, 60, 24)
		if len(visibleMessages(before)) == 0 {
			t.Fatalf("%#v", before)
		}
		sender, _ := newChatRemoteOperationQueue(4)
		var operation chatRemoteOperation
		if shell {
			operation = chatRemoteOperationRunShell{
				CommandID: "scroll-send",
				Command:   "printf hello",
			}
		} else {
			operation = chatRemoteOperationPrompt{
				CommandID: "scroll-send",
				Text:      "a new question",
				Images:    []PromptImage{},
			}
		}
		queueChatRemoteOperation(sender, operation, chat)
		rows := BufferLines(goldenChatBuffer(chat, 60, 24))
		assertStrings(t, visibleMessages(rows), visibleMessages(before)...)
		label := "historical view while sending prompt"
		if shell {
			label = "historical view while sending shell"
		}
		appendTranscriptGoldenState(&output, label, 60, 24, rows, nil)
		var accepted *core.TranscriptItem
		if shell {
			accepted = fixtureItem(
				41,
				"shell:scroll-send",
				core.TranscriptBodySystem{Text: "printf hello: completed"},
			)
		} else {
			accepted = userTranscriptItem(41, "a new question")
			accepted.StableID = "user:scroll-send"
		}
		session.Transcript = append(session.Transcript, accepted, agentMessageItem("agent:42", 42, "new streamed reply"))
		session.AppliedEventOrdinal = 42
		chat.ApplyMaterialized(session, nil, nil)
		rows = BufferLines(goldenChatBuffer(chat, 60, 24))
		assertStrings(t, visibleMessages(rows), visibleMessages(before)...)
		label = "historical view after prompt and reply"
		if shell {
			label = "historical view after shell and reply"
		}
		appendTranscriptGoldenState(&output, label, 60, 24, rows, nil)
		streaming := makeMutItem(&session.Transcript[len(session.Transcript)-1])
		body, ok := streaming.Body.(core.TranscriptBodyAgent)
		if !ok {
			t.Fatal("expected an agent reply")
		}
		body.Streaming = true
		body.Chunks = append(slices.Clone(body.Chunks), json.RawMessage(`{"content":{"type":"text","text":" grows in place"}}`))
		streaming.Body = body
		streaming.LatestContentEventOrdinal = transcriptPtr(uint64(43))
		session.AppliedEventOrdinal = 43
		chat.ApplyMaterialized(session, nil, nil)
		rows = BufferLines(goldenChatBuffer(chat, 60, 24))
		assertStrings(t, visibleMessages(rows), visibleMessages(before)...)
		if !shell {
			appendTranscriptGoldenState(&output, "historical view while reply grows", 60, 24, rows, nil)
		}
		// A sliding canonical window moves the selected entry's index.
		session.Transcript = slices.Clone(session.Transcript[10:])
		session.AppliedEventOrdinal = 44
		chat.ApplyMaterialized(session, nil, nil)
		rows = BufferLines(goldenChatBuffer(chat, 60, 24))
		assertStrings(t, visibleMessages(rows), visibleMessages(before)...)
		if !shell {
			appendTranscriptGoldenState(&output, "historical identity after window slides", 60, 24, rows, nil)
		}
		chat.HandleKey(ctrlKey(tea.KeyEnd))
		session.Transcript = append(session.Transcript, agentMessageItem("agent:45", 45, "reply at the followed tail"))
		session.AppliedEventOrdinal = 45
		chat.ApplyMaterialized(session, nil, nil)
		rows = BufferLines(goldenChatBuffer(chat, 60, 24))
		if !shows(rows, "reply at the followed tail") {
			t.Fatalf("%#v", rows)
		}
		if !shell {
			appendTranscriptGoldenState(&output, "explicit end resumes following updates", 60, 24, rows, nil)
		}
	}

	revealed := core.MaterializedSessionEmpty("revealed-scroll")
	revealed.Transcript = append(revealed.Transcript, agentMessageItem("agent:1", 1, "opening reply"))
	for position := uint64(2); position <= 40; position++ {
		revealed.Transcript = append(revealed.Transcript, userTranscriptItem(position, fmt.Sprintf("message %d", position)))
	}
	revealed.AppliedEventOrdinal = 40
	automatic := ChatStateFromMaterialized(&revealed, nil, nil)
	rows = BufferLines(goldenChatBuffer(automatic, 60, 24))
	if !shows(rows, "opening reply") {
		t.Fatalf("%#v", rows)
	}
	updated := cloneMaterializedSession(&revealed)
	updated.Transcript = append(updated.Transcript, agentMessageItem("agent:41", 41, "automatic reveal follows this reply"))
	updated.AppliedEventOrdinal = 41
	automatic.ApplyMaterialized(updated, nil, nil)
	rows = BufferLines(goldenChatBuffer(automatic, 60, 24))
	if !shows(rows, "automatic reveal follows this reply") {
		t.Fatalf("%#v", rows)
	}
	appendTranscriptGoldenState(&output, "automatic reveal follows materialized update", 60, 24, rows, nil)
	for _, navigateAway := range []bool{false, true} {
		chat := ChatStateFromMaterialized(&revealed, nil, nil)
		// An update before the first draw must retain the automatic reveal.
		updated := cloneMaterializedSession(&revealed)
		updated.AppliedEventOrdinal = 41
		chat.ApplyMaterialized(updated, nil, nil)
		before := drawnTranscript(chat, 60, 24)
		if !shows(before, "opening reply") {
			t.Fatalf("%#v", before)
		}
		if navigateAway {
			chat.HandleKey(key(tea.KeyPgDown))
		}
		// Same coordinate, or away/back with no intervening draw.
		chat.HandleKey(ctrlKey(tea.KeyHome))
		saved := chat.TranscriptPosition()
		reopened := ChatStateFromMaterialized(updated, nil, nil)
		reopened.RestoreTranscriptPosition(saved)
		updated.Transcript = append(updated.Transcript, agentMessageItem("agent:42", 42, "new reply below the reveal"))
		updated.AppliedEventOrdinal = 42
		for _, view := range []struct {
			label string
			chat  *ChatState
		}{
			{"explicit reveal navigation", chat},
			{"restored explicit reveal navigation", reopened},
		} {
			view.chat.ApplyMaterialized(updated, nil, nil)
			rows := BufferLines(goldenChatBuffer(view.chat, 60, 24))
			assertStrings(t, visibleMessages(rows), visibleMessages(before)...)
			if !shows(rows, "opening reply") {
				t.Fatalf("%#v", rows)
			}
			how := "same row"
			if navigateAway {
				how = "away and back"
			}
			appendTranscriptGoldenState(&output, fmt.Sprintf("%s (%s)", view.label, how), 60, 24, rows, nil)
		}
	}

	core.AssertPlatformGolden(t, "testdata", "transcript-navigation", output.String())
}

func TestGoldenTranscriptLinkRouting(t *testing.T) {
	var output strings.Builder
	code := "Run `curl https://example.com/inline` or:\n\n```\nGET https://example.com/block\n```"
	chat, rows := agentRows(code, 80)
	for _, check := range [][2]string{
		{"inline code URL", "example.com/inline"},
		{"fenced code URL", "example.com/block"},
		{"ordinary code text", "GET"},
	} {
		action := clickTextAction(t, chat, rows, check[1])
		appendTranscriptGoldenState(&output, check[0], 80, 24, rows,
			[]string{"action: " + debugChatAction(action)})
	}

	table := "| Issue | Status |\n| --- | --- |\n| [first bug](https://example.com/1) | open |\n| see https://example.com/2 | closed |"
	chat, rows = agentRows(table, 80)
	for _, check := range [][2]string{
		{"wide table link", "first bug"},
		{"wide table URL", "example.com/2"},
		{"wide table plain cell", "closed"},
	} {
		action := clickTextAction(t, chat, rows, check[1])
		appendTranscriptGoldenState(&output, check[0], 80, 24, rows,
			[]string{"action: " + debugChatAction(action)})
	}

	narrowTable := "| Issue | Notes |\n| --- | --- |\n| [first bug](https://example.com/1) | a long description that cannot fit a narrow grid |"
	chat, rows = agentRows(narrowTable, 30)
	for _, check := range [][2]string{
		{"narrow table link", "first bug"},
		{"narrow table label", "Issue"},
	} {
		action := clickTextAction(t, chat, rows, check[1])
		appendTranscriptGoldenState(&output, check[0], 30, 24, rows,
			[]string{"action: " + debugChatAction(action)})
	}

	core.AssertPlatformGolden(t, "testdata", "transcript-link-routing", output.String())
}

func TestGoldenConversationTitle(t *testing.T) {
	var output strings.Builder
	chat := newTranscriptChat()
	chat.SetHeaderSummary("podman", "codex3", "Review the build")
	draw := func(label string, chat *ChatState) {
		buffer := uv.NewBuffer(80, 10)
		frame := NewFrame(buffer)
		renderTranscript(frame, frame.Area(), chat, false, 0, 0, false)
		header := bufferRowSymbols(buffer, 0)
		titleStart := strings.Index(header, "Review the build")
		if titleStart < 0 {
			t.Fatalf("the session title is drawn: %q", header)
		}
		details := []string{fmt.Sprintf(
			"foreground: host=%s, session title=%s",
			debugTerminalColor(buffer.CellAt(2, 0).Style.Fg),
			debugTerminalColor(buffer.CellAt(titleStart, 0).Style.Fg),
		)}
		appendTranscriptGoldenState(&output, label, 80, 10, BufferLines(buffer), details)
	}

	draw("idle", chat)
	view := client.RuntimeReviewView{
		ReviewID:  "",
		Roots:     []string{},
		SessionID: "session",
		Questions: []client.ReviewerQuestion{},
		Phase:     core.TurnReviewPhaseLaunchingReviewer{},
		Roles:     []core.RoleStatus{},
		Status:    "starting the reviewer",
	}
	viewTemplate := view
	chat.setTurnReviews([]client.RuntimeReviewView{view})
	draw("reviewing", chat)
	view.Phase = core.TurnReviewPhaseVerdict{Verdict: core.ReviewVerdictFindings{
		Synthesis: "[P2] src/lib.rs:1 -- weak test",
	}}
	chat.setTurnReviews([]client.RuntimeReviewView{view})
	draw("findings", chat)
	// Two reviews of different worktrees: the one still working stands for
	// the session, though the other has findings.
	review := func(reviewID string, root string, phase core.TurnReviewPhase) client.RuntimeReviewView {
		review := viewTemplate
		review.ReviewID = reviewID
		review.Roots = []string{root}
		review.Phase = phase
		review.Status = "reviewing"
		return review
	}
	chat.setTurnReviews([]client.RuntimeReviewView{
		review(
			"review-a",
			"/workspace/a",
			core.TurnReviewPhaseVerdict{Verdict: core.ReviewVerdictFindings{
				Synthesis: "[P2] a.rs:1 -- weak test",
			}},
		),
		review(
			"review-c",
			"/workspace/c",
			core.TurnReviewPhaseRunning{Roles: []core.RoleStatus{}},
		),
	})
	draw("two reviews, one working", chat)
	chat.setTurnReviews([]client.RuntimeReviewView{})
	draw("idle restored", chat)

	core.AssertGolden(t, "testdata", "conversation-title", output.String())
}

// toolStyleDetail is one "<text> style: header=.., body=.." golden detail.
func toolStyleDetail(entry *core.ChatEntry) string {
	visual := entryVisual(entry)
	return fmt.Sprintf(
		"%s style: header=%s, body=%s",
		entry.Text, debugOptionColor(visual.headerStyle.Fg), debugOptionColor(visual.bodyStyle.Fg),
	)
}

// relaySessionUpdate projects one ACP session update into session as the
// relay records it at ordinal.
func relaySessionUpdate(t *testing.T, session *core.MaterializedSession, ordinal uint64, recordedAtMs int64, update string) {
	t.Helper()
	var decoded schema.SessionUpdate
	if err := json.Unmarshal([]byte(update), &decoded); err != nil {
		t.Fatal(err)
	}
	event := core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        ordinal,
		PreviousDigest: session.AppliedEventDigest,
		RecordedAtMs:   recordedAtMs,
		Observation:    core.RelayObservationSessionUpdate{Update: decoded},
	}
	digest, err := core.RelayEventDigest(&event)
	if err != nil {
		t.Fatal(err)
	}
	event.Digest = digest
	projected, err := transcript.ProjectRelayEvent(session, &event)
	if err != nil {
		t.Fatal(err)
	}
	if err := transcript.ApplyCommittedProjectionEvent(session, &event, projected.Mutation); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: d00682c9: expanded send_message details showed only the receipt, omitting its arguments.
// Hard-won: 464f66ded: raw-output-only failures omitted their error message beside expanded arguments.
// Hard-won: c823ba502: a failed Codex command expanded to only its terminal id.
func TestGoldenRichTranscriptToolPresentation(t *testing.T) {
	var output strings.Builder

	chat := newTranscriptChat()
	completed := completedTool(1, "completed command")
	completed.ToolContent = []string{"completed detail"}
	pending := core.ChatEntryTool(2, "pending command", nil, core.ToolStatusPending)
	pending.ToolContent = []string{"pending detail"}
	running := core.ChatEntryTool(3, "running command", nil, core.ToolStatusRunning)
	running.ToolContent = []string{"running detail"}
	failed := core.ChatEntryTool(4, "failed command", nil, core.ToolStatusFailed)
	failed.ToolContent = []string{"failed detail"}
	approval := core.ChatEntryTool(5, "approval command", nil, core.ToolStatusPending)
	approval.ToolContent = []string{"approval detail"}
	approval.AwaitingApproval = true
	chat.entries = append(chat.entries, completed, pending, running, failed, approval)
	buffer := goldenChatBuffer(chat, 80, 24)
	var styleDetails []string
	for _, entry := range []*core.ChatEntry{&completed, &pending, &running, &failed, &approval} {
		styleDetails = append(styleDetails, toolStyleDetail(entry))
	}
	appendTranscriptGoldenState(&output, "tool status labels and emphasis", 80, 24, BufferLines(buffer), styleDetails)

	chat = newTranscriptChat()
	running = core.ChatEntryTool(1, "running command", nil, core.ToolStatusRunning)
	running.ToolContent = []string{"running command --with every argument"}
	chat.entries = append(chat.entries,
		running,
		completedTool(2, "completed command"),
		core.ChatEntryTool(3, "failed command", nil, core.ToolStatusFailed),
	)
	_ = goldenChatBuffer(chat, 80, 24)
	target := findClickTarget(t, chat, 2, "completed tool is clickable")
	runningTarget := findClickTarget(t, chat, 1, "running tool is clickable")
	var targets []uint64
	for _, target := range chat.transcriptClickTargets {
		if !slices.Contains(targets, target.StartSeq) {
			targets = append(targets, target.StartSeq)
		}
	}
	slices.Sort(targets)
	if !slices.Equal(targets, []uint64{1, 2, 3}) {
		t.Fatalf("targets %v", targets)
	}
	appendTranscriptGoldenState(&output, "every tool has a click target", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)),
		[]string{"tool click targets: " + debugSet(targets)})
	chat.HandleMouse(leftClick(target.Rect.Min.X, target.Rect.Min.Y))
	appendTranscriptGoldenState(&output, "completed command expanded", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)),
		[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})
	chat.HandleMouse(leftClick(runningTarget.Rect.Min.X, runningTarget.Rect.Min.Y))
	appendTranscriptGoldenState(&output, "running command expanded", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)),
		[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})

	chat = newTranscriptChat()
	chat.entries = append(chat.entries,
		thought(1, "thinking about tools"),
		completedTool(2, "first-command"),
		completedTool(3, "second-command"),
	)
	rows := BufferLines(goldenChatBuffer(chat, 60, 24))
	clickRenderedText(t, chat, rows, "thinking about tools")
	rows = BufferLines(goldenChatBuffer(chat, 60, 24))
	appendTranscriptGoldenState(&output, "thought text does not expand tools", 60, 24, rows,
		[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})

	chat = newTranscriptChat()
	chat.applySessionUpdate(1, json.RawMessage(
		`{"sessionUpdate":"tool_call","toolCallId":"shell-1","title":"Bash","kind":"execute","status":"pending","rawInput":{"command":"git add src && cargo test --workspace | cat"}}`,
	))
	for _, step := range []struct {
		label  string
		seq    uint64
		status string
	}{
		{"execute pending", 1, ""},
		{"execute running", 2, "in_progress"},
		{"execute completed", 3, "completed"},
	} {
		if step.status != "" {
			chat.applySessionUpdate(step.seq, json.RawMessage(
				`{"sessionUpdate":"tool_call_update","toolCallId":"shell-1","status":`+transcriptJSONString(step.status)+`}`,
			))
		}
		appendTranscriptGoldenState(&output, step.label, 100, 24,
			BufferLines(goldenChatBuffer(chat, 100, 24)), nil)
	}
	chat.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "raw keeps provider title", 100, 24,
		BufferLines(goldenChatBuffer(chat, 100, 24)), nil)

	chat = newTranscriptChat()
	chat.applySessionUpdate(1, json.RawMessage(
		`{"sessionUpdate":"tool_call","toolCallId":"message-1","title":"mcp.mj_agents.send_message","status":"pending"}`,
	))
	chat.applySessionUpdate(2, json.RawMessage(
		`{"sessionUpdate":"tool_call_update","toolCallId":"message-1","status":"in_progress","rawInput":{"session_id":"peer-session","message":`+
			transcriptJSONString("Answer on the 48 s reverse queries: yes, that is the expected shape of today's target-scoped reverse design on Vector, not a regression, and two levers exist, one cheap.\nTry a larger reader page cache.")+
			`}}`,
	))
	rows = BufferLines(goldenChatBuffer(chat, 80, 28))
	clickRenderedText(t, chat, rows, "mj_send_message")
	appendTranscriptGoldenState(&output, "message input arrives while running", 80, 28,
		BufferLines(goldenChatBuffer(chat, 80, 28)), nil)
	chat.applySessionUpdate(3, json.RawMessage(
		`{"sessionUpdate":"tool_call_update","toolCallId":"message-1","title":"mcp__mj-agents__send_message","status":"completed","content":[{"type":"content","content":{"type":"text","text":"{\"status\":\"queued\",\"via\":\"mailbox\"}"}}]}`,
	))
	appendTranscriptGoldenState(&output, "message input survives result update", 80, 28,
		BufferLines(goldenChatBuffer(chat, 80, 28)), nil)
	chat.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "raw message includes arguments and receipt", 80, 28,
		BufferLines(goldenChatBuffer(chat, 80, 28)), nil)
	// An expanded call shows its local start time; pin it so the golden reads
	// the same in every timezone.
	started := time.Date(2026, time.October, 9, 15, 0, 0, 0, time.Local).UnixMilli()
	for index := range chat.entries {
		chat.entries[index].RecordedAtMs = transcriptPtr(started)
	}
	session := transcript.MaterializedSessionFromEntries(
		"message-session",
		chat.entries,
		3,
		core.WorkerPhaseIdle,
		core.SessionConfiguration{},
		nil,
		nil,
	)
	tool, ok := session.Transcript[0].Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatal("materialized tool call")
	}
	decoded, err := tool.Call.Decode()
	if err != nil {
		t.Fatal(err)
	}
	if decoded.Title != "mcp__mj-agents__send_message" {
		t.Fatalf("title %q", decoded.Title)
	}
	restored := ChatStateFromMaterialized(&session, nil, nil)
	rows = BufferLines(goldenChatBuffer(restored, 80, 28))
	clickRenderedText(t, restored, rows, "mj_send_message")
	appendTranscriptGoldenState(&output, "restored message includes original arguments", 80, 28,
		BufferLines(goldenChatBuffer(restored, 80, 28)), nil)
	chat.renderMode = client.TranscriptRenderModeRich
	chat.applySessionUpdate(4, json.RawMessage(
		`{"sessionUpdate":"tool_call_update","toolCallId":"message-1","rawInput":{}}`,
	))
	appendTranscriptGoldenState(&output, "empty input replaces earlier message arguments", 80, 28,
		BufferLines(goldenChatBuffer(chat, 80, 28)), nil)

	command := "git fetch origin master && git worktree add -b experiment-bbtree-performance .mj/lanes/experiment-bbtree-performance origin/master"
	terminalID := "exec-c88fb185-27bf-4e54-affa-c458b09d0b3c"
	terminalSession := core.MaterializedSessionEmpty("terminal-metadata-failure")
	for index, update := range []string{
		`{"sessionUpdate":"tool_call","toolCallId":` + transcriptJSONString(terminalID) +
			`,"title":` + transcriptJSONString(command) + `,"kind":"execute","status":"in_progress","rawInput":{"command":` +
			transcriptJSONString(command) + `,"cwd":"/home/jonathan/Projects/bifrost"},"content":[{"type":"terminal","terminalId":` +
			transcriptJSONString(terminalID) + `}],"_meta":{"terminal_info":{"terminal_id":` + transcriptJSONString(terminalID) +
			`,"cwd":"/home/jonathan/Projects/bifrost"}}}`,
		`{"sessionUpdate":"tool_call_update","toolCallId":` + transcriptJSONString(terminalID) +
			`,"_meta":{"terminal_output":{"terminal_id":` + transcriptJSONString(terminalID) +
			`,"data":"From github.com:BrokkAi/bifrost\n"}}}`,
		`{"sessionUpdate":"tool_call_update","toolCallId":` + transcriptJSONString(terminalID) +
			`,"_meta":{"terminal_output_delta":{"terminal_id":` + transcriptJSONString(terminalID) +
			`,"data":"\u001b[31mfatal: a branch named 'experiment-bbtree-performance' already exists\u001b[0m\n"}}}`,
		`{"sessionUpdate":"tool_call_update","toolCallId":` + transcriptJSONString(terminalID) + `,"status":"failed"}`,
		`{"sessionUpdate":"tool_call_update","toolCallId":` + transcriptJSONString(terminalID) +
			`,"_meta":{"terminal_exit":{"terminal_id":` + transcriptJSONString(terminalID) + `,"exit_code":128,"signal":null}}}`,
	} {
		relaySessionUpdate(t, &terminalSession, uint64(index)+1, started, update)
		if index == 2 || index == 4 {
			chat := ChatStateFromMaterialized(&terminalSession, nil, nil)
			rows := BufferLines(goldenChatBuffer(chat, 80, 28))
			clickRenderedText(t, chat, rows, "git fetch")
			rows = BufferLines(goldenChatBuffer(chat, 80, 28))
			rendered := strings.Join(rows, "\n")
			if !strings.Contains(rendered, "already exists") {
				t.Fatal(rendered)
			}
			if strings.Contains(rendered, "terminal exec-") {
				t.Fatal(rendered)
			}
			if strings.Contains(rendered, "exit status unavailable") {
				t.Fatal(rendered)
			}
			label := "failed terminal metadata expanded with exit code"
			if index == 2 {
				label = "streamed terminal output expanded while running"
			}
			appendTranscriptGoldenState(&output, label, 80, 28, rows, nil)
		}
	}
	// Reopening from the canonical checkpoint preserves the output and status.
	canonical, err := transcript.CanonicalSessionFromMaterialized(&terminalSession)
	if err != nil {
		t.Fatal(err)
	}
	reopenedTerminal, err := transcript.MaterializedSessionFromCanonical("terminal-metadata-failure", &canonical)
	if err != nil {
		t.Fatal(err)
	}
	chat = ChatStateFromMaterialized(&reopenedTerminal, nil, nil)
	chat.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "failed terminal metadata reopened in raw mode", 80, 28,
		BufferLines(goldenChatBuffer(chat, 80, 28)), nil)

	chat = newTranscriptChat()
	failureSession := core.MaterializedSessionEmpty("raw-output-failure")
	diagnostic := "permission denied while contacting peer"
	for index, step := range []struct {
		label  string
		update string
	}{
		{
			"raw-output failure pending expanded",
			`{"sessionUpdate":"tool_call","toolCallId":"peer-request","title":"contact_peer","status":"pending","rawInput":{"peer":"peer-session","message":"Check parser"}}`,
		},
		{
			"raw-output failure running expanded",
			`{"sessionUpdate":"tool_call_update","toolCallId":"peer-request","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"peer handshake started"}}]}`,
		},
		{
			"raw-output failure failed expanded",
			`{"sessionUpdate":"tool_call_update","toolCallId":"peer-request","status":"failed","rawOutput":{"result":null,"error":{"message":` + transcriptJSONString(diagnostic) + `}}}`,
		},
	} {
		seq := uint64(index) + 1
		// Keep the original ACP fields in the durable projection for replay.
		relaySessionUpdate(t, &failureSession, seq, started, step.update)
		chat.applySessionUpdateAt(seq, transcriptPtr(started), json.RawMessage(step.update))
		if index == 0 {
			rows := BufferLines(goldenChatBuffer(chat, 80, 24))
			clickRenderedText(t, chat, rows, "contact_peer")
		}
		rows := BufferLines(goldenChatBuffer(chat, 80, 24))
		if index == 2 {
			rendered := strings.Join(rows, "\n")
			if !strings.Contains(rendered, "peer-session") || !strings.Contains(rendered, "Check parser") {
				t.Fatal(rendered)
			}
			if strings.Count(rendered, "peer handshake started") != 1 {
				t.Fatal(rendered)
			}
			if strings.Count(rendered, diagnostic) != 1 {
				t.Fatal(rendered)
			}
		}
		appendTranscriptGoldenState(&output, step.label, 80, 24, rows, nil)
	}
	restored = ChatStateFromMaterialized(&failureSession, nil, nil)
	rows = BufferLines(goldenChatBuffer(restored, 80, 24))
	clickRenderedText(t, restored, rows, "contact_peer")
	rows = BufferLines(goldenChatBuffer(restored, 80, 24))
	rendered := strings.Join(rows, "\n")
	if !strings.Contains(rendered, "peer-session") || !strings.Contains(rendered, "Check parser") {
		t.Fatal(rendered)
	}
	if strings.Count(rendered, "peer handshake started") != 1 {
		t.Fatal(rendered)
	}
	if strings.Count(rendered, diagnostic) != 1 {
		t.Fatal(rendered)
	}
	appendTranscriptGoldenState(&output, "raw-output failure replayed and reopened", 80, 24, rows, nil)

	snapshotSession := transcript.MaterializedSessionFromEntries(
		"raw-output-failure",
		restored.entries,
		3,
		core.WorkerPhaseIdle,
		core.SessionConfiguration{},
		nil,
		nil,
	)
	snapshotRestored := ChatStateFromMaterialized(&snapshotSession, nil, nil)
	snapshotRows := BufferLines(goldenChatBuffer(snapshotRestored, 80, 24))
	clickRenderedText(t, snapshotRestored, snapshotRows, "contact_peer")
	if got := BufferLines(goldenChatBuffer(snapshotRestored, 80, 24)); !slices.Equal(got, rows) {
		t.Fatalf("the snapshot helper preserves the expanded error with its arguments and progress: %#v", got)
	}

	// Later partial updates own only the fields they carry. A new raw output
	// replaces the error rather than accumulating it with earlier failures.
	htmlError := "<error>peer denied</error>"
	for _, step := range []struct {
		seq           uint64
		fields        string
		expectedError string
	}{
		{4, `{"status":"failed"`, diagnostic},
		{5, `{"rawOutput":{"error":"peer retry rejected"}`, "peer retry rejected"},
		{6, `{"rawOutput":{"result":null,"error":null}`, ""},
		{
			7,
			`{"rawOutput":{"error":` + transcriptJSONString(htmlError) + `},"content":[{"type":"content","content":{"type":"text","text":"peer handshake started"}},{"type":"content","content":{"type":"text","text":` + transcriptJSONString(htmlError) + `}}]`,
			htmlError,
		},
	} {
		update := step.fields + `,"sessionUpdate":"tool_call_update","toolCallId":"peer-request"}`
		chat.applySessionUpdateAt(step.seq, transcriptPtr(started), json.RawMessage(update))
		if (chat.entries[0].ToolError != nil) != (step.expectedError != "") {
			t.Fatalf("seq %d: tool error %v", step.seq, chat.entries[0].ToolError)
		}
		rendered := strings.Join(BufferLines(goldenChatBuffer(chat, 80, 24)), "\n")
		if !strings.Contains(rendered, "peer-session") || !strings.Contains(rendered, "Check parser") {
			t.Fatal(rendered)
		}
		if strings.Count(rendered, "peer handshake started") != 1 {
			t.Fatal(rendered)
		}
		for _, toolError := range []string{diagnostic, "peer retry rejected", htmlError} {
			want := 0
			if step.expectedError == toolError {
				want = 1
			}
			if strings.Count(rendered, toolError) != want {
				t.Fatalf("seq %d: %q appears %d times: %s", step.seq, toolError, strings.Count(rendered, toolError), rendered)
			}
		}
	}

	fallback := fallbackTerminalSession(t, terminalRecord(transcriptPtr(uint32(0)), nil))
	chat = ChatStateFromMaterialized(fallback, nil, nil)
	appendTranscriptGoldenState(&output, "clean fallback terminal in rich mode", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)), nil)
	chat.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "clean fallback terminal in raw mode", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)), nil)

	terminal := core.ActiveAgentTerminal{
		TerminalID:  "term-1",
		Command:     "cargo mutants --in-diff diff",
		StartedAtMs: math.MaxInt64,
	}
	chat = newTranscriptChat()
	liveSession := core.MaterializedSessionEmpty("session-live-terminal")
	chat.setActiveAgentTerminals([]core.ActiveAgentTerminal{terminal}, &liveSession)
	appendTranscriptGoldenState(&output, "unclaimed live terminal", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)), nil)
	chat.setActiveAgentTerminals(nil, &liveSession)
	appendTranscriptGoldenState(&output, "live terminal removed after exit", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)), nil)

	chat = newTranscriptChat()
	for _, group := range []struct {
		start uint64
		name  string
	}{{1, "older"}, {5, "newer"}} {
		tool := completedTool(group.start+1, group.name+" provider title")
		tool.ToolSummary = transcriptPtr(group.name + "-command")
		tool.ToolContent = []string{group.name + " tool details"}
		chat.entries = append(chat.entries,
			thought(group.start, group.name+" thought with enough words to wrap across several rows"),
			tool,
			completedTool(group.start+2, group.name+"-companion"),
			core.ChatEntryPlain(group.start+3, core.ChatRoleAgent, group.name+" response"),
		)
	}
	for _, name := range []string{"older", "newer"} {
		rows := BufferLines(goldenChatBuffer(chat, 36, 48))
		appendTranscriptGoldenState(&output, name+" tool groups collapsed", 36, 48, rows, nil)
		clickRenderedText(t, chat, rows, name+"-command")
		rows = BufferLines(goldenChatBuffer(chat, 36, 48))
		appendTranscriptGoldenState(&output, name+" group expanded", 36, 48, rows,
			[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})
		clickRenderedText(t, chat, rows, name+" provider title")
		rows = BufferLines(goldenChatBuffer(chat, 36, 48))
		appendTranscriptGoldenState(&output, name+" group collapsed", 36, 48, rows,
			[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})
	}

	chat = newTranscriptChat()
	chat.entries = append(chat.entries,
		completedTool(1, "first command with a long argument"),
		completedTool(2, "second command with a long argument"),
		completedTool(3, "third command with a long argument"),
	)
	_ = goldenChatBuffer(chat, 30, 24)
	var secondTargets []TranscriptClickTarget
	for _, target := range chat.transcriptClickTargets {
		if target.StartSeq == 2 {
			secondTargets = append(secondTargets, target)
		}
	}
	secondTarget := secondTargets[1]
	chat.HandleMouse(leftClick(secondTarget.Rect.Min.X, secondTarget.Rect.Min.Y))
	appendTranscriptGoldenState(&output, "wrapped summary member expands from continuation row", 30, 24,
		BufferLines(goldenChatBuffer(chat, 30, 24)),
		[]string{
			fmt.Sprintf("second member hitboxes: %d", len(secondTargets)),
			"expanded tools: " + debugSet(expandedToolSet(chat)),
		})

	chat = newTranscriptChat()
	chat.applySessionUpdate(1, json.RawMessage(
		`{"sessionUpdate":"tool_call","toolCallId":"read-config","title":"read config","status":"pending"}`,
	))
	chat.applySessionUpdate(2, json.RawMessage(
		`{"sessionUpdate":"tool_call_update","toolCallId":"read-config","status":"completed"}`,
	))
	appendTranscriptGoldenState(&output, "tool call status update", 80, 24,
		BufferLines(goldenChatBuffer(chat, 80, 24)), nil)

	chat = newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, "**bold**"))
	chat.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "raw markdown markers", 30, 24,
		BufferLines(goldenChatBuffer(chat, 30, 24)), nil)

	// Settings › Advanced › Tool calls: Inline. Every call keeps its own row
	// with its full command, thoughts are not folded, and output shows its
	// head and tail around a count of the lines left out.
	kindTool := func(seq uint64, title string, kind acp.ToolKind, status core.ToolStatus) core.ChatEntry {
		call := acp.ToolCall{
			ToolCallID: acp.ToolCallId(fmt.Sprintf("call-%d", seq)),
			Title:      title,
			Kind:       &kind,
		}
		presentation := transcript.ToolCallPresentation(&call)
		entry := core.ChatEntryTool(seq, title, nil, status)
		entry.ToolSummary = transcriptPtr(presentation.Summary)
		entry.ToolPresentation = &presentation
		return entry
	}
	tests := kindTool(2, "cargo test -p brokk-mj-chat --lib", acp.ToolKindExecute, core.ToolStatusCompleted)
	testLines := make([]string, 0, 10)
	for test := 1; test <= 10; test++ {
		testLines = append(testLines, fmt.Sprintf("test case_%d ... ok", test))
	}
	tests.ToolContent = []string{fmt.Sprintf(
		"running 12 tests\n%s\ntest result: ok. 12 passed\nexited 0",
		strings.Join(testLines, "\n"),
	)}
	read := kindTool(4, "Read src/lib.rs", acp.ToolKindRead, core.ToolStatusCompleted)
	read.ToolContent = []string{"fn main() {}"}
	edit := kindTool(5, "Edit src/lib.rs", acp.ToolKindEdit, core.ToolStatusCompleted)
	edit.ToolDiffstats = []string{"src/lib.rs +3 -1"}
	failedCat := kindTool(6, "cat missing.txt", acp.ToolKindExecute, core.ToolStatusFailed)
	failedCat.ToolContent = []string{fmt.Sprintf(
		"cat: missing.txt: No such file or directory %s\nexited 1",
		strings.Repeat("x", 80),
	)}
	sleeping := kindTool(7, "sleep 30 && echo done", acp.ToolKindExecute, core.ToolStatusRunning)
	sleeping.ToolContent = []string{"waiting"}
	chat = newTranscriptChat()
	chat.SetToolDisplay(ToolDisplay{Layout: core.ToolOutputInline, OutputLines: 5})
	chat.entries = append(chat.entries,
		thought(1, "first thought"),
		tests,
		thought(3, "second thought"),
		read,
		edit,
		failedCat,
		sleeping,
	)
	for _, approval := range []struct {
		id    uint64
		title string
		kind  acp.ToolKind
	}{
		{8, "cargo fmt", acp.ToolKindExecute},
		{9, "Read private.txt", acp.ToolKindRead},
	} {
		entry := kindTool(approval.id, approval.title, approval.kind, core.ToolStatusPending)
		entry.AwaitingApproval = true
		chat.entries = append(chat.entries, entry)
	}
	rows = BufferLines(goldenChatBuffer(chat, 80, 36))
	appendTranscriptGoldenState(&output, "inline tool calls", 80, 36, rows, nil)
	clickRenderedText(t, chat, rows, "cargo test -p brokk-mj-chat")
	appendTranscriptGoldenState(&output, "inline call expanded", 80, 36,
		BufferLines(goldenChatBuffer(chat, 80, 36)),
		[]string{"expanded tools: " + debugSet(expandedToolSet(chat))})
	clear(chat.expandedToolCalls)
	chat.SetToolDisplay(ToolDisplay{Layout: core.ToolOutputInline, OutputLines: 1})
	WithSymbols(core.SymbolSetAscii, func() {
		rows = BufferLines(goldenChatBuffer(chat, 80, 36))
	})
	appendTranscriptGoldenState(&output, "inline with one output line and ASCII symbols", 80, 36, rows, nil)

	deliveryBytes, err := os.ReadFile("../transcript/testdata/tests/golden/mailbox-delivery-transcript.txt")
	if err != nil {
		t.Fatal(err)
	}
	_, afterBatch, ok := strings.Cut(string(deliveryBytes), "20: Mailbox events:\n")
	if !ok {
		t.Fatal("the delivery golden has its mailbox batch")
	}
	batch, _, ok := strings.Cut(afterBatch, "\n21: ")
	if !ok {
		t.Fatal("the delivery golden has its mailbox batch")
	}
	mailbox := newTranscriptChat()
	mailbox.entries = append(mailbox.entries, core.ChatEntryPlain(2, core.ChatRoleSystem, fmt.Sprintf(
		"Mailbox events:\n%s\n  - Child task Check parser finished: failed\n    支持 Unicode and <script>literal text</script>.\n    - A report bullet stays in this event.",
		batch,
	)))
	rows = BufferLines(goldenChatBuffer(mailbox, 60, 36))
	appendTranscriptGoldenState(&output, "mailbox events collapsed", 60, 36, rows, nil)
	// Hard-won: 2af612540: expanded events repeated their header in the body.
	clickRenderedText(t, mailbox, rows, "New PR #4623")
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 24))
	appendTranscriptGoldenState(&output, "an expanded event without a report wraps its header once", 44, 24, rows, nil)
	clickRenderedText(t, mailbox, rows, "CQ07")
	_ = goldenChatBuffer(mailbox, 60, 36)
	findMailboxTarget := func(id MailboxDisclosureId) TranscriptClickTarget {
		t.Helper()
		index := slices.IndexFunc(mailbox.transcriptClickTargets, func(target TranscriptClickTarget) bool {
			return target.MailboxEvent == id
		})
		if index < 0 {
			t.Fatalf("no click target for %#v", id)
		}
		return mailbox.transcriptClickTargets[index]
	}
	secondChild := findMailboxTarget(MailboxDisclosureIdEvent{StartSeq: 2, Index: 12})
	mailbox.HandleMouse(leftClick(secondChild.Rect.Min.X+10, secondChild.Rect.Min.Y))
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 36))
	appendTranscriptGoldenState(&output, "only the second repeated event expands after resize", 44, 36, rows, nil)
	assertMailboxOpen := func(want ...MailboxDisclosureId) {
		t.Helper()
		if len(mailbox.expandedMailboxEvents) != len(want) {
			t.Fatalf("open events %#v, want %#v", mailbox.expandedMailboxEvents, want)
		}
		for _, id := range want {
			if _, ok := mailbox.expandedMailboxEvents[id]; !ok {
				t.Fatalf("open events %#v, want %#v", mailbox.expandedMailboxEvents, want)
			}
		}
	}
	assertMailboxOpen(MailboxDisclosureIdEvent{StartSeq: 2, Index: 12})
	cached := mailbox.renderCache.entries[0]
	reportStart := slices.IndexFunc(cached.lines, func(line Line) bool {
		return strings.Contains(rowText(line), "支持 Unicode")
	})
	if reportStart < 0 {
		t.Fatal("the report is drawn")
	}
	reportEnd := len(cached.lines) - 2
	pane := transcriptPane(t, mailbox)
	copiedReport, ok := mailbox.TranscriptSelectionText(&SelectionRange{
		Start: NewContentPos(pane.TopRow+reportStart, 0),
		End:   NewContentPos(pane.TopRow+reportEnd, pane.Rect.Dx()-1),
	})
	if !ok {
		t.Fatal("the report has text")
	}
	if copiedReport != "支持 Unicode and <script>literal text</script>.\n- A report bullet stays in this event." {
		t.Fatalf("copied %q", copiedReport)
	}
	output.WriteString("copied report: " + debugString(copiedReport) + "\n\n")
	mailbox.entries[0].Text += "\n  - Your PR #4625 was merged."
	mailbox.entries[0].Revision++
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 36))
	appendTranscriptGoldenState(&output, "mailbox update retains the open event", 44, 36, rows, nil)
	firstChild := findMailboxTarget(MailboxDisclosureIdEvent{StartSeq: 2, Index: 10})
	mailbox.HandleMouse(leftClick(firstChild.Rect.Min.X, firstChild.Rect.Min.Y))
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 48))
	appendTranscriptGoldenState(&output, "both reports expand independently", 44, 48, rows, nil)
	mailbox.anchor = TranscriptAnchorRow{Entry: 0, Row: 9}
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 24))
	appendTranscriptGoldenState(&output, "mailbox report scrolled before collapse", 44, 24, rows, nil)
	// Hard-won: d453f1142: delivered-message bodies had no collapse target.
	clickRenderedText(t, mailbox, rows, "text</script>.")
	assertMailboxOpen(MailboxDisclosureIdEvent{StartSeq: 2, Index: 10})
	if mailbox.anchor != TranscriptAnchor(TranscriptAnchorRow{Entry: 0, Row: 9}) {
		t.Fatalf("anchor %#v", mailbox.anchor)
	}
	WithSymbols(core.SymbolSetAscii, func() {
		rows = BufferLines(goldenChatBuffer(mailbox, 44, 48))
	})
	appendTranscriptGoldenState(&output, "one report collapsed from wrapped body with ASCII arrows", 44, 48, rows, nil)

	firstReport := mailbox.renderCache.entries[0].mailbox.MailboxEvents[10].Start
	mailbox.anchor = TranscriptAnchorRow{Entry: 0, Row: firstReport + 3}
	rows = BufferLines(goldenChatBuffer(mailbox, 44, 14))
	if mailbox.anchor != TranscriptAnchor(TranscriptAnchorRow{Entry: 0, Row: firstReport + 3}) {
		t.Fatalf("anchor %#v", mailbox.anchor)
	}
	appendTranscriptGoldenState(&output, "mailbox header scrolled out of view", 44, 14, rows, nil)
	clickRenderedText(t, mailbox, rows, "verification timed out")
	if len(mailbox.expandedMailboxEvents) != 0 {
		t.Fatalf("open events %#v", mailbox.expandedMailboxEvents)
	}
	if mailbox.anchor != TranscriptAnchor(TranscriptAnchorRow{Entry: 0, Row: firstReport}) {
		t.Fatalf("anchor %#v", mailbox.anchor)
	}
	appendTranscriptGoldenState(&output, "body click collapses the report and restores its header", 44, 14,
		BufferLines(goldenChatBuffer(mailbox, 44, 14)), nil)
	mailbox.renderMode = client.TranscriptRenderModeRaw
	rows = BufferLines(goldenChatBuffer(mailbox, 60, 48))
	appendTranscriptGoldenState(&output, "raw mailbox text remains complete", 60, 48, rows, nil)

	deliveryTime := time.Date(2026, time.October, 9, 15, 0, 0, 0, time.Local).UnixMilli()
	incoming := newTranscriptChat()
	incoming.entries = append(incoming.entries,
		core.ChatEntryPlain(1, core.ChatRoleAgent, "I am checking the report.").WithRecordedAt(&deliveryTime),
	)
	for _, message := range []struct {
		key, title, text string
	}{
		{
			"review-one",
			"Review agent",
			"This report has one item.\nThe actual message continues here, with **markdown** and enough words to wrap on a narrow terminal.",
		},
		{
			"review-two",
			"Build agent",
			"The checks passed.\nKeep this second message visible too.",
		},
	} {
		event := core.MailboxEvent{
			Key:         message.key,
			Source:      "session_message",
			Wake:        false,
			CreatedAtMs: 50,
			Body: core.MailboxEventBodySessionMessage{
				From: core.Sender{
					Type:  core.SenderSession,
					ID:    message.key + "-session",
					Title: message.title,
				},
				Text: message.text,
			},
		}
		incoming.entries = append(incoming.entries, core.ChatEntryReceivedMessage(2, &event).WithRecordedAt(&deliveryTime))
	}
	incoming.entries = append(incoming.entries,
		core.ChatEntryPlain(3, core.ChatRoleAgent, "I will address that item.").WithRecordedAt(&deliveryTime),
	)
	incoming.latestSeq = 3
	assertIncomingOpen := func(entry int) {
		t.Helper()
		id := MailboxDisclosureIdMessage{MessageID: *incoming.entries[entry].MessageID}
		if _, ok := incoming.expandedMailboxEvents[id]; !ok || len(incoming.expandedMailboxEvents) != 1 {
			t.Fatalf("open messages %#v, want %#v", incoming.expandedMailboxEvents, id)
		}
	}
	rows = BufferLines(goldenChatBuffer(incoming, 44, 36))
	appendTranscriptGoldenState(&output, "incoming messages start collapsed between agent replies", 44, 36, rows, nil)
	clickRenderedText(t, incoming, rows, "Message from Review agent")
	assertIncomingOpen(1)
	rows = BufferLines(goldenChatBuffer(incoming, 44, 36))
	appendTranscriptGoldenState(&output, "header text opens only one message in a shared delivery", 44, 36, rows, nil)
	clickRenderedText(t, incoming, rows, "Message from Build agent")
	incoming.entries[1].Revision++
	rows = BufferLines(goldenChatBuffer(incoming, 32, 36))
	appendTranscriptGoldenState(&output, "both incoming messages stay open after update and resize", 32, 36, rows, nil)
	clickRenderedText(t, incoming, rows, "terminal.")
	assertIncomingOpen(2)
	appendTranscriptGoldenState(&output, "wrapped message body collapses only that message", 32, 36,
		BufferLines(goldenChatBuffer(incoming, 32, 36)), nil)
	incoming.renderMode = client.TranscriptRenderModeRaw
	appendTranscriptGoldenState(&output, "raw mode keeps complete incoming messages", 44, 36,
		BufferLines(goldenChatBuffer(incoming, 44, 36)), nil)
	if len(incoming.transcriptClickTargets) != 0 {
		t.Fatalf("targets %#v", incoming.transcriptClickTargets)
	}
	incoming.renderMode = client.TranscriptRenderModeRich
	incoming.anchor = TranscriptAnchorRow{Entry: 2, Row: 0}
	position := incoming.TranscriptPosition()
	encodedPosition, err := transcriptMarshalJSON(position)
	if err != nil {
		t.Fatal(err)
	}
	encoded := string(encodedPosition)
	materialized := transcript.MaterializedSessionFromEntries(
		"incoming-session",
		incoming.entries,
		3,
		core.WorkerPhaseIdle,
		core.SessionConfiguration{},
		nil,
		nil,
	)
	incomingCanonical, err := transcript.CanonicalSessionFromMaterialized(&materialized)
	if err != nil {
		t.Fatal(err)
	}
	restoredSession, err := transcript.MaterializedSessionFromCanonical("incoming-session", &incomingCanonical)
	if err != nil {
		t.Fatal(err)
	}
	reopened := ChatStateFromMaterialized(&restoredSession, nil, nil)
	var decodedPosition TranscriptPosition
	if err := json.Unmarshal([]byte(encoded), &decodedPosition); err != nil {
		t.Fatal(err)
	}
	reopened.RestoreTranscriptPosition(decodedPosition)
	if reopened.anchor != TranscriptAnchor(TranscriptAnchorRow{Entry: 2, Row: 0}) {
		t.Fatalf("anchor %#v", reopened.anchor)
	}
	appendTranscriptGoldenState(&output, "restored anchor distinguishes messages in one delivery", 44, 24,
		BufferLines(goldenChatBuffer(reopened, 44, 24)), []string{encoded})
	WithSymbols(core.SymbolSetAscii, func() {
		reopened.anchor = TranscriptAnchorBottom{}
		rows = BufferLines(goldenChatBuffer(reopened, 44, 36))
	})
	appendTranscriptGoldenState(&output, "incoming message ASCII marks", 44, 36, rows, nil)

	core.AssertPlatformGolden(t, "testdata", "rich-transcript-tool-presentation", output.String())
}

// Wrapping is only how a row fits the pane, so copying a wrapped message
// gives back its source lines: no gutter, no wrap indent, and no newline
// where a URL or a sentence was split across rows.
func TestCopyingWrappedRowsRejoinsTheLinesWrappingSplit(t *testing.T) {
	url := "https://github.com/organizations/BrokkAi/settings/apps/mergecopbot/permissions"
	chat := newTranscriptChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(
		1,
		core.ChatRoleAgent,
		fmt.Sprintf("1. Open %s. You can also get there through BrokkAi settings.\n\nThen save.", url),
	))
	drawnTranscript(chat, 40, 24)
	width := chat.renderCache.width
	rows := renderTranscriptEntry(&chat.entries[0], width, client.TranscriptRenderModeRich)
	if len(rows) <= 5 {
		t.Fatal("the fixture must wrap")
	}
	// Skip the header row, and stop before the entry's trailing blank row.
	body := transcriptPane(t, chat).TopRow + 1
	last := body + len(rows) - 3

	copied, ok := chat.TranscriptSelectionText(&SelectionRange{
		Start: NewContentPos(body, 0),
		End:   NewContentPos(last, width-1),
	})
	want := fmt.Sprintf("1. Open %s. You can also get there through BrokkAi settings.\n\nThen save.", url)
	if !ok || copied != want {
		t.Fatalf("copied %q, %v; want %q", copied, ok, want)
	}
}
