// Port of mj-chat/src/chat/active/tests.rs.

package chat

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"os"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// activeTestSnapshotChat is ChatState::new(&snapshot(), &[]).
func activeTestSnapshotChat() *ChatState {
	worker := snapshot()
	return NewChatState(&worker, nil)
}

// activeOpenChat is ActiveChat::open with the bundle every test uses; the
// chat is closed when the test ends, as Rust drops it.
func activeOpenChat(t *testing.T, session client.SessionHandle, sessionContext *ChatSessionContext, control client.SessionControl, header SessionHeaderIdentity, draft string) *ActiveChat {
	t.Helper()
	chat := ActiveChatOpen(session, "bundle-1", sessionContext, control, header, draft, NewNotices())
	t.Cleanup(chat.Close)
	return chat
}

// activeUnmarshal decodes a JSON literal as serde_json::from_value does,
// failing the test on an error.
func activeUnmarshal(t *testing.T, text string, target any) {
	t.Helper()
	if err := json.Unmarshal([]byte(text), target); err != nil {
		t.Fatalf("decode %s: %v", text, err)
	}
}

// activeCloneView is ManagedSessionView::clone: the snapshot is copied, so
// editing one view's operational state leaves the other alone.
func activeCloneView(view client.ManagedSessionView) client.ManagedSessionView {
	if view.Snapshot != nil {
		snapshot := *view.Snapshot
		view.Snapshot = &snapshot
	}
	return view
}

// activeEditOperational edits the view's operational state in place, as
// Rust's `view.snapshot.as_mut().unwrap().operational.<field> = ...` does.
func activeEditOperational(view *client.ManagedSessionView, edit func(*core.RelayOperationalState)) {
	snapshot := *view.Snapshot
	operational := snapshot.Operational.IntoInner()
	edit(&operational)
	snapshot.Operational = core.NewSharedSnapshot(operational)
	view.Snapshot = &snapshot
}

// activeBufferText is every cell's symbol in row order, as Rust collects
// `buffer.content().iter().map(|cell| cell.symbol())`.
func activeBufferText(buffer *uv.Buffer) string {
	var text strings.Builder
	bounds := buffer.Bounds()
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			text.WriteString(activeCellSymbol(buffer, x, y))
		}
	}
	return text.String()
}

// activeCellSymbol is a ratatui cell's symbol: ratatui resets the cells a
// wide grapheme covers to a space, where ultraviolet leaves them empty.
func activeCellSymbol(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

// activeDraw draws into a fresh width x height buffer and returns it.
func activeDraw(width, height int, draw func(frame *Frame)) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	draw(NewFrame(buffer))
	return buffer
}

func activeElicitationRequest(id, message string) core.ElicitationRequest {
	return core.ElicitationRequest{ID: id, Message: message, Fields: []core.ElicitationField{}}
}

// Hard-won: 61a1cfa: Codex command updates were lost during replay, hiding worker-owned goal controls
func TestLiveGoalCommandsFollowWorkerStateDespiteADamagedProjection(t *testing.T) {
	view := managedView(core.MaterializedSessionEmpty("goal-commands"))
	activeEditOperational(&view, func(operational *core.RelayOperationalState) {
		activeUnmarshal(t, `{
			"known": true,
			"capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
			"snapshot": {"objective": "finish", "status": "paused"},
			"execution": {"version": 1, "status": "idle"}
		}`, &operational.Goal)
	})
	// Older workers can also have lost the base command advertisement.
	if len(view.Snapshot.Operational.Get().AvailableCommands) != 0 {
		t.Fatal("the view advertises no commands")
	}
	fixture := client.NewReplacementSessionTestFixture("goal-commands", 12)
	fixture.ReplacementView.SendReplace(activeCloneView(view))
	session, err := fixture.Control.Session(context.Background(), "goal-commands")
	if err != nil {
		t.Fatal(err)
	}
	active := activeOpenChat(t, session, nil, fixture.Control, SessionHeaderIdentity{}, "")
	chat := active.ChatState
	resumes := func(action ChatAction) bool {
		goal, ok := action.(ChatActionGoalControl)
		return ok && goal.Action == core.GoalControlActionResume
	}
	chat.setInput("/goal resume")
	if action := chat.submitInput(); !resumes(action) {
		t.Fatalf("/goal resume submitted %#v", action)
	}
	for range 2 {
		if !applySessionView(chat, activeCloneView(view), nil) {
			t.Fatal("the view keeps the feed open")
		}
		chat.setInput("/goal finish the work")
		if prompt, ok := chat.submitInput().(ChatActionPrompt); !ok || prompt.Text != "/goal finish the work" {
			t.Fatalf("/goal finish the work submitted %#v", prompt)
		}
		chat.setInput("/goal resume")
		if action := chat.submitInput(); !resumes(action) {
			t.Fatalf("/goal resume submitted %#v", action)
		}
		chat.setInput("/unknown-command")
		if action := chat.submitInput(); !reflect.DeepEqual(action, ChatActionNone{}) {
			t.Fatalf("/unknown-command submitted %#v", action)
		}
	}
	// A newly connected adapter owns capabilities too; do not retain old controls.
	activeEditOperational(&view, func(operational *core.RelayOperationalState) {
		operational.Goal = core.GoalState{}
	})
	applySessionView(chat, view, nil)
	chat.setInput("/goal finish the work")
	if action := chat.submitInput(); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("/goal finish the work submitted %#v", action)
	}
}

func TestHandleEventResultReportsWhichEventsTheChatConsumed(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-event-result", 72)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")

	moved := chat.HandleEventResult(tea.MouseMotionMsg{X: 0, Y: 0})
	if moved.Consumed {
		t.Fatal("a mouse move is not the chat's")
	}

	ignored := chat.HandleEventResult(tea.KeyPressMsg{Code: tea.KeyF12})
	if ignored.Consumed {
		t.Fatal("F12 is not the chat's")
	}

	typed := chat.HandleEventResult(key('x'))
	if !typed.Consumed {
		t.Fatal("typing is the composer's")
	}
	if draft := chat.Draft(); draft != "x" {
		t.Fatalf("draft = %q", draft)
	}

	// A cursor move and a cursor move that is already clamped are both the
	// composer's to answer, so neither reaches the dashboard behind it.
	movedCursor := chat.HandleEventResult(tea.KeyPressMsg{Code: tea.KeyLeft})
	if !movedCursor.Consumed {
		t.Fatal("a cursor move is the composer's")
	}

	clamped := chat.HandleEventResult(tea.KeyPressMsg{Code: tea.KeyLeft})
	if !clamped.Consumed {
		t.Fatal("a clamped cursor move is the composer's")
	}
}

func TestControlCQueuesAbandonedPromptPersistenceAndKeepsLocalHistory(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("abandoned-prompt", 72)
	persistence := core.NewUnbounded[ChatDaemonRequest]()
	chat := ActiveChatOpenWithPersistence(
		fixture.Stopped,
		"bundle-1",
		nil,
		fixture.Control,
		SessionHeaderIdentity{},
		"",
		NewNotices(),
		persistence,
	)
	t.Cleanup(chat.Close)
	chat.setInput("unfinished prompt")

	chat.HandleEvent(ctrl('c'))

	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
	if !slices.Equal(chat.promptHistory, []string{"unfinished prompt"}) {
		t.Fatalf("prompt history = %q", chat.promptHistory)
	}
	request, ok := tryRecv(persistence)
	if !ok {
		t.Fatal("abandoned prompt request")
	}
	abandoned, ok := request.(ChatDaemonRequestRecordAbandonedPrompt)
	if !ok {
		t.Fatalf("unexpected daemon request: %#v", request)
	}
	if abandoned.SessionID != "abandoned-prompt" || abandoned.BundleID != "bundle-1" || abandoned.Text != "unfinished prompt" {
		t.Fatalf("abandoned prompt = %#v", abandoned)
	}

	chat.HandleEvent(key(tea.KeyUp))
	if chat.input != "unfinished prompt" {
		t.Fatalf("input = %q", chat.input)
	}
}

func TestADisconnectedViewWithoutASnapshotStopsStaleAnimation(t *testing.T) {
	chat := activeTestSnapshotChat()
	started := uint64(1)
	chat.turnStartedAtEpochSeconds = &started
	if !chat.NeedsAnimation() {
		t.Fatal("a running turn animates")
	}
	applySessionView(chat, client.ManagedSessionView{Snapshot: nil, Connected: false, Error: nil}, nil)
	if chat.NeedsAnimation() {
		t.Fatal("a disconnected view stops the animation")
	}
}

// TestCaptureChatPreview captures the real conversation renderer for visual
// review. The caller chooses an artifact path; ordinary test runs never
// write screenshots (Rust marks it #[ignore]; here it runs only when the
// path is set).
func TestCaptureChatPreview(t *testing.T) {
	path := os.Getenv("MJ_CHAT_CAPTURE_PATH")
	if path == "" {
		t.Skip("writes a terminal-cell capture to MJ_CHAT_CAPTURE_PATH")
	}
	dimension := func(name string, fallback int) int {
		value, ok := os.LookupEnv(name)
		if !ok {
			return fallback
		}
		parsed, err := strconv.ParseUint(value, 10, 16)
		if err != nil {
			t.Fatal("capture dimensions must be unsigned integers")
		}
		return int(parsed)
	}
	columns := dimension("MJ_CHAT_CAPTURE_COLUMNS", 110)
	rows := dimension("MJ_CHAT_CAPTURE_ROWS", 40)
	chat := activeTestSnapshotChat()
	chat.SetHeaderSummary("local / mjolnir", "Claude · Sonnet", "")
	chat.markPromptSubmitted("Make the terminal feel beautifully crafted.")
	started := core.EpochSeconds() - min(42, core.EpochSeconds())
	chat.turnStartedAtEpochSeconds = &started
	step := max(core.EpochMillis()-7_000, 0)
	chat.setCurrentStepStart(&step)
	running := core.RelayExecutionStateRunning
	chat.setSessionActivity(client.SessionActivity{
		CheckingResponse: false,
		Execution:        &running,
	})
	tool := func(seq uint64, title, summary string, status core.ToolStatus) core.ChatEntry {
		entry := core.ChatEntryTool(seq, title, nil, status)
		entry.ToolSummary = &summary
		return entry
	}
	chat.entries = []core.ChatEntry{
		core.ChatEntryPlain(1, core.ChatRoleUser,
			"Make the terminal feel beautifully crafted. Keep it fast, readable, and calm."),
		core.ChatEntryPlain(2, core.ChatRoleAgent,
			"I’m bringing the interface together around a midnight palette, clear hierarchy, and the original animated activity indicators.\n\n### A little more room to think\n\n- Focus follows a soft teal border\n- **Your conversation stays readable** while tools work\n- Code and keyboard shortcuts have their own quiet surfaces"),
		tool(3, "cd dir && python x.py | cat | wc ; print ok", "cd && python | cat | wc ; print", core.ToolStatusCompleted),
		tool(4, "cargo test -p brokk-mj-chat", "cargo test", core.ToolStatusCompleted),
		core.ChatEntryPlain(5, core.ChatRoleAgent,
			"The shared theme is in place. Here’s the panel style used throughout the app:\n\n```rust\nlet panel = theme::panel(focused)\n    .title(\" Conversation \");\n```\n\nI’m checking the narrow layouts and selection behavior now."),
		tool(6, "cargo clippy --all-targets -- -D warnings", "cargo clippy", core.ToolStatusRunning),
	}
	// Keep the capture representative of the in-place tool expansion: the
	// first completed call opens to its provider title and splits the
	// surrounding completed streak.
	chat.expandedToolCalls[3] = struct{}{}
	buffer := activeDraw(columns, rows, func(frame *Frame) { renderFullFrame(frame, chat, false) })
	channels := func(drawn color.Color, fallback [3]uint8) [3]uint8 {
		if rgba, ok := drawn.(color.RGBA); ok {
			return [3]uint8{rgba.R, rgba.G, rgba.B}
		}
		return fallback
	}
	captureRows := make([][]map[string]any, 0, buffer.Height())
	for y := range buffer.Height() {
		row := make([]map[string]any, 0, buffer.Width())
		for x := range buffer.Width() {
			cell := buffer.CellAt(x, y)
			row = append(row, map[string]any{
				"text":      activeCellSymbol(buffer, x, y),
				"fg":        channels(cell.Style.Fg, [3]uint8{223, 235, 244}),
				"bg":        channels(cell.Style.Bg, [3]uint8{11, 18, 32}),
				"bold":      cell.Style.Attrs&uv.AttrBold != 0,
				"italic":    cell.Style.Attrs&uv.AttrItalic != 0,
				"underline": cell.Style.Underline != 0,
			})
		}
		captureRows = append(captureRows, row)
	}
	capture, err := json.Marshal(map[string]any{"width": buffer.Width(), "height": buffer.Height(), "rows": captureRows})
	if err != nil {
		t.Fatalf("encode preview: %v", err)
	}
	if err := os.WriteFile(path, capture, 0o644); err != nil {
		t.Fatalf("write preview: %v", err)
	}
}

func TestReplacementChatPreservesTheLatestSameSessionDraftEvenWhenCleared(t *testing.T) {
	prepare := func(id, draft string) PreparedChat {
		fixture := client.NewReplacementSessionTestFixture(id, 89)
		return ActiveChatPrepareWithPersistence(
			fixture.Stopped,
			"bundle-1",
			nil,
			fixture.Control,
			SessionHeaderIdentity{},
			draft,
			NewNotices(),
			nil,
		)
	}
	previous := prepare("moving", "saved before preparation").Open()
	t.Cleanup(previous.Close)
	pending := prepare("moving", "stale saved draft")
	previous.setInput("edited during preparation")
	replacement := pending.OpenReplacing(previous)
	t.Cleanup(replacement.Close)
	if draft := replacement.Draft(); draft != "edited during preparation" {
		t.Fatalf("draft = %q", draft)
	}

	replacement.clearInput()
	cleared := prepare("moving", "stale draft must not return").OpenReplacing(replacement)
	t.Cleanup(cleared.Close)
	if draft := cleared.Draft(); draft != "" {
		t.Fatalf("draft = %q", draft)
	}
	different := prepare("other", "other session draft").OpenReplacing(previous)
	t.Cleanup(different.Close)
	if draft := different.Draft(); draft != "other session draft" {
		t.Fatalf("draft = %q", draft)
	}
}

func managedView(session core.MaterializedSession) client.ManagedSessionView {
	sessionID := session.SessionID
	latestOrdinal := session.AppliedEventOrdinal
	latestDigest := session.AppliedEventDigest
	protocol := core.RelayProtocolVersion
	return client.ManagedSessionView{
		Snapshot: &core.ManagedSessionSnapshot{
			Window:       core.ProjectionWindowOf(&session),
			Materialized: session,
			Operational: core.NewSharedSnapshot(core.RelayOperationalState{
				UndeliveredMailboxEventKeys: []string{},
				RelayProtocolVersion:        &protocol,
				NativeAgents:                []core.NativeAgent{},
				SessionID:                   sessionID,
				Execution:                   core.RelayExecutionStateIdle,
				LatestOrdinal:               latestOrdinal,
				LatestDigest:                latestDigest,
				AcknowledgedThrough:         latestOrdinal,
				AcknowledgedDigest:          latestDigest,
				RecoveryFloorOrdinal:        0,
				RecoveryFloorDigest:         core.RelayEventGenesisDigest,
				ConfigOptions:               []schema.SessionConfigOption{},
				AvailableCommands:           []schema.AvailableCommand{},
				Config:                      map[string]string{},
				QueuedPrompts:               []core.QueuedRelayPrompt{},
				ActiveUserShells:            []core.ActiveUserShell{},
				ActiveAgentTerminals:        []core.ActiveAgentTerminal{},
			}),
		},
		Connected: true,
		Error:     nil,
	}
}

func TestDetachingLeavesTheUnsentInputWhereTheDashboardSavesItFrom(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.setInput("half typed thought")
	session := core.MaterializedSessionEmpty("session-detach")
	session.AppliedEventOrdinal = 12
	session.Transcript = []*core.TranscriptItem{agentTranscriptItem("read through here", 12)}
	applySessionView(chat, managedView(session), nil)
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "queued"))

	// Detaching keeps the composer intact: the warm chat goes on holding
	// it, and the surface reads it here to write it to the session row.
	if seen := detachChat(chat); seen != 12 {
		t.Fatalf("read through %d", seen)
	}
	if chat.input != "half typed thought" {
		t.Fatalf("input = %q", chat.input)
	}
	if len(chat.entries) != 1 {
		t.Fatalf("%d entries", len(chat.entries))
	}
	if len(chat.queuedPrompts) != 0 {
		t.Fatalf("queued prompts = %#v", chat.queuedPrompts)
	}

	if seen := detachChat(chat); seen != 12 {
		t.Fatalf("read through %d", seen)
	}
	if chat.input != "half typed thought" {
		t.Fatalf("input = %q", chat.input)
	}

	empty := activeTestSnapshotChat()
	detachChat(empty)
	if empty.input != "" {
		t.Fatalf("input = %q", empty.input)
	}
}

func TestTranscriptWheelScrollsTheReducedViewportIndependently(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.entries = nil
	for index := range uint64(40) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(index, core.ChatRoleAgent, fmt.Sprintf("transcript row %d", index)))
	}
	chat.elicitation = newElicitationDialog(activeElicitationRequest("scroll-question", "Answer this"))
	chat.taskDialogOpen = true
	drawnTranscript(chat, 80, 24)
	transcriptSurface := chat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("transcript registered")
	}
	transcript := transcriptSurface.Rect
	before := chat.anchor

	if action := chat.HandleMouse(tea.MouseWheelMsg{
		X:      transcript.Min.X + 1,
		Y:      transcript.Min.Y + 1,
		Button: tea.MouseWheelUp,
	}); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("wheel returned %#v", action)
	}
	if chat.anchor == before {
		t.Fatal("the wheel scrolled the transcript")
	}
	if chat.elicitation == nil {
		t.Fatal("the question stays open")
	}

	scrollbarX := chat.FrameSurfaces().Surface(SurfaceIdTranscript{}).Rect.Max.X
	if action := chat.HandleMouse(tea.MouseClickMsg{
		X:      scrollbarX,
		Y:      transcript.Min.Y + transcript.Dy()/2,
		Button: tea.MouseLeft,
	}); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("scrollbar press returned %#v", action)
	}
	if !chat.TranscriptScrollbarDragging() {
		t.Fatal("pressing the scrollbar drags it")
	}
	chat.HandleMouse(tea.MouseReleaseMsg{
		X:      scrollbarX,
		Y:      transcript.Min.Y + transcript.Dy()/2,
		Button: tea.MouseLeft,
	})
	if chat.TranscriptScrollbarDragging() {
		t.Fatal("releasing the scrollbar ends the drag")
	}
}

func TestQuestionPointerCaptureWinsWhenDraggedIntoTranscript(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.entries = nil
	for index := range uint64(40) {
		chat.entries = append(chat.entries, core.ChatEntryPlain(index, core.ChatRoleAgent, "transcript"))
	}
	notByDefault := false
	request := activeElicitationRequest("captured-question", "Choose whether to continue")
	request.Fields = []core.ElicitationField{{
		ID:       "continue",
		Title:    "Continue",
		Required: false,
		Secret:   false,
		Kind:     core.ElicitationFieldKindBoolean{Default: &notByDefault},
	}}
	chat.elicitation = newElicitationDialog(request)
	drawnTranscript(chat, 80, 24)
	formSurface := chat.FrameSurfaces().Surface(SurfaceIdModalBody{})
	if formSurface == nil {
		t.Fatal("question form registered")
	}
	form := formSurface.Rect
	press := tea.MouseClickMsg{X: form.Min.X + 1, Y: form.Min.Y + 1, Button: tea.MouseLeft}
	if !chat.ComponentHandlesMouse(press) {
		t.Fatal("the form handles its own press")
	}
	chat.HandleMouse(press)
	transcriptSurface := chat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("transcript registered")
	}
	transcript := transcriptSurface.Rect
	drag := tea.MouseMotionMsg{X: transcript.Min.X + 1, Y: transcript.Min.Y + 1, Button: tea.MouseLeft}
	if !chat.ComponentHandlesMouse(drag) {
		t.Fatal("the form keeps a drag it captured")
	}
	before := chat.anchor
	chat.HandleMouse(drag)
	if chat.anchor != before {
		t.Fatal("a captured drag does not scroll the transcript")
	}
	chat.HandleMouse(tea.MouseReleaseMsg{X: drag.X, Y: drag.Y, Button: tea.MouseLeft})
}

func TestAnOffScreenChatFollowsTheSessionView(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.setTranscriptLoading(true)
	session := core.MaterializedSessionEmpty("session-warm")
	session.AppliedEventOrdinal = 5
	session.AppliedEventDigest = strings.Repeat("a", 64)
	session.Transcript = []*core.TranscriptItem{agentTranscriptItem("first", 5)}

	firstView := managedView(session)
	activeEditOperational(&firstView, func(operational *core.RelayOperationalState) {
		started := int64(12_345)
		operational.CurrentStepStartedAtMs = &started
	})
	if !applySessionView(chat, firstView, nil) {
		t.Fatal("the view keeps the feed open")
	}
	if chat.LatestSeq() != 5 {
		t.Fatalf("latest seq %d", chat.LatestSeq())
	}
	if len(chat.entries) != 1 {
		t.Fatalf("%d entries", len(chat.entries))
	}
	if chat.currentStepStartedAtMs == nil || *chat.currentStepStartedAtMs != 12_345 {
		t.Fatalf("current step started at %v", chat.currentStepStartedAtMs)
	}
	if chat.transcriptLoading {
		t.Fatal("a snapshot ends the loading row")
	}

	session.AppliedEventOrdinal = 8
	session.Transcript = append(slices.Clone(session.Transcript), agentTranscriptItem("second", 8))
	if !applySessionView(chat, managedView(session), nil) {
		t.Fatal("the view keeps the feed open")
	}
	if chat.LatestSeq() != 8 {
		t.Fatalf("latest seq %d", chat.LatestSeq())
	}
	if len(chat.entries) != 2 {
		t.Fatalf("%d entries", len(chat.entries))
	}
	if chat.currentStepStartedAtMs != nil {
		t.Fatalf("current step started at %v", *chat.currentStepStartedAtMs)
	}
}

func TestATransientErrorBeforeTheFirstSnapshotKeepsTheLoadingRow(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.setTranscriptLoading(true)
	view := client.ManagedSessionView{
		Snapshot:  nil,
		Connected: false,
		Error:     &client.ViewError{Kind: client.ViewErrorKindUnreachable, Message: "database is busy"},
	}

	if !applySessionView(chat, view, nil) {
		t.Fatal("the view keeps the feed open")
	}
	if !chat.transcriptLoading {
		t.Fatal("a transient error keeps the loading row")
	}
	if notice, _ := chat.Notice(); notice != "connection lost: database is busy" {
		t.Fatalf("notice = %q", notice)
	}
	if text := transcriptText(chat, 80); !slices.Equal(text, []string{"Loading…"}) {
		t.Fatalf("transcript = %q", text)
	}
}

func TestAStoppedSessionManagerRetiresItsFeedAndSaysSo(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.setTranscriptLoading(true)

	open := applySessionView(chat, client.ManagedSessionView{}, errors.New("session manager stopped"))

	if open {
		t.Fatal("a stopped manager retires the feed")
	}
	if !chat.transcriptLoading {
		t.Fatal("the loading row stays")
	}
	if notice, _ := chat.Notice(); notice != "connection lost: session manager stopped" {
		t.Fatalf("notice = %q", notice)
	}
}

func TestDictationCompletionPreservesEditsAndRecoversAfterErrors(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-dictation", 72)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "original")
	chat.voiceActive = true
	chat.setInput("edited while recording")
	chat.applyVoiceUpdate(VoiceUpdateFinished{Text: "spoken words"})
	if draft := chat.Draft(); draft != "edited while recording spoken words" {
		t.Fatalf("draft = %q", draft)
	}
	if chat.voiceActive {
		t.Fatal("a finished dictation stops listening")
	}
	chat.voiceActive = true
	chat.applyVoiceUpdate(VoiceUpdateFinished{Err: errors.New("capture failed")})
	if draft := chat.Draft(); draft != "edited while recording spoken words" {
		t.Fatalf("draft = %q", draft)
	}
	if chat.voiceActive {
		t.Fatal("a failed dictation stops listening")
	}
	if notice, _ := chat.Notice(); !strings.Contains(notice, "capture failed") {
		t.Fatalf("notice = %q", notice)
	}
	chat.applyVoiceUpdate(VoiceUpdateFinished{Text: ""})
	if draft := chat.Draft(); draft != "edited while recording spoken words" {
		t.Fatalf("draft = %q", draft)
	}
}

// TestTheDictationChordSaysWhyDictationIsUnavailable: launch findings A-4
// and E-9: with no voice helper or no dictation sign-in, the dictation chord
// changed nothing on screen and wrote no log line, so the key looked broken.
// It now says why nothing started.
// Hard-won: 6003829: unavailable dictation chords were silently ignored
func TestTheDictationChordSaysWhyDictationIsUnavailable(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-dictation-unavailable", 72)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")

	notice, ok := chat.ToggleDictation()
	if !ok {
		t.Fatal("the chord explains itself")
	}

	if chat.voiceActive {
		t.Fatal("unavailable dictation does not start")
	}
	if !strings.HasPrefix(notice, "Dictation is unavailable") {
		t.Fatal(notice)
	}
	if draft := chat.Draft(); draft != "" {
		t.Fatalf("draft = %q", draft)
	}

	// Once the probe has answered, the chord gives its reason.
	reason := dictationNeedsACodexProfile
	chat.applyVoiceUpdate(VoiceUpdateAvailability{Unavailable: &reason})
	chat.clearNotice()
	if notice, ok := chat.ToggleDictation(); !ok || notice != dictationNeedsACodexProfile {
		t.Fatalf("notice = %q, %v", notice, ok)
	}
	if chat.voiceActive {
		t.Fatal("unavailable dictation does not start")
	}
}

// activePumpUntil pumps the chat until done reports true, failing after
// limit, as the Rust tests wrap their pump loops in tokio::time::timeout.
func activePumpUntil(t *testing.T, chat *ActiveChat, limit time.Duration, done func() bool, failure string) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), limit)
	defer cancel()
	for {
		ActiveChatPump(ctx, chat)
		if done() {
			return
		}
		if ctx.Err() != nil {
			t.Fatal(failure)
		}
	}
}

func TestAnOpenChatHandsOffToAReplacementActorWithoutLosingItsDraft(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-replaced", 73)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "half-written prompt")

	activePumpUntil(t, chat, time.Second, func() bool {
		return chat.SessionFeedOpen() && !chat.session.IsStopped()
	}, "the replacement actor became the live chat feed")

	if draft := chat.Draft(); draft != "half-written prompt" {
		t.Fatalf("draft = %q", draft)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
}

// activeTurnReview is the running review the reviewer tests project.
func activeTurnReview(sessionID string, request core.ElicitationRequest) client.RuntimeReviewView {
	child := "reviewer-a"
	return client.RuntimeReviewView{
		ReviewID:       "",
		Roots:          []string{},
		ChildSessionID: nil,
		SessionID:      sessionID,
		Phase:          core.TurnReviewPhaseRunning{Roles: []core.RoleStatus{}},
		Roles: []core.RoleStatus{{
			Role:           "reviewer",
			Label:          "Reviewer",
			State:          core.RoleStateRunning,
			ChildSessionID: &child,
		}},
		Status:  "Review running",
		Verdict: nil,
		Questions: []client.ReviewerQuestion{{
			Role:    "reviewer-a",
			Request: request,
		}},
	}
}

// Hard-won: 1f62bc101: reviewer answer failures were invisible when only the notification form was open.
func TestDetachingAChatKeepsAReviewerDraftWaitingForItsLateStream(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-deferred-review", 74)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	request := activeElicitationRequest("reviewer-deferred-1", "Allow the reviewer to continue?")
	reviewer := activeTestSnapshotChat()
	reviewerA := "reviewer-a"
	if !reviewer.showReviewRoleElicitation(&reviewerA, request) {
		t.Fatal("the reviewer form shows")
	}
	reviewerDraft := reviewer.elicitationDraft()
	if reviewerDraft == nil {
		t.Fatal("reviewer form can be snapshotted")
	}

	// The primary form is visible while the sidecar has not surfaced its
	// form yet. A session switch must retain both local snapshots so the
	// reviewer answer is restored when its stream catches up.
	chat.restoreElicitation(request)
	chat.deferredElicitationDrafts = []ChatElicitationDraft{*reviewerDraft}
	drafts := chat.ElicitationDrafts()
	if len(drafts) != 2 {
		t.Fatalf("%d drafts", len(drafts))
	}
	if !slices.ContainsFunc(drafts, func(draft ChatElicitationDraft) bool { return !draft.Reviewer() }) {
		t.Fatal("the primary draft is kept")
	}
	if !slices.ContainsFunc(drafts, func(draft ChatElicitationDraft) bool {
		role, ok := draft.ReviewerRole()
		return draft.Reviewer() && ok && role == "reviewer-a"
	}) {
		t.Fatal("the reviewer draft is kept")
	}
	// Hard-won: 1f62bc101: reviewer answer refusals disappeared from notification-only forms.
	chat.elicitation = nil
	chat.elicitationIsReviewers = false
	chat.deferredElicitationDrafts = nil
	request, err := core.ElicitationRequestFromACPParams("reviewer-failure", json.RawMessage(`{
		"mode": "form", "sessionId": "reviewer-a", "message": "Review answer", "requestedSchema": {
			"type": "object", "properties": {"answer": {"type": "string"}}
		}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	chat.setTurnReviews([]client.RuntimeReviewView{activeTurnReview("session-deferred-review", request)})
	chat.showReviewRoleElicitation(&reviewerA, request)
	drawElicitation := func(chat *ActiveChat) string {
		return activeBufferText(activeDraw(120, 18, func(frame *Frame) {
			chat.DrawElicitationIn(frame, frame.Area(), true)
		}))
	}
	drawElicitation(chat)
	chat.HandleElicitationEvent(tea.PasteMsg{Content: "kept reviewer answer"})
	draft := chat.elicitationDraft()
	if draft == nil {
		t.Fatal("the reviewer form has a draft")
	}
	chat.HandleElicitationEvent(key(tea.KeyEnter))
	if chat.reviewerElicitationOpen() {
		t.Fatal("the accepted answer closes the form")
	}
	if !chat.HasPendingElicitationInput() {
		t.Fatal("accepted answer retains its result owner after the form closes")
	}
	// Hard-won: f69edee: a failure for A lost its draft while reviewer B owned the dialog.
	reviewA := chat.turnReviews[0].view
	reviewB := reviewA
	reviewB.Roles = slices.Clone(reviewA.Roles)
	reviewB.Questions = slices.Clone(reviewA.Questions)
	questionB := *draft.request()
	questionB.ID = "reviewer-b-question"
	questionB.Message = "Reviewer B question"
	roleB := reviewB.Roles[0]
	childB := "reviewer-b"
	roleB.ChildSessionID = &childB
	reviewB.Roles = append(reviewB.Roles, roleB)
	reviewB.Questions = append(reviewB.Questions, client.ReviewerQuestion{Role: "reviewer-b", Request: questionB})
	chat.ApplyReviewViews([]client.RuntimeReviewView{reviewB})
	if chat.elicitation == nil || !chat.elicitation.request().Equal(&questionB) {
		t.Fatal("reviewer B's question is on screen")
	}
	parked := *draft
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: &parked, Err: errors.New("reviewer lookup failed")})
	if chat.elicitation == nil || !chat.elicitation.request().Equal(&questionB) {
		t.Fatal("reviewer B's question stays on screen")
	}
	if len(chat.deferredFormResults) != 1 {
		t.Fatalf("%d deferred results", len(chat.deferredFormResults))
	}
	// Hard-won: 5dc2084: upgrade snapshots omitted a rejected draft waiting behind another form.
	saved, err := json.Marshal(chat.ElicitationDrafts())
	if err != nil {
		t.Fatal(err)
	}
	upgraded := clipboardTestChat(t)
	// Hard-won: 57e492d: ChatOpened restores snapshots before installing the review projection.
	var restored []ChatElicitationDraft
	if err := json.Unmarshal(saved, &restored); err != nil {
		t.Fatal(err)
	}
	upgraded.RestoreElicitationDrafts(restored)
	upgraded.ApplyReviewViews([]client.RuntimeReviewView{reviewB})
	recovered := drawElicitation(upgraded)
	if !strings.Contains(recovered, "kept reviewer answer") {
		t.Fatal(recovered)
	}
	if !strings.Contains(recovered, "reviewer lookup failed") {
		t.Fatal(recovered)
	}
	// B is resolved elsewhere; the authoritative refresh reveals A again.
	chat.ApplyReviewViews([]client.RuntimeReviewView{reviewA})
	if len(chat.deferredFormResults) != 0 {
		t.Fatalf("%d deferred results", len(chat.deferredFormResults))
	}

	text := drawElicitation(chat)
	if !strings.Contains(text, "reviewer lookup failed") || !strings.Contains(text, "kept reviewer answer") {
		t.Fatal(text)
	}
	if chat.HasPendingElicitationInput() {
		t.Fatal("the failed answer released its result owner")
	}
	// Hard-won: f69edee: a standby composer consumed a reviewer failure without its review projection.
	chat.HandleElicitationEvent(key(tea.KeyEnter))
	var config core.Config
	activeUnmarshal(t, `{"version":0}`, &config)
	standby := ChatStateStandby("session-deferred-review", &config, SessionHeaderIdentity{}, NewNotices())
	taken := chat.TakeLiveComposer()
	if taken == nil {
		t.Fatal("the chat hands over its composer")
	}
	standby.AdoptLiveComposer(taken)
	parked = *draft
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: &parked, Err: errors.New("parked reviewer failure")})
	standby.DrainComposer()
	if len(standby.deferredFormResults) != 1 {
		t.Fatalf("%d deferred results", len(standby.deferredFormResults))
	}
	// Hard-won: 57e492d: parked composer failures were omitted from upgrade snapshots.
	parkedSaved, err := json.Marshal(standby.ElicitationDrafts())
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(parkedSaved), "parked reviewer failure") || !strings.Contains(string(parkedSaved), "kept reviewer answer") {
		t.Fatal(string(parkedSaved))
	}
	if standby.elicitation != nil {
		t.Fatal("a standby composer shows no form")
	}
	replacement := clipboardTestChat(t)
	standbyComposer := standby.TakeLiveComposer()
	if standbyComposer == nil {
		t.Fatal("the standby composer hands over its state")
	}
	replacement.AdoptLiveComposer(*standbyComposer)
	if len(replacement.deferredFormResults) != 1 {
		t.Fatalf("the review projection is not known yet: %d deferred results", len(replacement.deferredFormResults))
	}
	replacement.ApplyReviewViews([]client.RuntimeReviewView{reviewA})
	if len(replacement.deferredFormResults) != 0 {
		t.Fatalf("%d deferred results", len(replacement.deferredFormResults))
	}
	text = drawElicitation(replacement)
	if !strings.Contains(text, "parked reviewer failure") || !strings.Contains(text, "kept reviewer answer") {
		t.Fatal(text)
	}
	chat = replacement
	// Hard-won: 56747ef: a failed answer resurrected a question removed by the latest review projection.
	chat.ApplyReviewViews(nil)
	parked = *draft
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: &parked, Err: errors.New("reviewer lookup failed")})
	if chat.reviewerElicitationOpen() {
		t.Fatal("a removed question does not come back")
	}
	parked = *draft
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: &parked, Err: errors.New("request is no longer pending")})
	if chat.reviewerElicitationOpen() {
		t.Fatal("a removed question does not come back")
	}
	if !slices.ContainsFunc(chat.conversationNotices, func(notice conversationNotice) bool {
		return strings.Contains(notice.Text, "not delivered")
	}) {
		t.Fatal("the undelivered answer is reported")
	}
	notices := len(chat.conversationNotices)
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: nil, Err: errors.New("request is no longer pending")})
	if len(chat.conversationNotices) != notices {
		t.Fatal("automatic late plan answers stay quiet")
	}
	newer := *draft.request()
	newer.ID = "new-primary-question"
	newer.Message = "New primary question"
	chat.elicitation = nil
	chat.elicitationIsReviewers = false
	chat.elicitationRole = nil
	chat.restoreElicitation(newer)
	parked = *draft
	chat.applyRemoteResult(chatRemoteResultReviewerAnswer{Draft: &parked, Err: errors.New("old failure")})
	text = drawElicitation(chat)
	if !strings.Contains(text, "New primary question") {
		t.Fatal(text)
	}
	if strings.Contains(text, "old failure") {
		t.Fatal(text)
	}
}

func TestAnExternalReviewRemovalClosesItsVisibleQuestion(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-review-removed", 76)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	request := activeElicitationRequest("reviewer-removed-1", "Allow the reviewer to continue?")
	reviewerA := "reviewer-a"
	if !chat.showReviewRoleElicitation(&reviewerA, request) {
		t.Fatal("the reviewer form shows")
	}
	if !chat.reviewerElicitationOpen() {
		t.Fatal("the reviewer form is open")
	}

	// A missing runtime review is authoritative even when the journal poll
	// that used to carry the form has no more events.
	chat.ApplyReviewViews(nil)
	if chat.reviewerElicitationOpen() {
		t.Fatal("the removed review's form closes")
	}
	if chat.ElicitationDraft() != nil {
		t.Fatal("nothing is left to snapshot")
	}
}

// TestACodexSessionListsPlanFromTheHeaderHarness: a Codex session exposes
// plan mode through its collaboration_mode config, which the chat reads only
// once it knows the harness is Codex. That fact reaches the chat through the
// header now that the daemon owns the recovery context the open path used to
// carry, so a Codex session must still list /plan.
// Hard-won: 1714125: Codex /plan disappeared when recovery stopped supplying harness context
func TestACodexSessionListsPlanFromTheHeaderHarness(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-codex", 75)
	codex := core.HarnessKindCodex
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{HarnessKind: &codex}, "")

	chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("collaboration_mode", "default", []string{"default", "plan"})})

	if !chat.listsCommand("plan") {
		t.Fatal("a Codex session lists /plan once the header names the harness")
	}
}

// contextTestWorkspace is a workspace id no configuration uses, so the
// per-workspace rows the constructor reads are simply absent and it falls
// back to its defaults — the same tolerance the other open tests rely on.
const contextTestWorkspace = "workspace-for-chat-session-context-tests"

func contextSessionRecord(id, workspaceID string) core.SessionRecord {
	return core.SessionRecord{
		OracleChoice:              nil,
		Oracle:                    true,
		NotificationsIgnored:      false,
		ID:                        id,
		WorkspaceID:               workspaceID,
		Title:                     "work",
		HarnessKind:               core.HarnessKindCodex,
		LastProfile:               "codex-1",
		BundleID:                  "bundle-1",
		NoProjectMemory:           false,
		NoGithubEvents:            false,
		NoGithubCredentials:       false,
		TargetTemplateID:          "podman",
		AdditionalMounts:          []core.AdditionalMount{},
		State:                     core.SessionStateRunning,
		Archived:                  false,
		CreatedAt:                 "2026-08-09T12:00:00Z",
		UpdatedAt:                 "2026-08-09T12:01:00Z",
		ViewedThroughEventOrdinal: 0,
		DraftInput:                "",
		FailureGeneration:         0,
	}
}

// activeHarnessProfile is one (id, kind) pair of configWithProfiles.
type activeHarnessProfile struct {
	id   string
	kind core.HarnessKind
}

func configWithProfiles(profiles []activeHarnessProfile) core.Config {
	config := core.DefaultConfig()
	config.Profiles = map[string]core.HarnessProfile{}
	for _, profile := range profiles {
		config.Profiles[profile.id] = core.NewHarnessProfile(profile.kind, "/profiles/"+profile.id)
	}
	return config
}

func chatContext(sessionID string, profiles []activeHarnessProfile) *ChatSessionContext {
	return &ChatSessionContext{
		Config:  configWithProfiles(profiles),
		Session: contextSessionRecord(sessionID, contextTestWorkspace),
	}
}

// TestReviewStatusConfigurationIsAppliedOnOpenAndRefresh: both review
// surfaces reflect the shared settings as configuration changes.
func TestReviewStatusConfigurationIsAppliedOnOpenAndRefresh(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-review", 88)
	sessionContext := chatContext("session-review", nil)
	terra := core.SubagentChoice{Model: "gpt-6-terra", Effort: nil}
	sessionContext.Config.Review = core.ReviewConfig{
		Legacy:      nil,
		Disabled:    false,
		BriefWriter: nil,
		Reviewers:   []core.SubagentChoice{terra, terra},
	}
	chat := activeOpenChat(t, fixture.Stopped, sessionContext, fixture.Control, SessionHeaderIdentity{}, "")

	want := core.ReviewConfig{Reviewers: []core.SubagentChoice{terra, terra}}
	if got := chat.ReviewConfig(); !reflect.DeepEqual(got, want) {
		t.Fatalf("review config = %#v", got)
	}
	title := promptTitleParts(chat.ChatState)
	// Review runs before push, not at the end of a turn, so the composer
	// shows nothing for it.
	if slices.Contains(title, "review") {
		t.Fatalf("%q", title)
	}
	if slices.Contains(title, "quick") || slices.Contains(title, "extended") {
		t.Fatalf("%q", title)
	}

	reloaded := core.DefaultConfig()
	reloaded.Review.Reviewers = []core.SubagentChoice{{Model: "gpt-6-sol", Effort: nil}}
	chat.RefreshContext(&reloaded, nil, nil)

	if got := chat.ReviewConfig(); !reflect.DeepEqual(got, reloaded.Review) {
		t.Fatalf("review config = %#v", got)
	}
}

// TestARecordedCheckpointErrorReachesTheNoticeWhenTheChatOpens: a failed
// recovery copy is the one thing the user has to see on opening the session,
// so it is raised after the connection notice a cold open also sets: a notice
// is a single slot, and the last write wins.
// Hard-won: dc7ee47: the checkpoint failure notice disappeared behind cold-open connection feedback
func TestARecordedCheckpointErrorReachesTheNoticeWhenTheChatOpens(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-checkpoint", 81)
	sessionContext := chatContext("session-checkpoint", nil)
	failure := "the target ran out of disk"
	sessionContext.Session.LastCheckpointError = &failure

	chat := activeOpenChat(t, fixture.Stopped, sessionContext, fixture.Control, SessionHeaderIdentity{}, "")

	if notice, _ := chat.Notice(); notice != "Recovery copy failed: the target ran out of disk" {
		t.Fatalf("notice = %q", notice)
	}
}

// TestARefreshedConfigUpdatesContextForReview: the chat snapshots the
// configuration when it opens, so a reload has to be handed to it; otherwise
// a long-lived conversation goes on offering the profiles that existed when
// it was opened.
func TestARefreshedConfigUpdatesContextForReview(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-refresh", 85)
	chat := activeOpenChat(t, fixture.Stopped,
		chatContext("session-refresh", []activeHarnessProfile{{"codex-1", core.HarnessKindCodex}}),
		fixture.Control, SessionHeaderIdentity{}, "")
	if len(chat.context.Config.Profiles) != 1 {
		t.Fatalf("%d profiles", len(chat.context.Config.Profiles))
	}

	reloaded := configWithProfiles([]activeHarnessProfile{
		{"codex-1", core.HarnessKindCodex},
		{"claude-1", core.HarnessKindClaude},
	})
	moved := contextSessionRecord("session-refresh", "workspace-moved")
	chat.RefreshContext(&reloaded, &moved, &moved)

	profiles := make([]string, 0, len(chat.context.Config.Profiles))
	for id := range chat.context.Config.Profiles {
		profiles = append(profiles, id)
	}
	slices.Sort(profiles)
	if !slices.Equal(profiles, []string{"claude-1", "codex-1"}) {
		t.Fatalf("profiles = %q", profiles)
	}
	if chat.context.Session.WorkspaceID != "workspace-moved" {
		t.Fatalf("workspace = %q", chat.context.Session.WorkspaceID)
	}

	// Another session's record is not this session's, so it is ignored.
	other := contextSessionRecord("session-other", "workspace-other")
	chat.RefreshContext(&reloaded, &other, &other)
	if chat.context.Session.WorkspaceID != "workspace-moved" {
		t.Fatalf("workspace = %q", chat.context.Session.WorkspaceID)
	}

	// A chat opened without a context has nothing to refresh.
	fixture = client.NewReplacementSessionTestFixture("session-bare", 86)
	bare := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	bare.RefreshContext(&reloaded, nil, nil)
	if bare.context != nil {
		t.Fatal("a bare chat stays without a context")
	}
}

func TestASameSessionContextRefreshUpdatesTheVisibleHeaderWithoutLosingChatState(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-header-refresh", 89)
	initial := chatContext("session-header-refresh", []activeHarnessProfile{{"codex-1", core.HarnessKindCodex}})
	initial.Session.TargetTemplateID = "localhost"
	initial.Session.LastProfile = "codex-1"
	codex := core.HarnessKindCodex
	chat := activeOpenChat(t, fixture.Stopped, initial, fixture.Control, SessionHeaderIdentity{
		Target:        "localhost",
		Profile:       "codex-1",
		Title:         "Original session title",
		HarnessKind:   &codex,
		SubagentCount: 0,
		Reviewers:     ReviewerCounts{},
	}, "keep this draft")
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleUser, "history that must remain"))

	reloaded := configWithProfiles([]activeHarnessProfile{
		{"codex-1", core.HarnessKindCodex},
		{"claude-2", core.HarnessKindClaude},
	})
	moved := contextSessionRecord("session-header-refresh", "workspace-moved")
	moved.TargetTemplateID = "podman"
	moved.LastProfile = "claude-2"
	moved.HarnessKind = core.HarnessKindClaude
	harnessTitle := "Harness session title"
	renamed := "Renamed session"
	moved.ACPSessionTitle = &harnessTitle
	moved.SessionTitleOverride = &renamed
	chat.RefreshContext(&reloaded, &moved, &moved)

	if draft := chat.Draft(); draft != "keep this draft" {
		t.Fatalf("draft = %q", draft)
	}
	if !slices.ContainsFunc(chat.entries, func(entry core.ChatEntry) bool { return entry.Text == "history that must remain" }) {
		t.Fatal("the transcript survives the refresh")
	}

	rendered := activeBufferText(activeDraw(100, 24, func(frame *Frame) {
		renderFullFrame(frame, chat.ChatState, false)
	}))
	if !strings.Contains(rendered, "podman  Idle  claude-2  Renamed session") {
		t.Fatalf("the refreshed target/profile must be visible in the conversation header: %q", rendered)
	}
	if strings.Contains(rendered, "localhost  Idle  codex-1") {
		t.Fatal(rendered)
	}
	if strings.Contains(rendered, "Original session title") {
		t.Fatal(rendered)
	}
	if strings.Contains(rendered, "Harness session title") {
		t.Fatal(rendered)
	}
}

// TestAnUnnamedSessionSHeaderUsesItsListedTitleNotItsID: launch finding
// R3-11: before the harness named a new session, the conversation header
// showed its 32-hex id while the Sessions row showed the title it was
// created with ("project via fake"). The header uses the listed title too.
// Hard-won: b05328c: unnamed session headers showed the UUID instead of the listed title
func TestAnUnnamedSessionSHeaderUsesItsListedTitleNotItsID(t *testing.T) {
	sessionID := "a225e234d043d75737319553cd926f50"
	fixture := client.NewReplacementSessionTestFixture(sessionID, 91)
	chat := activeOpenChat(t, fixture.Stopped,
		chatContext(sessionID, []activeHarnessProfile{{"codex-1", core.HarnessKindCodex}}),
		fixture.Control, SessionHeaderIdentity{}, "")
	reloaded := configWithProfiles([]activeHarnessProfile{{"codex-1", core.HarnessKindCodex}})
	unnamed := contextSessionRecord(sessionID, "workspace-1")
	unnamed.Title = "project via fake"
	unnamed.ACPSessionTitle = nil
	unnamed.SessionTitleOverride = nil
	chat.RefreshContext(&reloaded, &unnamed, &unnamed)

	rendered := activeBufferText(activeDraw(120, 24, func(frame *Frame) {
		renderFullFrame(frame, chat.ChatState, false)
	}))
	if !strings.Contains(rendered, "project via fake") {
		t.Fatalf("%q", rendered)
	}
	if strings.Contains(rendered, "a225e234d043") {
		t.Fatalf("%q", rendered)
	}
}

func TestAnActiveRuntimeRecordRearmsAChatAfterItsHandoffTimedOut(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-resumed", 74)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "still drafting")
	originalInstance := chat.Instance()
	chat.sessionOpen = false
	chat.finishSessionReconnect(client.SessionHandle{}, errors.New("session session-resumed is not managed"))

	chat.SetSessionFeedExpected(true)
	activePumpUntil(t, chat, time.Second, func() bool {
		return chat.SessionFeedOpen() && !chat.session.IsStopped()
	}, "the active runtime record restarted the session handoff")
	if chat.IsInstance(originalInstance) {
		t.Fatal("a reattached actor retires the previous view identity")
	}

	if draft := chat.Draft(); draft != "still drafting" {
		t.Fatalf("draft = %q", draft)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
}

// Hard-won: 66c7315: an intentional stop triggered a false reconnect wait
func TestARetiringSessionDoesNotReconnectWhenItsFeedCloses(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-stop", 12)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	chat.SetSessionRetiring(true)

	chat.applySessionView(client.ManagedSessionView{}, errors.New("session manager stopped: channel closed"))

	if chat.SessionFeedOpen() {
		t.Fatal("the feed is retired")
	}
	if chat.sessionReconnectInFlight {
		t.Fatal("a deliberate stop starts no handoff")
	}
	if notice, ok := chat.Notice(); ok && (strings.Contains(notice, "Could not reconnect") || strings.Contains(notice, "connection lost")) {
		t.Fatalf("a deliberate stop reports neither a lost connection nor a failed handoff, but the notice was %q", notice)
	}

	// A session that becomes runnable again is expected once more, and the
	// handoff comes back with it.
	chat.SetSessionFeedExpected(true)
	if chat.SessionRetiring() {
		t.Fatal("an expected feed is not retiring")
	}
	if !chat.sessionReconnectInFlight {
		t.Fatal("the handoff comes back")
	}
}

// Hard-won: 66c7315: an intentional stop produced a false relay reconnect error
func TestARetiringSessionsReconnectFailureIsNotReported(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-destroy", 13)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	chat.sessionOpen = false
	chat.SetSessionRetiring(true)

	chat.finishSessionReconnect(client.SessionHandle{}, errors.New("session session-destroy is not managed"))

	if notice, ok := chat.Notice(); ok && strings.Contains(notice, "Could not reconnect") {
		t.Fatalf("a deliberate stop reports no reconnect failure, but the notice was %q", notice)
	}
	if chat.sessionReconnectInFlight {
		t.Fatal("a deliberate stop starts no handoff")
	}
}

// Hard-won: 79c0a8c: resume reused a sealed chat view and rejected the next prompt
func TestRetiringTheSessionFeedKeepsAClosingPhaseInPlace(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.phase = core.WorkerPhaseClosed

	if applySessionView(chat, client.ManagedSessionView{}, errors.New("session manager stopped")) {
		t.Fatal("a stopped manager retires the feed")
	}
	if chat.Phase() != core.WorkerPhaseClosed {
		t.Fatalf("phase = %q", chat.Phase())
	}
}

// TestTheComposerFooterNamesThePrefixAtEveryWidth: the composer's own row is
// where a user typing in it learns the keys, so it carries the same groups
// the dashboard's row does: the composer's own keys, then the host's prefix
// chords. A-12: the composer's footer dropped the prefix label with the
// first chord, so from 100 columns down it read `: / ? palette` — plain
// keys, as far as the reader could tell. The label must ride on the first
// chord left.
// Hard-won: 48c2a9c: narrow composer footers hid the prefix and made a chord look like a plain key
func TestTheComposerFooterNamesThePrefixAtEveryWidth(t *testing.T) {
	chat := activeTestSnapshotChat()
	chords := []string{
		"c create",
		"g sessions",
		"a read",
		"b panes",
		"q detach",
		"u web",
		": / ? palette",
	}
	for _, width := range []int{140, 100, 80} {
		text := activeBufferText(activeDraw(width, 1, func(frame *Frame) {
			footer := ChatFooter{
				Area:        frame.Area(),
				Chords:      chords,
				ChordPrefix: "ctrl+b then ",
				Functions:   []string{},
				Banner:      nil,
			}
			renderChatFooter(frame, footer, chat, true)
		}))
		groups := strings.Split(text, "│")
		if len(groups) < 2 {
			t.Fatalf("%d: no chord group in %q", width, text)
		}
		chordGroup := groups[1]
		if !strings.HasPrefix(strings.TrimLeftFunc(chordGroup, unicode.IsSpace), "ctrl+b then ") {
			t.Fatalf("%d: %q", width, text)
		}
		if !strings.Contains(text, ": / ? palette") {
			t.Fatalf("%d: %q", width, text)
		}
		if strings.Contains(text, "then:") {
			t.Fatalf("%d: %q", width, text)
		}
	}
}

// TestTheASCIISymbolSetReachesTheComposersOwnFooterHints: symbols = "ascii"
// is for a Linux console or a locale without UTF-8. The composer's own hints
// were written out with a literal middle dot joining them, so splitting on
// the glyph set's separator found nothing under the ASCII set and the dot
// reached the screen anyway. Each branch that builds those hints (idle,
// queued, and dictating) is checked here.
// Hard-won: 8dc493a: ASCII mode still emitted a Unicode middle-dot separator
func TestTheASCIISymbolSetReachesTheComposersOwnFooterHints(t *testing.T) {
	chat := activeTestSnapshotChat()
	draw := func(chat *ChatState) string {
		// Wide enough that no hint gives way to the row's width limit; a
		// dropped hint would hide the joiner this test exists to check.
		var text string
		WithSymbols(core.SymbolSetAscii, func() {
			text = activeBufferText(activeDraw(200, 1, func(frame *Frame) {
				renderChatFooter(frame, testFooter(frame.Area()), chat, true)
			}))
		})
		return text
	}
	isASCII := func(text string) bool {
		for _, character := range text {
			if character > unicode.MaxASCII {
				return false
			}
		}
		return true
	}

	idle := draw(chat)
	if !isASCII(idle) {
		t.Fatalf("%q", idle)
	}
	paste := "Ctrl-V paste"
	if runtime.GOOS == "darwin" {
		paste = "Cmd-V paste"
	}
	if !strings.Contains(idle, paste) {
		t.Fatalf("%q", idle)
	}

	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "next"))
	queuedFooter := draw(chat)
	if !isASCII(queuedFooter) {
		t.Fatalf("%q", queuedFooter)
	}
	if !strings.Contains(queuedFooter, "Ctrl-R history") {
		t.Fatalf("%q", queuedFooter)
	}
	chat.queuedPrompts = nil

	// The dictating hint keeps its own "…" (unrelated to the separator this
	// fix addresses), so only the joiner between hints is checked here.
	chat.voiceActive = true
	dictating := draw(chat)
	if strings.ContainsRune(dictating, '·') {
		t.Fatalf("%q", dictating)
	}
	if !strings.Contains(dictating, "Listening") {
		t.Fatalf("%q", dictating)
	}
}

// TestEscapeInterruptsANativeGoalTurnWithoutAnActivePrompt: a phase alone is
// not evidence of an interruptible turn.
func TestEscapeInterruptsANativeGoalTurnWithoutAnActivePrompt(t *testing.T) {
	for _, harnessStarted := range []bool{false, true} {
		fixture := client.NewReplacementSessionTestFixture("goal-interrupt", 12)
		chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
		materialized := core.MaterializedSessionEmpty("goal-interrupt")
		var goal core.GoalState
		activeUnmarshal(t, `{
			"known": true,
			"snapshot": {"objective": "finish the work", "status": "active"},
			"execution": {"version": 1, "status": "running", "turnId": "native-turn"}
		}`, &goal)
		view := managedView(materialized)
		activeEditOperational(&view, func(operational *core.RelayOperationalState) {
			operational.Goal = goal
			operational.Execution = core.RelayExecutionStateRunning
			if harnessStarted {
				operational.HarnessTurn = &core.HarnessTurn{StartedAtMs: 1}
			}
		})
		applySessionView(chat.ChatState, view, nil)
		if chat.promptInFlight {
			t.Fatal("no prompt of ours is in flight")
		}
		if !chat.sessionActivity.PursuingGoal {
			t.Fatal("the session pursues its goal")
		}
		control := promptBottomQueueControl(chat.ChatState)
		if control == nil || !strings.Contains(control.String(), "Esc interrupts") {
			t.Fatalf("queue control = %#v", control)
		}

		chat.HandleEvent(tea.KeyPressMsg{Code: tea.KeyEscape})
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		submitted, ok := fixture.Submitted.Recv(ctx)
		cancel()
		if !ok {
			t.Fatal("Esc must submit cancellation for the native turn")
		}
		if !reflect.DeepEqual(submitted, core.RelayCommandCancelTurn{}) {
			t.Fatalf("submitted %#v", submitted)
		}

		// An active goal between turns is not itself a running turn.
		chat.goalState.Execution.Status = "idle"
		chat.sessionActivity.HarnessTurnStartedAtMs = nil
		if chat.turnInterruptible() {
			t.Fatal("an idle goal is not interruptible")
		}
		if promptBottomQueueControl(chat.ChatState) != nil {
			t.Fatal("no turn control is offered")
		}
	}
}

func TestEscapeNamesSteeringThroughSubmissionAndAcceptance(t *testing.T) {
	version := func(value uint32) *uint32 { return &value }
	supports := func(value bool) *bool { return &value }
	type steeringCase struct {
		supported *bool
		queueKind core.QueuedCommandKind
		hint      string
		sending   string
		requested string
	}
	for _, protocol := range []*uint32{nil, version(16), version(17), version(40)} {
		for _, row := range []steeringCase{
			{supports(true), core.QueuedCommandKindPrompt{}, "Esc steers next", "Steering turn…", "Steering requested"},
			{supports(false), core.QueuedCommandKindPrompt{}, "Esc steers next", "Steering turn…", "Cancellation requested"},
			{nil, core.QueuedCommandKindPrompt{}, "Esc steers next", "Steering turn…", "Queued prompt requested"},
			{
				supports(true),
				core.QueuedCommandKindSetConfig{Key: "model", Value: "next-model"},
				"Esc interrupts",
				"Interrupting turn…",
				"Cancellation requested",
			},
			{supports(true), nil, "Esc interrupts", "Interrupting turn…", "Cancellation requested"},
		} {
			fixture := client.NewReplacementSessionTestFixture("steering-session", 12)
			chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
			materialized := core.MaterializedSessionEmpty("steering-session")
			targeted := protocol != nil && *protocol >= 17
			published := protocol != nil && *protocol >= 40
			steering := published &&
				(row.supported == nil || *row.supported) &&
				row.queueKind != nil && row.queueKind.IsPrompt()
			apply := published && row.queueKind != nil && !steering
			hint := "Esc interrupts"
			switch {
			case steering:
				hint = row.hint
			case apply:
				hint = "Esc applies queued"
			}
			sending := "Interrupting turn…"
			switch {
			case steering:
				sending = row.sending
			case apply:
				sending = "Applying queued input…"
			}
			var expected core.RelayCommand
			switch {
			case !targeted:
				expected = core.RelayCommandCancelTurn{}
			case steering:
				expected = core.RelayCommandSteer{ActivePromptID: "running-prompt", QueuedPromptID: "queued-correction"}
			case apply:
				expected = core.RelayCommandApplyQueuedFor{ActivePromptID: "running-prompt", QueuedCommandID: "queued-correction"}
			default:
				expected = core.RelayCommandCancelTurnFor{ActivePromptID: "running-prompt"}
			}
			if row.queueKind != nil {
				materialized.QueuedPrompts = append(materialized.QueuedPrompts, core.MaterializedQueuedPrompt{
					AcceptedOrdinal: nil,
					CommandID:       "queued-correction",
					Kind:            row.queueKind,
					Content:         []any{map[string]any{"type": "text", "text": "change direction"}},
					QueuedAtMs:      0,
				})
			}
			view := managedView(materialized)
			activeEditOperational(&view, func(operational *core.RelayOperationalState) {
				operational.RelayProtocolVersion = protocol
				operational.SteeringSupported = row.supported
				if published {
					kind := core.TurnControlKindInterrupt
					switch {
					case steering:
						kind = core.TurnControlKindSteer
					case apply:
						kind = core.TurnControlKindApplyQueued
					}
					operational.TurnControls = &core.TurnControls{
						Status:    core.TurnControlStatusReady,
						Primary:   &core.TurnControlAction{Kind: kind, Command: expected},
						Interrupt: core.RelayCommandCancelTurnFor{ActivePromptID: "running-prompt"},
						Message:   nil,
					}
				}
				operational.ActivePrompt = &core.ActiveRelayPrompt{
					CommandID:   "running-prompt",
					CreatedAtMs: 0,
					StartedAtMs: 0,
				}
			})
			applySessionView(chat.ChatState, view, nil)
			screen := strings.Join(drawnTranscript(chat.ChatState, 100, 24), "\n")
			if !strings.Contains(screen, hint) {
				t.Fatal(screen)
			}

			if published {
				action := chat.HandleKey(tea.KeyPressMsg{Code: tea.KeyEscape, Mod: tea.ModCtrl})
				want := ChatActionTurnControl{Command: core.RelayCommandCancelTurnFor{ActivePromptID: "running-prompt"}}
				if !reflect.DeepEqual(action, want) {
					t.Fatalf("Ctrl-Esc = %#v", action)
				}
			}
			previousFeedback, hadFeedback := chat.Notice()
			chat.HandleEvent(tea.KeyPressMsg{Code: tea.KeyEscape})
			if feedback, has := chat.Notice(); feedback != previousFeedback || has != hadFeedback {
				t.Fatalf("notice = %q", feedback)
			}
			pendingScreen := strings.Join(drawnTranscript(chat.ChatState, 100, 24), "\n")
			if !strings.Contains(pendingScreen, sending) {
				t.Fatal(pendingScreen)
			}

			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			var result chatRemoteResult
			for result == nil {
				received, ok := chat.remote.recv(ctx)
				if !ok {
					cancel()
					t.Fatal("turn control request completes")
				}
				if _, isCancel := received.(chatRemoteResultCancel); isCancel {
					result = received
				}
			}
			submitted, ok := fixture.Submitted.Recv(ctx)
			cancel()
			if !ok || !reflect.DeepEqual(submitted, expected) {
				t.Fatalf("submitted %#v, want %#v", submitted, expected)
			}

			// A newer view may already have consumed the queue. The reply
			// must still describe the request that was actually submitted.
			chat.queuedPrompts = nil
			applyChatRemoteResult(chat.ChatState, result)
			if feedback, has := chat.Notice(); feedback != previousFeedback || has != hadFeedback {
				t.Fatalf("notice = %q", feedback)
			}
			if _, ok := chat.operationFeedback["turn-control"]; !ok {
				t.Fatal("acceptance is not execution")
			}
			chat.HandleEvent(tea.KeyPressMsg{Code: tea.KeyEscape})
			if _, ok := tryRecv(fixture.Submitted); ok {
				t.Fatal("Escape cannot escalate while waiting for execution state")
			}
		}
	}
}

// TestComposerTitleNamesTheWorkTheAgentLeftRunning: background commands are
// exposed through the embedded task control; the composer title stays
// focused on meaningful state.
func TestComposerTitleNamesTheWorkTheAgentLeftRunning(t *testing.T) {
	nowSeconds := int64(10_000)
	startedAtMs := nowSeconds*1_000 - 2_616_000
	chat := activeTestSnapshotChat()
	if promptTitle(chat) != "" {
		t.Fatalf("title = %q", promptTitle(chat))
	}

	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{{
			ID:          "test:active",
			StartedAtMs: startedAtMs,
			Command:     "cargo   test",
			CanStop:     false,
		}},
		ActiveUserShells: []core.ActiveUserShell{},
	})
	if promptTitle(chat) != "" {
		t.Fatalf("title = %q", promptTitle(chat))
	}
	if strings.Contains(promptTitle(chat), "Background") {
		t.Fatalf("title = %q", promptTitle(chat))
	}

	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{
			{ID: "test:active-1", StartedAtMs: startedAtMs, Command: "cargo test", CanStop: false},
			{ID: "test:active-2", StartedAtMs: startedAtMs + 1_000, Command: "npm run build", CanStop: false},
		},
		ActiveUserShells: []core.ActiveUserShell{},
	})
	screen := strings.Join(drawnTranscript(chat, 100, 24), "\n")
	if !strings.Contains(screen, "Tasks (2)") {
		t.Fatal(screen)
	}

	// The spinner represents a running turn, whatever it left behind.
	chat.phase = core.WorkerPhaseRunning
	if strings.Contains(promptTitle(chat), "Running") {
		t.Fatalf("title = %q", promptTitle(chat))
	}
}

// activeColorIs reports whether a drawn cell color is the palette color.
func activeColorIs(drawn interface{ RGBA() (r, g, b, a uint32) }, want Color) bool {
	expected := want.TerminalColor()
	if drawn == nil || expected == nil {
		return drawn == nil && expected == nil
	}
	r1, g1, b1, a1 := drawn.RGBA()
	r2, g2, b2, a2 := expected.RGBA()
	return r1 == r2 && g1 == g2 && b1 == b2 && a1 == a2
}

func TestEffortSeparatorBetweenPromptChipsIsNotClickable(t *testing.T) {
	for _, palette := range core.UiThemeAll {
		WithTheme(palette, func() {
			chat := activeTestSnapshotChat()
			chat.setConfigOptions([]schema.SessionConfigOption{
				selectConfig(t, "model", "fast", []string{"fast", "slow"}),
				selectConfig(t, "effort", "high", []string{"low", "high"}),
			})
			title, _, chips := promptTitleLine(chat, uv.Rect(0, 0, 100, 3))
			chat.configChipAreas = chips

			separator := slices.IndexFunc(title.Spans, func(span Span) bool { return span.Content == "· " })
			if separator < 0 {
				t.Fatal("the effort separator is rendered")
			}
			if title.Spans[separator].Style != Muted() {
				t.Fatalf("separator style = %#v", title.Spans[separator].Style)
			}
			effortValue, _ := chat.currentEffort()
			if content := title.Spans[separator+1].Content; content != effortValue+" ▾ " {
				t.Fatalf("effort chip = %q", content)
			}
			if title.Spans[separator+1].Style != ActionableChip() {
				t.Fatalf("effort style = %#v", title.Spans[separator+1].Style)
			}

			chipArea := func(key string) uv.Rectangle {
				index := slices.IndexFunc(chat.configChipAreas, func(chip configChipArea) bool { return chip.key == key })
				if index < 0 {
					t.Fatalf("the %s chip is rendered", key)
				}
				return chat.configChipAreas[index].area
			}
			model := chipArea("model")
			effort := chipArea("effort")
			if effort.Min.X != model.Max.X+displayWidth("· ") {
				t.Fatalf("effort at %d, model ends at %d", effort.Min.X, model.Max.X)
			}
			if key, ok := chat.promptConfigChipAt(model.Max.X, model.Min.Y); ok {
				t.Fatalf("the separator dot has no click hitbox, found %q", key)
			}
			if key, ok := chat.promptConfigChipAt(model.Max.X+1, model.Min.Y); ok {
				t.Fatalf("found %q", key)
			}
			if key, ok := chat.promptConfigChipAt(model.Min.X, model.Min.Y); !ok || key != "model" {
				t.Fatalf("found %q", key)
			}
			if key, ok := chat.promptConfigChipAt(effort.Min.X, effort.Min.Y); !ok || key != "effort" {
				t.Fatalf("found %q", key)
			}
			buffer := activeDraw(100, 24, func(frame *Frame) { renderFullFrame(frame, chat, false) })
			for _, chip := range chat.configChipAreas {
				cell := buffer.CellAt(chip.area.Min.X, chip.area.Min.Y)
				if !activeColorIs(cell.Style.Fg, ThemePalette().Text) {
					t.Fatalf("%s chip foreground = %#v", chip.key, cell.Style.Fg)
				}
				if !activeColorIs(cell.Style.Bg, ThemePalette().Selection) {
					t.Fatalf("%s chip background = %#v", chip.key, cell.Style.Bg)
				}
			}
		})
	}
}

func selectConfig(t *testing.T, key, current string, values []string) schema.SessionConfigOption {
	t.Helper()
	options := make([]map[string]string, 0, len(values))
	for _, value := range values {
		options = append(options, map[string]string{"value": value, "name": value})
	}
	encoded, err := json.Marshal(map[string]any{
		"id":           key,
		"name":         key,
		"type":         "select",
		"currentValue": current,
		"options":      options,
	})
	if err != nil {
		t.Fatal(err)
	}
	var option schema.SessionConfigOption
	activeUnmarshal(t, string(encoded), &option)
	return option
}

// Hard-won: 6e7ecf3: title styling made hint descriptions bold and erased key emphasis
func TestPromptHintKeysAreBoldButDescriptionsAreNot(t *testing.T) {
	chat := activeTestSnapshotChat()
	buffer := activeDraw(100, 24, func(frame *Frame) { renderFullFrame(frame, chat, false) })
	bounds := buffer.Bounds()
	rowText := func(y int) string {
		var text strings.Builder
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			text.WriteString(activeCellSymbol(buffer, x, y))
		}
		return text.String()
	}
	promptY, promptText := -1, ""
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		if text := rowText(y); strings.Contains(text, " Enter  send ") {
			promptY, promptText = y, text
			break
		}
	}
	if promptY < 0 {
		t.Fatal("the prompt hints are rendered")
	}
	column := func(byteIndex int) int {
		return bounds.Min.X + len([]rune(promptText[:byteIndex]))
	}
	enterByte := strings.Index(promptText, "Enter")
	if enterByte < 0 {
		t.Fatal("the Enter hint is rendered")
	}
	enterColumn := column(enterByte)
	slashOffset := strings.Index(promptText[enterByte:], " / ")
	if slashOffset < 0 {
		t.Fatal("the command hint is rendered")
	}
	slashByte := slashOffset + enterByte
	slashColumn := column(slashByte) + 1
	sendOffset := strings.Index(promptText[enterByte:], "send")
	if sendOffset < 0 {
		t.Fatal("the send description is rendered")
	}
	sendColumn := column(sendOffset + enterByte)
	commandsOffset := strings.Index(promptText[slashByte:], "commands")
	if commandsOffset < 0 {
		t.Fatal("the commands description is rendered")
	}
	commandsColumn := column(commandsOffset + slashByte)

	bold := func(x int) bool { return buffer.CellAt(x, promptY).Style.Attrs&uv.AttrBold != 0 }
	for x := enterColumn; x < enterColumn+len("Enter"); x++ {
		if !bold(x) {
			t.Fatalf("Enter is bold at %d", x)
		}
		if activeColorIs(buffer.CellAt(x, promptY).Style.Bg, ThemePalette().Selection) {
			t.Fatalf("Enter has no selection background at %d", x)
		}
	}
	if !bold(slashColumn) {
		t.Fatal("/ is bold")
	}
	if activeColorIs(buffer.CellAt(slashColumn, promptY).Style.Bg, ThemePalette().Selection) {
		t.Fatal("/ has no selection background")
	}
	for _, description := range []struct {
		start int
		word  string
	}{{sendColumn, "send"}, {commandsColumn, "commands"}} {
		for x := description.start; x < description.start+len(description.word); x++ {
			if bold(x) {
				t.Fatalf("%s is not bold at %d", description.word, x)
			}
		}
	}
}

// TestTheTaskDialogClaimsOnlyThePointerOverItself: the pane a conversation
// is drawn into is the whole of what its dialogs may cover. A host with
// several panes gives each one a sub-rectangle of the terminal, so a dialog
// centred in the frame would paint over its neighbours. The background-task
// dialog used to claim every pointer position on the screen, which swallowed
// clicks on the host's own panes beside it. It now answers only for the
// rectangle it drew into.
func TestTheTaskDialogClaimsOnlyThePointerOverItself(t *testing.T) {
	pane := uv.Rect(50, 2, 48, 26)
	chat := activeTestSnapshotChat()
	chat.taskDialogOpen = true
	activeDraw(140, 40, func(frame *Frame) {
		renderIn(frame, chat, ChatRegions{
			Transcript:    uv.Rect(50, 2, 48, 19),
			Prompt:        uv.Rect(50, 21, 48, 7),
			Footer:        nil,
			Overlay:       pane,
			TitleControls: 0,
			TitleLead:     0,
			PaneFocused:   true,
		}, false, false)
	})

	bodySurface := chat.FrameSurfaces().Surface(SurfaceIdModalBody{})
	if bodySurface == nil {
		t.Fatal("the dialog registers a modal body")
	}
	body := bodySurface.Rect
	click := func(column, row int) tea.MouseClickMsg {
		return tea.MouseClickMsg{X: column, Y: row, Button: tea.MouseLeft}
	}
	if !chat.ComponentHandlesMouse(click(body.Min.X, body.Min.Y)) {
		t.Fatal("the dialog still answers for its own body")
	}
	if chat.ComponentHandlesMouse(click(2, 6)) {
		t.Fatal("a click on the host's navigator, far from the pane, is not the dialog's")
	}
	if chat.ComponentHandlesMouse(click(pane.Min.X-1, pane.Min.Y+4)) {
		t.Fatal("a click just outside the pane is not the dialog's")
	}
}

// TestComposerBorderHoldsActivityWithoutMovingTheTranscriptOrInput: a host
// that owns the rest of the frame gives the chat two rectangles; nothing it
// draws may leak outside them.
func TestComposerBorderHoldsActivityWithoutMovingTheTranscriptOrInput(t *testing.T) {
	spinnerGlyph := func(character rune) bool {
		switch character {
		case '·', '∙', '•', '●':
			return true
		}
		return false
	}
	for _, width := range []int{16, 32, 48, 80} {
		chat := activeTestSnapshotChat()
		chat.SetSpinnerStyle(core.SpinnerStylePulse)
		chat.setInput("my follow-up")
		idle := drawnTranscript(chat, width, 24)
		input := chat.FrameSurfaces().Surface(SurfaceIdPromptInput{}).Rect

		transcript := chat.FrameSurfaces().Surface(SurfaceIdTranscript{}).Rect
		chat.markPromptSubmitted("continue")
		running := drawnTranscript(chat, width, 24)
		promptTop := max(input.Min.Y-1, 0)
		promptBottom := promptTop + 1 + input.Dy()
		title := running[promptTop]
		if !strings.Contains(title, voiceButtonGlyph()) {
			t.Fatal(title)
		}
		if strings.Contains(title, "Prompt") || strings.Contains(title, "Running") {
			t.Fatal(title)
		}
		characters := []rune(title)
		spinnerWidth := 0
		lastSpinner := -1
		for index, character := range characters {
			if spinnerGlyph(character) {
				spinnerWidth++
				lastSpinner = index
			}
		}
		wantWidth := SpinnerWidth
		if width == 16 {
			wantWidth = 1
		}
		if spinnerWidth != wantWidth {
			t.Fatalf("%d spinner cells: %s", spinnerWidth, title)
		}
		if lastSpinner < 0 {
			t.Fatal("spinner frame on the prompt title")
		}
		if lastSpinner != len(characters)-3 {
			t.Fatalf("spinner occupies the upper-right title: %s", title)
		}
		// A 16-column composer has 14 cells of border, one short of the
		// whole hint; the border clips it there.
		hint := "Esc interrupt"
		if width > 16 {
			hint = "Esc interrupts"
		}
		if !strings.Contains(running[promptBottom], hint) {
			t.Fatalf("%q", running)
		}
		if running[0] != idle[0] {
			t.Fatal("the transcript keeps its full height")
		}
		if running[input.Min.Y] != idle[input.Min.Y] {
			t.Fatalf("input row moved: %q vs %q", running[input.Min.Y], idle[input.Min.Y])
		}
		if got := chat.FrameSurfaces().Surface(SurfaceIdTranscript{}).Rect; got != transcript {
			t.Fatal("activity must not consume a transcript row")
		}

		chat.phase = core.WorkerPhaseIdle
		chat.promptInFlight = false
		chat.turnStartedAtEpochSeconds = nil
		if again := drawnTranscript(chat, width, 24); !slices.Equal(again, idle) {
			t.Fatalf("%q != %q", again, idle)
		}
	}
}

// longSession is a conversation long enough that opening it converts the
// tail only.
func longSession() core.MaterializedSession {
	session := core.MaterializedSessionEmpty("session-long")
	session.Transcript = nil
	for position := uint64(1); position <= 300; position++ {
		session.Transcript = append(session.Transcript,
			agentMessageItem(fmt.Sprintf("agent:%d", position), position, fmt.Sprintf("message %d", position)))
	}
	session.AppliedEventOrdinal = 301
	return session
}

// Hard-won: 2b229f7: opening a long conversation blocked the event loop for seconds
func TestTheConvertedHistoryCompletesAChatOpenedOnItsTail(t *testing.T) {
	session := longSession()
	chat := ChatStateFromMaterializedTail(&session, nil, nil)
	pending := chat.unconvertedPrefix
	if pending <= 0 {
		t.Fatal("a long session opens on its tail")
	}
	prefix := client.MaterializedPrefixEntries(session.Transcript[:pending], session.AppliedEventOrdinal)

	rebuild := applyChatIoUpdate(chat, ChatIoUpdateTranscriptPrefix{Attempt: 1, Entries: prefix, Diffstats: nil})

	if rebuild != (PrefixRebuildNotNeeded{}) {
		t.Fatalf("rebuild = %#v", rebuild)
	}
	if chat.unconvertedPrefix != 0 {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if len(chat.entries) != len(session.Transcript) {
		t.Fatalf("%d entries", len(chat.entries))
	}
	if chat.entries[0].Text != "message 1" {
		t.Fatalf("first entry %q", chat.entries[0].Text)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
}

func TestHistoryThatNoLongerFitsTheTailIsRebuiltAndThenGivesUpWithANotice(t *testing.T) {
	session := longSession()
	chat := ChatStateFromMaterializedTail(&session, nil, nil)
	pending := chat.unconvertedPrefix
	// History from a transcript compaction rewrote: it overlaps the tail.
	stale := client.MaterializedPrefixEntries(session.Transcript[len(session.Transcript)-pending:], session.AppliedEventOrdinal)

	rebuild := applyChatIoUpdate(chat, ChatIoUpdateTranscriptPrefix{Attempt: 1, Entries: slices.Clone(stale)})

	if rebuild != (PrefixRebuildNeeded{Attempt: 2}) {
		t.Fatalf("rebuild = %#v", rebuild)
	}
	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}

	// A saved reading position in the unloaded prefix must stop waiting when
	// conversion gives up, so the reported recent history is readable.
	var saved TranscriptPosition
	activeUnmarshal(t, `{"Row": {"entry": 0, "row": 0}}`, &saved)
	chat.RestoreTranscriptPosition(saved)
	exhausted := applyChatIoUpdate(chat, ChatIoUpdateTranscriptPrefix{Attempt: maxPrefixConversionAttempts, Entries: stale})

	if exhausted != (PrefixRebuildNotNeeded{}) {
		t.Fatalf("rebuild = %#v", exhausted)
	}
	if chat.unconvertedPrefix != pending {
		t.Fatalf("unconverted prefix %d", chat.unconvertedPrefix)
	}
	if chat.anchor != (TranscriptAnchorBottom{}) {
		t.Fatalf("anchor = %#v", chat.anchor)
	}
	if notice, ok := chat.Notice(); !ok || !strings.Contains(notice, "Earlier messages") {
		t.Fatalf("giving up on the history has to be reported: %q", notice)
	}
}

func TestAFailedHistoryConversionIsReportedInsteadOfDropped(t *testing.T) {
	session := longSession()
	chat := ChatStateFromMaterializedTail(&session, nil, nil)
	var saved TranscriptPosition
	activeUnmarshal(t, `{"Row": {"entry": 0, "row": 0}}`, &saved)
	chat.RestoreTranscriptPosition(saved)

	rebuild := applyChatIoUpdate(chat, ChatIoUpdateTranscriptPrefix{Attempt: 1, Err: errors.New("worker panicked")})

	if rebuild != (PrefixRebuildNotNeeded{}) {
		t.Fatalf("rebuild = %#v", rebuild)
	}
	if chat.anchor != (TranscriptAnchorBottom{}) {
		t.Fatalf("anchor = %#v", chat.anchor)
	}
	if notice, _ := chat.Notice(); notice != "Earlier messages failed to load: worker panicked" {
		t.Fatalf("notice = %q", notice)
	}
}

func TestEarlierHistoryIgnoresClosedReadersAndAllowsRetryWithoutLosingTheDraft(t *testing.T) {
	worker := core.WorkerSnapshotSummary("history", core.WorkerPhaseIdle, 0)
	chat := NewChatState(&worker, nil)
	chat.input = "unfinished draft"
	chat.openEarlierMessages()
	retired := chat.earlier.Generation
	chat.earlierKey(tea.KeyEscape)
	applyChatIoUpdate(chat, ChatIoUpdateEarlierMessages{Generation: retired, Lines: []Line{RawLine("obsolete")}})
	if chat.earlier != nil {
		t.Fatal("a closed reader stays closed")
	}
	chat.openEarlierMessages()
	current := chat.earlier.Generation
	applyChatIoUpdate(chat, ChatIoUpdateEarlierMessages{Generation: retired, Lines: []Line{RawLine("obsolete")}})
	if len(chat.earlier.Lines) != 0 {
		t.Fatal("a retired page is ignored")
	}
	chat.earlier.Loading = true
	applyChatIoUpdate(chat, ChatIoUpdateEarlierMessages{Generation: current, Err: errors.New("database unavailable")})
	if chat.earlier.Loading {
		t.Fatal("a failed read stops loading")
	}
	if chat.earlier.Error == nil || !strings.Contains(*chat.earlier.Error, "database unavailable") {
		t.Fatalf("error = %v", chat.earlier.Error)
	}
	chat.earlierKey(tea.KeyEnter)
	if !chat.earlier.Requested {
		t.Fatal("Enter retries")
	}
	chat.handleTerminalPaste("accidental paste")
	if chat.input != "unfinished draft" {
		t.Fatalf("input = %q", chat.input)
	}
	applyChatIoUpdate(chat, ChatIoUpdateEarlierMessages{Generation: current, Lines: []Line{RawLine("old message")}})
	if !chat.earlier.Loaded {
		t.Fatal("the page loaded")
	}
	if chat.earlier.Error != nil {
		t.Fatalf("error = %q", *chat.earlier.Error)
	}
	if !reflect.DeepEqual(chat.earlier.Lines, []Line{RawLine("old message")}) {
		t.Fatalf("lines = %#v", chat.earlier.Lines)
	}
}

func TestClipboardCapabilityFollowsInitializedSessionViews(t *testing.T) {
	chat := activeTestSnapshotChat()
	if !chat.clipboardIsTextOnly() {
		t.Fatal("a fresh chat pastes text only")
	}
	view := managedView(core.MaterializedSessionEmpty("clipboard-capabilities"))
	for _, row := range []struct {
		wire     string
		textOnly bool
	}{
		{`{}`, true},
		{`{"promptCapabilities": {"image": true}}`, false},
		{`{"promptCapabilities": {"image": false}}`, true},
	} {
		activeEditOperational(&view, func(operational *core.RelayOperationalState) {
			var capabilities schema.AgentCapabilities
			activeUnmarshal(t, row.wire, &capabilities)
			operational.AgentCapabilities = &capabilities
		})
		applySessionView(chat, activeCloneView(view), nil)
		if chat.clipboardIsTextOnly() != row.textOnly {
			t.Fatalf("%s: text only = %v", row.wire, chat.clipboardIsTextOnly())
		}
	}
	activeEditOperational(&view, func(operational *core.RelayOperationalState) {
		operational.AgentCapabilities = nil
	})
	applySessionView(chat, view, nil)
	if !chat.clipboardIsTextOnly() {
		t.Fatal("no capabilities paste text only")
	}
}

func clipboardTestChat(t *testing.T) *ActiveChat {
	t.Helper()
	fixture := client.NewReplacementSessionTestFixture("clipboard-admission", 72)
	chat := activeOpenChat(t, fixture.Stopped, nil, fixture.Control, SessionHeaderIdentity{}, "")
	// Hold processing at the queue boundary so tests control completion
	// order and never write to an actual session's attachment store.
	chat.composerRuntime.attachmentTasksInFlight = maxAttachmentTasks
	return chat
}

func clipboardResult(chat *ActiveChat, content ClipboardContent) composerIoUpdate {
	return composerIoUpdateClipboard{
		generation: chat.InputGeneration(),
		target:     chat.clipboardTarget(),
		staging:    false,
		result:     content,
	}
}

// activeTestImage is chat/tests.rs's test_image: a 2x2 RGBA PNG.
func activeTestImage(t *testing.T) ClipboardImage {
	t.Helper()
	picture := image.NewNRGBA(image.Rect(0, 0, 2, 2))
	copy(picture.Pix, []byte{255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255})
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, picture); err != nil {
		t.Fatal(err)
	}
	clipboardImage, err := ClipboardImageFromPNGBase64(base64.StdEncoding.EncodeToString(encoded.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	return clipboardImage
}

// activePopAttachment takes the next queued attachment job.
func activePopAttachment(t *testing.T, chat *ActiveChat) queuedAttachment {
	t.Helper()
	if len(chat.composerRuntime.attachmentQueue) == 0 {
		t.Fatal("an attachment job is queued")
	}
	job := chat.composerRuntime.attachmentQueue[0]
	chat.composerRuntime.attachmentQueue = chat.composerRuntime.attachmentQueue[1:]
	return job
}

func TestClipboardCompletionRequiresImageSupportAndPreservesPlainText(t *testing.T) {
	chat := clipboardTestChat(t)
	if chat.promptImagesSupported {
		t.Fatal("images are not supported until advertised")
	}
	chat.applyComposerUpdate(clipboardResult(chat, ClipboardContentImage{Image: activeTestImage(t)}))
	if chat.Draft() != "" {
		t.Fatalf("draft = %q", chat.Draft())
	}
	if len(chat.composerRuntime.attachmentQueue) != 0 {
		t.Fatal("no attachment is queued")
	}
	if notice, _ := chat.Notice(); !strings.Contains(notice, "advertised image support") {
		t.Fatalf("notice = %q", notice)
	}
	chat.applyComposerUpdate(clipboardResult(chat, ClipboardContentText{Text: "ordinary text"}))
	if chat.Draft() != "ordinary text" {
		t.Fatalf("draft = %q", chat.Draft())
	}

	chat.setPromptImagesSupported(true)
	picture := activeTestImage(t)
	chat.applyComposerUpdate(clipboardResult(chat, ClipboardContentImage{Image: picture}))
	job := activePopAttachment(t, chat)
	if chat.input != "ordinary text[image 1]" {
		t.Fatalf("input = %q", chat.input)
	}
	chat.applyAttachmentResult(attachmentResult{
		sequence: job.sequence,
		fallback: attachmentFallbackFailedMarker{},
		result:   picture,
	})
	if !chat.inputImages[0].Image.Equal(&picture) {
		t.Fatal("the attachment holds the image")
	}
	if action := chat.submitInput(); !reflect.DeepEqual(action, ChatActionPrompt{Text: "ordinary text[image 1]"}) {
		t.Fatalf("submitted %#v", action)
	}
	if submitted := chat.takeSubmittingImages(); !submitted[0].Image.Equal(&picture) {
		t.Fatal("the submitted image is the attachment")
	}
}

func TestPastedImagePathsAttachAndAFailedOneReturnsAsPastedText(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(true)
	pasted := "'/shots/one.png'\n/shots/two.jpg"
	if runtime.GOOS == "windows" {
		pasted = "\"C:\\shots\\one.png\"\nC:\\shots\\two.jpg"
	}
	chat.HandleEvent(tea.PasteMsg{Content: pasted})
	if chat.input != "[image 1]\n[image 2]" {
		t.Fatalf("input = %q", chat.input)
	}
	first := activePopAttachment(t, chat)
	second := activePopAttachment(t, chat)
	// The user keeps typing while both files are read.
	chat.pastePlain(" look")

	lines := strings.Split(pasted, "\n")
	picture := activeTestImage(t)
	chat.applyAttachmentResult(attachmentResult{
		sequence: second.sequence,
		fallback: attachmentFallbackText(lines[1]),
		result:   picture,
	})
	chat.applyAttachmentResult(attachmentResult{
		sequence: first.sequence,
		fallback: attachmentFallbackText(lines[0]),
		err:      errors.New("no such file"),
	})
	if want := lines[0] + "\n[image 2] look"; chat.input != want {
		t.Fatalf("input = %q, want %q", chat.input, want)
	}
	if chat.inputCursor != len(chat.input) {
		t.Fatalf("cursor %d", chat.inputCursor)
	}
	if len(chat.inputImages) != 1 || !chat.inputImages[0].Image.Equal(&picture) {
		t.Fatalf("images = %#v", chat.inputImages)
	}
	if notice, _ := chat.Notice(); !strings.Contains(notice, "no such file") {
		t.Fatalf("notice = %q", notice)
	}
}

func TestStaleClipboardResultsDoNotCrossCapabilityOrInputContextChanges(t *testing.T) {
	for change := range 5 {
		chat := clipboardTestChat(t)
		chat.setPromptImagesSupported(true)
		pending := clipboardResult(chat, ClipboardContentImage{Image: activeTestImage(t)})
		switch change {
		case 0:
			chat.setPromptImagesSupported(false)
		case 1:
			chat.HandleKey(ctrl('r'))
		case 2:
			chat.HandleKey(tea.KeyPressMsg{Code: tea.KeyPgUp, Mod: tea.ModCtrl})
		case 3:
			chat.turnControlDialogOpen = true
		default:
			chat.HandleKey(key('x'))
		}
		before := chat.draftPayload()
		chat.applyComposerUpdate(pending)
		if after := chat.draftPayload(); !after.Equal(&before) {
			t.Fatalf("change %d: draft moved to %#v", change, after)
		}
		if len(chat.composerRuntime.attachmentQueue) != 0 {
			t.Fatalf("change %d: an attachment was queued", change)
		}
		if chat.composerRuntime.pasteInFlight {
			t.Fatalf("change %d: the paste is still in flight", change)
		}
	}
	// Hard-won: 5dc2084: reused IDs delivered clipboard text to a replacement request or reviewer role.
	for _, changedRole := range []bool{false, true} {
		chat := clipboardTestChat(t)
		request, err := core.ElicitationRequestFromACPParams("reused", json.RawMessage(`{"mode":"form", "sessionId":"clipboard", "message":"Original", "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}}`))
		if err != nil {
			t.Fatal(err)
		}
		if changedRole {
			firstReviewer := "first-reviewer"
			chat.showReviewRoleElicitation(&firstReviewer, request)
		} else {
			chat.restoreElicitation(request)
		}
		chat.selectElicitationSurface(ElicitationSurfaceNotification)
		pending := clipboardResult(chat, ClipboardContentText{Text: "stale answer"})
		chat.elicitation = nil
		if changedRole {
			otherReviewer := "other-reviewer"
			chat.showReviewRoleElicitation(&otherReviewer, request)
		} else {
			replacement := request
			replacement.Message = "Replacement"
			chat.restoreElicitation(replacement)
		}
		chat.applyComposerUpdate(pending)
		saved, err := json.Marshal(chat.elicitationDraft())
		if err != nil {
			t.Fatal(err)
		}
		if strings.Contains(string(saved), "stale answer") {
			t.Fatal(string(saved))
		}
	}
	// A second presentation is the same answer target, even if its
	// conversation redraws the earlier-message reader during the read.
	chat := clipboardTestChat(t)
	shared, err := core.ElicitationRequestFromACPParams("shared-form", json.RawMessage(`{
		"mode": "form", "sessionId": "clipboard-test", "message": "Answer", "requestedSchema": {
			"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]
		}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	chat.restoreElicitation(shared)
	chat.earlier = &EarlierMessages{}
	chat.selectElicitationSurface(ElicitationSurfaceNotification)
	// Hard-won: c9867384e: releasing a notification parked a read before it could update its form draft.
	chat.composerRuntime.pasteInFlight = true
	if !chat.HasPendingElicitationInput() {
		t.Fatal("the paste in flight belongs to the form")
	}
	pending := clipboardResult(chat, ClipboardContentText{Text: "shared answer"})
	chat.selectElicitationSurface(ElicitationSurfaceConversation)
	chat.applyComposerUpdate(pending)
	if chat.HasPendingElicitationInput() {
		t.Fatal("the paste landed")
	}
	chat.selectElicitationSurface(ElicitationSurfaceNotification)
	answer, ok := chat.HandleKey(key(tea.KeyEnter)).(ChatActionRespondElicitation)
	if !ok {
		t.Fatal("notification answer remains editable while the conversation reader is open")
	}
	want := core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"path": core.ElicitationValueString("shared answer")},
	}
	if !answer.Response.Equal(want) {
		t.Fatalf("response = %#v", answer.Response)
	}
}

func TestCapabilityLossDuringAttachmentProcessingKeepsDraftButBlocksSend(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(true)
	picture := activeTestImage(t)
	chat.applyComposerUpdate(clipboardResult(chat, ClipboardContentImage{Image: picture}))
	job := activePopAttachment(t, chat)
	chat.setPromptImagesSupported(false)
	chat.applyAttachmentResult(attachmentResult{
		sequence: job.sequence,
		fallback: attachmentFallbackFailedMarker{},
		result:   picture,
	})
	draft := chat.draftPayload()
	if action := chat.submitInput(); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("submitted %#v", action)
	}
	if after := chat.draftPayload(); !after.Equal(&draft) {
		t.Fatalf("draft = %#v", after)
	}
	chat.setPromptImagesSupported(true)
	if _, ok := chat.submitInput().(ChatActionPrompt); !ok {
		t.Fatal("the restored capability sends the prompt")
	}
	if submitted := chat.takeSubmittingImages(); !submitted[0].Image.Equal(&picture) {
		t.Fatal("the submitted image is the attachment")
	}
}

func TestClipboardErrorsKeepTheDraftAndReportTheUnderlyingFailure(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setInput("keep me")
	chat.applyComposerUpdate(composerIoUpdateClipboard{
		generation: chat.InputGeneration(),
		target:     chat.clipboardTarget(),
		staging:    false,
		err:        errors.New("clipboard provider disconnected"),
	})
	if chat.Draft() != "keep me" {
		t.Fatalf("draft = %q", chat.Draft())
	}
	notice, _ := chat.Notice()
	if !strings.Contains(notice, "clipboard provider disconnected") || !strings.Contains(notice, "only clipboard text") {
		t.Fatalf("notice = %q", notice)
	}
}

func TestOpeningAChatUsesTheInitializedImageCapability(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("clipboard-open", 1)
	view := managedView(core.MaterializedSessionEmpty("clipboard-open"))
	activeEditOperational(&view, func(operational *core.RelayOperationalState) {
		var capabilities schema.AgentCapabilities
		activeUnmarshal(t, `{"promptCapabilities": {"image": true}}`, &capabilities)
		operational.AgentCapabilities = &capabilities
	})
	fixture.ReplacementView.SendReplace(view)
	session, err := fixture.Control.Session(context.Background(), "clipboard-open")
	if err != nil {
		t.Fatal(err)
	}
	chat := activeOpenChat(t, session, nil, fixture.Control, SessionHeaderIdentity{}, "")
	if chat.clipboardIsTextOnly() {
		t.Fatal("the opened chat accepts images")
	}
}

func TestAQuestionOpenedDuringClipboardReadReceivesNoStrayContent(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(true)
	for _, content := range []ClipboardContent{
		ClipboardContentText{Text: "wrong field"},
		ClipboardContentImage{Image: activeTestImage(t)},
	} {
		pending := clipboardResult(chat, content)
		chat.restoreElicitation(activeElicitationRequest("question", "Answer"))
		if !chat.clipboardIsTextOnly() {
			t.Fatal("a question pastes text only")
		}
		chat.applyComposerUpdate(pending)
		if len(chat.inputImages) != 0 || chat.input != "" {
			t.Fatalf("input = %q, images = %d", chat.input, len(chat.inputImages))
		}
		if len(chat.composerRuntime.attachmentQueue) != 0 {
			t.Fatal("no attachment is queued")
		}
		chat.elicitation = nil
	}
}

func TestRemovedOrFailedPendingAttachmentsDoNotReappear(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(true)
	for _, result := range []attachmentResult{
		{result: activeTestImage(t)},
		{err: errors.New("invalid image encoding")},
	} {
		if !chat.reserveAttachment(1) {
			t.Fatal("the attachment is reserved")
		}
		chat.clearInput()
		result.sequence = 1
		result.fallback = attachmentFallbackFailedMarker{}
		chat.applyAttachmentResult(result)
		if len(chat.inputImages) != 0 || chat.input != "" {
			t.Fatalf("input = %q, images = %d", chat.input, len(chat.inputImages))
		}
	}
	if !chat.reserveAttachment(2) {
		t.Fatal("the attachment is reserved")
	}
	chat.applyAttachmentResult(attachmentResult{
		sequence: 2,
		fallback: attachmentFallbackFailedMarker{},
		err:      errors.New("invalid image encoding"),
	})
	if notice, _ := chat.Notice(); !strings.Contains(notice, "invalid image encoding") {
		t.Fatalf("notice = %q", notice)
	}
	if action := chat.submitInput(); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("submitted %#v", action)
	}
}

// TestAnOpenTranscriptFollowsASymbolSetChangeOnTheNextDraw: launch campaign
// finding A-15: switching Setup's symbols to ASCII left an open transcript
// drawn with `❯` until a restart, because its row cache was kept across the
// change. The next draw uses the new symbols.
// Hard-won: 987af2e: an open transcript reused Unicode rows after switching to ASCII
func TestAnOpenTranscriptFollowsASymbolSetChangeOnTheNextDraw(t *testing.T) {
	chat := activeTestSnapshotChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleUser, "hello there"))
	var unicodeRows, asciiRows []string
	WithSymbols(core.SymbolSetUnicode, func() { unicodeRows = drawnTranscript(chat, 60, 12) })
	if !slices.ContainsFunc(unicodeRows, func(row string) bool { return strings.ContainsRune(row, '❯') }) {
		t.Fatalf("%q", unicodeRows)
	}
	WithSymbols(core.SymbolSetAscii, func() { asciiRows = drawnTranscript(chat, 60, 12) })
	if slices.ContainsFunc(asciiRows, func(row string) bool { return strings.ContainsRune(row, '❯') }) {
		t.Fatalf("%q", asciiRows)
	}
}

// Hard-won: b4adff5: empty image paste read an unsupported clipboard and showed the wrong failure
func TestEmptyPasteWithoutImageSupportReportsItWithoutReadingTheClipboard(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(false)
	chat.input = "draft"
	chat.HandleEventResult(tea.PasteMsg{Content: ""})
	if chat.composerRuntime.pasteInFlight {
		t.Fatal("the clipboard must not be read")
	}
	if notice, _ := chat.Notice(); notice != imagePasteUnsupportedNotice {
		t.Fatalf("notice = %q", notice)
	}
	if chat.input != "draft" {
		t.Fatalf("input = %q", chat.input)
	}
}

func activeGoldenBuffer(chat *ChatState, width, height int) *uv.Buffer {
	return activeDraw(width, height, func(frame *Frame) { renderFullFrame(frame, chat, false) })
}

func appendActiveGoldenState(output *strings.Builder, label string, buffer *uv.Buffer, details []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, buffer.Width(), buffer.Height())
	output.WriteString(strings.Join(BufferLines(buffer), "\n"))
	output.WriteByte('\n')
	for _, detail := range details {
		fmt.Fprintln(output, detail)
	}
}

func activeGoldenRect(chat *ChatState, id SurfaceId) string {
	var rect uv.Rectangle
	if surface := chat.FrameSurfaces().Surface(id); surface != nil {
		rect = surface.Rect
	}
	return fmt.Sprintf("%s rect=(%d, %d, %d, %d)", activeSurfaceIDDebug(id), rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy())
}

// activeSurfaceIDDebug is SurfaceId's Debug: the variant name, with its
// value for the variants that carry one.
func activeSurfaceIDDebug(id SurfaceId) string {
	name := strings.TrimPrefix(fmt.Sprintf("%T", id), "chat.SurfaceId")
	value := reflect.ValueOf(id)
	if value.Kind() == reflect.Struct && value.NumField() == 0 {
		return name
	}
	return fmt.Sprintf("%s(%v)", name, id)
}

// activeOptionalSurfaceDebug is Option<SurfaceId>'s Debug.
func activeOptionalSurfaceDebug(surface *SurfaceFrame) string {
	if surface == nil {
		return "None"
	}
	return "Some(" + activeSurfaceIDDebug(surface.ID) + ")"
}

// activeRectDebug is ratatui's Rect Debug.
func activeRectDebug(rect uv.Rectangle) string {
	return fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }", rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy())
}

// activeCellColorDebug is the ratatui Color Debug of a drawn cell color: a
// cell without a color holds Color::Reset.
func activeCellColorDebug(drawn color.Color) string {
	switch drawn := drawn.(type) {
	case nil:
		return "Reset"
	case color.RGBA:
		return fmt.Sprintf("Rgb(%d, %d, %d)", drawn.R, drawn.G, drawn.B)
	case ansi.IndexedColor:
		return fmt.Sprintf("Indexed(%d)", uint8(drawn))
	case ansi.BasicColor:
		return (ColorBlack + Color(drawn)).GoString()
	}
	r, g, b, _ := drawn.RGBA()
	return fmt.Sprintf("Rgb(%d, %d, %d)", r>>8, g>>8, b>>8)
}

// activeCellModifierDebug is ratatui's Modifier Debug of a drawn cell's
// attributes, in ratatui's bit order.
func activeCellModifierDebug(style uv.Style) string {
	var names []string
	for _, flag := range []struct {
		set  bool
		name string
	}{
		{style.Attrs&uv.AttrBold != 0, "BOLD"},
		{style.Attrs&uv.AttrFaint != 0, "DIM"},
		{style.Attrs&uv.AttrItalic != 0, "ITALIC"},
		{style.Underline != 0, "UNDERLINED"},
		{style.Attrs&uv.AttrBlink != 0, "SLOW_BLINK"},
		{style.Attrs&uv.AttrRapidBlink != 0, "RAPID_BLINK"},
		{style.Attrs&uv.AttrReverse != 0, "REVERSED"},
		{style.Attrs&uv.AttrConceal != 0, "HIDDEN"},
		{style.Attrs&uv.AttrStrikethrough != 0, "CROSSED_OUT"},
	} {
		if flag.set {
			names = append(names, flag.name)
		}
	}
	if len(names) == 0 {
		return "NONE"
	}
	return strings.Join(names, " | ")
}

func activeGoldenStyle(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	return fmt.Sprintf("fg=%s bg=%s modifiers=%s",
		activeCellColorDebug(cell.Style.Fg), activeCellColorDebug(cell.Style.Bg), activeCellModifierDebug(cell.Style))
}

// activeTerminal stands for a ratatui TestBackend terminal: each draw gets
// a fresh buffer, and the cursor keeps the last position a frame showed it
// at, starting at the origin.
type activeTerminal struct {
	width, height int
	cursor        uv.Position
	buffer        *uv.Buffer
}

func (t *activeTerminal) draw(draw func(frame *Frame)) *uv.Buffer {
	t.buffer = uv.NewBuffer(t.width, t.height)
	frame := NewFrame(t.buffer)
	draw(frame)
	if position, ok := frame.CursorPosition(); ok {
		t.cursor = position
	}
	return t.buffer
}

func (t *activeTerminal) cursorDebug() string {
	return fmt.Sprintf("Ok(Position { x: %d, y: %d })", t.cursor.X, t.cursor.Y)
}

// activeLaunchingReview is a review still launching its reviewer.
func activeLaunchingReview(reviewID string, roots []string, status string) client.RuntimeReviewView {
	return client.RuntimeReviewView{
		ReviewID:       reviewID,
		Roots:          roots,
		ChildSessionID: nil,
		SessionID:      "session-review",
		Phase:          core.TurnReviewPhaseLaunchingReviewer{},
		Roles:          []core.RoleStatus{},
		Status:         status,
		Verdict:        nil,
		Questions:      []client.ReviewerQuestion{},
	}
}

func TestGoldenActiveChatRender(t *testing.T) {
	var output strings.Builder

	chat := activeTestSnapshotChat()
	chat.phase = core.WorkerPhaseIdle
	appendActiveGoldenState(&output, "empty conversation prompt", activeGoldenBuffer(chat, 100, 24), nil)
	chat.transcriptLoading = true
	appendActiveGoldenState(&output, "transcript still loading", activeGoldenBuffer(chat, 100, 24), nil)
	chat.transcriptLoading = false
	chat.unconvertedPrefix = 1
	appendActiveGoldenState(&output, "history prefix not converted", activeGoldenBuffer(chat, 100, 24), nil)
	chat.unconvertedPrefix = 0
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleUser, "Hello"))
	appendActiveGoldenState(&output, "conversation has history", activeGoldenBuffer(chat, 100, 24), nil)
	chat.entries = nil
	chat.phase = core.WorkerPhaseRunning
	appendActiveGoldenState(&output, "running conversation follow-up", activeGoldenBuffer(chat, 100, 24), nil)

	chat = activeTestSnapshotChat()
	chat.setTurnReviews([]client.RuntimeReviewView{activeLaunchingReview("", []string{}, "Review running · 2 reviewers")})
	chat.setInput("Can you inspect the changes?")
	buffer := activeGoldenBuffer(chat, 100, 24)
	appendActiveGoldenState(&output, "review running with editable composer", buffer,
		[]string{activeGoldenRect(chat, SurfaceIdPromptInput{})})

	// Several reviews at once: the footer names each by its repository.
	chat = activeTestSnapshotChat()
	chat.setTurnReviews([]client.RuntimeReviewView{
		activeLaunchingReview("review-a", []string{"/workspace/a"}, "2 reviewers running"),
		activeLaunchingReview("review-c", []string{"/workspace/c"}, "preparing the reviewer"),
	})
	chat.setInput("Anything else to review?")
	buffer = activeGoldenBuffer(chat, 100, 24)
	appendActiveGoldenState(&output, "two reviews running", buffer,
		[]string{activeGoldenRect(chat, SurfaceIdPromptInput{})})

	chat = activeTestSnapshotChat()
	chat.setInput("/")
	buffer = activeGoldenBuffer(chat, 80, 24)
	popup := chat.FrameSurfaces().Surface(SurfaceIdAutocompletePopup{})
	if popup == nil {
		t.Fatal("autocomplete popup is registered")
	}
	popupHit := chat.FrameSurfaces().SurfaceAt(popup.Rect.Min.X, popup.Rect.Max.Y-1)
	appendActiveGoldenState(&output, "autocomplete popup and hit target", buffer, []string{
		activeGoldenRect(chat, SurfaceIdTranscript{}),
		activeGoldenRect(chat, SurfaceIdPromptInput{}),
		"popup bottom hit=" + activeOptionalSurfaceDebug(popupHit),
	})

	chat = activeTestSnapshotChat()
	buffer = activeGoldenBuffer(chat, 80, 24)
	transcriptSurface := chat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("transcript is registered")
	}
	transcript := transcriptSurface.Rect
	promptSurface := chat.FrameSurfaces().Surface(SurfaceIdPromptInput{})
	if promptSurface == nil {
		t.Fatal("prompt is registered")
	}
	prompt := promptSurface.Rect
	promptHit := chat.FrameSurfaces().SurfaceAt(prompt.Min.X, prompt.Min.Y)
	appendActiveGoldenState(&output, "transcript and composer registered surfaces", buffer, []string{
		activeGoldenRect(chat, SurfaceIdTranscript{}),
		activeGoldenRect(chat, SurfaceIdPromptInput{}),
		fmt.Sprintf("transcript before prompt=%v prompt hit=%s", transcript.Max.Y <= prompt.Min.Y, activeOptionalSurfaceDebug(promptHit)),
	})

	chat = activeTestSnapshotChat()
	chat.entries = append(chat.entries, core.ChatEntryPlain(1, core.ChatRoleAgent, "UNDERLYING CHAT SENTINEL"))
	overlaid := activeElicitationRequest("question-1", "Visible dialog message")
	overlaidTitle := "Overlaid dialog"
	overlaid.Title = &overlaidTitle
	chat.elicitation = newElicitationDialog(overlaid)
	chat.taskDialogOpen = true
	buffer = activeGoldenBuffer(chat, 100, 24)
	transcriptSurface = chat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("reduced transcript is registered")
	}
	transcript = transcriptSurface.Rect
	messageSurface := chat.FrameSurfaces().Surface(SurfaceIdElicitationMessage{})
	if messageSurface == nil {
		t.Fatal("question message is registered")
	}
	message := messageSurface.Rect
	dialogHit := chat.FrameSurfaces().SurfaceAt(message.Min.X, message.Min.Y)
	lines := BufferLines(buffer)
	footer := ""
	if len(lines) > 0 {
		footer = lines[len(lines)-1]
	}
	appendActiveGoldenState(&output, "elicitation stays inside chat and leaves transcript visible", buffer, []string{
		activeGoldenRect(chat, SurfaceIdTranscript{}),
		activeGoldenRect(chat, SurfaceIdModalBody{}),
		activeGoldenRect(chat, SurfaceIdElicitationMessage{}),
		fmt.Sprintf("transcript above message=%v prompt hidden=%v message hit=%s",
			transcript.Max.Y <= message.Min.Y,
			chat.FrameSurfaces().Surface(SurfaceIdPromptInput{}) == nil,
			activeOptionalSurfaceDebug(dialogHit)),
		"footer=" + footer,
	})

	short := activeTestSnapshotChat()
	shortRequest := activeElicitationRequest("short-question", "One short question")
	customTitle := "Custom title"
	shortRequest.Title = &customTitle
	short.elicitation = newElicitationDialog(shortRequest)
	buffer = activeGoldenBuffer(short, 80, 24)
	transcriptSurface = short.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("transcript is registered")
	}
	transcript = transcriptSurface.Rect
	messageSurface = short.FrameSurfaces().Surface(SurfaceIdElicitationMessage{})
	if messageSurface == nil {
		t.Fatal("question message is registered")
	}
	message = messageSurface.Rect
	natural := short.elicitation.naturalHeight(80)
	questionHeight := 23 - transcript.Max.Y - 1
	appendActiveGoldenState(&output, "short elicitation uses natural height", buffer, []string{
		activeGoldenRect(short, SurfaceIdTranscript{}),
		activeGoldenRect(short, SurfaceIdElicitationMessage{}),
		fmt.Sprintf("natural question height=%d actual=%d transcript taller than message=%v",
			natural, questionHeight, transcript.Dy() > message.Dy()),
	})

	tall := activeTestSnapshotChat()
	tallRequest := activeElicitationRequest("tall-question", "Question message")
	tallTitle := "Tall question"
	tallDescription := "This field description is intentionally long enough to wrap into many rows when it is shown in the focused form."
	answerDescription := strings.Repeat("The focused answer description also consumes wrapped rows. ", 20)
	tallRequest.Title = &tallTitle
	tallRequest.Description = &tallDescription
	tallRequest.Fields = []core.ElicitationField{{
		ID:          "answer",
		Title:       "Answer",
		Description: &answerDescription,
		Required:    false,
		Secret:      false,
		Kind:        core.ElicitationFieldKindText{},
	}}
	tall.elicitation = newElicitationDialog(tallRequest)
	natural = tall.elicitation.naturalHeight(80)
	buffer = activeGoldenBuffer(tall, 80, 24)
	transcriptSurface = tall.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if transcriptSurface == nil {
		t.Fatal("transcript is registered")
	}
	transcript = transcriptSurface.Rect
	messageSurface = tall.FrameSurfaces().Surface(SurfaceIdElicitationMessage{})
	if messageSurface == nil {
		t.Fatal("question message is registered")
	}
	message = messageSurface.Rect
	questionHeight = 23 - transcript.Max.Y - 1
	appendActiveGoldenState(&output, "tall elicitation is capped without overlap", buffer, []string{
		activeGoldenRect(tall, SurfaceIdTranscript{}),
		activeGoldenRect(tall, SurfaceIdElicitationMessage{}),
		fmt.Sprintf("natural question height=%d actual=%d transcript before message=%v prompt hidden=%v",
			natural, questionHeight, transcript.Max.Y <= message.Min.Y,
			tall.FrameSurfaces().Surface(SurfaceIdPromptInput{}) == nil),
	})

	chat = activeTestSnapshotChat()
	shared := NewNotices()
	chat.notices = shared
	shared.Set("Background import finished")
	buffer = activeGoldenBuffer(chat, 120, 24)
	footerRow := buffer.Bounds().Max.Y - 1
	appendActiveGoldenState(&output, "shared warning notice in the composer footer", buffer, []string{
		"footer text fg=" + activeCellColorDebug(buffer.CellAt(buffer.Bounds().Min.X, footerRow).Style.Fg),
	})
	shared.Clear()
	buffer = activeGoldenBuffer(chat, 120, 24)
	appendActiveGoldenState(&output, "cleared notice restores normal footer hints", buffer, []string{
		"footer text fg=" + activeCellColorDebug(buffer.CellAt(buffer.Bounds().Min.X, footerRow).Style.Fg),
	})

	chat = activeTestSnapshotChat()
	footerTerminal := &activeTerminal{width: 200, height: 1}
	footerTerminal.draw(func(frame *Frame) { renderChatFooter(frame, testFooter(frame.Area()), chat, true) })
	appendActiveGoldenState(&output, "composer and host key hints", footerTerminal.buffer, nil)
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "next"))
	footerTerminal.draw(func(frame *Frame) { renderChatFooter(frame, testFooter(frame.Area()), chat, true) })
	appendActiveGoldenState(&output, "queued prompt keeps composer hints", footerTerminal.buffer, nil)

	chat = activeTestSnapshotChat()
	footerTerminal = &activeTerminal{width: 32, height: 1}
	footerTerminal.draw(func(frame *Frame) { renderChatFooter(frame, testFooter(frame.Area()), chat, true) })
	appendActiveGoldenState(&output, "narrow footer preserves palette and help hints", footerTerminal.buffer, nil)

	var modelAndEffort []schema.SessionConfigOption
	activeUnmarshal(t, `[
		{"id": "model", "name": "Model", "category": "model", "type": "select",
		 "currentValue": "gpt-5.6-sol", "options": [{"value": "gpt-5.6-sol", "name": "Sol"}]},
		{"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
		 "currentValue": "high", "options": [{"value": "high", "name": "High"}]}
	]`, &modelAndEffort)
	chat = activeTestSnapshotChat()
	chat.phase = core.WorkerPhaseRunning
	chat.setPromptInFlight(true)
	chat.activityReachable = false
	chat.setConfigOptions(modelAndEffort)
	appendActiveGoldenState(&output, "live model and effort during an in-flight prompt", activeGoldenBuffer(chat, 100, 24), nil)

	fast := activeTestSnapshotChat()
	fast.setConfigOptions([]schema.SessionConfigOption{fastModeOption("off")})
	appendActiveGoldenState(&output, "fast mode disabled", activeGoldenBuffer(fast, 80, 24), nil)
	fast.setConfigOptions([]schema.SessionConfigOption{fastModeOption("on")})
	appendActiveGoldenState(&output, "fast mode confirmed active", activeGoldenBuffer(fast, 80, 24), nil)
	fast.setConfigOptions(nil)
	appendActiveGoldenState(&output, "fast mode option removed", activeGoldenBuffer(fast, 80, 24), nil)

	interrupt := activeTestSnapshotChat()
	interrupt.phase = core.WorkerPhaseRunning
	interrupt.activityReachable = false
	appendActiveGoldenState(&output, "running without our prompt", activeGoldenBuffer(interrupt, 80, 24), nil)
	interrupt.setPromptInFlight(true)
	appendActiveGoldenState(&output, "our prompt can be interrupted", activeGoldenBuffer(interrupt, 80, 24), nil)

	plan := grokChat()
	plan.finishPlanModeChange(true)
	plan.phase = core.WorkerPhaseRunning
	appendActiveGoldenState(&output, "plan mode during a turn", activeGoldenBuffer(plan, 80, 24), nil)

	goalCommands := json.RawMessage(`{
		"sessionUpdate": "available_commands_update",
		"availableCommands": [{"name": "goal", "description": "set a persistent goal"}]
	}`)
	goal := activeTestSnapshotChat()
	goal.applySessionUpdate(1, goalCommands)
	goalRequest := "goal"
	goal.applyEvent(&core.SequencedEvent{
		Seq:          2,
		RecordedAtMs: nil,
		RequestID:    &goalRequest,
		Event: core.WorkerEventPromptAccepted{
			RequestID:   "goal",
			Text:        "/goal ship the release",
			Attachments: []core.Attachment{},
		},
	})
	goal.activityReachable = false
	appendActiveGoldenState(&output, "advertised active goal title", activeGoldenBuffer(goal, 100, 24), nil)
	goal.applyEvent(&core.SequencedEvent{
		Seq:          3,
		RecordedAtMs: nil,
		RequestID:    nil,
		Event:        core.WorkerEventTurnCompleted{},
	})
	appendActiveGoldenState(&output, "completed goal clears composer title", activeGoldenBuffer(goal, 100, 24), nil)

	ordinary := activeTestSnapshotChat()
	ordinary.markPromptSubmitted("/goal ship the release")
	ordinary.setPromptInFlight(false)
	appendActiveGoldenState(&output, "unadvertised goal text stays an ordinary prompt", activeGoldenBuffer(ordinary, 100, 24), nil)
	ordinary.applySessionUpdate(1, goalCommands)
	ordinary.markPromptSubmitted("please ship the release")
	ordinary.setPromptInFlight(false)
	appendActiveGoldenState(&output, "ordinary text with goal advertised", activeGoldenBuffer(ordinary, 100, 24), nil)

	title := activeTestSnapshotChat()
	buffer = activeGoldenBuffer(title, 80, 24)
	// Rust takes the byte offset of the title in the row as its column.
	titleColumn := strings.Index(BufferLines(buffer)[0], "Conversation")
	if titleColumn < 0 {
		t.Fatal("conversation title is visible")
	}
	ruleColumn := -1
	for x := buffer.Bounds().Max.X - 1; x >= buffer.Bounds().Min.X; x-- {
		if activeCellSymbol(buffer, x, 0) == "─" {
			ruleColumn = x
			break
		}
	}
	if ruleColumn < 0 {
		t.Fatal("quiet rule follows the title")
	}
	appendActiveGoldenState(&output, "conversation title and quiet rule", buffer, []string{
		"title style " + activeGoldenStyle(buffer, titleColumn, 0),
		"rule style " + activeGoldenStyle(buffer, ruleColumn, 0),
	})

	for _, uiTheme := range core.UiThemeAll {
		controls := activeTestSnapshotChat()
		controls.SetSubagentCount(2)
		controls.SetReviewerCounts(ReviewerCounts{Total: 1, Open: 1})
		controls.setSessionActivity(client.SessionActivity{
			BackgroundCommands: []core.BackgroundCommand{{
				ID:          "test:task",
				StartedAtMs: 0,
				Command:     "cargo test",
				CanStop:     false,
			}},
			ActiveUserShells: []core.ActiveUserShell{},
		})
		controls.activityReachable = false
		var neutral *uv.Buffer
		WithTheme(uiTheme, func() { neutral = activeGoldenBuffer(controls, 100, 24) })
		if controls.subagentControlArea == nil {
			t.Fatal("subagent control is rendered")
		}
		subagent := *controls.subagentControlArea
		if controls.taskControlArea == nil {
			t.Fatal("task control is rendered")
		}
		task := *controls.taskControlArea
		label := func(buffer *uv.Buffer, area uv.Rectangle) string {
			var text strings.Builder
			for x := area.Min.X; x < area.Max.X; x++ {
				text.WriteString(activeCellSymbol(buffer, x, area.Min.Y))
			}
			return text.String()
		}
		neutralStyles := fmt.Sprintf("subagents=%s %s tasks=%s %s",
			core.RustDebugString(label(neutral, subagent)),
			activeGoldenStyle(neutral, subagent.Min.X, subagent.Min.Y),
			core.RustDebugString(label(neutral, task)),
			activeGoldenStyle(neutral, task.Min.X, task.Min.Y))
		appendActiveGoldenState(&output, uiTheme.Label()+" theme: neutral navigation controls", neutral, []string{neutralStyles})

		focus := PromptControlSubagents
		controls.promptControlFocus = &focus
		var focused *uv.Buffer
		WithTheme(uiTheme, func() { focused = activeGoldenBuffer(controls, 100, 24) })
		focusedStyles := fmt.Sprintf("subagents %s tasks %s",
			activeGoldenStyle(focused, subagent.Min.X, subagent.Min.Y),
			activeGoldenStyle(focused, task.Min.X, task.Min.Y))
		appendActiveGoldenState(&output, uiTheme.Label()+" theme: focused navigation controls", focused, []string{focusedStyles})
	}

	pane := uv.Rect(50, 2, 48, 26)
	regions := func() ChatRegions {
		return ChatRegions{
			Transcript:    uv.Rect(50, 2, 48, 19),
			Prompt:        uv.Rect(50, 21, 48, 7),
			Footer:        nil,
			Overlay:       pane,
			TitleControls: 0,
			TitleLead:     0,
			PaneFocused:   true,
		}
	}
	tasks := activeTestSnapshotChat()
	tasks.taskDialogOpen = true
	overlayTerminal := &activeTerminal{width: 140, height: 40}
	taskBuffer := overlayTerminal.draw(func(frame *Frame) { renderIn(frame, tasks, regions(), false, false) })
	taskBodySurface := tasks.FrameSurfaces().Surface(SurfaceIdModalBody{})
	if taskBodySurface == nil {
		t.Fatal("task dialog body is registered")
	}
	taskBody := taskBodySurface.Rect
	appendActiveGoldenState(&output, "task dialog stays in host overlay", taskBuffer, []string{
		fmt.Sprintf("modal body=(%d,%d,%d,%d)", taskBody.Min.X, taskBody.Min.Y, taskBody.Dx(), taskBody.Dy()),
	})

	regionsChat := activeTestSnapshotChat()
	regionsTerminal := &activeTerminal{width: 80, height: 24}
	customRegions := ChatRegions{
		Transcript:    uv.Rect(0, 4, 80, 12),
		Prompt:        uv.Rect(0, 16, 80, 5),
		Footer:        nil,
		Overlay:       uv.Rect(0, 0, 80, 24),
		TitleControls: 0,
		TitleLead:     0,
		PaneFocused:   false,
	}
	regionsBuffer := regionsTerminal.draw(func(frame *Frame) { renderIn(frame, regionsChat, customRegions, true, false) })
	regionTranscript := regionsChat.FrameSurfaces().Surface(SurfaceIdTranscript{})
	if regionTranscript == nil {
		t.Fatal("custom transcript is registered")
	}
	regionPrompt := regionsChat.FrameSurfaces().Surface(SurfaceIdPromptInput{})
	if regionPrompt == nil {
		t.Fatal("custom prompt is registered")
	}
	appendActiveGoldenState(&output, "transcript and prompt stay within host regions", regionsBuffer, []string{
		"transcript rect=" + activeRectDebug(regionTranscript.Rect),
		"prompt rect=" + activeRectDebug(regionPrompt.Rect),
	})

	cursor := activeTestSnapshotChat()
	cursorFooter := testFooter(uv.Rect(0, 22, 80, 1))
	cursorRegions := ChatRegions{
		Transcript:    uv.Rect(0, 0, 80, 16),
		Prompt:        uv.Rect(0, 16, 80, 6),
		Footer:        &cursorFooter,
		Overlay:       uv.Rect(0, 0, 80, 24),
		TitleControls: 0,
		TitleLead:     0,
		PaneFocused:   false,
	}
	cursorTerminal := &activeTerminal{width: 80, height: 24}
	unfocused := cursorTerminal.draw(func(frame *Frame) { renderIn(frame, cursor, cursorRegions, false, false) })
	appendActiveGoldenState(&output, "composer without keyboard focus", unfocused, []string{"cursor=" + cursorTerminal.cursorDebug()})
	focusedComposer := cursorTerminal.draw(func(frame *Frame) { renderIn(frame, cursor, cursorRegions, true, false) })
	appendActiveGoldenState(&output, "composer with keyboard focus", focusedComposer, []string{"cursor=" + cursorTerminal.cursorDebug()})

	core.AssertPlatformGolden(t, "testdata", "active-chat-render", output.String())
}

// activeTestReferenceImage is chat/tests.rs's test_reference_image: the
// test image installed as an attachment reference.
func activeTestReferenceImage(t *testing.T) ClipboardImage {
	t.Helper()
	source := activeTestImage(t)
	decoded, err := base64.StdEncoding.DecodeString(source.DataBase64)
	if err != nil {
		t.Fatal(err)
	}
	reference, err := core.NewAttachmentRef(decoded, source.MimeType, 2, 2)
	if err != nil {
		t.Fatal(err)
	}
	image, err := ClipboardImageFromReference(reference)
	if err != nil {
		t.Fatal(err)
	}
	return image
}

// activeVersionZeroConfig is `serde_json::from_str(r#"{"version":0}"#)`.
func activeVersionZeroConfig(t *testing.T) core.Config {
	t.Helper()
	var config core.Config
	activeUnmarshal(t, `{"version":0}`, &config)
	return config
}

// activeSendOperation queues one operation for the chat's background worker,
// as Rust's bounded `operations().send(..).await` does.
func activeSendOperation(t *testing.T, chat *ActiveChat, operation chatRemoteOperation) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for {
		if permit, ok := chat.remote.operations().tryReserve(); ok {
			permit.send(operation)
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("the operation queue stays full")
		}
		time.Sleep(time.Millisecond)
	}
}

func TestClipboardAndAttachmentResultsFollowTheLiveComposerAcrossRegistrationAndAttach(t *testing.T) {
	config := activeVersionZeroConfig(t)
	launch := ChatStateStandby("launching", &config, SessionHeaderIdentity{}, NewNotices())
	launch.BindComposerSession(nil)
	// Hold the installation boundary; a launch must not install in the
	// placeholder store.
	launch.composerRuntime.attachmentTasksInFlight = maxAttachmentTasks
	destination := launch.composerRuntime.tx
	read := composerIoUpdateClipboard{
		generation: launch.InputGeneration(),
		target:     launch.clipboardTarget(),
		staging:    true,
		result:     ClipboardContentImage{Image: activeTestReferenceImage(t)},
	}
	launch.beginSubmission("accepted-before-parking", SubmissionKindPrompt, PromptPayloadText("already accepted"), "Sending")
	launch.AdoptSessionID("test-clipboard-owner")
	saved := launch.Draft()
	unrelated := clipboardTestChat(t)
	unrelated.setInput("another session")
	attached := clipboardTestChat(t)
	composer := launch.TakeLiveComposer()
	if composer == nil {
		t.Fatal("the launch hands over its composer")
	}
	attached.AdoptLiveComposer(*composer)
	if attached.Draft() != saved {
		t.Fatalf("saved pending-submission metadata travels with the composer: %q != %q", attached.Draft(), saved)
	}
	// The worker still has not advertised images: accepted staging work
	// survives this handoff.
	if attached.promptImagesSupported {
		t.Fatal("images are not advertised yet")
	}
	if err := destination.Send(read); err != nil {
		t.Fatal(err)
	}
	attached.DrainComposer()
	if attached.input != "[image 1]" {
		t.Fatalf("input = %q", attached.input)
	}
	if unrelated.input != "another session" {
		t.Fatalf("unrelated input = %q", unrelated.input)
	}
	job := activePopAttachment(t, attached)
	picture := activeTestReferenceImage(t)
	if err := destination.Send(composerIoUpdateAttachmentFinished(attachmentResult{
		sequence: job.sequence,
		fallback: attachmentFallbackFailedMarker{},
		result:   picture,
	})); err != nil {
		t.Fatal(err)
	}
	attached.DrainComposer()
	if !attached.inputImages[0].Image.Equal(&picture) {
		t.Fatal("the attachment holds the image")
	}
	if action := attached.submitInput(); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("submitted %#v", action)
	}
	if attached.input != "[image 1]" {
		t.Fatalf("unsupported worker retains the draft: %q", attached.input)
	}
	attached.setPromptImagesSupported(true)
	if _, ok := attached.submitInput().(ChatActionPrompt); !ok {
		t.Fatal("a supporting worker sends the prompt")
	}
	if submitted := attached.takeSubmittingImages(); !submitted[0].Image.Equal(&picture) {
		t.Fatal("the submitted image is the attachment")
	}
}

func TestPendingImageSubmissionAndDeletedMarkersSurviveALiveComposerTransfer(t *testing.T) {
	config := activeVersionZeroConfig(t)
	standby := ChatStateStandby("test-transfer", &config, SessionHeaderIdentity{}, NewNotices())
	standby.composerRuntime.attachmentTasksInFlight = maxAttachmentTasks
	standby.handleClipboardContent(ClipboardContentText{Text: "describe "})
	if !standby.reserveAttachment(17) {
		t.Fatal("the attachment is reserved")
	}
	if action := standby.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("Enter = %#v", action)
	}
	destination := standby.composerRuntime.tx
	receiver := ChatStateStandby("test-transfer", &config, SessionHeaderIdentity{}, NewNotices())
	receiver.AdoptLiveComposer(standby.TakeLiveComposer())
	if err := destination.Send(composerIoUpdateAttachmentFinished(attachmentResult{
		sequence: 17,
		fallback: attachmentFallbackFailedMarker{},
		result:   activeTestReferenceImage(t),
	})); err != nil {
		t.Fatal(err)
	}
	drained, ok := receiver.DrainComposer()
	if !ok || drained.Payload.Text != "describe [image 1]" {
		t.Fatalf("drained %#v", drained)
	}
	latest, ok := receiver.LatestStartupPrompt()
	if !ok {
		t.Fatal("the startup prompt is queued")
	}
	if len(latest.Payload.Images) != 1 {
		t.Fatalf("%d images", len(latest.Payload.Images))
	}
	recalled := receiver.HandleKey(key(tea.KeyUp))
	if removal, ok := recalled.(ChatActionRemoveQueuedPrompt); !ok || removal.ID != latest.CommandID {
		t.Fatalf("Up = %#v", recalled)
	}
	if payload := receiver.draftPayload(); !payload.Equal(&latest.Payload) {
		t.Fatalf("draft = %#v", payload)
	}

	receiver.HandleKey(ctrl('a'))
	receiver.HandleKey(ctrl('k'))
	if !receiver.reserveAttachment(18) {
		t.Fatal("the attachment is reserved")
	}
	receiver.clearInput()
	attached := clipboardTestChat(t)
	attached.setInput("old warm text")
	attached.AdoptLiveComposer(*receiver.TakeLiveComposer())
	if err := destination.Send(composerIoUpdateAttachmentFinished(attachmentResult{
		sequence: 18,
		fallback: attachmentFallbackFailedMarker{},
		result:   activeTestReferenceImage(t),
	})); err != nil {
		t.Fatal(err)
	}
	attached.DrainComposer()
	if attached.Draft() != "" {
		t.Fatalf("empty authoritative composer must beat old warm text: %q", attached.Draft())
	}
}

func TestRefusedStartupImagesMergeWithoutRetiringNewerPendingAttachmentJobs(t *testing.T) {
	chat := clipboardTestChat(t)
	chat.setPromptImagesSupported(true)
	if !chat.reserveAttachment(12) {
		t.Fatal("the attachment is reserved")
	}
	currentNumber := chat.inputImages[0].Number
	refused := StartupPrompt{
		MoveOperationID: nil,
		CommandID:       "refused-image",
		Payload:         PromptPayloadWithImage("earlier ", activeTestImage(t)),
	}
	if err := chat.RestoreStartupPrompt(refused); err != nil {
		t.Fatal(err)
	}
	if len(chat.inputImages) != 2 {
		t.Fatalf("%d images", len(chat.inputImages))
	}
	if number, ok := chat.pendingAttachmentMarkers[12]; !ok || number != currentNumber {
		t.Fatalf("marker = %d, %v", number, ok)
	}
	picture := activeTestImage(t)
	chat.applyAttachmentResult(attachmentResult{
		sequence: 12,
		fallback: attachmentFallbackFailedMarker{},
		result:   picture,
	})
	if !chat.inputImages[0].Image.Equal(&picture) {
		t.Fatal("the pending job landed on its own marker")
	}
	if chat.inputImages[0].Image.isPending() {
		t.Fatal("the landed image is no longer pending")
	}
}

func TestLatePromptRefusalFromAParkedWorkerViewReachesTheLiveComposer(t *testing.T) {
	workerView := clipboardTestChat(t)
	workerView.beginSubmission("parked-command", SubmissionKindPrompt, PromptPayloadText("old prompt"), "Sending")
	workerView.setInput("newer text")
	config := activeVersionZeroConfig(t)
	standby := ChatStateStandby("test-parked-result", &config, SessionHeaderIdentity{}, NewNotices())
	standby.AdoptLiveComposer(workerView.TakeLiveComposer())
	if workerView.HasLiveComposer() {
		t.Fatal("the worker view no longer owns its composer")
	}
	workerView.applyRemoteResult(chatRemoteResultPrompt{
		CommandID: "parked-command",
		Text:      "old prompt",
		Images:    []PromptImage{},
		Err:       errors.New("worker refused before move"),
	})
	standby.DrainComposer()
	if workerView.Draft() != "" {
		t.Fatalf("worker view draft = %q", workerView.Draft())
	}
	if draft := standby.Draft(); !strings.Contains(draft, "old prompt") || !strings.Contains(draft, "newer text") {
		t.Fatalf("standby draft = %q", draft)
	}
	if len(standby.pendingSubmissions) != 0 {
		t.Fatalf("%d pending submissions", len(standby.pendingSubmissions))
	}
}

// sessionPromptsCall is one listing of the session's stored prompts that a
// test answers.
type sessionPromptsCall chan sessionPromptsAnswer

type sessionPromptsAnswer struct {
	entries []core.PromptHistoryEntry
	err     error
}

// delayedSubmit is one submitted command and the channel its test answers
// with the accepted ordinal or the failure.
type delayedSubmit struct {
	command core.RelayCommand
	reply   chan delayedSubmitAnswer
}

type delayedSubmitAnswer struct {
	ordinal uint64
	err     error
}

// delayedNativeSession is a session whose submissions wait for the test to
// answer them, and whose prompt listings can be answered by the test.
type delayedNativeSession struct {
	delegate     client.SessionHandle
	replies      *core.Unbounded[delayedSubmit]
	submittedIDs *core.Unbounded[string]
	// history, when set, takes session-prompt listings instead of the
	// delegate.
	history *core.Unbounded[sessionPromptsCall]
}

func (s *delayedNativeSession) ToolCallDetail(context.Context, string) (core.ToolCallDetailResult, error) {
	return nil, nil
}

func (s *delayedNativeSession) CloneBox() client.SessionHandleBackend {
	clone := *s
	clone.delegate = s.delegate.Clone()
	return &clone
}

func (s *delayedNativeSession) SessionID() string { return s.delegate.SessionID() }

func (s *delayedNativeSession) View() client.ManagedSessionView { return s.delegate.View() }

func (s *delayedNativeSession) IsStopped() bool { return false }

func (s *delayedNativeSession) HasChanged() (bool, error) { return s.delegate.HasChanged() }

func (s *delayedNativeSession) Changed(ctx context.Context) (client.ManagedSessionView, error) {
	return s.delegate.Changed(ctx)
}

func (s *delayedNativeSession) SearchPrompts(ctx context.Context, bundle string, scope core.HistoryScope, query string, after *core.HistoryCursor, limit int) (core.PromptHistoryPage, error) {
	return s.delegate.SearchPrompts(ctx, bundle, scope, query, after, limit)
}

func (s *delayedNativeSession) SessionPrompts(ctx context.Context) ([]core.PromptHistoryEntry, error) {
	if s.history == nil {
		return s.delegate.SessionPrompts(ctx)
	}
	reply := make(sessionPromptsCall, 1)
	if err := s.history.Send(reply); err != nil {
		return nil, errors.New("test history closed")
	}
	select {
	case answer := <-reply:
		return answer.entries, answer.err
	case <-ctx.Done():
		return nil, errors.New("test history reply dropped")
	}
}

func (s *delayedNativeSession) ConfigResult(context.Context, string) (*string, bool, error) {
	return nil, true, nil
}

func (s *delayedNativeSession) EnqueueSubmit(ctx context.Context, commandID string, command core.RelayCommand) (client.PendingRelaySubmit, error) {
	if s.submittedIDs != nil {
		if err := s.submittedIDs.Send(commandID); err != nil {
			return client.PendingRelaySubmit{}, errors.New("test identity observer closed")
		}
	}
	reply := make(chan delayedSubmitAnswer, 1)
	if err := s.replies.Send(delayedSubmit{command: command, reply: reply}); err != nil {
		return client.PendingRelaySubmit{}, errors.New("test observer closed")
	}
	return client.NewPendingRelaySubmit(func(ctx context.Context) (uint64, error) {
		select {
		case answer := <-reply:
			return answer.ordinal, answer.err
		case <-ctx.Done():
			return 0, errors.New("test reply closed")
		}
	}), nil
}

func (s *delayedNativeSession) EnqueueSync(ctx context.Context) (client.PendingRelaySync, error) {
	return s.delegate.EnqueueSync(ctx)
}

func (s *delayedNativeSession) RespondElicitation(ctx context.Context, id string, response core.ElicitationResponse) error {
	return s.delegate.RespondElicitation(ctx, id, response)
}

func (s *delayedNativeSession) StopBackgroundTask(ctx context.Context, id string) error {
	return s.delegate.StopBackgroundTask(ctx, id)
}

func (s *delayedNativeSession) Reviewer(ctx context.Context, _ *string, action client.ReviewerAction) (client.ReviewerOutcome, error) {
	return s.delegate.Reviewer(ctx, action)
}

// activeRecv receives from a test feed within limit.
func activeRecv[T any](t *testing.T, feed *core.Unbounded[T], limit time.Duration, failure string) T {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), limit)
	defer cancel()
	value, ok := feed.Recv(ctx)
	if !ok {
		t.Fatal(failure)
	}
	return value
}

func TestNativeAcknowledgementsSurviveDroppingTheViewThatSubmittedThem(t *testing.T) {
	for _, plan := range []bool{false, true} {
		for _, unconfirmed := range []bool{false, true} {
			fixture := client.NewReplacementSessionTestFixture("test-native-transfer", 1)
			view := managedView(core.MaterializedSessionEmpty("test-native-transfer"))
			activeEditOperational(&view, func(operational *core.RelayOperationalState) {
				var capabilities schema.AgentCapabilities
				activeUnmarshal(t, `{"promptCapabilities":{"image":true}}`, &capabilities)
				operational.AgentCapabilities = &capabilities
			})
			fixture.ReplacementView.SendReplace(view)
			delegate, err := fixture.Control.Session(context.Background(), "test-native-transfer")
			if err != nil {
				t.Fatal(err)
			}
			observed := core.NewUnbounded[delayedSubmit]()
			session := client.NewSessionHandle(&delayedNativeSession{delegate: delegate, replies: observed})
			source := ActiveChatOpen(session, "bundle", nil, fixture.Control, SessionHeaderIdentity{}, "", NewNotices())
			decoded, err := base64.StdEncoding.DecodeString(activeTestImage(t).DataBase64)
			if err != nil {
				t.Fatal(err)
			}
			reference, err := core.NewAttachmentRef(decoded, "image/png", 2, 2)
			if err != nil {
				t.Fatal(err)
			}
			picture, err := ClipboardImageFromReference(reference)
			if err != nil {
				t.Fatal(err)
			}
			payload := PromptPayloadWithImage("original ", picture)
			commandID := "native-image"
			if plan {
				payload = PromptPayloadText("/plan original prompt")
				commandID = "native-plan-prompt"
			}
			source.beginSubmission(commandID, SubmissionKindPrompt, payload, "Sending")
			if plan {
				followup := "original prompt"
				activeSendOperation(t, source, chatRemoteOperationPlanCommand{
					CommandID:       "native-plan",
					Original:        payload.Text,
					Control:         core.PlanControlSetConfig{Key: "mode", Value: "plan"},
					RequestedActive: true,
					Prompt:          &followup,
				})
				mode := activeRecv(t, observed, 2*time.Second, "mode command reached the session")
				mode.reply <- delayedSubmitAnswer{ordinal: 2}
			} else {
				activeSendOperation(t, source, chatRemoteOperationPrompt{
					CommandID: commandID,
					Text:      payload.Text,
					Images:    slices.Clone(payload.Images),
				})
			}
			submitted := activeRecv(t, observed, 2*time.Second, "prompt reached the session")
			if _, ok := submitted.command.(core.RelayCommandPrompt); !ok {
				t.Fatalf("submitted %#v", submitted.command)
			}
			config := activeVersionZeroConfig(t)
			owner := ChatStateStandby("test-native-transfer", &config, SessionHeaderIdentity{}, NewNotices())
			owner.AdoptLiveComposer(source.TakeLiveComposer())
			source.Close()
			if unconfirmed {
				submitted.reply <- delayedSubmitAnswer{err: client.DeliveryUnconfirmed{}}
			} else {
				submitted.reply <- delayedSubmitAnswer{err: errors.New("definite worker refusal")}
			}
			for {
				result := activeRecv(t, owner.composerRuntime.rx, 2*time.Second, "the composer receives the result")
				done := false
				if remote, ok := result.(composerIoUpdateRemote); ok {
					switch remote.result.(type) {
					case chatRemoteResultPrompt, chatRemoteResultPlanCommand,
						chatRemoteResultDeliveryUnconfirmed, chatRemoteResultFollowupUnconfirmed:
						done = true
					}
				}
				owner.applyComposerUpdate(result)
				if done {
					break
				}
			}
			if unconfirmed {
				if owner.input != "" {
					t.Fatalf("unknown acceptance must not create a resendable draft: %q", owner.input)
				}
				if !strings.Contains(owner.Draft(), "Delivery unconfirmed") {
					t.Fatalf("draft = %q", owner.Draft())
				}
			} else {
				if owner.input != payload.Text {
					t.Fatalf("input = %q", owner.input)
				}
				if len(owner.pendingSubmissions) != 0 {
					t.Fatalf("%d pending submissions", len(owner.pendingSubmissions))
				}
				if !plan && !owner.inputImages[0].Image.Equal(&picture) {
					t.Fatal("the refused image returns to the composer")
				}
			}
		}
	}
}

func TestDeferredMovePromptsKeepTheirOriginalOperationThroughAttachAndCancel(t *testing.T) {
	config := activeVersionZeroConfig(t)
	origin := func(generation uint64, id string) *ComposerMoveOrigin {
		if id == "" {
			return &ComposerMoveOrigin{Generation: generation}
		}
		return &ComposerMoveOrigin{Generation: generation, ID: &id}
	}
	moving := ChatStateStandby("test-origin", &config, SessionHeaderIdentity{}, NewNotices())
	moving.SetComposerMoveOrigin(origin(1, "move-a"))
	moving.composerRuntime.attachmentTasksInFlight = maxAttachmentTasks
	if !moving.reserveAttachment(1) {
		t.Fatal("the attachment is reserved")
	}
	if action := moving.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("Enter = %#v", action)
	}
	attached := clipboardTestChat(t)
	attached.AdoptLiveComposer(*moving.TakeLiveComposer())
	attached.SetComposerMoveOrigin(origin(2, "move-b"))
	attached.applyAttachmentResult(attachmentResult{
		sequence: 1,
		fallback: attachmentFallbackFailedMarker{},
		result:   activeTestReferenceImage(t),
	})
	attached.drain(context.Background())
	prepared := attached.TakePreparedStartupPrompts()
	if len(prepared) == 0 {
		t.Fatal("the deferred prompt was prepared")
	}
	prompt := prepared[len(prepared)-1]
	if prompt.MoveOperationID == nil || *prompt.MoveOperationID != "move-a" {
		t.Fatalf("move operation = %v", prompt.MoveOperationID)
	}
	if len(prompt.Payload.Images) != 1 {
		t.Fatalf("%d images", len(prompt.Payload.Images))
	}

	preparing := ChatStateStandby("test-preparing", &config, SessionHeaderIdentity{}, NewNotices())
	preparing.SetComposerMoveOrigin(origin(3, ""))
	preparing.Paste("do not send toward the source")
	if action := preparing.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("Enter = %#v", action)
	}
	if _, ok := preparing.DrainComposer(); ok {
		t.Fatal("nothing is sent while the Move is preparing")
	}
	preparing.KeepDeferredSubmissionAsDraft()
	preparing.SetComposerMoveOrigin(nil)
	if _, ok := preparing.DrainComposer(); ok {
		t.Fatal("a kept draft is not sent")
	}
	if preparing.Draft() != "do not send toward the source" {
		t.Fatalf("draft = %q", preparing.Draft())
	}
	if _, ok := preparing.HandleKey(key(tea.KeyEnter)).(ChatActionPrompt); !ok {
		t.Fatal("an ordinary Enter sends")
	}
	latest, ok := preparing.LatestStartupPrompt()
	if !ok || latest.MoveOperationID != nil {
		t.Fatal("a later ordinary Enter must not inherit a failed Move identity")
	}
	unresolved := ChatStateStandby("test-unresolved", &config, SessionHeaderIdentity{}, NewNotices())
	unresolved.SetComposerMoveOrigin(origin(4, ""))
	unresolved.Paste("finish on the original destination")
	unresolved.HandleKey(key(tea.KeyEnter))
	unresolved.SetComposerMoveOrigin(nil)
	completed := "completed-move"
	unresolved.SettleDeferredMoveOrigin(4, &completed, true)
	drained, ok := unresolved.DrainComposer()
	if !ok || drained.MoveOperationID == nil || *drained.MoveOperationID != "completed-move" {
		t.Fatalf("drained %#v", drained)
	}

	unresolved.SetComposerMoveOrigin(origin(5, ""))
	unresolved.Paste("do not rebind this deferred prompt")
	unresolved.HandleKey(key(tea.KeyEnter))
	unresolved.SetComposerMoveOrigin(origin(6, "different-move"))
	if _, ok := unresolved.DrainComposer(); ok {
		t.Fatal("a deferred prompt is not rebound to another Move")
	}
	cancelled := "cancelled-original"
	unresolved.SettleDeferredMoveOrigin(5, &cancelled, false)
	if _, ok := unresolved.DrainComposer(); ok {
		t.Fatal("a cancelled Move sends nothing")
	}
	if unresolved.Draft() != "do not rebind this deferred prompt" {
		t.Fatalf("draft = %q", unresolved.Draft())
	}
}

// activeDrainUntil drains the chat until done reports true, within limit.
func activeDrainUntil(t *testing.T, chat *ActiveChat, limit time.Duration, done func() bool, failure string) {
	t.Helper()
	deadline := time.Now().Add(limit)
	for {
		chat.drain(context.Background())
		if done() {
			return
		}
		if time.Now().After(deadline) {
			t.Fatal(failure)
		}
		runtime.Gosched()
		time.Sleep(time.Millisecond)
	}
}

// Hard-won: 747d8ed78: a prompt sent to an open session from the web never reached Up
func TestAnAcceptedPromptListsTheSessionsPromptsAgainForUp(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("history-refresh", 1)
	fixture.ReplacementView.SendReplace(managedView(core.MaterializedSessionEmpty("history-refresh")))
	delegate, err := fixture.Control.Session(context.Background(), "history-refresh")
	if err != nil {
		t.Fatal(err)
	}
	listings := core.NewUnbounded[sessionPromptsCall]()
	session := client.NewSessionHandle(&delayedNativeSession{
		delegate: delegate,
		replies:  core.NewUnbounded[delayedSubmit](),
		history:  listings,
	})
	chat := activeOpenChat(t, session, nil, fixture.Control, SessionHeaderIdentity{}, "")
	entry := func(id int64, text string) core.PromptHistoryEntry {
		return core.PromptHistoryEntry{ID: id, SessionID: "history-refresh", Text: text}
	}
	answer := func(entries []core.PromptHistoryEntry) {
		reply := activeRecv(t, listings, 2*time.Second, "the session's prompts were listed")
		reply <- sessionPromptsAnswer{entries: entries}
		activeDrainUntil(t, chat, 2*time.Second, func() bool { return len(chat.sessionHistory) > 0 }, "the listing arrived")
	}

	answer([]core.PromptHistoryEntry{entry(1, "typed here")})
	// The session accepted a prompt sent from the web viewer.
	phonePrompt := "phone-prompt"
	chat.applyEvent(&core.SequencedEvent{
		Seq:          2,
		RecordedAtMs: nil,
		RequestID:    &phonePrompt,
		Event: core.WorkerEventPromptAccepted{
			RequestID:   "phone-prompt",
			Text:        "sent from the web",
			Attachments: []core.Attachment{},
		},
	})
	chat.drain(context.Background())
	chat.sessionHistory = nil
	answer([]core.PromptHistoryEntry{entry(2, "sent from the web"), entry(1, "typed here")})

	chat.HandleEventResult(key(tea.KeyUp))
	if chat.input != "sent from the web" {
		t.Fatalf("input = %q", chat.input)
	}
}

// Hard-won: standby adoption dropped the session-history read
func TestStandbyAndParkedComposersKeepOneSessionHistoryRead(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("standby-history", 1)
	fixture.ReplacementView.SendReplace(managedView(core.MaterializedSessionEmpty("standby-history")))
	delegate, err := fixture.Control.Session(context.Background(), "standby-history")
	if err != nil {
		t.Fatal(err)
	}
	listings := core.NewUnbounded[sessionPromptsCall]()
	session := client.NewSessionHandle(&delayedNativeSession{
		delegate: delegate,
		replies:  core.NewUnbounded[delayedSubmit](),
		history:  listings,
	})
	defaults := core.DefaultConfig()
	standby := ChatStateStandby("standby-history", &defaults, SessionHeaderIdentity{}, NewNotices())
	prepare := func(session client.SessionHandle) PreparedChat {
		return ActiveChatPrepareWithPersistence(session, "bundle-1", nil, fixture.Control,
			SessionHeaderIdentity{}, "", NewNotices(), nil)
	}
	chat := prepare(session.Clone()).OpenWithComposer(*standby.TakeLiveComposer())
	t.Cleanup(chat.Close)

	reply := activeRecv(t, listings, 2*time.Second, "adopted composer requests history")
	// A lifecycle transition parks the composer and hands it back while its
	// read is still in flight; that read must survive.
	parked := chat.TakeLiveComposer()
	chat.AdoptLiveComposer(*parked)
	// A warm reattach hands it to the replacement chat; same rule.
	warm := chat.TakeLiveComposer()
	chat = prepare(session).OpenWithComposer(*warm)
	t.Cleanup(chat.Close)
	reply <- sessionPromptsAnswer{entries: []core.PromptHistoryEntry{{
		ID:        1,
		SessionID: "standby-history",
		Text:      "stored prompt",
	}}}
	activeDrainUntil(t, chat, 2*time.Second, func() bool { return len(chat.sessionHistory) > 0 }, "history result arrives")
	if _, ok := tryRecv(listings); ok {
		t.Fatal("the chat reads history once")
	}
	chat.HandleEventResult(key(tea.KeyUp))
	if chat.input != "stored prompt" {
		t.Fatalf("input = %q", chat.input)
	}
}

// activeUserItem is a user message item with the given stable id.
func activeUserItem(stableID string, position uint64, text string) *core.TranscriptItem {
	item := *agentMessageItem(stableID, position, text)
	content, err := json.Marshal(map[string]any{"type": "text", "text": text})
	if err != nil {
		panic(err)
	}
	item.Body = core.TranscriptBodyUser{Content: []json.RawMessage{content}}
	return &item
}

// Hard-won: #1480: transferred submissions miss an already applied delivery snapshot.
func TestDeliveredSubmissionDisappearsWhenComposerArrivesAfterSnapshot(t *testing.T) {
	for _, queuedDelivery := range []bool{false, true} {
		for _, unconfirmed := range []bool{false, true} {
			source := clipboardTestChat(t)
			source.beginSubmission("delivered-before-attach", SubmissionKindPrompt, PromptPayloadText("already delivered"), "Sending")
			if unconfirmed {
				source.applyRemoteResult(chatRemoteResultDeliveryUnconfirmed{
					CommandID: "delivered-before-attach",
					Error:     "Connection reset by peer (os error 104)",
				})
			} else {
				source.applyRemoteResult(chatRemoteResultPrompt{
					CommandID: "delivered-before-attach",
					Text:      "already delivered",
					Images:    []PromptImage{},
					Ordinal:   1,
				})
			}
			session := core.MaterializedSessionEmpty("clipboard-admission")
			session.AppliedEventOrdinal = 2
			if queuedDelivery {
				accepted := uint64(1)
				session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
					CommandID:       "delivered-before-attach",
					Kind:            core.QueuedCommandKindPrompt{},
					Content:         []any{map[string]any{"type": "text", "text": "already delivered"}},
					QueuedAtMs:      1,
					AcceptedOrdinal: &accepted,
				})
			} else {
				session.Transcript = append(session.Transcript, activeUserItem("user:delivered-before-attach", 2, "already delivered"))
			}
			fixture := client.NewReplacementSessionTestFixture("clipboard-admission", 72)
			fixture.ReplacementView.SendReplace(managedView(session))
			handle, err := fixture.Control.Session(context.Background(), "clipboard-admission")
			if err != nil {
				t.Fatal(err)
			}
			destination := activeOpenChat(t, handle, nil, fixture.Control, SessionHeaderIdentity{}, "")
			destination.AdoptLiveComposer(*source.TakeLiveComposer())
			if entries := destination.submissionEntries(); len(entries) != 0 {
				t.Fatalf("%d submission rows", len(entries))
			}
			var wantQueued []string
			if queuedDelivery {
				wantQueued = []string{"already delivered"}
			}
			if texts := destination.QueuedPromptTexts(); !slices.Equal(texts, wantQueued) {
				t.Fatalf("queued = %q", texts)
			}
			screen := strings.Join(transcriptText(destination.ChatState, 100), "\n")
			if strings.Contains(screen, "Delivery unconfirmed") {
				t.Fatal(screen)
			}
			if !queuedDelivery && !strings.Contains(screen, "already delivered") {
				t.Fatal(screen)
			}
		}
	}
}

// Hard-won: #1480: an unacknowledged submit had no idempotent resend or dismissal.
func TestUnconfirmedSubmissionCanBeResentWithItsIdentityOrDismissed(t *testing.T) {
	for _, resend := range []bool{false, true} {
		fixture := client.NewReplacementSessionTestFixture("recover-submission", 1)
		fixture.ReplacementView.SendReplace(managedView(core.MaterializedSessionEmpty("recover-submission")))
		delegate, err := fixture.Control.Session(context.Background(), "recover-submission")
		if err != nil {
			t.Fatal(err)
		}
		observed := core.NewUnbounded[delayedSubmit]()
		observedIDs := core.NewUnbounded[string]()
		session := client.NewSessionHandle(&delayedNativeSession{
			delegate:     delegate,
			replies:      observed,
			submittedIDs: observedIDs,
		})
		chat := activeOpenChat(t, session, nil, fixture.Control, SessionHeaderIdentity{}, "")
		chat.setInput("original prompt")
		chat.HandleEvent(tea.KeyPressMsg{Code: tea.KeyEnter})
		id := activeRecv(t, observedIDs, 2*time.Second, "the prompt was submitted")
		first := activeRecv(t, observed, 2*time.Second, "the prompt reached the session")
		first.reply <- delayedSubmitAnswer{err: client.DeliveryUnconfirmed{}}
		for {
			if _, ok := chat.recoverableSubmissionID(); ok {
				break
			}
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			result, ok := chat.remote.recv(ctx)
			cancel()
			if !ok {
				t.Fatal("the submission result arrives")
			}
			chat.applyRemoteResult(result)
		}
		saved := chat.encodedDraft()
		if !strings.Contains(saved, id) {
			t.Fatalf("saved draft %q lacks %q", saved, id)
		}
		if chat.input != "" {
			t.Fatalf("an unconfirmed result must not restore a sendable draft: %q", chat.input)
		}
		chat.setInput("new draft")
		letter := 'd'
		if resend {
			letter = 'r'
		}
		event := tea.KeyPressMsg{Code: letter, Mod: tea.ModCtrl | tea.ModAlt}
		if !chat.HandleEventResult(event).Consumed {
			t.Fatal("the recovery chord is the chat's")
		}
		if chat.input != "new draft" {
			t.Fatalf("input = %q", chat.input)
		}
		if resend {
			if resent := activeRecv(t, observedIDs, 2*time.Second, "the resend was submitted"); resent != id {
				t.Fatalf("resent %q, want %q", resent, id)
			}
			again := activeRecv(t, observed, 2*time.Second, "the resend reached the session")
			if _, ok := again.command.(core.RelayCommandPrompt); !ok {
				t.Fatalf("resent %#v", again.command)
			}
			if len(chat.pendingSubmissions) != 1 {
				t.Fatalf("%d pending submissions", len(chat.pendingSubmissions))
			}
			again.reply <- delayedSubmitAnswer{ordinal: 1}
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			result, ok := chat.remote.recv(ctx)
			cancel()
			if !ok {
				t.Fatal("the resend result arrives")
			}
			chat.applyRemoteResult(result)
			delivered := core.MaterializedSessionEmpty("recover-submission")
			delivered.AppliedEventOrdinal = 2
			delivered.Transcript = append(delivered.Transcript, activeUserItem("user:"+id, 2, "original prompt"))
			fixture.ReplacementView.SendReplace(managedView(delivered))
			view, err := chat.session.Changed(context.Background())
			chat.applySessionView(view, err)
		} else if _, ok := tryRecv(observedIDs); ok {
			t.Fatal("dismiss must not send a command")
		}
		if len(chat.pendingSubmissions) != 0 {
			t.Fatalf("%d pending submissions", len(chat.pendingSubmissions))
		}
		saved = chat.encodedDraft()
		if strings.Contains(saved, id) {
			t.Fatal("saved draft must drop the retired receipt")
		}
		restored := activeTestSnapshotChat()
		restored.restoreDraft(saved)
		if restored.input != "new draft" {
			t.Fatalf("restored input = %q", restored.input)
		}
		if len(restored.pendingSubmissions) != 0 {
			t.Fatalf("%d restored pending submissions", len(restored.pendingSubmissions))
		}
	}
}

// activeTrimmedScreen is the transcript text with trailing spaces removed.
func activeTrimmedScreen(chat *ChatState) string {
	lines := transcriptText(chat, 100)
	for index, line := range lines {
		lines[index] = strings.TrimRight(line, " \t\r\n")
	}
	return strings.Join(lines, "\n")
}

// Hard-won: #1480: an unconfirmed local row stayed below every newer reply.
func TestGoldenUnconfirmedSubmissionPrecedesNewerTurns(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("inline-submission", 1)
	session := core.MaterializedSessionEmpty("inline-submission")
	session.AppliedEventOrdinal = 10
	session.Transcript = append(session.Transcript, agentMessageItem("agent:before", 10, "Previous reply"))
	fixture.ReplacementView.SendReplace(managedView(session))
	handle, err := fixture.Control.Session(context.Background(), "inline-submission")
	if err != nil {
		t.Fatal(err)
	}
	chat := activeOpenChat(t, handle, nil, fixture.Control, SessionHeaderIdentity{}, "")
	chat.beginSubmission("unconfirmed-original", SubmissionKindPrompt, PromptPayloadText("Prompt whose acknowledgement was lost"), "Sending")
	chat.applyRemoteResult(chatRemoteResultDeliveryUnconfirmed{
		CommandID: "unconfirmed-original",
		Error:     "Connection reset by peer (os error 104)",
	})
	saved := chat.encodedDraft()
	session.AppliedEventOrdinal = 30
	session.Transcript = append(slices.Clone(session.Transcript),
		agentMessageItem("agent:later", 20, "A later turn"),
		agentMessageItem("agent:latest", 30, "Newest reply"))
	fixture.ReplacementView.SendReplace(managedView(session))
	view, err := chat.session.Changed(context.Background())
	chat.applySessionView(view, err)
	// Timestamps are unrelated to placement and vary with the local timezone.
	for index := range chat.entries {
		chat.entries[index].RecordedAtMs = nil
	}
	screen := activeTrimmedScreen(chat.ChatState)
	if strings.Index(screen, "Previous reply") >= strings.Index(screen, "Prompt whose") {
		t.Fatal(screen)
	}
	if strings.Index(screen, "Delivery unconfirmed") >= strings.Index(screen, "A later turn") {
		t.Fatal(screen)
	}
	screenLines := rustStrLines(strings.TrimRight(screen, " \t\r\n"))
	if len(screenLines) == 0 || screenLines[len(screenLines)-1] != "│ Newest reply" {
		t.Fatalf("last line of %q", screen)
	}
	core.AssertGolden(t, "testdata", "unconfirmed-submission-placement", screen)
	if entries := chat.submissionEntries(); len(entries) != 0 {
		t.Fatalf("only actively waiting rows may be pinned: %d", len(entries))
	}
	// Reopening keeps the original frontier rather than anchoring at the new
	// tail.
	reopened := ChatStateFromMaterialized(&session, nil, nil)
	reopened.restoreDraft(saved)
	for index := range reopened.entries {
		reopened.entries[index].RecordedAtMs = nil
	}
	if reopenedScreen := activeTrimmedScreen(reopened); reopenedScreen != screen {
		t.Fatalf("%q != %q", reopenedScreen, screen)
	}
	chat.RestoreTranscriptPosition(TranscriptPositionAtStableID("agent:later"))
	position := chat.TranscriptPosition()
	chat.HandleEvent(tea.KeyPressMsg{Code: 'd', Mod: tea.ModCtrl | tea.ModAlt})
	if after := chat.TranscriptPosition(); !reflect.DeepEqual(after, position) {
		t.Fatal("dismiss must preserve the reader's position")
	}
}

func toolDetailProjection(t *testing.T, sessionID string, position uint64) core.MaterializedSession {
	t.Helper()
	var call schema.ToolCall
	activeUnmarshal(t, fmt.Sprintf(`{
		"toolCallId": "%d",
		"title": "Edit file-%d",
		"status": "completed",
		"rawInput": {"path": "notes.txt"},
		"content": [
			{"type": "content", "content": {"type": "text", "text": "complete tool output"}},
			{"type": "diff", "path": "notes.txt", "oldText": "old\n", "newText": "new\n"}
		]
	}`, position, position), &call)
	core.CompactToolCallContent(call.Content)
	encoded, err := json.Marshal(call)
	if err != nil {
		t.Fatal(err)
	}
	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = position
	projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
		StableID:                  fmt.Sprintf("tool:%d", position),
		Position:                  position,
		LatestContentEventOrdinal: nil,
		CreatedAtMs:               100,
		LastChangedAtMs:           120,
		Body: core.TranscriptBodyTool{
			Call:            core.OpaqueToolCallFromValue(encoded),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
			Presentation:    nil,
		},
	})
	return projection
}

// activeCloneSession is MaterializedSession::clone for these tests: the
// items are copied, so trimming one projection leaves the other whole.
func activeCloneSession(session core.MaterializedSession) core.MaterializedSession {
	session.Transcript = slices.Clone(session.Transcript)
	return session
}

func trimDetailProjection(t *testing.T, projection *core.MaterializedSession) {
	t.Helper()
	for index, shared := range projection.Transcript {
		item := *shared
		if !transcript.CompactToolCallForRetention(&item.Body) {
			t.Fatal("the tool row compacts for retention")
		}
		projection.Transcript[index] = &item
	}
}

func clickToolDetail(t *testing.T, chat *ChatState, seq uint64) {
	t.Helper()
	drawnTranscript(chat, 90, 24)
	index := slices.IndexFunc(chat.transcriptClickTargets, func(target TranscriptClickTarget) bool { return target.StartSeq == seq })
	if index < 0 {
		t.Fatalf("no click target for %d", seq)
	}
	target := chat.transcriptClickTargets[index]
	if !chat.toggleTranscriptAt(target.Rect.Min.X, target.Rect.Min.Y) {
		t.Fatal("the click toggles the tool row")
	}
}

// activePopToolDetailRequest takes the newest tool detail request.
func activePopToolDetailRequest(t *testing.T, chat *ChatState) core.ChatEntry {
	t.Helper()
	if len(chat.pendingToolDetailRequests) == 0 {
		t.Fatal("a tool detail request is pending")
	}
	last := len(chat.pendingToolDetailRequests) - 1
	request := chat.pendingToolDetailRequests[last]
	chat.pendingToolDetailRequests = chat.pendingToolDetailRequests[:last]
	return request
}

func activeAnyLineContains(lines []string, text string) bool {
	return slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, text) })
}

func TestToolCallDetailClickLoadsRetriesAndRendersTheSameAsInlineDetail(t *testing.T) {
	full := toolDetailProjection(t, "tool-detail-chat", 1)
	compact := activeCloneSession(full)
	trimDetailProjection(t, &compact)
	chat := ChatStateFromMaterialized(&compact, nil, nil)
	clickToolDetail(t, chat, 1)
	loading := drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(loading, "Loading tool details…") {
		t.Fatalf("%q", loading)
	}
	requested := activePopToolDetailRequest(t, chat)
	if requested.StableID == nil || *requested.StableID != "tool:1" {
		t.Fatalf("requested %v", requested.StableID)
	}

	generation := chat.toolDetailGeneration
	revision, ok := entryToolDetailRevision(&requested)
	if !ok {
		t.Fatal("the request has a detail revision")
	}
	applyChatIoUpdate(chat, ChatIoUpdateToolCallDetail{
		Generation: generation,
		StableID:   "tool:1",
		Revision:   revision,
		Err:        errors.New("spill was removed"),
	})
	unavailable := drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(unavailable, "Tool details unavailable: spill was removed") {
		t.Fatalf("%q", unavailable)
	}
	if !activeAnyLineContains(unavailable, "Edit file-1") {
		t.Fatalf("%q", unavailable)
	}
	clickToolDetail(t, chat, 1)
	if len(chat.pendingToolDetailRequests) != 0 {
		t.Fatal("collapsing requests nothing")
	}
	clickToolDetail(t, chat, 1)
	requested = activePopToolDetailRequest(t, chat)
	hydrated := client.ToolDetailChatEntry(requested.Source.Item, requested.Seq, full.Transcript[0].Body)
	applyChatIoUpdate(chat, ChatIoUpdateToolCallDetail{
		Generation: generation,
		StableID:   "tool:1",
		Revision:   revision,
		Entry:      &hydrated,
	})
	rendered := drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(rendered, "complete tool output") || !activeAnyLineContains(rendered, "+new") {
		t.Fatalf("%q", rendered)
	}
	inline := ChatStateFromMaterialized(&full, nil, nil)
	diffstats, ok := transcript.MaterializedToolDiffstats(full.Transcript[0])
	if !ok {
		t.Fatal("the full tool row has a final diff")
	}
	inline.applyDiffstats("tool:1", 120, diffstats, nil)
	clickToolDetail(t, inline, 1)
	if inlineRows := drawnTranscript(inline, 90, 24); !slices.Equal(rendered, inlineRows) {
		t.Fatalf("%q != %q", rendered, inlineRows)
	}
	clickToolDetail(t, chat, 1)
	clickToolDetail(t, chat, 1)
	if len(chat.pendingToolDetailRequests) != 0 {
		t.Fatal("a loaded detail is not requested again")
	}
}

// activeReviseTool changes the tool row's output in a new revision.
func activeReviseTool(full *core.MaterializedSession, ordinal uint64, output string) {
	full.AppliedEventOrdinal = ordinal
	full.Transcript = slices.Clone(full.Transcript)
	item := *full.Transcript[0]
	item.LastChangedAtMs = 121
	item.LatestContentEventOrdinal = &ordinal
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		panic("the row is a tool call")
	}
	body.Call.Edit(func(value *any) {
		object := (*value).(jsonvalue.Object)
		content, _ := object.Get("content")
		items := content.([]any)
		first := items[0].(jsonvalue.Object)
		inner, _ := first.Get("content")
		innerObject := inner.(jsonvalue.Object)
		innerObject.Set("text", output)
		first.Set("content", innerObject)
		items[0] = first
		object.Set("content", items)
		*value = object
	})
	item.Body = body
	full.Transcript[0] = &item
}

func TestToolCallDetailRefetchesExpandedToolsAndRejectsSupersededReplies(t *testing.T) {
	full := toolDetailProjection(t, "tool-detail-revisions", 1)
	compact := func(full core.MaterializedSession) core.MaterializedSession {
		projection := activeCloneSession(full)
		trimDetailProjection(t, &projection)
		return projection
	}
	detail := func(request *core.ChatEntry, body core.TranscriptBody) *core.ChatEntry {
		entry := client.ToolDetailChatEntry(request.Source.Item, request.Seq, body)
		return &entry
	}
	compacted := compact(full)
	chat := ChatStateFromMaterialized(&compacted, nil, nil)
	clickToolDetail(t, chat, 1)
	original := activePopToolDetailRequest(t, chat)
	generation := chat.toolDetailGeneration
	reply := func(request *core.ChatEntry, entry *core.ChatEntry) ChatIoUpdate {
		revision, ok := entryToolDetailRevision(request)
		if !ok {
			t.Fatal("the request has a detail revision")
		}
		return ChatIoUpdateToolCallDetail{
			Generation: generation,
			StableID:   *request.StableID,
			Revision:   revision,
			Entry:      entry,
		}
	}
	applyChatIoUpdate(chat, reply(&original, detail(&original, full.Transcript[0].Body)))
	if !activeAnyLineContains(drawnTranscript(chat, 90, 24), "complete tool output") {
		t.Fatal("the loaded detail is drawn")
	}

	activeReviseTool(&full, 2, "intermediate tool output")
	if !applySessionView(chat, managedView(compact(full)), nil) {
		t.Fatal("the view keeps the feed open")
	}
	intermediate := activePopToolDetailRequest(t, chat)
	intermediateBody := full.Transcript[0].Body
	loading := drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(loading, "Loading tool details…") {
		t.Fatalf("%q", loading)
	}
	if activeAnyLineContains(loading, "complete tool output") {
		t.Fatalf("%q", loading)
	}

	// The next update arrives in the same millisecond as the in-flight one.
	activeReviseTool(&full, 3, "latest tool output")
	if !applySessionView(chat, managedView(compact(full)), nil) {
		t.Fatal("the view keeps the feed open")
	}
	latest := activePopToolDetailRequest(t, chat)
	applyChatIoUpdate(chat, reply(&intermediate, detail(&intermediate, intermediateBody)))
	loading = drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(loading, "Loading tool details…") {
		t.Fatalf("%q", loading)
	}
	if activeAnyLineContains(loading, "intermediate tool output") {
		t.Fatalf("%q", loading)
	}

	applyChatIoUpdate(chat, reply(&latest, detail(&latest, full.Transcript[0].Body)))
	if !activeAnyLineContains(drawnTranscript(chat, 90, 24), "latest tool output") {
		t.Fatal("the latest detail is drawn")
	}
}

func TestToolCallDetailIgnoresASessionSwitchReplyForTheSameStableID(t *testing.T) {
	projection := toolDetailProjection(t, "first", 1)
	trimDetailProjection(t, &projection)
	chat := ChatStateFromMaterialized(&projection, nil, nil)
	clickToolDetail(t, chat, 1)
	previousGeneration := chat.toolDetailGeneration
	projection.SessionID = "second"
	if !applySessionView(chat, managedView(projection), nil) {
		t.Fatal("the view keeps the feed open")
	}
	if len(chat.pendingToolDetailRequests) != 0 {
		t.Fatal("a session switch drops pending requests")
	}
	if len(chat.expandedToolCalls) != 0 {
		t.Fatal("a session switch collapses tool rows")
	}
	clickToolDetail(t, chat, 1)
	revision, ok := entryToolDetailRevision(&chat.pendingToolDetailRequests[0])
	if !ok {
		t.Fatal("the request has a detail revision")
	}
	applyChatIoUpdate(chat, ChatIoUpdateToolCallDetail{
		Generation: previousGeneration,
		StableID:   "tool:1",
		Revision:   revision,
		Err:        errors.New("stale lookup"),
	})
	rendered := drawnTranscript(chat, 90, 24)
	if !activeAnyLineContains(rendered, "Loading tool details…") {
		t.Fatalf("%q", rendered)
	}
	if activeAnyLineContains(rendered, "stale lookup") {
		t.Fatalf("%q", rendered)
	}
}

func TestToolCallDetailRawModeQueuesOnlyToolsInTheDrawnViewport(t *testing.T) {
	sessionID := "tool-detail-raw"
	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = 100
	for position := uint64(1); position <= 100; position++ {
		projection.Transcript = append(projection.Transcript, toolDetailProjection(t, sessionID, position).Transcript...)
	}
	trimDetailProjection(t, &projection)
	chat := ChatStateFromMaterialized(&projection, nil, nil)
	drawnTranscript(chat, 90, 16)
	if len(chat.pendingToolDetailRequests) != 0 {
		t.Fatal("Rich mode requests nothing")
	}

	// The host's Alt-T command calls this same state transition.
	chat.ToggleRenderMode()
	tail := drawnTranscript(chat, 90, 16)
	requested := chat.pendingToolDetailRequests
	chat.pendingToolDetailRequests = nil
	if len(requested) == 0 || len(requested) >= 16 {
		t.Fatalf("%d requests", len(requested))
	}
	if slices.ContainsFunc(requested, func(entry core.ChatEntry) bool { return entry.StartSeq <= 90 }) {
		t.Fatal("only the drawn tail is requested")
	}
	if !activeAnyLineContains(tail, "Edit file-100") {
		t.Fatalf("%q", tail)
	}

	chat.anchor = TranscriptAnchorRow{Entry: 0, Row: 0}
	beginning := drawnTranscript(chat, 90, 16)
	if !activeAnyLineContains(beginning, "Edit file-1") {
		t.Fatalf("%q", beginning)
	}
	if len(chat.pendingToolDetailRequests) == 0 {
		t.Fatal("the drawn beginning is requested")
	}
	if slices.ContainsFunc(chat.pendingToolDetailRequests, func(entry core.ChatEntry) bool { return entry.StartSeq >= 10 }) {
		t.Fatal("only the drawn beginning is requested")
	}
}
