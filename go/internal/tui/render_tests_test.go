// Port of mj-tui/src/render/tests.rs.

package tui

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// renderRender is Rust's #[cfg(test)] render (render.rs): the combined
// surface with no conversation on it. The binary always has an ActiveChat
// to pass, or a workspace with no live session, which is what this stands
// for.
func renderRender(frame *chat.Frame, dashboard *DashboardState) {
	chats := map[string]*chat.ActiveChat{}
	opening := map[PaneId]string{}
	withNoColorOverrideForTest(true, func() []string {
		return renderCombinedForTest(frame, dashboard, chats, opening, false)
	})
}

// renderTerminal is ratatui's Terminal over a TestBackend: every draw starts
// from a blank buffer of the terminal's size, and the buffer keeps the last
// frame drawn.
type renderTerminal struct {
	width, height int
	buffer        *uv.Buffer
}

func newRenderTerminal(width, height int) *renderTerminal {
	return &renderTerminal{width: width, height: height, buffer: uv.NewBuffer(width, height)}
}

func (t *renderTerminal) draw(render func(frame *chat.Frame)) {
	t.buffer = uv.NewBuffer(t.width, t.height)
	render(chat.NewFrame(t.buffer))
}

// renderCellStyle is the drawn cell's style at (x, y), as ratatui's Cell
// reports it.
func renderCellStyle(buffer *uv.Buffer, x, y int) chat.Style {
	return chat.CellStyle(buffer.CellAt(x, y))
}

// renderCellSymbol is the drawn cell's symbol at (x, y); a blank cell reads as
// a space, as ratatui's does.
func renderCellSymbol(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

func sessionMetadataText(session *core.SessionRecord, detail *SessionDetail, operation *SessionOperationDisplay, renderNowEpochSeconds uint64, config *core.Config) string {
	state := core.NewStateState()
	return sessionActivityLine(
		"",
		session,
		detail,
		nil,
		false,
		false,
		AttentionLevelIdle,
		nil,
		operation,
		renderNowEpochSeconds,
		sessionTargetLabel(&state, session, operation, config),
		sessionPermissionBadge(session, operation, config),
		nil,
		120,
		false,
	).String()
}

func minimizeAllPanes(dashboard *DashboardState) {
	for _, pane := range []SupportPane{SupportPaneSessions, SupportPaneTargets, SupportPaneQuota} {
		dashboard.SetPaneSize(pane, core.PaneSizeMinimized)
	}
}

func renderAppendGoldenState(output *strings.Builder, label string, width, height int, lines []string) {
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
}

func renderAppendGoldenValue(output *strings.Builder, label string, value any) {
	fmt.Fprintf(output, "%s: %s\n", label, renderRustDebug(value))
}

func renderAppendDashboardGolden(output *strings.Builder, label string, width, height int, dashboard *DashboardState) {
	lines := drawn(dashboard, width, height)
	renderAppendGoldenState(output, label, width, height, lines)
}

// renderRustDebug renders value as Rust's `{:?}` does for the values the goldens
// record: strings quoted, booleans and numbers plain, options as `Some(..)`
// or `None`, and slices as `[a, b]`.
func renderRustDebug(value any) string {
	switch value := value.(type) {
	case nil:
		return "None"
	case string:
		return fmt.Sprintf("%q", value)
	case fmt.GoStringer:
		return value.GoString()
	}
	reflected := reflect.ValueOf(value)
	switch reflected.Kind() {
	case reflect.Pointer:
		if reflected.IsNil() {
			return "None"
		}
		return "Some(" + renderRustDebug(reflected.Elem().Interface()) + ")"
	case reflect.Slice, reflect.Array:
		items := make([]string, reflected.Len())
		for i := range items {
			items[i] = renderRustDebug(reflected.Index(i).Interface())
		}
		return "[" + strings.Join(items, ", ") + "]"
	}
	return fmt.Sprint(value)
}

func clearSessionActivity(dashboard *DashboardState, sessionID string) {
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionIdle)
	detail, ok := (*dashboard.sessionDetails.Mut())[sessionID]
	if !ok {
		panic("the session detail is present")
	}
	detail.currentTurnStartedAt = nil
	detail.activity = client.SessionActivity{}
}

// testQuestionParams is the form question the tests' agents ask.
const testQuestionParams = `{
	"mode": "form",
	"sessionId": "session-1",
	"message": "Choose a path",
	"requestedSchema": {
		"type": "object",
		"properties": {
			"path": {"type": "string"}
		}
	}
}`

func testElicitation(t *testing.T) core.ElicitationRequest {
	t.Helper()
	request, err := core.ElicitationRequestFromACPParams("request-1", json.RawMessage(testQuestionParams))
	if err != nil {
		t.Fatalf("valid test question: %v", err)
	}
	return request
}

func TestPendingQuestionsMarkTheSessionAndMinimizedNavigator(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	title := strings.Repeat("長いセッション名のテスト", 4)
	dashboard.state.Mut().Sessions.GetMut("session-1").SessionTitleOverride = &title
	session := materializedSessionFor("session-1", nil)
	session.PendingElicitations = []core.ElicitationRequest{testElicitation(t)}
	dashboard.ApplyMaterializedSession(&session)
	serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)

	foreign := runningSession()
	foreign.ID = "foreign-session"
	foreign.WorkspaceID = "other-workspace"
	pending := slices.Clone((*dashboard.sessionDetails.Get())["session-1"].pendingElicitations)
	(*dashboard.sessionDetails.Mut())[foreign.ID] = &SessionDetail{pendingElicitations: pending}
	dashboard.state.Mut().Sessions.Insert(foreign.ID, foreign)
	level, count, ok := dashboard.sessionsAttentionSummary()
	if !ok || level != AttentionLevelWaiting || count != 1 {
		t.Fatalf("sessions attention summary = (%v, %d, %v), want (Waiting, 1)", level, count, ok)
	}

	expanded := strings.Join(drawn(dashboard, 120, 30), "\n")
	if !strings.Contains(expanded, "Question") {
		t.Fatal(expanded)
	}

	minimizeAllPanes(dashboard)
	minimized := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(minimized, "Question") {
		t.Fatal(minimized)
	}
	if !strings.Contains(minimized, "Sessions [?1]") && !strings.Contains(minimized, "?1") {
		t.Fatal(minimized)
	}
}

func TestDrawingTheDashboardRegistersEachPaneInteriorForSelection(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	terminal := newRenderTerminal(120, 30)

	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })

	if dashboard.paneAreas == nil {
		t.Fatal("dashboard pane hitboxes")
	}
	panes := *dashboard.paneAreas
	surfaces := dashboard.FrameSurfaces()
	for index, pane := range panes {
		id := chat.SurfaceIdDashboardPane(index)
		surface := surfaces.Surface(id)
		if surface == nil {
			t.Fatalf("pane %d registered", index)
		}
		if surface.Rect != borderedContent(pane) {
			t.Fatalf("pane %d surface %v, want %v", index, surface.Rect, borderedContent(pane))
		}
		at := surfaces.SurfaceAt(surface.Rect.Min.X, surface.Rect.Min.Y)
		if at == nil || at.ID != id {
			t.Fatalf("pane %d: surface at its corner is %#v", index, at)
		}
	}
	// The border rows and the scrollbar column stay out of every surface,
	// so a selection can never pick up their glyphs.
	if surfaces.SurfaceAt(panes[0].Min.X, panes[0].Min.Y) != nil {
		t.Fatal("the border row is a surface")
	}
	if surfaces.SurfaceAt(panes[0].Max.X-1, panes[0].Min.Y+1) != nil {
		t.Fatal("the scrollbar column is a surface")
	}
}

func TestAnOpenDialogRegistersItsBodyAndListAboveThePanes(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.ShowResumeDialog(1, nil)
	terminal := newRenderTerminal(120, 30)

	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })

	surfaces := dashboard.FrameSurfaces()
	body := surfaces.Surface(chat.SurfaceIdModalBody{})
	if body == nil {
		t.Fatal("dialog body")
	}
	list := surfaces.Surface(chat.SurfaceIdResumeList{})
	if list == nil {
		t.Fatal("session list")
	}
	// The dialog covers the panes, and its list covers the dialog.
	if at := surfaces.SurfaceAt(body.Rect.Min.X, body.Rect.Min.Y); at == nil || at.ID != (chat.SurfaceIdModalBody{}) {
		t.Fatalf("surface at the body's corner is %#v", at)
	}
	if at := surfaces.SurfaceAt(list.Rect.Min.X, list.Rect.Min.Y); at == nil || at.ID != (chat.SurfaceIdResumeList{}) {
		t.Fatalf("surface at the list's corner is %#v", at)
	}
	// Away from the popup the panes underneath still own their cells: the
	// dialog covers them, it does not clear them.
	sessions := surfaces.Surface(chat.SurfaceIdDashboardPane(0))
	if sessions == nil {
		t.Fatal("sessions pane")
	}
	if at := surfaces.SurfaceAt(sessions.Rect.Min.X, sessions.Rect.Min.Y); at == nil || at.ID != chat.SurfaceIdDashboardPane(0) {
		t.Fatalf("surface at the sessions pane's corner is %#v", at)
	}
}

func TestUnansweredUserLineStaysBrightAndShowsTheLatestAgentActivity(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	transcript := numberedConversation(1)
	transcript = append(transcript,
		transcriptItem(3, core.TranscriptBodyUser{Content: []json.RawMessage{
			json.RawMessage(`{"type":"text","text":"unanswered follow-up"}`),
		}}),
		thought(4, "Checking the workspace"))
	applyMaterializedTranscript(dashboard, transcript)
	terminal := newRenderTerminal(120, 30)

	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	buffer := terminal.buffer
	lines := bufferLines(buffer)
	rendered := strings.Join(lines, "\n")

	if !strings.Contains(rendered, "Checking the workspace") {
		t.Fatal(rendered)
	}
	if strings.Contains(rendered, "You: unanswered follow-up") {
		t.Fatal(rendered)
	}
	if strings.Contains(rendered, "answer 0") {
		t.Fatal(rendered)
	}
	userRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "Checking the workspace") })
	if userRow < 0 {
		t.Fatal("current activity line")
	}
	userColumn := cellColumn(lines[userRow], "Checking the workspace")
	if renderCellStyle(buffer, userColumn, userRow).Fg == chat.ThemePalette().Muted {
		t.Fatal("the current activity line is muted")
	}
}

// RCL-2 (2026-09-29): a minimized Sessions pane swaps its title for the
// attention count when the title is narrow. A filter in force must still
// show on it: the label gives way first, and the clear chip stays.
//
// Hard-won: daa8f51d: With the Sessions pane minimized, the compact attention title dropped the active filter and its clear chip.
func TestANarrowSessionsTitleWithAttentionKeepsTheFilterClearChip(t *testing.T) {
	for _, symbolCase := range []struct {
		symbols core.SymbolSet
		close   string
	}{{core.SymbolSetUnicode, "×"}, {core.SymbolSetAscii, "x"}} {
		chat.WithSymbols(symbolCase.symbols, func() {
			close := symbolCase.close
			sawLabel := false
			for width := 14; width <= 60; width++ {
				title := sessionsTitleWithAttention("/needle", width, AttentionLevelWaiting, 1, true, true, true)
				text := title.line.String()
				budget := paneTitleContentWidth(width, true)
				if utf8.RuneCountInString(text) > budget {
					// Only the base title may overflow, and it is the one
					// that truncates its own label; it still ends with the
					// chip.
					if !strings.Contains(text, close) {
						t.Fatalf("%v %d: %q", symbolCase.symbols, width, text)
					}
					continue
				}
				// The full form puts the attention suffix after the chip.
				if !strings.Contains(text, " "+close+" ") {
					t.Fatalf("%v %d: %q", symbolCase.symbols, width, text)
				}
				if title.clearChip == nil {
					t.Fatalf("%v %d: chip has room", symbolCase.symbols, width)
				}
				if !strings.HasPrefix(string([]rune(text)[*title.clearChip:]), " "+close+" ") {
					t.Fatalf("%v %d: %q", symbolCase.symbols, width, text)
				}
				sawLabel = sawLabel || strings.Contains(text, "/n")
			}
			if !sawLabel {
				t.Fatalf("%v: some width keeps part of the label", symbolCase.symbols)
			}
			none := sessionsTitleWithAttention("", 20, AttentionLevelWaiting, 1, true, true, false)
			if strings.Contains(none.line.String(), close) {
				t.Fatal("no filter, no chip")
			}
		})
	}
}

// The drawn Sessions pane shows the chip on its title row exactly while a
// filter is in force, with the state label and, given the room, the hidden
// count before it.
func TestPaneSizeControlsAreStyledRegisteredAndClickableWithoutMovingFocus(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.workspaceName = "workspace"
	terminal := newRenderTerminal(120, 40)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })

	panes := []SupportPane{SupportPaneSessions, SupportPaneTargets, SupportPaneQuota, SupportPaneNotifications}
	expectedControlCount := 0
	for _, pane := range panes {
		if dashboard.paneMaximizeEnabled(pane) {
			expectedControlCount += 3
		} else {
			expectedControlCount += 2
		}
	}
	if len(dashboard.paneSizeControlAreas) != expectedControlCount {
		t.Fatalf("%d size controls, want %d", len(dashboard.paneSizeControlAreas), expectedControlCount)
	}
	buffer := terminal.buffer
	for _, pane := range panes {
		maximizeEnabled := dashboard.paneMaximizeEnabled(pane)
		var expectedSizes []core.PaneSize
		for _, size := range []core.PaneSize{core.PaneSizeMinimized, core.PaneSizeStandard, core.PaneSizeMaximized} {
			if size != core.PaneSizeMaximized || maximizeEnabled {
				expectedSizes = append(expectedSizes, size)
			}
		}
		var controls []paneSizeControlArea
		for _, control := range dashboard.paneSizeControlAreas {
			if control.pane == pane {
				controls = append(controls, control)
			}
		}
		if len(controls) != len(expectedSizes) {
			t.Fatalf("pane %v: %d controls, want %d", pane, len(controls), len(expectedSizes))
		}
		for i, control := range controls {
			if control.size != expectedSizes[i] {
				t.Fatalf("pane %v control %d is %v, want %v", pane, i, control.size, expectedSizes[i])
			}
			x, y := control.area.Min.X+1, control.area.Min.Y
			glyph := map[core.PaneSize]string{
				core.PaneSizeMinimized: "▁",
				core.PaneSizeStandard:  "▄",
				core.PaneSizeMaximized: "□",
			}[control.size]
			if symbol := renderCellSymbol(buffer, x, y); symbol != glyph {
				t.Fatalf("pane %v control %v draws %q, want %q", pane, control.size, symbol, glyph)
			}
			style := renderCellStyle(buffer, x, y)
			if control.size == core.PaneSizeStandard {
				if style.Bg != chat.ActiveControl().Bg {
					t.Fatalf("active control background %#v", style.Bg)
				}
				if style.Fg != chat.ThemePalette().Accent {
					t.Fatalf("active control foreground %#v", style.Fg)
				}
				if !style.AddModifier.Contains(chat.ModifierBold) {
					t.Fatal("active control is bold")
				}
			} else {
				if style.Bg != chat.ThemePalette().Surface {
					t.Fatalf("inactive control background %#v", style.Bg)
				}
				if style.Fg != chat.ThemePalette().Text {
					t.Fatalf("inactive control foreground %#v", style.Fg)
				}
			}
		}
	}

	dashboard.SetPaneSize(SupportPaneTargets, core.PaneSizeMinimized)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	buffer = terminal.buffer
	var targetControls []paneSizeControlArea
	for _, control := range dashboard.paneSizeControlAreas {
		if control.pane == SupportPaneTargets {
			targetControls = append(targetControls, control)
		}
	}
	if len(targetControls) != 2 {
		t.Fatalf("%d Targets controls, want 2", len(targetControls))
	}
	for i, expected := range []struct {
		glyph string
		size  core.PaneSize
	}{{"▁", core.PaneSizeMinimized}, {"▄", core.PaneSizeStandard}} {
		control := targetControls[i]
		if control.size != expected.size {
			t.Fatalf("Targets control %d is %v", i, control.size)
		}
		if symbol := renderCellSymbol(buffer, control.area.Min.X+1, control.area.Min.Y); symbol != expected.glyph {
			t.Fatalf("Targets control %d draws %q", i, symbol)
		}
	}
	targetsArea := dashboard.paneAreas[1]
	if symbol := renderCellSymbol(buffer, targetsArea.Max.X-1, targetsArea.Min.Y); symbol != "─" {
		t.Fatalf("minimized Targets right edge draws %q", symbol)
	}

	dashboard.focus = FocusTargets
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	var focusedTargets string
	for _, line := range bufferLines(terminal.buffer) {
		if strings.Contains(line, "Targets") {
			focusedTargets = line
			break
		}
	}
	if focusedTargets == "" {
		t.Fatal("focused Targets row")
	}
	if !strings.Contains(focusedTargets, "─ Targets ──") || !strings.HasSuffix(focusedTargets, "─") || strings.Contains(focusedTargets, "═") {
		t.Fatal(focusedTargets)
	}
	dashboard.focus = FocusSessions
	for _, control := range dashboard.paneSizeControlAreas {
		if control.pane == SupportPaneTargets && control.size == core.PaneSizeMaximized {
			t.Fatal("a minimized Targets pane offers maximize")
		}
	}
	if dashboard.Focus() != FocusSessions {
		t.Fatal("focus moved")
	}
}

// band is the band a row draws, derived exactly as the renderer derives
// it: the shared attention ladder first, then the live-session colours.
func band(detail *SessionDetail, level AttentionLevel, state core.SessionState) chat.Color {
	return sessionBandColor(level, detail, state)
}

func TestSummaryBandColorsPrioritizeAttentionActivityAndLifecycle(t *testing.T) {
	palette := chat.ThemePalette()
	one := uint64(1)
	check := func(got, want chat.Color, message string) {
		t.Helper()
		if got != want {
			t.Fatalf("%s: band %#v, want %#v", message, got, want)
		}
	}
	normal := &SessionDetail{currentTurnStartedAt: &one}
	check(band(normal, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity, "a running turn")

	unread := &SessionDetail{currentTurnStartedAt: &one, unreadAgentMessages: 1}
	check(band(unread, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity,
		"a running turn is activity, whatever is still unread")

	unreadIdle := &SessionDetail{unreadAgentMessages: 1}
	check(band(unreadIdle, AttentionLevelUnread, core.SessionStateRunning), palette.SessionAttention,
		"a finished turn nobody has read wants a person")

	readIdle := &SessionDetail{}
	check(band(readIdle, AttentionLevelIdle, core.SessionStateRunning), palette.SessionIdle, "read and idle")

	foregroundStarted := int64(1)
	foreground := &SessionDetail{activity: client.SessionActivity{ForegroundToolStartedAtMs: &foregroundStarted}}
	check(band(foreground, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity,
		"foreground work is not idle")

	unreadBackground := &SessionDetail{
		unreadAgentMessages: 1,
		activity: client.SessionActivity{
			BackgroundCommands: []core.BackgroundCommand{{
				ID:          "test-background",
				StartedAtMs: 1,
				Command:     "cargo test",
				CanStop:     false,
			}},
		},
	}
	check(band(unreadBackground, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity,
		"background work is still work, not something waiting on a person")
	readBackground := &SessionDetail{activity: unreadBackground.activity}
	check(band(readBackground, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity,
		"background work is not idle after it has been read")

	interruptedIdle := &SessionDetail{unreadInterruptions: 1}
	check(band(interruptedIdle, AttentionLevelUnread, core.SessionStateRunning), palette.SessionAttention,
		"an unread interruption needs attention")

	interruptedRunning := &SessionDetail{currentTurnStartedAt: &one, unreadInterruptions: 1}
	check(band(interruptedRunning, AttentionLevelWorking, core.SessionStateRunning), palette.SessionActivity,
		"a running turn is activity, whatever is still unread")

	needsInput := &SessionDetail{pendingElicitations: []core.ElicitationRequest{testElicitation(t)}}
	check(band(needsInput, AttentionLevelWaiting, core.SessionStateRunning), palette.SessionAttention,
		"pending input overrides the idle blue")

	check(band(readIdle, AttentionLevelWorking, core.SessionStateProvisioning), palette.SessionActivity,
		"provisioning is a lifecycle state, not a live idle session")
	check(band(readIdle, AttentionLevelFailed, core.SessionStateError), palette.SessionError, "error overrides idle")
	check(band(nil, AttentionLevelIdle, core.SessionStateRunning), palette.SessionActivity,
		"unknown detail stays at the default")

	check(band(unread, AttentionLevelUnreachable, core.SessionStateRunning), palette.SessionError,
		"an unreachable worker is red, overriding every other state")
	check(band(nil, AttentionLevelUnreachable, core.SessionStateRunning), palette.SessionError, "unreachable")
}

func TestCurrentAgentExcerptNeverRepeatsAnOldAnswer(t *testing.T) {
	text := func(value string) *string { return &value }
	excerpt := func(detail *SessionDetail) *string {
		if message, ok := currentAgentExcerpt(detail); ok {
			return &message
		}
		return nil
	}
	same := func(got *string, want *string) bool {
		return (got == nil) == (want == nil) && (got == nil || *got == *want)
	}

	waiting := &SessionDetail{lastAgentMessage: text("previous answer"), lastUserMessage: text("new request")}
	if got := excerpt(waiting); got != nil {
		t.Fatalf("waiting: %q", *got)
	}

	thinking := &SessionDetail{
		lastAgentMessage:                 text("previous answer"),
		lastUserMessage:                  text("new request"),
		latestAgentActivityAfterLastUser: text("checking files"),
	}
	if got := excerpt(thinking); !same(got, text("checking files")) {
		t.Fatalf("thinking: %v", got)
	}

	replying := &SessionDetail{
		lastAgentMessage:                 text("current answer"),
		lastUserMessage:                  text("new request"),
		lastAgentMessageFollowsLastUser:  true,
		latestAgentActivityAfterLastUser: text("older thought"),
	}
	if got := excerpt(replying); !same(got, text("current answer")) {
		t.Fatalf("replying: %v", got)
	}

	oldOnly := &SessionDetail{lastAgentMessage: text("old answer")}
	if got := excerpt(oldOnly); got != nil {
		t.Fatalf("old only: %q", *got)
	}
}

// A long reply is rendered for its Sessions row once, not on every frame.
// Later frames reuse its rows until the reply or the room for it changes,
// and the rows are the reply's first two rows as a fresh rendering gives.
//
// Hard-won: 36763162: Long replies were wrapped and rendered repeatedly on every frame, slowing dashboard input as sessions accumulated.
func TestALongReplyIsRenderedForItsSessionRowOnceAcrossFrames(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	id := slices.Collect(dashboard.state.Get().Sessions.Keys())[0]
	reply := func(tag string) string {
		var text strings.Builder
		for index := range 80 {
			fmt.Fprintf(&text, "### %s finding %d\n\n- the daemon publishes a revision and "+
				"every waiter wakes to reload `file_%d.rs`\n\n", tag, index, index)
		}
		return text.String()
	}
	setReply := func(text string) {
		details := dashboard.sessionDetails.Mut()
		detail, ok := (*details)[id]
		if !ok {
			detail = &SessionDetail{}
			(*details)[id] = detail
		}
		detail.lastAgentMessage = &text
		detail.lastAgentMessageFollowsLastUser = true
	}
	renders := func() int { return (*dashboard.sessionDetails.Get())[id].outputPreview.renders }
	// The rows a fresh rendering of text gives for the frame just drawn.
	expectedRows := func(text string) []string {
		if dashboard.paneAreas == nil {
			t.Fatal("a drawn frame")
		}
		contentWidth := dashboard.paneAreas[0].Dx() - 2
		outputWidth := contentWidth - utf8.RuneCountInString("  │")
		var rows []string
		for _, line := range chat.RenderAgentMessageHead(strings.ReplaceAll(text, "\n", " "), outputWidth, 2) {
			var row strings.Builder
			for _, span := range line.Spans {
				row.WriteString(span.Content)
			}
			// A full row's ellipsis can fall past the pane's edge.
			rows = append(rows, strings.TrimRight(row.String(), "…"))
		}
		return rows
	}

	first := reply("first")
	setReply(first)
	screen := drawn(dashboard, 140, 40)
	rows := expectedRows(first)
	if len(rows) != 2 {
		t.Fatalf("%d rows", len(rows))
	}
	for _, row := range rows {
		if !slices.ContainsFunc(screen, func(line string) bool { return strings.Contains(line, row) }) {
			t.Fatalf("%q is on screen: %#v", row, screen)
		}
	}
	for range 3 {
		if again := drawn(dashboard, 140, 40); !slices.Equal(again, screen) {
			t.Fatalf("a later frame differs: %#v", again)
		}
	}
	if renders() != 1 {
		t.Fatalf("later frames reuse the rows: %d renders", renders())
	}

	second := reply("second")
	setReply(second)
	joined := strings.Join(drawn(dashboard, 140, 40), "\n")
	if renders() != 2 {
		t.Fatalf("a new reply is rendered again: %d renders", renders())
	}
	for _, row := range expectedRows(second) {
		if !strings.Contains(joined, row) {
			t.Fatalf("%q is on screen: %s", row, joined)
		}
	}

	joined = strings.Join(drawn(dashboard, 200, 40), "\n")
	if renders() != 3 {
		t.Fatalf("a new width is rendered again: %d renders", renders())
	}
	for _, row := range expectedRows(second) {
		if !strings.Contains(joined, row) {
			t.Fatalf("%q is on screen: %s", row, joined)
		}
	}
}

// Hard-won: 86ab9abd: A pending Muse question row showed the request_user_input tool name instead of the question text.
func TestAPendingQuestionIsTheExcerptInsteadOfTheToolName(t *testing.T) {
	tool := "request_user_input"
	asking := &SessionDetail{
		latestAgentActivityAfterLastUser: &tool,
		pendingElicitations:              []core.ElicitationRequest{question("session-1")},
	}
	if excerpt, ok := currentAgentExcerpt(asking); !ok || excerpt != "Choose a path" {
		t.Fatalf("excerpt %q, %v", excerpt, ok)
	}
}

// footerHints is every hint in the footer, whichever separator it sits
// between.
//
// Command search is the one hint worth more than any other, so it survives
// every focus and every width squeeze. A hint cut in half names a key that
// does not exist. Narrow terminals therefore lose whole hints from the
// right, and never part of one. A half-typed chord owns the footer: the
// reader needs the way out of it and the key that lists the rest, not the
// hints they are part-way through.
func footerHints(footer string) []string {
	var hints []string
	for _, group := range strings.Split(footer, chat.FooterGroupSeparator()) {
		for _, hint := range strings.Split(group, chat.FooterSeparator()) {
			if hint != "" {
				hints = append(hints, hint)
			}
		}
	}
	return hints
}

// The prefix label rides on the first chord hint, which is also the first
// chord a narrow row gives up. The label must outlive it: a row reading
// `: / ? palette` would say only `:` opens the palette.
//
// Hard-won: 22d10cb4: A narrow footer dropped the prefix label with the first chord and made the remaining shortcut look like a plain key.
func TestThePrefixIsStillNamedWhenOnlyProtectedChordsSurvive(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if footer := combinedFooterText(dashboard, 60); footer != "Enter open │ ctrl+b then : / ? palette" {
		t.Fatal(footer)
	}
	if strings.Contains(combinedFooterText(dashboard, 200), "then : / ? palette") {
		t.Fatal("a wide row keeps the label on the first chord")
	}
}

// The quota footer follows the action registered for Enter.
func TestTheQuotaFooterSaysEnterExpandsThePool(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.focus = FocusQuota
	footer := combinedFooterText(dashboard, 200)
	if !strings.HasPrefix(footer, "Enter expand/collapse") || strings.Contains(footer, "edit profile") {
		t.Fatal(footer)
	}
}

// A-12: at every ordinary width and for every pane focus, the chord group
// leads with the prefix, and the palette's `:` never follows a colon.
//
// Hard-won: 48c2a9ca: The composer footer dropped its chord-prefix label at narrower widths and presented prefix chords as plain keys.
func TestEveryPaneFooterNamesThePrefixAt140100And80Columns(t *testing.T) {
	for _, focus := range []Focus{FocusWorkspaces, FocusSessions, FocusQuota, FocusTargets} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.focus = focus
		for _, width := range []int{140, 100, 80} {
			footer := combinedFooterText(dashboard, width)
			groups := strings.Split(footer, chat.FooterGroupSeparator())
			if len(groups) < 2 {
				t.Fatalf("%v %d: %s", focus, width, footer)
			}
			if !strings.HasPrefix(groups[1], "ctrl+b then ") {
				t.Fatalf("%v %d: %s", focus, width, footer)
			}
			if !strings.HasSuffix(footer, ": / ? palette") {
				t.Fatalf("%v %d: %s", focus, width, footer)
			}
			if strings.Contains(footer, "then: :") {
				t.Fatalf("%v %d: %s", focus, width, footer)
			}
		}
	}
}

// The prefix chords give way before the pane's own hints, and palette
// aliases remain visible after every other hint has been dropped. The pane
// group is the short list of keys that work right here; the chord group is
// the long one the palette holds in full, so the long one is what a narrow
// row gives up.
//
// Hard-won: 36fbf1f1: A 140-column footer kept chord hints while dropping the focused pane hints that teach its active controls.
func TestFooterDropsChordHintsBeforePaneHintsAndKeepsPaletteLongest(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.FocusSessions()

	full := combinedFooterText(dashboard, 200)
	if !slices.Contains(footerHints(full), "Enter open") {
		t.Fatal(full)
	}

	// Narrow enough to lose every chord, still wide enough for everything
	// the focused pane answers.
	if squeezed := combinedFooterText(dashboard, 90); squeezed != "Enter open · / search (filter a/b/w/i/d) · Tab pane │ ctrl+b then : / ? palette" {
		t.Fatal(squeezed)
	}

	// Narrower still, the pane hints give way from the right as well, and
	// the prefix label stays on the first chord left standing.
	if footer := combinedFooterText(dashboard, 52); footer != "Enter open │ ctrl+b then : / ? palette" {
		t.Fatal(footer)
	}

	if footer := combinedFooterText(dashboard, 20); footer != ": / ? palette" {
		t.Fatal(footer)
	}
	for _, width := range []int{6, 5, 0} {
		if footer := combinedFooterText(dashboard, width); footer != "" {
			t.Fatalf("%d: %q", width, footer)
		}
	}
}

// The footer is generated from the same table the keyboard reads, so
// pressing what it names must do what it says. This is the test that makes
// the registry worth having.
//
// Hard-won: 7bd8ab16: Hand-written key, footer, and help mappings drifted, so the displayed command could disagree with dispatched behavior.
func TestEveryFooterHintDispatchesTheCommandItNames(t *testing.T) {
	fixture := func(focus Focus) *DashboardState {
		dashboard := dashboardWithSession(runningSession())
		dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
		dashboard.BeginSessionOperationAt("session-1", SessionOperationKindLaunching, nil, 1_000)
		dashboard.focus = focus
		return dashboard
	}
	for _, focus := range []Focus{FocusSessions, FocusTargets, FocusQuota} {
		dashboard := fixture(focus)

		for _, id := range available(dashboard, nil) {
			spec := spec(id)
			word, ok := spec.footer(dashboard)
			if !ok {
				continue
			}
			keyLabel, ok := dashboard.footerKey(id)
			if !ok {
				continue
			}
			footer := combinedFooterText(dashboard, 400)
			if !strings.Contains(footer, keyLabel+" "+word) {
				t.Fatalf("%v: %s omits %q", focus, footer, spec.label)
			}

			// Pressing the advertised key and dispatching the command it
			// names have to leave the surface in the same place.
			pressed := fixture(focus)
			dispatched := fixture(focus)

			var keys []tea.KeyMsg
			switch spec.footerGroup {
			case FooterGroupChord:
				keys = chordKeys(pressed, id)
			case FooterGroupPane:
				if len(spec.paneKeys) == 0 {
					t.Fatal("a pane hint has a key")
				}
				keys = []tea.KeyMsg{key(spec.paneKeys[0].code)}
			}
			byKey := route(pressed, keys)
			byDispatch := dispatched.DispatchCommand(id)
			if !reflect.DeepEqual(byKey, byDispatch) {
				t.Fatalf("%v: %q: by key %#v, by dispatch %#v", focus, spec.label, byKey, byDispatch)
			}
			if modeDiscriminant(pressed.mode) != modeDiscriminant(dispatched.mode) {
				t.Fatalf("%v: %q: modes differ", focus, spec.label)
			}
			if pressed.focus != dispatched.focus {
				t.Fatalf("%v: %q: focus %v, %v", focus, spec.label, pressed.focus, dispatched.focus)
			}
		}
	}
}

// The empty band has two causes and they need different advice. Telling
// someone there is no live session while the pane above lists one is a
// plain lie.
//
// Hard-won: 465c6478: A live session existed but was not open, yet the empty prompt falsely said there was no live session.
func TestTheEmptyPromptDistinguishesNoSessionFromNoConversation(t *testing.T) {
	empty := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	lines := strings.Join(drawn(empty, 120, 44), "\n")
	if !strings.Contains(lines, "No live session") ||
		!strings.Contains(lines, "ctrl+b c to create a session or ctrl+b g to find one") {
		t.Fatal(lines)
	}

	// A live session that simply is not open says so instead.
	live := dashboardWithSession(runningSession())
	lines = strings.Join(drawn(live, 120, 44), "\n")
	if !strings.Contains(lines, "No conversation open") || !strings.Contains(lines, "Enter on the one to open") ||
		strings.Contains(lines, "No live session") || strings.Contains(lines, "Opening session") {
		t.Fatal(lines)
	}
}

func renderNowSeconds() uint64 {
	return uint64(max(time.Now().Unix(), 0))
}

func hostUsage(cpuPercent uint8) core.DeploymentCapacityUsage {
	diskTotal := uint64(64)
	return core.DeploymentCapacityUsage{
		EC2InstanceType:  nil,
		PhysicalMemory:   nil,
		MemoryPool:       false,
		CPUPercent:       &cpuPercent,
		MemoryUsedBytes:  1,
		MemoryTotalBytes: 4,
		LogicalCores:     8,
		DiskTotalBytes:   &diskTotal,
		Storage:          nil,
	}
}

// settledRefreshTime is two hours back.
//
// Hard-won: quota review: a reading stamped with the current second rendered
// "refreshed 0s ago" or "1s ago" depending on whether a second passed before
// the frame; two hours back renders "2h ago" however long the test takes.
func settledRefreshTime() uint64 {
	return renderNowSeconds() - 2*3_600
}

// weeklyQuota is a profile quota with remaining percent of its weekly
// window left.
func weeklyQuota(profileID string, remaining uint8) client.ProfileQuota {
	return client.ProfileQuota{
		ProfileID: profileID,
		Harness:   core.HarnessKindClaude,
		Windows: []client.QuotaWindow{{
			Label:            "weekly",
			RemainingPercent: &remaining,
		}},
		RefreshedAtEpochSeconds: settledRefreshTime(),
	}
}

// weeklyAndFiveHourQuota is a profile quota reporting both windows, the way
// the subscription harnesses do.
func weeklyAndFiveHourQuota(profileID string, weekly, fiveHour uint8) client.ProfileQuota {
	quota := weeklyQuota(profileID, weekly)
	quota.Windows = append(quota.Windows, client.QuotaWindow{
		Label:            "5h",
		RemainingPercent: &fiveHour,
	})
	return quota
}

// renderAPIQuota is what a usage-priced harness reports: no window at all, and
// the API label in place of one.
func renderAPIQuota(profileID string) client.ProfileQuota {
	label := client.APILabel
	return client.ProfileQuota{
		ProfileID:               profileID,
		Harness:                 core.HarnessKindCodex,
		Windows:                 []client.QuotaWindow{},
		Extra:                   &label,
		RefreshedAtEpochSeconds: settledRefreshTime(),
	}
}

// An agent that is idle but left a command running says so, in the wide
// rows and in the minimized grid, from the one fact the daemon forwards.
//
// Hard-won: db2e99a5: Sessions that left a command running in the background were shown as idle across Mjolnir surfaces.
func TestBackgroundWorkReachesBothSessionRowForms(t *testing.T) {
	startedAtMs := int64(core.EpochSeconds())*1_000 - 2_616_000
	activity := client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{{
			ID:          "test-background",
			StartedAtMs: startedAtMs,
			Command:     "cargo test",
			CanStop:     false,
		}},
		ActiveUserShells: nil,
	}

	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if !slices.ContainsFunc(drawn(dashboard, 120, 44), func(line string) bool { return strings.Contains(line, "No messages yet") }) {
		t.Fatal("an idle session with no current output keeps the output block stable")
	}

	dashboard.SetSessionActivity("session-1", activity)
	detail, ok := (*dashboard.sessionDetails.Mut())["session-1"]
	if !ok {
		t.Fatal("session detail")
	}
	detail.currentTurnStartedAt = nil
	expanded := drawn(dashboard, 120, 44)
	if !slices.ContainsFunc(expanded, func(line string) bool {
		return strings.Contains(line, "1 task") && strings.Contains(line, "43m3")
	}) {
		t.Fatalf("the expanded row: %#v", expanded)
	}

	minimized := dashboardWithSession(runningSession())
	minimized.SetSessionActivity("session-1", activity)
	minimizeAllPanes(minimized)
	summary := strings.Join(drawn(minimized, 120, 44), "\n")
	if !strings.Contains(summary, "1 task") || strings.Contains(summary, "You:") || strings.Contains(summary, "Agent:") {
		t.Fatal(summary)
	}
}

// minimizedSessionsDashboard is projects projects, perProject live sessions
// in each, laid out so the minimized list has headings and enough sessions
// to scroll. Project directories are zero-padded so they sort in the
// obvious order.
//
// Every expanded session is the same height, so the layout can be computed
// from a count and rows never jitter as messages arrive. A session with
// nothing to show still draws its two agent rows.
func minimizedSessionsDashboard(projects, perProject int) *DashboardState {
	sessions := core.NewSnapshotMap[string, core.SessionRecord]()
	index := 0
	for project := range projects {
		for range perProject {
			session := runningSession()
			session.ID = fmt.Sprintf("session-%02d", index)
			session.CreatedAt = fmt.Sprintf("2026-08-%02dT00:00:00Z", index+1)
			directory := fmt.Sprintf("/projects/proj%02d", project)
			session.ProjectDirectory = &directory
			sessions.Insert(session.ID, session)
			index++
		}
	}
	state := core.NewStateState()
	state.Version = core.StateVersion
	state.Sessions = sessions
	dashboard := NewDashboardState(config(), state, map[string]client.ProfileQuota{})
	publishFixtureVerdicts(dashboard)
	// One turn of the dial reaches the minimized list.
	minimizeAllPanes(dashboard)
	return dashboard
}

// minimizedContentRows is the content rows of the Sessions pane (inside its
// border) for a minimized list at the given terminal size.
func minimizedContentRows(dashboard *DashboardState, width, height int) []string {
	lines := drawn(dashboard, width, height)
	pane := dashboard.paneAreas[0]
	var rows []string
	for _, line := range lines[pane.Min.Y+1 : pane.Max.Y-1] {
		characters := []rune(line)
		start := min(pane.Min.X+1, len(characters))
		end := min(start+pane.Dx()-2, len(characters))
		rows = append(rows, string(characters[start:end]))
	}
	return rows
}

// Minimized Sessions keeps the familiar vertical list but drops every
// transcript preview beneath the session's summary line.
func TestAStartingSessionKeepsOnePreviewAcrossIncompleteStatePublications(t *testing.T) {
	session := runningSession()
	session.State = core.SessionStateProvisioning
	startup := "Startup"
	session.SessionTitleOverride = &startup
	id := session.ID
	dashboard := dashboardWithSession(session)
	dashboard.config.Mut().Advanced.SessionOrder = core.SessionOrderPriority
	dashboard.SelectActiveSession(id)
	dashboard.BeginSessionOperation(id, SessionOperationKindLaunching, nil)
	pane := dashboard.BrowsePane()
	dashboard.SetPaneSession(pane, &id)
	for _, omit := range []bool{false, true} {
		if omit {
			publication := *dashboard.state.Get()
			publication.Sessions.Remove(id)
			dashboard.SetState(publication)
		}
		rendered := strings.Join(drawn(dashboard, 140, 40), "\n")
		if !strings.Contains(rendered, "Startup") {
			t.Fatal(rendered)
		}
		if selected, ok := dashboard.SelectedSessionID(); !ok || selected != id {
			t.Fatalf("selected %q, %v", selected, ok)
		}
		if shown, ok := dashboard.PaneSession(pane); !ok || shown != id {
			t.Fatalf("pane shows %q, %v", shown, ok)
		}
		rows := drawnSessionRows(dashboard, 60)
		if len(rows) != 1 {
			t.Fatalf("%d rows", len(rows))
		}
		if rows[0].contentHeight() != 1 {
			t.Fatal("startup remains a single preview line")
		}
	}

	ready, _ := dashboard.state.Get().Sessions.Get(id)
	ready.State = core.SessionStateRunning
	readyTitle := "Ready session"
	ready.SessionTitleOverride = &readyTitle
	publication := *dashboard.state.Get()
	publication.Sessions.Insert(id, ready)
	dashboard.SetState(publication)
	publishFixtureVerdicts(dashboard)
	if height := drawnSessionRows(dashboard, 60)[0].contentHeight(); height != 1 {
		t.Fatalf("height %d", height)
	}
	dashboard.FinishSessionOperation(id)
	if height := drawnSessionRows(dashboard, 60)[0].contentHeight(); height <= 1 {
		t.Fatalf("height %d", height)
	}
	rendered := strings.Join(drawn(dashboard, 140, 40), "\n")
	if !strings.Contains(rendered, "Ready session") {
		t.Fatal(rendered)
	}
	if selected, ok := dashboard.SelectedSessionID(); !ok || selected != id {
		t.Fatalf("selected %q, %v", selected, ok)
	}
}

func TestFinishingALaunchRemovesAPreviewAbsentFromAuthoritativeState(t *testing.T) {
	session := runningSession()
	id := session.ID
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation(id, SessionOperationKindLaunching, nil)
	publication := *dashboard.state.Get()
	publication.Sessions.Remove(id)
	dashboard.SetState(publication)
	if !dashboard.state.Get().Sessions.ContainsKey(id) {
		t.Fatal("the launch keeps its preview")
	}
	dashboard.FinishSessionOperation(id)
	if dashboard.state.Get().Sessions.ContainsKey(id) {
		t.Fatal("finishing removed the preview")
	}
	if len(dashboard.orderedSessions()) != 0 {
		t.Fatal("no session is ordered")
	}
}

// A session row is coloured by the same state rule the expanded rows use:
// idle is blue, active work yellow, and a failed session red.
func TestTheMinimizedListColoursASessionRowByState(t *testing.T) {
	colourOf := func(dashboard *DashboardState) chat.Color {
		terminal := newRenderTerminal(120, 44)
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		lines := bufferLines(terminal.buffer)
		row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "ACP pretty") })
		if row < 0 {
			t.Fatal("a session row")
		}
		return renderCellStyle(terminal.buffer, cellColumn(lines[row], "ACP pretty"), row).Fg
	}

	healthy := minimizedSessionsDashboard(1, 1)
	serveAttention(healthy, []string{"session-00"}, core.SessionAttentionIdle)
	if colour := colourOf(healthy); colour != chat.ThemePalette().SessionIdle {
		t.Fatalf("healthy %#v", colour)
	}

	busy := minimizedSessionsDashboard(1, 1)
	one := uint64(1)
	detail, ok := (*busy.sessionDetails.Mut())["session-00"]
	if !ok {
		t.Fatal("the session detail")
	}
	detail.currentTurnStartedAt = &one
	serveAttention(busy, []string{"session-00"}, core.SessionAttentionWorking)
	if colour := colourOf(busy); colour != chat.ThemePalette().SessionActivity {
		t.Fatalf("busy %#v", colour)
	}

	failed := minimizedSessionsDashboard(1, 1)
	failed.state.Mut().Sessions.GetMut("session-00").State = core.SessionStateError
	serveAttention(failed, []string{"session-00"}, core.SessionAttentionFailed)
	if colour := colourOf(failed); colour != chat.ThemePalette().SessionError {
		t.Fatalf("failed %#v", colour)
	}
}

// The minimized list is a viewport: selecting a session past the visible
// rows scrolls the window so it shows, and earlier rows leave view.
func TestTheMinimizedListScrollsToKeepTheSelectionVisible(t *testing.T) {
	dashboard := minimizedSessionsDashboard(12, 1)
	shows := func(rows []string, text string) bool {
		return slices.ContainsFunc(rows, func(line string) bool { return strings.Contains(line, text) })
	}

	// Selecting the first session keeps the window at the start.
	dashboard.SelectActiveSession("session-00")
	rows := minimizedContentRows(dashboard, 120, 20)
	if !shows(rows, "proj00") {
		t.Fatalf("first project visible: %#v", rows)
	}
	if shows(rows, "proj11") {
		t.Fatalf("last project not yet visible: %#v", rows)
	}

	// Selecting the last session scrolls it into view and the first out.
	dashboard.SelectActiveSession("session-11")
	rows = minimizedContentRows(dashboard, 120, 20)
	if !shows(rows, "proj11") {
		t.Fatalf("last project scrolled into view: %#v", rows)
	}
	if shows(rows, "proj00") {
		t.Fatalf("first project scrolled out: %#v", rows)
	}
}

func TestTheMinimizedListFollowsTheTerminalHeightEachFrame(t *testing.T) {
	dashboard := minimizedSessionsDashboard(3, 2)

	tall := drawn(dashboard, 120, 44)
	tallPane := dashboard.paneAreas[0]
	if !strings.Contains(tall[tallPane.Min.Y], "╭") || !strings.Contains(tall[tallPane.Min.Y], "Sessi") {
		t.Fatal(tall[tallPane.Min.Y])
	}
	if dashboard.paneAreas[0].Dy() != 40 {
		t.Fatalf("tall pane height %d", dashboard.paneAreas[0].Dy())
	}

	short := drawn(dashboard, 120, 20)
	shortPane := dashboard.paneAreas[0]
	if !strings.Contains(short[shortPane.Min.Y], "╭") || !strings.Contains(short[shortPane.Min.Y], "Sessi") {
		t.Fatal(short[shortPane.Min.Y])
	}
	if dashboard.paneAreas[0].Dy() != 16 {
		t.Fatalf("short pane height %d", dashboard.paneAreas[0].Dy())
	}

	drawn(dashboard, 120, 44)
	if dashboard.paneAreas[0].Dy() != 40 {
		t.Fatalf("tall pane height again %d", dashboard.paneAreas[0].Dy())
	}
}

// minimizedProfilesRow is the minimized Profiles row of a drawn dashboard.
func minimizedProfilesRow(t *testing.T, dashboard *DashboardState) string {
	t.Helper()
	for _, line := range drawn(dashboard, 120, 44) {
		if strings.Contains(line, "─ Profiles ──") {
			return line
		}
	}
	t.Fatal("the minimized Profiles row")
	return ""
}

// Hard-won: 0cb06579: Independent daemon and dashboard quota pollers could disagree; the pane now follows the daemon report and replaces stale snapshots.
func TestTheProfilesPaneShowsTheDaemonsReportAndProbingSetAndNothingOlder(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	minimizeAllPanes(dashboard)

	dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
		Reports: map[string]client.ProfileQuota{
			"claude-1": weeklyQuota("claude-1", 63),
			"codex-1":  weeklyQuota("codex-1", 10),
		},
		Probing: map[string]struct{}{},
		Cycles:  1,
	})
	if row := minimizedProfilesRow(t, dashboard); !strings.Contains(row, "Anthropic ~63%, OpenAI ~10%*") {
		t.Fatalf("%q", row)
	}

	// Refreshing preserves the last pool reading and marks the affected
	// pool.
	dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
		Reports: map[string]client.ProfileQuota{
			"claude-1": weeklyQuota("claude-1", 63),
			"codex-1":  weeklyQuota("codex-1", 10),
		},
		Probing: map[string]struct{}{"codex-1": {}},
		Cycles:  1,
	})
	row := minimizedProfilesRow(t, dashboard)
	if !strings.Contains(row, "Anthropic ~63%") || !strings.Contains(row, "OpenAI ~10%* (refreshing)") {
		t.Fatalf("%q", row)
	}

	// The snapshot replaces what the dashboard held; it does not add to it.
	dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
		Reports: map[string]client.ProfileQuota{"codex-1": weeklyQuota("codex-1", 12)},
	})
	row = minimizedProfilesRow(t, dashboard)
	if strings.Contains(row, "63%") || !strings.Contains(row, "OpenAI ~12%*") {
		t.Fatalf("%q", row)
	}
}

func TestARefusedQuotaCheckReadsTheNextCheckTimeNotUnavailable(t *testing.T) {
	// A short hold inside the regular interval: the note lasts until the
	// next check, and the last good reading stays on screen beside it.
	held := weeklyQuota("claude-1", 63)
	heldUntil, nextCheck := renderNowSeconds()+60, renderNowSeconds()+5*60
	held.RateLimitedUntilEpochSeconds = &heldUntil
	held.NextCheckAtEpochSeconds = &nextCheck
	// No good reading behind the hold: the report itself is the rate limit.
	onlyLimited := weeklyQuota("codex-1", 0)
	onlyLimited.Windows = nil
	rateLimited := "rate limited"
	onlyLimited.Error = &rateLimited
	limitedUntil := renderNowSeconds() + 5*60
	onlyLimited.RateLimitedUntilEpochSeconds = &limitedUntil

	dashboard := dashboardWithSession(runningSession())
	dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
		Reports: map[string]client.ProfileQuota{"claude-1": held, "codex-1": onlyLimited},
	})
	lines := drawn(dashboard, 120, 44)
	for _, id := range []string{"claude-1", "codex-1"} {
		index := slices.IndexFunc(lines, func(line string) bool {
			return strings.Contains(line, id) && strings.Contains(line, "quota check refused")
		})
		if index < 0 {
			t.Fatalf("a rate limited row for %s: %#v", id, lines)
		}
		row := lines[index]
		if !strings.Contains(row, "quota check refused · next in") || strings.Contains(row, "unavailable") {
			t.Fatalf("%q", row)
		}
		if strings.Contains(row, "63%") != (id == "claude-1") {
			t.Fatalf("%q", row)
		}
	}

	minimizeAllPanes(dashboard)
	row := minimizedProfilesRow(t, dashboard)
	if !strings.Contains(row, "Anthropic ~63%") || !strings.Contains(row, "codex-1 check refused") {
		t.Fatalf("%q", row)
	}
}

// An exhausted profile reads 0%, the same as the open pane's bar. Showing
// how much has been *used* would read 100% there, which looks like a
// profile in the best possible shape rather than one with nothing left.
func TestAnExhaustedQuotaReadsZeroInTheMinimizedRow(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.ApplyQuota(weeklyQuota("claude-1", 0))
	minimizeAllPanes(dashboard)

	quota := minimizedProfilesRow(t, dashboard)
	if !strings.Contains(quota, "Anthropic ~0%") || strings.Contains(quota, "Anthropic ~100%") {
		t.Fatalf("%q", quota)
	}
}

func TestARollingOnlyQuotaKeepsItsPercentageVisible(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	quota := weeklyAndFiveHourQuota("claude-1", 100, 40)
	quota.Windows = quota.Windows[1:]
	dashboard.ApplyQuota(quota)
	minimizeAllPanes(dashboard)

	row := minimizedProfilesRow(t, dashboard)
	if !strings.Contains(row, "Anthropic 5H ~40%") || strings.Contains(row, "Anthropic unavailable") {
		t.Fatalf("%q", row)
	}
}

// A reading that cannot be trusted has to say so. A number that is
// actually missing, stale or inapplicable is worse than no number.
func TestTheMinimizedRowsStayExplicitAboutReadingsTheyDoNotHave(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	minimizeAllPanes(dashboard)

	// No sample at all.
	targets := func(lines []string) string {
		for _, line := range lines {
			if strings.Contains(line, "─ Targets ──") {
				return line
			}
		}
		t.Fatal("the minimized Targets row")
		return ""
	}
	if row := targets(drawn(dashboard, 120, 44)); !strings.Contains(row, "local unavailable") {
		t.Fatalf("%q", row)
	}

	// A probe in flight.
	var capacities []client.RuntimeHostCapacity
	for _, detail := range capacityDetailsInKeyOrder(dashboard) {
		capacities = append(capacities, *detail)
	}
	capacities[0].Refreshing = true
	dashboard.SetHostCapacities(capacities)
	if row := targets(drawn(dashboard, 120, 44)); !strings.Contains(row, "local refreshing…") {
		t.Fatalf("%q", row)
	}

	// A sample too old to trust.
	usage := hostUsage(7)
	dashboard.applyDeploymentCapacity("local", &usage, nil, renderNowSeconds()-core.CapacitySampleStaleAfterSeconds-60)
	if row := targets(drawn(dashboard, 120, 44)); !strings.Contains(row, "local 7% (stale)") {
		t.Fatalf("%q", row)
	}

	// No quota report yet is distinct from a failed probe.
	if pending := strings.Join(drawn(dashboard, 120, 44), "\n"); !strings.Contains(pending, "claude-1 refreshing") {
		t.Fatal(pending)
	}
	failed := weeklyAndFiveHourQuota("claude-1", 0, 0)
	failed.Windows = nil
	unavailable := "unavailable"
	failed.Error = &unavailable
	dashboard.ApplyQuota(failed)
	// A quota that failed to refresh.
	if quota := minimizedProfilesRow(t, dashboard); !strings.Contains(quota, "claude-1 unavailable") {
		t.Fatalf("%q", quota)
	}
}

// A failed session used to render identically to a healthy one, so the
// only thing that told you it had failed was pressing Enter on it. Red is
// the same signal an unreachable relay carries: this row needs attention
// rather than reading.
func TestAFailedSessionDrawsARedSummaryAtBothPaneSizes(t *testing.T) {
	for _, size := range []core.PaneSize{core.PaneSizeStandard, core.PaneSizeMinimized} {
		healthy := dashboardWithSession(runningSession())
		healthy.SetPaneSize(SupportPaneSessions, size)
		session := runningSession()
		session.State = core.SessionStateError
		bootstrap := "worker bootstrap failed"
		session.LastError = &bootstrap
		failed := dashboardWithSession(session)
		failed.SetPaneSize(SupportPaneSessions, size)
		serveAttention(failed, []string{"session-1"}, core.SessionAttentionFailed)

		rowColour := func(dashboard *DashboardState) chat.Color {
			terminal := newRenderTerminal(120, 44)
			terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
			lines := bufferLines(terminal.buffer)
			status := "Idle"
			for session := range dashboard.state.Get().Sessions.Values() {
				if session.State == core.SessionStateError {
					status = "Error"
				}
			}
			row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, status) })
			if row < 0 {
				t.Fatalf("the session's row (%s): %#v", status, lines)
			}
			return renderCellStyle(terminal.buffer, cellColumn(lines[row], status), row).Fg
		}

		if colour := rowColour(failed); colour != chat.ThemePalette().Error {
			t.Fatalf("%v: %#v", size, colour)
		}
		if rowColour(healthy) == chat.ThemePalette().Error {
			t.Fatalf("%v: only a session that needs attention is red", size)
		}
	}
}

// A week with headroom left is no comfort while the next five hours are
// spent, so a profile that has dipped into its week reports both figures.
// An untouched week says everything there is to say on its own, and a
// profile with no five-hour window has nothing more to add.
func TestReadIdleSessionDistinguishesStatusFromIdentityInExpandedAndCollapsedRows(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	// The detach cursor sits past the only agent message, so nothing is
	// unread; a truly idle live session is still blue.
	session.ViewedThroughEventOrdinal = 1
	for _, collapsed := range []bool{false, true} {
		dashboard := dashboardWithSession(session)
		dashboard.focus = FocusQuota
		materialized := materializedSessionFor("session-1", []*core.TranscriptItem{agentMessage(1, "seen response")})
		materialized.Execution = core.MaterializedExecutionStateIdle{}
		dashboard.ApplyMaterializedSession(&materialized)
		if collapsed {
			dashboard.FocusSessions()
			dashboard.HandleKey(key('1'))
			dashboard.focus = FocusQuota
		}

		terminal := newRenderTerminal(140, 28)
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		buffer := terminal.buffer
		lines := chat.BufferLines(buffer)
		statusY := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "podman") })
		if statusY < 0 {
			t.Fatal("the session's summary row")
		}
		status := lines[statusY]
		if strings.Contains(status, "unread") {
			t.Fatal(status)
		}
		pane := dashboard.paneAreas[0]
		identityX := cellColumn(status, "podman")
		for x := pane.Min.X + 1; x < identityX; x++ {
			if summaryTextCell(buffer.CellAt(x, statusY)) && renderCellStyle(buffer, x, statusY).Fg != chat.ThemePalette().SessionIdle {
				t.Fatalf("%v: %s", collapsed, status)
			}
		}
		if renderCellStyle(buffer, identityX, statusY).Fg != chat.ThemePalette().Muted {
			t.Fatalf("%v: the identity is not muted", collapsed)
		}
	}
}

// A session the harness has not named and nobody renamed is listed by the
// title it was created with ("project via fake"), not its id, which the row
// would otherwise repeat (launch finding R2-8).
//
// Hard-won: e7eb288e: An unnamed dashboard-created session fell back to its hexadecimal ID instead of the created project/profile title.
func TestSessionNamePrefersOverrideThenAcpTitleThenCreatedTitle(t *testing.T) {
	session := stoppedSession()
	if name := sessionName(&session); name != "ACP pretty name" {
		t.Fatal(name)
	}

	session.ACPSessionTitle = nil
	if name := sessionName(&session); name != session.Title {
		t.Fatal(name)
	}

	mine := "My name"
	session.SessionTitleOverride = &mine
	if name := sessionName(&session); name != "My name" {
		t.Fatal(name)
	}

	session.SessionTitleOverride = nil
	session.Title = " "
	if name := sessionName(&session); name != "session-1" {
		t.Fatal(name)
	}
}

// renderedSymbols is every symbol of the drawn dashboard in buffer order,
// as ratatui's buffer content joins them.
func renderedSymbols(dashboard *DashboardState, width, height int) string {
	terminal := newRenderTerminal(width, height)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	var text strings.Builder
	for y := range height {
		for x := range width {
			text.WriteString(renderCellSymbol(terminal.buffer, x, y))
		}
	}
	return text.String()
}

// The standard local container targets exist whether or not an engine is
// installed. The Targets pane reads the same availability answer the
// pickers use, and marks one whose engine is missing rather than listing it
// as if it could run.
//
// Hard-won: 0b408d01: Launch finding J-7 showed the Targets pane omitted whether a listed local container engine was unavailable.
func TestTargetsPaneMarksALocalContainerTargetWhoseEngineIsMissing(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	target := testCapacityTarget()
	target.TargetIDs = []string{"podman", "localhost"}
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{target})
	if strings.Contains(renderedSymbols(dashboard, 120, 40), "podman (unavailable)") {
		t.Fatal("marked before the answer")
	}

	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"podman"}, nil)
	})

	if rendered := renderedSymbols(dashboard, 120, 40); !strings.Contains(rendered, "podman (unavailable)") {
		t.Fatal(rendered)
	}
}

// A machine whose every listed target is unavailable says so once, after
// the host name: no target list, no capacity cells, no Disks button, and no
// clickable region for one.
func TestTargetsRowCollapsesWhenEveryTargetIsUnavailable(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	target := precisionCapacityTarget()
	target.TargetIDs = []string{"podman"}
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{target})
	usage := hostCapacityUsage()
	dashboard.applyDeploymentCapacity("ssh:precision-3260", &usage, nil, renderNowEpochSeconds())
	dashboard.setTestCapacityStorage([]core.TargetStorageView{precisionStorage(40<<30, 40<<30)})

	// While the target is available the row has a Disks button.
	before := drawnDashboard(dashboard, 120)
	if !strings.Contains(before, "Disks ▾") {
		t.Fatal(before)
	}
	if !slices.ContainsFunc(dashboard.capacityDisksAreas, func(area *uv.Rectangle) bool { return area != nil }) {
		t.Fatal("a Disks hitbox while a target is available")
	}

	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"podman"}, nil)
	})
	rendered := drawnDashboard(dashboard, 120)
	var row string
	for _, line := range strings.Split(rendered, "\n") {
		if strings.Contains(line, "precision-3260") {
			row = line
			break
		}
	}
	if row == "" {
		t.Fatal("collapsed row")
	}
	var cells []string
	for _, cell := range strings.Split(row, "  ") {
		if cell = strings.TrimSpace(cell); cell != "" {
			cells = append(cells, cell)
		}
	}
	start := slices.IndexFunc(cells, func(cell string) bool { return strings.Contains(cell, "precision-3260") })
	var collapsedCells []string
	if start >= 0 {
		for _, cell := range cells[start:] {
			if cell = strings.TrimSpace(strings.Trim(cell, "│")); cell != "" {
				collapsedCells = append(collapsedCells, cell)
			}
		}
	}
	if !slices.Equal(collapsedCells, []string{"precision-3260", "unavailable"}) {
		t.Fatalf("%q", row)
	}
	if strings.Contains(rendered, "Disks") || strings.Contains(rendered, "podman (unavailable)") {
		t.Fatal(rendered)
	}
	if slices.ContainsFunc(dashboard.capacityDisksAreas, func(area *uv.Rectangle) bool { return area != nil }) {
		t.Fatal("no Disks hitbox on a collapsed row")
	}
}

// A default candidate whose engine is missing (the user has no Docker and
// wrote no `docker` target) is not listed in the host row. A target the
// user wrote whose engine is missing stays, marked unavailable. The config
// the dashboard installs comes off the runtime feed, which does not
// serialize Config.DefaultTargets. The origin travels beside the config, so
// a host without Docker lists no `docker` the user never wrote, while a
// `[targets.docker]` the user wrote stays, marked unavailable.
//
// Hard-won: 9a81ac2b: The runtime-fed dashboard could not distinguish a synthesized Docker candidate and showed it as unavailable on hosts without Docker.
func TestFeedInstalledConfigHidesDefaultDockerButKeepsAUserWrittenOne(t *testing.T) {
	dockerMissingRow := func(userWroteDocker bool) string {
		daemonConfig := core.DefaultConfig()
		if userWroteDocker {
			docker := core.DefaultConfig().WithLocalTargets().Targets["docker"]
			if local, ok := docker.(core.TargetTemplateLocalDocker); ok {
				local.ContainerTemplate.Image = "example.test/own:latest"
				docker = local
			}
			daemonConfig.Targets["docker"] = docker
		}
		daemonConfig = daemonConfig.WithLocalTargets()
		sent := client.RuntimeMetadata{
			DefaultTargets: daemonConfig.DefaultTargets,
			Config:         core.NewSharedSnapshot(daemonConfig),
		}
		encoded, err := json.Marshal(sent)
		if err != nil {
			t.Fatal(err)
		}
		var received client.RuntimeMetadata
		if err := json.Unmarshal(encoded, &received); err != nil {
			t.Fatal(err)
		}
		dashboard := NewDashboardState(received.InstalledConfig(), core.NewStateState(), map[string]client.ProfileQuota{})
		target := testCapacityTarget()
		target.TargetIDs = []string{"docker", "localhost"}
		dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{target})
		answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
			return availabilityAnswer(dashboard, []string{"docker"}, nil)
		})
		return drawnDashboard(dashboard, 160)
	}

	if defaultOnly := dockerMissingRow(false); strings.Contains(defaultOnly, "docker") {
		t.Fatal(defaultOnly)
	}
	if userWritten := dockerMissingRow(true); !strings.Contains(userWritten, "docker (unavailable)") {
		t.Fatal(userWritten)
	}
}

// precision-3260: an unreachable session on a full disk read as plain
// "Unreachable". Its row now says "Disk full", and the Targets row shows the
// host as full, both from the daemon's one storage verdict.
//
// Hard-won: 540c9202: A real full-disk incident made every worker appear merely unreachable and caused recovery to retry writes on the same full filesystem.
func TestAnUnreachableSessionOnAFullDiskSaysDiskFull(t *testing.T) {
	session := precisionSession()
	dashboard := dashboardWithSession(session)
	dashboard.SetSessionConnectivity(session.ID, false)
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{{
		ID:        "ssh:precision-3260",
		Host:      "precision-3260",
		TargetIDs: []string{"precision"},
		Kind:      core.DeploymentCapacityKindHost,
		Local:     false,
	}})
	draw := func() string { return renderedSymbols(dashboard, 160, 40) }
	rendered := draw()
	if !strings.Contains(rendered, "Unreachable") || strings.Contains(rendered, "Disk full") {
		t.Fatal(rendered)
	}

	// A full filesystem the session does not write to changes nothing.
	elsewhere := precisionStorage(40<<30, 40<<30)
	elsewhere.Filesystems[1].Paths = []string{"/srv"}
	elsewhere.Filesystems[1].Condition = core.StorageConditionFull
	dashboard.setTestCapacityStorage([]core.TargetStorageView{elsewhere})
	if strings.Contains(draw(), "Disk full") {
		t.Fatal("a full filesystem the session does not use")
	}

	// Its managed clone's filesystem is full: the row and the Targets pane
	// say so.
	dashboard.setTestCapacityStorage([]core.TargetStorageView{precisionStorage(40<<30, 0)})
	rendered = draw()
	if !strings.Contains(rendered, "Disk full") {
		t.Fatal(rendered)
	}
	if !strings.Contains(rendered, "/home/jonathan/Projects full ▾") {
		t.Fatalf("the Targets row: %s", rendered)
	}
}

// `symbols = "ascii"` must reach the Targets pane's own row text, such as
// the Disks cell's dropdown mark. The symbol set is read from the
// dashboard's own configuration, the way a running session selects it,
// rather than through the thread-local override the render pipeline itself
// already scopes to that configuration.
func TestAsciiSymbolsReachTheCapacityRow(t *testing.T) {
	asciiConfig := config()
	ascii := core.SymbolSetAscii
	asciiConfig.Advanced.Symbols = &ascii
	dashboard := NewDashboardState(asciiConfig, core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{precisionCapacityTarget()})
	usage := hostCapacityUsage()
	dashboard.applyDeploymentCapacity("ssh:precision-3260", &usage, nil, renderNowEpochSeconds())
	dashboard.setTestCapacityStorage([]core.TargetStorageView{precisionStorage(40<<30, 40<<30)})
	rendered := drawnDashboard(dashboard, 200)
	row := renderedLineContaining(t, rendered, "precision-3260")
	for _, character := range row {
		if character > 0x7f {
			t.Fatalf("%q", row)
		}
	}
	if !strings.Contains(row, "37%") || !strings.Contains(row, "75%") || !strings.Contains(row, "Disks v") {
		t.Fatalf("%q", row)
	}
}

// renderedLineContaining is the first line of rendered containing text.
func renderedLineContaining(t *testing.T, rendered, text string) string {
	t.Helper()
	for _, line := range strings.Split(rendered, "\n") {
		if strings.Contains(line, text) {
			return line
		}
	}
	t.Fatalf("no line contains %q: %s", text, rendered)
	return ""
}

func precisionCapacityTarget() core.DeploymentCapacityTarget {
	return core.DeploymentCapacityTarget{
		ID:        "ssh:precision-3260",
		Host:      "precision-3260",
		TargetIDs: []string{"precision"},
		Kind:      core.DeploymentCapacityKindHost,
		Local:     false,
	}
}

// The Disks cell names a filesystem only when it is low or full; free space
// on filesystems that are fine answers no question. Clicking the cell, or
// `d` on the selected row, opens its details in a dropdown without changing
// the one-line table layout.
func TestCapacityDisksCellOpensADropdownAndKeepsTheTableOneLinePerTarget(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{precisionCapacityTarget()})
	dashboard.setTestCapacityStorage([]core.TargetStorageView{precisionStorage(40<<30, 40<<30)})
	rendered := drawnDashboard(dashboard, 160)
	if !strings.Contains(rendered, "Disks ▾") || strings.Contains(rendered, "free") {
		t.Fatal(rendered)
	}

	// ~/Projects drops below the low-space line, and `/` is fine.
	dashboard.setTestCapacityStorage([]core.TargetStorageView{precisionStorage(40<<30, 3<<30)})
	rendered = drawnDashboard(dashboard, 160)
	summary := "/home/jonathan/Projects low ▾"
	lines := strings.Split(rendered, "\n")
	row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, summary) })
	if row < 0 {
		t.Fatal("the low filesystem is named")
	}
	at := uv.Pos(cellColumn(lines[row], summary), row)
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
	if dashboard.focus != FocusTargets {
		t.Fatalf("focus %v", dashboard.focus)
	}
	if lines := capacityTableLines(dashboard); lines != 1 {
		t.Fatalf("%d table lines", lines)
	}
	if dashboard.paneMenu == nil {
		t.Fatal("the dropdown opened")
	}

	rendered = drawnDashboard(dashboard, 160)
	for _, text := range []string{
		"/home/jonathan/Projects low ▴",
		"/ 40.0G free of 500.0G",
		"/home/jonathan/Projects 3.0G free of 1000.0G low",
	} {
		if !strings.Contains(rendered, text) {
			t.Fatalf("%q: %s", text, rendered)
		}
	}
	if count := strings.Count(rendered, "precision-3260"); count != 1 {
		t.Fatalf("%d host rows: %s", count, rendered)
	}
	terminal := newRenderTerminal(160, 40)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	drawnLines := bufferLines(terminal.buffer)
	lowRow := slices.IndexFunc(drawnLines, func(line string) bool { return strings.Contains(line, "3.0G free of 1000.0G low") })
	if lowRow < 0 {
		t.Fatal("low filesystem detail")
	}
	lowColumn := cellColumn(drawnLines[lowRow], "3.0G free of 1000.0G low")
	if colour := renderCellStyle(terminal.buffer, lowColumn, lowRow).Fg; colour != chat.ThemePalette().Warning {
		t.Fatalf("low detail colour %#v", colour)
	}

	// The cell remains a close target even though the dropdown is modal.
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
	if dashboard.paneMenu != nil {
		t.Fatal("the dropdown closed")
	}

	// The command opens the same anchored details list for the selected
	// row.
	dashboard.HandleKey(key('d'))
	if dashboard.paneMenu == nil {
		t.Fatal("d opened the dropdown")
	}
	if lines := capacityTableLines(dashboard); lines != 1 {
		t.Fatalf("%d table lines", lines)
	}
	if rendered := drawnDashboard(dashboard, 160); !strings.Contains(rendered, "/home/jonathan/Projects 3.0G free of 1000.0G low") {
		t.Fatal(rendered)
	}
	dashboard.HandleKey(key('d'))
	if dashboard.paneMenu != nil {
		t.Fatal("d closed the dropdown")
	}
}

func renderNowEpochSeconds() uint64 {
	return uint64(max(time.Now().Unix(), 0))
}

func hostCapacityUsage() core.DeploymentCapacityUsage {
	cpu := uint8(37)
	return core.DeploymentCapacityUsage{
		CPUPercent:       &cpu,
		MemoryUsedBytes:  3,
		MemoryTotalBytes: 4,
		LogicalCores:     8,
	}
}

func TestCapacityPaneNeverPresentsMissingReadingsAsZeroResourceUse(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	usage := core.DeploymentCapacityUsage{LogicalCores: 8}
	dashboard.applyDeploymentCapacity("local", &usage, nil, renderNowEpochSeconds())

	row := renderedLineContaining(t, drawnDashboard(dashboard, 200), "podman")
	if strings.Count(row, "unavailable") != 2 || strings.Contains(row, "0%") {
		t.Fatalf("%q", row)
	}
}

func drawnDashboard(dashboard *DashboardState, width int) string {
	terminal := newRenderTerminal(width, 40)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	return strings.Join(bufferLines(terminal.buffer), "\n")
}

// A probe that failed and a reading that stopped refreshing both keep the
// last numbers on screen and say why they cannot be trusted, instead of
// rendering exactly like a reading taken a moment ago.
func TestCapacityRowsMarkAFailedProbeAndASampleThatStoppedRefreshing(t *testing.T) {
	timedOut := "probe timed out"
	failed := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	failed.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	usage := hostCapacityUsage()
	failed.applyDeploymentCapacity("local", &usage, nil, renderNowEpochSeconds())
	freshWidth := capacityTableWidth(failed)
	failed.applyDeploymentCapacity("local", nil, &timedOut, renderNowEpochSeconds())
	if width := capacityTableWidth(failed); width != freshWidth {
		t.Fatalf("a notice must not widen the Targets table: %d, was %d", width, freshWidth)
	}
	rendered := drawnDashboard(failed, 200)
	for _, text := range []string{"37%", "75%", "stale: probe timed out"} {
		if !strings.Contains(rendered, text) {
			t.Fatalf("%q: %s", text, rendered)
		}
	}

	// The notice takes its own line inside the target's row, starting under
	// the Targets column and running on across the columns after it, so the
	// pane never asks for a wider terminal to show it.
	lines := strings.Split(rendered, "\n")
	readings := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "37%") })
	if readings < 0 {
		t.Fatal("the row of readings")
	}
	notice := lines[readings+1]
	if !strings.Contains(notice, "stale: probe timed out") {
		t.Fatal(rendered)
	}
	if strings.Index(notice, "stale") != strings.Index(lines[readings], "podman") {
		t.Fatal(rendered)
	}
	if lines := capacityTableLines(failed); lines != 2 {
		t.Fatalf("%d table lines: %s", lines, rendered)
	}

	aged := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	aged.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	agedUsage := hostCapacityUsage()
	aged.applyDeploymentCapacity("local", &agedUsage, nil, saturatingSubUint64(renderNowEpochSeconds(), 3_600))
	if rendered := drawnDashboard(aged, 200); !strings.Contains(rendered, "stale: sampled 1h ago") {
		t.Fatal(rendered)
	}

	neverSampled := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	neverSampled.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	neverSampled.applyDeploymentCapacity("local", nil, &timedOut, renderNowEpochSeconds())
	if rendered := drawnDashboard(neverSampled, 200); !strings.Contains(rendered, "unavailable") || strings.Contains(rendered, "stale") {
		t.Fatal(rendered)
	}
}

// drawnTargetsPane is the Targets pane alone, drawn into an area of width
// by height at the origin of a terminal termWidth wide.
func drawnTargetsPane(dashboard *DashboardState, termWidth, width, height int) []string {
	terminal := newRenderTerminal(termWidth, height)
	terminal.draw(func(frame *chat.Frame) {
		dashboard.beginSurfaceFrame()
		renderCapacity(frame, uv.Rect(0, 0, width, height), dashboard, nil)
		dashboard.endSurfaceFrame()
	})
	return bufferLines(terminal.buffer)
}

// A notice never costs the readings their place. A table row taller than
// the pane makes the table scroll past the selected row, so in a pane with
// room for one content line the reading keeps that line and the notice
// waits for the room it needs.
func TestAOneLineTargetsViewportKeepsAStaleReadingOnScreen(t *testing.T) {
	timedOut := "probe timed out"
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	usage := hostCapacityUsage()
	dashboard.applyDeploymentCapacity("local", &usage, nil, renderNowEpochSeconds())
	dashboard.applyDeploymentCapacity("local", nil, &timedOut, renderNowEpochSeconds())

	has := func(lines []string, match func(string) bool) bool { return slices.ContainsFunc(lines, match) }

	// Border, heading, and a single content line.
	lines := drawnTargetsPane(dashboard, 40, 40, 4)
	if !has(lines, func(line string) bool { return strings.Contains(line, "local") && strings.Contains(line, "37%") }) {
		t.Fatalf("the selected target keeps its reading: %#v", lines)
	}
	if has(lines, func(line string) bool { return strings.Contains(line, "stale") }) {
		t.Fatalf("a notice with no line to itself is not drawn over a reading: %#v", lines)
	}

	// One more row of room and the notice is back on its own line.
	lines = drawnTargetsPane(dashboard, 40, 40, 5)
	if !has(lines, func(line string) bool { return strings.Contains(line, "37%") }) ||
		!has(lines, func(line string) bool { return strings.Contains(line, "stale: probe timed out") }) {
		t.Fatalf("%#v", lines)
	}

	// The same holds with a target after the stale one: the selected target
	// keeps its line rather than the pane settling on the next one.
	dashboard = NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	var targets []core.DeploymentCapacityTarget
	for _, host := range []string{"host-0", "host-1"} {
		targets = append(targets, core.DeploymentCapacityTarget{
			ID:        host,
			Host:      host,
			TargetIDs: []string{"podman"},
			Kind:      core.DeploymentCapacityKindHost,
		})
	}
	dashboard.setDeploymentCapacityTargets(targets)
	first, second := hostUsage(11), hostUsage(12)
	dashboard.applyDeploymentCapacity("host-0", &first, nil, renderNowEpochSeconds())
	dashboard.applyDeploymentCapacity("host-1", &second, nil, renderNowEpochSeconds())
	dashboard.applyDeploymentCapacity("host-1", nil, &timedOut, renderNowEpochSeconds())
	dashboard.capacityIndex = 1
	lines = drawnTargetsPane(dashboard, 40, 40, 4)
	if !has(lines, func(line string) bool { return strings.Contains(line, "host-1") && strings.Contains(line, "12%") }) {
		t.Fatalf("the selected target keeps its reading: %#v", lines)
	}
	if has(lines, func(line string) bool { return strings.Contains(line, "host-0") }) {
		t.Fatalf("the pane does not settle on the target before it: %#v", lines)
	}
}

// Each notice line belongs to its own row, wherever the table scrolled to:
// it is drawn from the layout the table settled on, not from the row's
// index.
func TestAScrolledTargetsTableKeepsEachNoticeUnderItsOwnRow(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	var targets []core.DeploymentCapacityTarget
	for index := range 4 {
		target := testCapacityTarget()
		id := fmt.Sprintf("host-%d", index)
		target.ID = id
		target.Host = id
		target.TargetIDs = []string{"podman"}
		targets = append(targets, target)
	}
	dashboard.setDeploymentCapacityTargets(targets)
	for index := range uint8(4) {
		usage := hostUsage(10 + index)
		dashboard.applyDeploymentCapacity(fmt.Sprintf("host-%d", index), &usage, nil, renderNowEpochSeconds())
	}
	// Two hosts whose probes failed, one above the other, so a notice drawn
	// from the row index instead of the row's height lands on the wrong
	// line.
	timedOut := "probe timed out"
	for _, host := range []string{"host-1", "host-2"} {
		dashboard.applyDeploymentCapacity(host, nil, &timedOut, renderNowEpochSeconds())
	}
	dashboard.capacityIndex = 3

	lines := drawnTargetsPane(dashboard, 60, 50, 8)
	if slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "host-0") }) {
		t.Fatalf("the table scrolled past its first rows: %#v", lines)
	}
	for _, expected := range []struct {
		index int
		cpu   string
	}{{1, "11%"}, {2, "12%"}} {
		host := fmt.Sprintf("host-%d", expected.index)
		readings := slices.IndexFunc(lines, func(line string) bool {
			return strings.Contains(line, host) && strings.Contains(line, expected.cpu)
		})
		if readings < 0 {
			t.Fatalf("%s keeps its own row: %#v", host, lines)
		}
		notice := lines[readings+1]
		if !strings.Contains(notice, "stale: probe timed out") {
			t.Fatalf("%#v", lines)
		}
		if strings.Index(notice, "stale") != strings.Index(lines[readings], "podman") {
			t.Fatalf("the notice starts under the Targets column: %#v", lines)
		}
	}
	count := 0
	for _, line := range lines {
		if strings.Contains(line, "probe timed out") {
			count++
		}
	}
	if count != 2 {
		t.Fatalf("one notice per failed probe, each on its own line: %#v", lines)
	}
}

// Hard-won: 6dec78fe: An in-flight Resuming row kept showing its source profile and target after the operation had recorded its destination.
func TestResumingRowShowsTheDestinationProfileAndTargetNotTheStaleRecord(t *testing.T) {
	// The controller updates the session's own last_profile/target as soon
	// as a resume starts, but the dashboard's local session snapshot only
	// refreshes once the operation finishes. The in-flight row must show
	// where the resume is going, not where it came from.
	session := stoppedSession()
	if session.LastProfile != "codex-1" || session.TargetTemplateID != "podman" {
		t.Fatalf("fixture %q on %q", session.LastProfile, session.TargetTemplateID)
	}
	resuming := operation(SessionOperationKindResuming, nil)
	resuming.resumeDestination = &ResumeDestination{ProfileID: "grok-1", TargetTemplateID: "localhost"}

	testConfig := config()
	text := sessionMetadataText(&session, nil, &resuming, 1_012, &testConfig)

	if !strings.Contains(text, "grok-1") || !strings.Contains(text, "localhost") {
		t.Fatal(text)
	}
}

// insertActiveStage is Rust's `active_stages.insert(stage, at)` on the
// operation's stage map: the stages stay in stage order and a stage already
// in flight takes the new start.
func insertActiveStage(operation *SessionOperationDisplay, kind core.ProvisionStageKind, at uint64) {
	entry := ProvisionStageStart{Stage: core.ProvisionStage{Kind: kind}, StartedAt: at}
	index, found := slices.BinarySearchFunc(operation.activeStages, entry, func(a, b ProvisionStageStart) int {
		return a.Stage.Compare(b.Stage)
	})
	if found {
		operation.activeStages[index] = entry
	} else {
		operation.activeStages = slices.Insert(operation.activeStages, index, entry)
	}
}

func TestMovePanelElapsedCountsTheOperationAcrossStageChanges(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	now := renderNowSeconds()
	dashboard.BeginSessionOperationAt("session-1", SessionOperationKindMoving, nil, now-125)
	insertActiveStage((*dashboard.sessionOperations.Mut())["session-1"], core.ProvisionStageRestoring, now)
	terminal := newRenderTerminal(160, 40)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	text := strings.Join(bufferLines(terminal.buffer), "\n")
	if !strings.Contains(text, "Current stage: Restore") || !strings.Contains(text, "Elapsed: 2m") {
		t.Fatal(text)
	}
}

// Hard-won: 90c11161: The launch clock paired each new stage name with elapsed time from the whole operation instead of that stage.
func TestStageClockCountsFromWhenTheStageBeganNotTheOperation(t *testing.T) {
	session := stoppedSession()
	booting := core.ProvisionStage{Kind: core.ProvisionStageBooting}
	operation := operation(SessionOperationKindLaunching, &booting)
	// The operation started at 1_000 but the stage only began at 1_040;
	// the clock must count from the stage, not the whole operation.
	insertActiveStage(&operation, core.ProvisionStageBooting, 1_040)

	testConfig := config()
	if text := sessionMetadataText(&session, nil, &operation, 1_052, &testConfig); !strings.Contains(text, "Boot 12s") {
		t.Fatal(text)
	}
}

func TestLaunchClockNamesConcurrentStagesInLifecycleOrder(t *testing.T) {
	session := stoppedSession()
	operation := operation(SessionOperationKindLaunching, nil)
	insertActiveStage(&operation, core.ProvisionStageSyncing, 1_003)
	insertActiveStage(&operation, core.ProvisionStageCloning, 1_002)

	testConfig := config()
	if text := sessionMetadataText(&session, nil, &operation, 1_012, &testConfig); !strings.Contains(text, "Clone, Sync 10s") {
		t.Fatal(text)
	}
}

func TestFocusedPanesUseQuietRoundedBordersAndAccentTitlesWithoutFocusLabels(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	rendered := renderedSymbols(dashboard, 120, 40)
	if !strings.Contains(rendered, "╭ Sessions ") || !strings.Contains(rendered, "Targets") || strings.Contains(rendered, "[focused]") {
		t.Fatal(rendered)
	}

	for _, expected := range []struct {
		focus   Focus
		rounded string
	}{{FocusQuota, "╭ Profiles"}, {FocusTargets, "╭ Targets"}} {
		dashboard.focus = expected.focus
		terminal := newRenderTerminal(120, 40)
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		var symbols strings.Builder
		for y := range 40 {
			for x := range 120 {
				symbols.WriteString(renderCellSymbol(terminal.buffer, x, y))
			}
		}
		rendered := symbols.String()
		if !strings.Contains(rendered, expected.rounded) || strings.Contains(rendered, "[focused]") {
			t.Fatalf("%v: %q", expected.focus, rendered)
		}
		paneIndex := 1
		if expected.focus == FocusQuota {
			paneIndex = 2
		}
		area := dashboard.paneAreas[paneIndex]
		border := renderCellStyle(terminal.buffer, area.Min.X, area.Min.Y)
		if border.Fg != chat.Border(true).Fg {
			t.Fatalf("%v: border colour %#v", expected.focus, border.Fg)
		}
		if border.AddModifier != chat.Border(true).AddModifier {
			t.Fatalf("%v: border modifiers %#v", expected.focus, border.AddModifier)
		}
		if title := renderCellStyle(terminal.buffer, area.Min.X+2, area.Min.Y).Fg; title != chat.ThemePalette().Accent {
			t.Fatalf("%v: title colour %#v", expected.focus, title)
		}
	}
}

// dashboardWithMeasuredSession is one session whose title is title,
// measured at recent permille.
func dashboardWithMeasuredSession(title string, recent uint16) *DashboardState {
	session := runningSession()
	session.Title = title
	session.ACPSessionTitle = nil
	id := session.ID
	dashboard := dashboardWithSession(session)
	setCPU(dashboard, []cpuEntry{{id, recent, recent}})
	return dashboard
}

func rowTexts(dashboard *DashboardState, width int, minimized bool) []string {
	options := sessionRowsRenderOptionsDashboard
	if minimized {
		options = sessionRowsRenderOptionsMinimized
	}
	var texts []string
	for _, row := range drawnSessionRowsWithOptions(dashboard, width, options) {
		for _, line := range row.lines {
			texts = append(texts, line.String())
		}
	}
	return texts
}

// nameAndFollowing is the rows from the session's name line on, past the
// project heading.
func nameAndFollowing(dashboard *DashboardState, width int, minimized bool) []string {
	lines := rowTexts(dashboard, width, minimized)
	start := max(slices.IndexFunc(lines, func(line string) bool { return strings.HasPrefix(line, "›") }), 0)
	return lines[start:]
}

func TestANarrowSessionRowTruncatesTheNameBeforeDroppingCpu(t *testing.T) {
	title := "Refactor the parser to stream tokens"
	dashboard := dashboardWithMeasuredSession(title, 230)
	narrow := nameAndFollowing(dashboard, 30, false)
	if !strings.HasSuffix(narrow[0], "23%") || strings.Contains(narrow[0], title) ||
		!strings.Contains(narrow[0], "Refactor") || !strings.Contains(narrow[0], chat.ThemeGlyphs().Ellipsis) {
		t.Fatalf("%#v", narrow)
	}
	// Too narrow for even a minimal name beside the figure: the figure
	// goes.
	tiny := nameAndFollowing(dashboard, 16, false)
	if slices.ContainsFunc(tiny, func(line string) bool { return strings.Contains(line, "%") }) || !strings.Contains(tiny[0], "Refac") {
		t.Fatalf("%#v", tiny)
	}
}

// localCapacityTargets is a local host target per machine, each listing the
// target of its own name.
func localCapacityTargets(hosts ...string) []core.DeploymentCapacityTarget {
	var targets []core.DeploymentCapacityTarget
	for _, host := range hosts {
		targets = append(targets, core.DeploymentCapacityTarget{
			ID:        host,
			Host:      host,
			TargetIDs: []string{host},
			Kind:      core.DeploymentCapacityKindHost,
			Local:     true,
		})
	}
	return targets
}

// twoMachineCPUReport is an open CPU report over two machines: machine-a
// lists three sessions and machine-b two. Every measured session's recent
// share is 23%.
func twoMachineCPUReport() (*DashboardState, core.SnapshotMap[string, client.SessionCpuView]) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.state.Mut().Sessions.Clear()
	cpu := core.NewSnapshotMap[string, client.SessionCpuView]()
	hourly := func(value uint16) *uint16 { return &value }
	for _, entry := range []struct {
		id, machine string
		hourly      *uint16
	}{
		{"slow", "machine-a", hourly(100)},
		{"fast", "machine-a", hourly(300)},
		{"other", "machine-b", hourly(200)},
		{"missing", "machine-b", nil},
		{"failed", "machine-a", nil},
	} {
		session := runningSession()
		session.ID = entry.id
		session.Title = entry.id
		session.ACPSessionTitle = nil
		session.TargetTemplateID = entry.machine
		dashboard.state.Mut().Sessions.Insert(entry.id, session)
		if entry.hourly != nil {
			cpu.Insert(entry.id, client.SessionCpuView{
				State: client.SessionCpuViewMeasured,
				Usage: core.SessionCpuUsage{
					RecentPermille:    230,
					HourlyPermille:    *entry.hourly,
					HourlyCoveredSecs: 840,
					OnlineCpus:        8,
					Scope:             core.CpuSampleScopeProcessTree,
				},
			})
		}
	}
	cpu.Insert("failed", client.SessionCpuView{State: client.SessionCpuViewUnavailable, Reason: "permission denied"})
	dashboard.SetSessionCPU(cloneCPUViews(cpu))
	dashboard.setDeploymentCapacityTargets(localCapacityTargets("machine-a", "machine-b"))
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	return dashboard, cpu
}

// cloneCPUViews is a copy of views that later inserts do not reach.
func cloneCPUViews(views core.SnapshotMap[string, client.SessionCpuView]) core.SnapshotMap[string, client.SessionCpuView] {
	clone := core.NewSnapshotMap[string, client.SessionCpuView]()
	for id, view := range views.All() {
		clone.Insert(id, *view)
	}
	return clone
}

// modalRows is the rows a modal titled title spans, from its top border to
// its bottom, as [first, last].
func modalRows(t *testing.T, lines []string, title string) [2]int {
	t.Helper()
	top := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, title) })
	if top < 0 {
		t.Fatalf("%s is drawn:\n%s", title, strings.Join(lines, "\n"))
	}
	column := strings.Index(lines[top], "╭")
	if column < 0 {
		t.Fatal("the title row is the top border")
	}
	for row := top + 1; row < len(lines); row++ {
		if column <= len(lines[row]) && strings.HasPrefix(lines[row][column:], "╰") {
			return [2]int{top, row}
		}
	}
	t.Fatal("the modal has a bottom border")
	return [2]int{}
}

func TestSessionCpuReportKeepsItsSizeAcrossTabsAndSwitchesMetric(t *testing.T) {
	dashboard, _ := twoMachineCPUReport()
	terminal := newRenderTerminal(160, 40)
	draw := func() []string {
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		return bufferLines(terminal.buffer)
	}

	first := draw()
	rows := modalRows(t, first, "CPU by session")
	if !strings.Contains(strings.Join(first, "\n"), "fast  [codex-1]") {
		t.Fatal(strings.Join(first, "\n"))
	}
	dashboard.HandleKey(key(tea.KeyRight))
	second := draw()
	if !strings.Contains(strings.Join(second, "\n"), "other  [codex-1]") {
		t.Fatal(strings.Join(second, "\n"))
	}
	if got := modalRows(t, second, "CPU by session"); got != rows {
		t.Fatalf("the shorter machine keeps the dialog's size: %v, was %v", got, rows)
	}

	// The stacked buttons choose the figure the rows, totals and tabs show.
	dashboard.HandleKey(key(tea.KeyLeft))
	text := strings.Join(draw(), "\n")
	for _, expected := range []string{"machine-a · 40%+ hourly", "30% hourly", "(process tree; 14m)"} {
		if !strings.Contains(text, expected) {
			t.Fatalf("%q: %s", expected, text)
		}
	}
	recent := point(draw(), "Recent")
	clickCPUReport(dashboard, recent)
	text = strings.Join(draw(), "\n")
	for _, expected := range []string{"machine-a · 46%+ recent", "machine-b · 23%+ recent", "fast  [codex-1]  23% recent"} {
		if !strings.Contains(text, expected) {
			t.Fatalf("%q: %s", expected, text)
		}
	}
	if strings.Contains(text, "hourly") {
		t.Fatal(text)
	}
	if strings.Contains(text, "(process tree; 14m)") {
		t.Fatalf("the covered period qualifies only the hourly average:\n%s", text)
	}
	hourly := point(draw(), "Hourly")
	clickCPUReport(dashboard, hourly)
	if !strings.Contains(strings.Join(draw(), "\n"), "machine-a · 40%+ hourly") {
		t.Fatal("back to hourly")
	}
}

// renderBefore reports whether first appears in text before second; both must
// appear.
func renderBefore(t *testing.T, text, first, second string) bool {
	t.Helper()
	firstAt, secondAt := strings.Index(text, first), strings.Index(text, second)
	if firstAt < 0 || secondAt < 0 {
		t.Fatalf("%q or %q missing: %s", first, second, text)
	}
	return firstAt < secondAt
}

func TestSessionCpuReportGroupsSortsAndRefreshesWhileOpen(t *testing.T) {
	dashboard, cpu := twoMachineCPUReport()
	terminal := newRenderTerminal(160, 40)
	draw := func() string {
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		return strings.Join(bufferLines(terminal.buffer), "\n")
	}
	text := draw()
	if !renderBefore(t, text, "machine-a ·", "machine-b ·") {
		t.Fatal(text)
	}
	for _, expected := range []string{"machine-a · 40%+ hourly", "machine-b · 20%+ hourly", "permission denied", "(process tree; 14m)"} {
		if !strings.Contains(text, expected) {
			t.Fatalf("%q: %s", expected, text)
		}
	}
	if !renderBefore(t, text, "fast  [codex-1]", "slow  [codex-1]") || !renderBefore(t, text, "slow  [codex-1]", "failed  [codex-1]") {
		t.Fatal(text)
	}
	if strings.Contains(text, "other  [codex-1]") {
		t.Fatalf("the second machine is a tab:\n%s", text)
	}

	// Tabs stay alphabetically ordered, and Right displays the next machine.
	dashboard.HandleKey(key(tea.KeyRight))
	text = draw()
	if !strings.Contains(text, "other  [codex-1]") || !strings.Contains(text, "no CPU data yet") || strings.Contains(text, "fast  [codex-1]") {
		t.Fatal(text)
	}

	// A click on a tab selects it without changing the alphabetic tab
	// order.
	tab := point(strings.Split(text, "\n"), "machine-a ·")
	clickCPUReport(dashboard, tab)
	text = draw()
	if !strings.Contains(text, "fast  [codex-1]") || !renderBefore(t, text, "machine-a ·", "machine-b ·") {
		t.Fatal(text)
	}
	dashboard.AcknowledgeRender()
	cpu.Insert("other", client.SessionCpuView{
		State: client.SessionCpuViewMeasured,
		Usage: core.SessionCpuUsage{
			RecentPermille:    230,
			HourlyPermille:    900,
			HourlyCoveredSecs: 3600,
			OnlineCpus:        8,
			Scope:             core.CpuSampleScopeProcessTree,
		},
	})
	dashboard.SetSessionCPU(cloneCPUViews(cpu))
	if !dashboard.ClockChanged() {
		t.Fatal("an open report must be invalidated by a CPU sample")
	}
	if text := draw(); !renderBefore(t, text, "machine-a ·", "machine-b ·") {
		t.Fatal(text)
	}
}

func clickCPUReport(dashboard *DashboardState, position uv.Position) DashboardAction {
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, position))
	return dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, position))
}

func TestSessionCpuReportOpensOnTheSelectedSessionsMachine(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	first := runningSession()
	first.ID = "machine-a-session"
	first.Title = first.ID
	first.ACPSessionTitle = nil
	first.TargetTemplateID = "machine-a"
	selected := runningSession()
	selected.ID = "machine-b-session"
	selected.Title = selected.ID
	selected.ACPSessionTitle = nil
	selected.TargetTemplateID = "machine-b"
	state := *dashboard.state.Get()
	state.Sessions = core.NewSnapshotMap[string, core.SessionRecord]()
	state.Sessions.Insert(first.ID, first)
	state.Sessions.Insert(selected.ID, selected)
	dashboard.SetState(state)
	dashboard.setDeploymentCapacityTargets(localCapacityTargets("machine-a", "machine-b"))
	dashboard.SelectActiveSession(selected.ID)
	dashboard.DispatchCommand(CommandIdSessionCpuReport)

	text := strings.Join(drawn(dashboard, 120, 32), "\n")
	if !strings.Contains(text, "machine-b-session  [codex-1]") || strings.Contains(text, "machine-a-session  [codex-1]") {
		t.Fatal(text)
	}
}

func measuredCPU(recent, hourly uint16) client.SessionCpuView {
	return client.SessionCpuView{
		State: client.SessionCpuViewMeasured,
		Usage: core.SessionCpuUsage{
			RecentPermille:    recent,
			HourlyPermille:    hourly,
			HourlyCoveredSecs: 3600,
			OnlineCpus:        8,
			Scope:             core.CpuSampleScopeProcessTree,
		},
	}
}

// dashboardWithSubagents is a parent with the given running sub-agents,
// each named by its id.
func dashboardWithSubagents(children []string) (*DashboardState, string) {
	dashboard, parent := dashboardWithOneSubagent()
	state := *dashboard.state.Get()
	template, _ := state.Sessions.Get("child-session")
	relation, _ := state.Subagents.Get("child-session")
	state.Sessions.Remove("child-session")
	state.Subagents.Remove("child-session")
	for _, id := range children {
		child := template
		child.ID = id
		child.Title = id
		child.ACPSessionTitle = nil
		link := relation
		link.ChildSessionID = id
		link.RequestKey = "request-" + id
		state.Sessions.Insert(child.ID, child)
		state.Subagents.Insert(id, link)
	}
	dashboard.SetState(state)
	return dashboard, parent
}

func dashboardWithNestedWorkspaces() (*DashboardState, string) {
	dashboard, root := dashboardWithSubagents([]string{"orchestrator", "worker", "reviewer"})
	state := *dashboard.state.Get()
	for _, titled := range []struct{ id, title string }{
		{root, "Root"},
		{"orchestrator", "Orchestrator"},
		{"worker", "Worker"},
		{"reviewer", "Reviewer"},
	} {
		session := state.Sessions.GetMut(titled.id)
		session.Title = titled.title
		session.ACPSessionTitle = nil
	}
	state.Subagents.GetMut("orchestrator").Orchestrator = true
	for _, id := range []string{"worker", "reviewer"} {
		state.Subagents.GetMut(id).ParentSessionID = "orchestrator"
	}
	state.Subagents.GetMut("reviewer").Kind = core.SubagentKindReviewer
	dashboard.SetState(state)
	serveAttention(dashboard, []string{"orchestrator", "worker", "reviewer"}, core.SessionAttentionWorking)
	dashboard.FocusSessions()
	return dashboard, root
}

func TestGoldenSubagentWorkspaceDepthTwo(t *testing.T) {
	dashboard, root := dashboardWithNestedWorkspaces()
	var output strings.Builder
	renderAppendDashboardGolden(&output, "Top-level Sessions", 108, 26, dashboard)
	dashboard.OpenSubagentWorkspace(root)
	dashboard.OpenSubagentWorkspace("orchestrator")
	renderAppendDashboardGolden(&output, "Depth-two sub-agents", 108, 26, dashboard)
	dashboard.OpenReviewerWorkspace("orchestrator")
	renderAppendDashboardGolden(&output, "Depth-two reviewers", 108, 26, dashboard)
	core.AssertGolden(t, "testdata", "subagent-workspace-depth-two", output.String())
}

// renderOrderedSessionIDs is the ids of the sessions the Sessions pane lists, in
// order.
func renderOrderedSessionIDs(dashboard *DashboardState) []string {
	var ids []string
	for _, session := range dashboard.orderedSessions() {
		ids = append(ids, session.ID)
	}
	return ids
}

// renderRequireSelected fails unless the selected session is want.
func renderRequireSelected(t *testing.T, dashboard *DashboardState, want string) {
	t.Helper()
	if selected, ok := dashboard.SelectedSessionID(); !ok || selected != want {
		t.Fatalf("selected %q (%v), want %q", selected, ok, want)
	}
}

// renderRequireSubagentParent fails unless the open sub-agent workspace is
// want's; an empty want means no workspace is open.
func renderRequireSubagentParent(t *testing.T, dashboard *DashboardState, want string) {
	t.Helper()
	parent, ok := dashboard.SubagentParentID()
	if want == "" {
		if ok {
			t.Fatalf("sub-agent workspace of %q is open", parent)
		}
		return
	}
	if !ok || parent != want {
		t.Fatalf("sub-agent workspace of %q (%v), want %q", parent, ok, want)
	}
}

// renderRequireAction fails unless got is want.
func renderRequireAction(t *testing.T, got, want DashboardAction) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("action %#v, want %#v", got, want)
	}
}

func TestSubagentWorkspaceDepthTwoNavigationAndNarrowBreadcrumb(t *testing.T) {
	dashboard, root := dashboardWithNestedWorkspaces()
	if ids := renderOrderedSessionIDs(dashboard); !slices.Equal(ids, []string{root}) {
		t.Fatalf("%q", ids)
	}
	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyRight)), DashboardActionOpenSubagents{ParentID: root})
	dashboard.OpenSubagentWorkspace(root)
	renderRequireSelected(t, dashboard, "orchestrator")
	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyRight)), DashboardActionOpenSubagents{ParentID: "orchestrator"})
	dashboard.OpenSubagentWorkspace("orchestrator")
	renderRequireSelected(t, dashboard, "worker")

	dashboard.state.Mut().Sessions.GetMut(root).Title = strings.Repeat("大きなルートの名前", 4)
	width := 25
	height := workspacePaneHeight(dashboard, width)
	if height != 3 {
		t.Fatalf("height %d", height)
	}
	narrow := newRenderTerminal(width, height)
	narrow.draw(func(frame *chat.Frame) { renderWorkspaceTabs(frame, frame.Area(), dashboard) })
	lines := bufferLines(narrow.buffer)
	if !strings.Contains(lines[1], "… › Orchestrator") || !strings.HasSuffix(lines[1], "[X]│") {
		t.Fatalf("%#v", lines)
	}

	drawn(dashboard, 108, 26)
	close := dashboard.subagentWorkspaceCloseArea
	renderRequireAction(t,
		dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, uv.Pos(close.Min.X+1, close.Min.Y))),
		DashboardActionExitSubagentWorkspace{})
	dashboard.CloseSubagentWorkspace()
	renderRequireSubagentParent(t, dashboard, root)
	renderRequireSelected(t, dashboard, "orchestrator")
	dashboard.FocusSessions()
	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionExitSubagentWorkspace{})
	dashboard.CloseSubagentWorkspace()
	renderRequireSubagentParent(t, dashboard, "")
	renderRequireSelected(t, dashboard, root)

	dashboard.OpenSubagentWorkspace(root)
	renderRequireAction(t, chord(dashboard, CommandIdOpenSubagents), DashboardActionOpenSubagents{ParentID: "orchestrator"})
	dashboard.OpenSubagentWorkspace("orchestrator")
	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionExitSubagentWorkspace{})
	dashboard.CloseSubagentWorkspace()
	renderRequireSelected(t, dashboard, "orchestrator")
	dashboard.OpenReviewerWorkspace("orchestrator")
	renderRequireSelected(t, dashboard, "reviewer")
	for _, expected := range []string{"orchestrator", root} {
		renderRequireAction(t, dashboard.HandleKey(key('p')), DashboardActionExitSubagentWorkspace{})
		dashboard.CloseSubagentWorkspace()
		renderRequireSelected(t, dashboard, expected)
	}
	renderRequireSubagentParent(t, dashboard, "")
}

// Hard-won: 298330b19: suspended reviewer history inflated the parent's reviewer badge.
func TestReviewerWorkspaceFiltersChildKindsAndReturnsThroughNestedWorkspaces(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"worker-a", "reviewer-a"})
	state := *dashboard.state.Get()
	state.Subagents.GetMut("reviewer-a").Kind = core.SubagentKindReviewer
	dashboard.SetState(state)
	requireOpenReviewers := func(want int) {
		t.Helper()
		if open := dashboard.ReviewerCountsFor(parent).Open; open != want {
			t.Fatalf("%d open reviewers, want %d", open, want)
		}
	}
	requireOpenReviewers(1)
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	if ids := renderOrderedSessionIDs(dashboard); !slices.Equal(ids, []string{"worker-a"}) {
		t.Fatalf("%q", ids)
	}
	dashboard.OpenReviewerWorkspace(parent)
	if ids := renderOrderedSessionIDs(dashboard); !slices.Equal(ids, []string{"reviewer-a"}) {
		t.Fatalf("%q", ids)
	}
	renderRequireSelected(t, dashboard, "reviewer-a")
	if text := strings.Join(drawn(dashboard, 120, 40), "\n"); !strings.Contains(text, "Reviewers") {
		t.Fatal(text)
	}
	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionExitSubagentWorkspace{})
	dashboard.CloseSubagentWorkspace()
	renderRequireSelected(t, dashboard, parent)
	renderRequireSubagentParent(t, dashboard, "")

	// A reviewer's own child workspace returns to Reviewers, not Subagents.
	dashboard.OpenSubagentWorkspace("reviewer-a")
	dashboard.CloseSubagentWorkspace()
	if !dashboard.ReviewersWorkspaceOpen() {
		t.Fatal("back in Reviewers")
	}
	renderRequireSubagentParent(t, dashboard, parent)
	renderRequireSelected(t, dashboard, "reviewer-a")

	// The shared workspace must select a known reviewer relationship before
	// the scoped daemon feed has loaded that reviewer's full session record.
	loaded := *dashboard.state.Get()
	compact := loaded
	compact.Sessions.Remove("reviewer-a")
	dashboard.SetState(compact)
	requireOpenReviewers(0)
	dashboard.OpenReviewerWorkspace(parent)
	requireCurrent := func(want string) {
		t.Helper()
		if current, ok := dashboard.CurrentSessionID(); !ok || current != want {
			t.Fatalf("current %q (%v), want %q", current, ok, want)
		}
	}
	requireCurrent("reviewer-a")
	dashboard.SetState(compact)
	requireCurrent("reviewer-a")
	dashboard.SetState(loaded)
	renderRequireSelected(t, dashboard, "reviewer-a")
	requireOpenReviewers(1)
	dashboard.CloseSubagentWorkspace()
	dashboard.SetOpeningSession(&parent)
	if text := strings.Join(drawn(dashboard, 120, 40), "\n"); !strings.Contains(text, "Reviewers · 1") {
		t.Fatal(text)
	}

	// Lifecycle updates leave the relationship in place for history. The
	// badge follows the session's open state even when that history is
	// loaded.
	closing := *dashboard.state.Get()
	closing.Sessions.GetMut("reviewer-a").State = core.SessionStateClosing
	dashboard.SetState(closing)
	requireOpenReviewers(1)
	stopped := *dashboard.state.Get()
	stopped.Sessions.GetMut("reviewer-a").State = core.SessionStateStopped
	dashboard.SetState(stopped)
	requireOpenReviewers(0)
	if total := dashboard.ReviewerCountsFor(parent).Total; total != 1 {
		t.Fatalf("%d reviewers in total", total)
	}
	if text := strings.Join(drawn(dashboard, 120, 40), "\n"); !strings.Contains(text, "Reviewers · 0") {
		t.Fatal(text)
	}
	if ids := dashboard.managedChildIDsOfKind(parent, core.SubagentKindReviewer); !slices.Equal(ids, []string{"reviewer-a"}) {
		t.Fatalf("%q", ids)
	}

	// Leaving and reopening the scoped history cannot resurrect the count.
	compact = stopped
	compact.Sessions.Remove("reviewer-a")
	dashboard.SetState(compact)
	requireOpenReviewers(0)
	dashboard.SetState(stopped)
	requireOpenReviewers(0)
}

// cpuEntry is one session's measured CPU: its id, recent and hourly
// permille.
type cpuEntry struct {
	id             string
	recent, hourly uint16
}

func setCPU(dashboard *DashboardState, entries []cpuEntry) {
	cpu := core.NewSnapshotMap[string, client.SessionCpuView]()
	for _, entry := range entries {
		cpu.Insert(entry.id, measuredCPU(entry.recent, entry.hourly))
	}
	dashboard.SetSessionCPU(cpu)
}

func TestSessionCpuReportExcludesParkedStoppedAndFailedSubagents(t *testing.T) {
	dashboard, _ := dashboardWithSubagents([]string{"parked-child", "stopped-child", "error-child", "active-child"})
	state := *dashboard.state.Get()
	state.Sessions.GetMut("parked-child").State = core.SessionStateParked
	state.Sessions.GetMut("stopped-child").State = core.SessionStateStopped
	state.Sessions.GetMut("error-child").State = core.SessionStateError
	dashboard.SetState(state)

	var ids []string
	for _, group := range reportGroups(dashboard, CpuMetricHourly) {
		ids = append(ids, group.sessionIDs...)
	}
	if !slices.Contains(ids, "session-1") {
		t.Fatalf("parent is active: %q", ids)
	}
	if !slices.Contains(ids, "active-child") {
		t.Fatalf("active child: %q", ids)
	}
	for _, excluded := range []string{"parked-child", "stopped-child", "error-child"} {
		if slices.Contains(ids, excluded) {
			t.Fatalf("%q", ids)
		}
	}
}

// cpuReportDialog is the open CPU report, failing when none is.
func cpuReportDialog(t *testing.T, dashboard *DashboardState, message string) *SessionCpuReportDialog {
	t.Helper()
	mode, ok := dashboard.mode.(ModeSessionCpuReport)
	if !ok {
		t.Fatal(message)
	}
	return mode.SessionCpuReportDialog
}

func TestSessionCpuReportSelectionStaysWithItsSessionAfterCpuResort(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	state := *dashboard.state.Get()
	state.Sessions = core.NewSnapshotMap[string, core.SessionRecord]()
	for _, id := range []string{"fast", "middle", "slow"} {
		session := runningSession()
		session.ID = id
		session.Title = id
		session.ACPSessionTitle = nil
		session.TargetTemplateID = "shared-machine"
		state.Sessions.Insert(id, session)
	}
	dashboard.SetState(state)
	setCPU(dashboard, []cpuEntry{{"fast", 300, 300}, {"middle", 200, 200}, {"slow", 100, 100}})
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	dashboard.HandleKey(key(tea.KeyEnd))
	dialog := cpuReportDialog(t, dashboard, "CPU report is open")
	if dialog.selectedSessionID == nil || *dialog.selectedSessionID != "slow" {
		t.Fatalf("selected %v", dialog.selectedSessionID)
	}

	setCPU(dashboard, []cpuEntry{{"fast", 50, 50}, {"middle", 200, 200}, {"slow", 500, 500}})
	drawn(dashboard, 120, 30)
	dialog = cpuReportDialog(t, dashboard, "CPU report remains open")
	if dialog.selectedSessionID == nil || *dialog.selectedSessionID != "slow" {
		t.Fatalf("the cursor follows the selected id through re-sorting: %v", dialog.selectedSessionID)
	}
	if dialog.selectedRow != 0 {
		t.Fatalf("selected row %d", dialog.selectedRow)
	}
}

func TestEnterOnACpuReportTopLevelRowOpensItsSession(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	sessionID, _ := dashboard.SelectedSessionID()
	dashboard.DispatchCommand(CommandIdSessionCpuReport)

	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: sessionID})
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	renderRequireSelected(t, dashboard, sessionID)
}

func TestEnterOnACpuReportSubagentOpensItsParentViewAndConversation(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a"})
	// A filter that would hide the child is cleared by going to it.
	dashboard.sessionsFilter.Set(&SessionsFilter{Query: chat.TextInputFromValue("matches nothing")})
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	dashboard.HandleKey(key(tea.KeyDown))

	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: "kid-a"})
	renderRequireSubagentParent(t, dashboard, parent)
	renderRequireSelected(t, dashboard, "kid-a")
	if *dashboard.sessionsFilter.Get() != nil {
		t.Fatal("the filter was cleared")
	}
}

func TestDoubleClickOnACpuReportSubagentOpensItsParentView(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a"})
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	first := drawn(dashboard, 120, 35)
	childRow := point(first, "kid-a")
	renderRequireAction(t, clickCPUReport(dashboard, childRow), DashboardActionNone{})
	dialog := cpuReportDialog(t, dashboard, "first click keeps the CPU report open")
	if dialog.selectedSessionID == nil || *dialog.selectedSessionID != "kid-a" {
		t.Fatalf("selected %v", dialog.selectedSessionID)
	}
	drawn(dashboard, 120, 35)

	renderRequireAction(t, clickCPUReport(dashboard, childRow), DashboardActionOpen{SessionID: "kid-a"})
	renderRequireSubagentParent(t, dashboard, parent)
	renderRequireSelected(t, dashboard, "kid-a")
}

func TestCpuReportSubagentNavigationRestoresTheParentViewAfterWorkspaceSwitch(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a"})
	state := *dashboard.state.Get()
	state.Sessions.GetMut(parent).WorkspaceID = "other"
	state.Sessions.GetMut("kid-a").WorkspaceID = "other"
	dashboard.SetState(state)
	dashboard.workspaceOrder = append(dashboard.workspaceOrder, "other")
	if dashboard.workspaceNames == nil {
		dashboard.workspaceNames = map[string]string{}
	}
	dashboard.workspaceNames["other"] = "Other"
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	dashboard.HandleKey(key(tea.KeyDown))

	renderRequireAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionSelectWorkspace{WorkspaceID: "other"})
	if dashboard.pendingNavigation == nil || dashboard.pendingNavigation.sessionID != "kid-a" {
		t.Fatalf("pending navigation %#v", dashboard.pendingNavigation)
	}
	if dashboard.navigationSubagentParent == nil || *dashboard.navigationSubagentParent != parent {
		t.Fatalf("navigation parent %v", dashboard.navigationSubagentParent)
	}

	other := "other"
	dashboard.SetActiveWorkspace(&other)
	renderRequireSubagentParent(t, dashboard, parent)
	renderRequireSelected(t, dashboard, "kid-a")
	if opened, ok := dashboard.TakeSessionToOpen(); !ok || opened != "kid-a" {
		t.Fatalf("session to open %q (%v)", opened, ok)
	}
}

func TestParentCpuIsItsOwnPlusEveryLiveSubagent(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a", "kid-b"})
	setCPU(dashboard, []cpuEntry{{parent, 30, 20}, {"kid-a", 400, 300}, {"kid-b", 250, 100}})
	share, ok := dashboard.cpuShare(parent)
	if !ok || share.permille != 680 || share.partial {
		t.Fatalf("share %#v (%v)", share, ok)
	}
	if hourly := dashboard.cpuRollup(parent).hourlyPermille; hourly != 420 {
		t.Fatalf("hourly %d", hourly)
	}
	// A child's own figure is its own, not folded into anything.
	child, ok := dashboard.cpuShare("kid-a")
	if !ok || child.permille != 400 || child.partial {
		t.Fatalf("child share %#v (%v)", child, ok)
	}
}

func TestParentCpuIsMarkedPartialWhenASubagentIsUnmeasured(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a", "kid-b"})
	setCPU(dashboard, []cpuEntry{{parent, 30, 20}, {"kid-a", 400, 300}})
	share, ok := dashboard.cpuShare(parent)
	if !ok || share.permille != 430 || !share.partial {
		t.Fatalf("share %#v (%v)", share, ok)
	}
	if label := share.label(); label != "43%+" {
		t.Fatal(label)
	}
	rollup := dashboard.cpuRollup(parent)
	if rollup.measured != 2 || rollup.members != 3 {
		t.Fatalf("rollup %#v", rollup)
	}
	// Nothing measured anywhere in the tree shows nothing.
	setCPU(dashboard, nil)
	if _, ok := dashboard.cpuShare(parent); ok {
		t.Fatal("an unmeasured tree has a share")
	}
}

func TestCpuRollupIsDerivedOnlyWhenSamplesOrTheTreeChange(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a"})
	setCPU(dashboard, []cpuEntry{{parent, 30, 20}, {"kid-a", 400, 300}})
	terminal := newRenderTerminal(120, 30)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	dashboard.ClockChanged()
	settled := dashboard.cpuRollupDerivations()
	for range 3 {
		terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
		dashboard.ClockChanged()
	}
	if derivations := dashboard.cpuRollupDerivations(); derivations != settled {
		t.Fatalf("%d derivations, was %d", derivations, settled)
	}
	setCPU(dashboard, []cpuEntry{{parent, 30, 20}, {"kid-a", 500, 300}})
	if share, _ := dashboard.cpuShare(parent); share.permille != 530 {
		t.Fatalf("share %d", share.permille)
	}
	if derivations := dashboard.cpuRollupDerivations(); derivations != settled+1 {
		t.Fatalf("%d derivations, want %d", derivations, settled+1)
	}
	state := *dashboard.state.Get()
	state.Subagents.Remove("kid-a")
	dashboard.SetState(state)
	if share, _ := dashboard.cpuShare(parent); share.permille != 30 {
		t.Fatalf("share %d", share.permille)
	}
	if derivations := dashboard.cpuRollupDerivations(); derivations != settled+2 {
		t.Fatalf("%d derivations, want %d", derivations, settled+2)
	}
}

// reportLineTexts is the CPU report's lines for metric, as text.
func reportLineTexts(dashboard *DashboardState, metric CpuMetric) []string {
	var lines []string
	for _, line := range reportLines(dashboard, metric) {
		lines = append(lines, line.String())
	}
	return lines
}

// Hard-won: b7d5c8399: process-tree sampling omitted shared-container work and could double count children.
func TestSessionCpuReportNestsSubagentsUnderTheirParentWithTheTreeTotal(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"kid-a", "kid-b"})
	setCPU(dashboard, []cpuEntry{{parent, 30, 20}, {"kid-a", 400, 300}})
	dashboard.DispatchCommand(CommandIdSessionCpuReport)
	terminal := newRenderTerminal(140, 30)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, dashboard) })
	t.Log(strings.Join(bufferLines(terminal.buffer), "\n"))
	lines := reportLineTexts(dashboard, CpuMetricHourly)
	parentLine := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "tree of 3") })
	if parentLine < 0 {
		t.Fatalf("%#v", lines)
	}
	if !strings.Contains(lines[parentLine], "32% hourly+") && !strings.Contains(lines[parentLine], "32%+ hourly") {
		t.Fatalf("%#v", lines)
	}
	// The legacy process-tree figure remains distinct from the tree total.
	if !strings.Contains(lines[parentLine], "process tree 2.0%, 2 of 3 measured") {
		t.Fatalf("%#v", lines)
	}
	recent := reportLineTexts(dashboard, CpuMetricRecent)
	if !strings.Contains(recent[parentLine], "43%+ recent (tree of 3; process tree 3.0%, 2 of 3 measured)") {
		t.Fatalf("%#v", recent)
	}
	if !strings.HasPrefix(lines[parentLine+1], "    kid-a") || !strings.HasPrefix(lines[parentLine+2], "    kid-b") ||
		!strings.Contains(lines[parentLine+2], "no CPU data yet") {
		t.Fatalf("%#v", lines)
	}
	golden := strings.Join(lines, "\n")
	state := *dashboard.state.Get()
	for _, borrowed := range []struct {
		id           string
		borrowedFrom *string
	}{{parent, nil}, {"kid-a", &parent}} {
		state.Sessions.GetMut(borrowed.id).Target = core.TargetLocatorLocalDocker{
			ContainerID:  "shared-container",
			BorrowedFrom: borrowed.borrowedFrom,
		}
	}
	// A separately sampled child, including after moving away from the
	// shared container.
	state.Sessions.GetMut("kid-b").Target = core.TargetLocatorLocalBare{WorkerRoot: "/independent"}
	dashboard.SetState(state)
	cpu := map[string]client.SessionCpuView{}
	for id, view := range dashboard.sessionCPU.Get().All() {
		cpu[id] = *view
	}
	setScope := func(id string, scope core.CpuSampleScope) {
		if view := cpu[id]; view.State == client.SessionCpuViewMeasured {
			view.Usage.Scope = scope
			cpu[id] = view
		}
	}
	publish := func() {
		views := core.NewSnapshotMap[string, client.SessionCpuView]()
		for id, view := range cpu {
			views.Insert(id, view)
		}
		dashboard.SetSessionCPU(views)
	}
	setScope(parent, core.CpuSampleScopeContainer)
	delete(cpu, "kid-a")
	cpu["kid-b"] = measuredCPU(200, 100)
	publish()
	report := func() string { return strings.Join(reportLineTexts(dashboard, CpuMetricHourly), "\n") }
	golden += "\n\nShared container and independent child\n" + report()
	if hourly := dashboard.cpuRollup(parent).hourlyPermille; hourly != 120 {
		t.Fatalf("hourly %d", hourly)
	}
	if dashboard.cpuRollup(parent).isPartial() {
		t.Fatal("the rollup is complete")
	}
	if share, _ := dashboard.cpuShare("kid-a"); share.label() != "included in parent" {
		t.Fatal(share.label())
	}
	if summary := cpuSummary(dashboard, "kid-a"); summary.main != "included in parent" {
		t.Fatal(summary.main)
	}
	// A still-running old child sample is covered by an upgraded container
	// owner.
	cpu["kid-a"] = measuredCPU(400, 300)
	publish()
	if hourly := dashboard.cpuRollup(parent).hourlyPermille; hourly != 120 {
		t.Fatalf("hourly %d", hourly)
	}
	setScope("kid-a", core.CpuSampleScopeContainer)
	publish()
	sharedState := *dashboard.state.Get()
	moved := sharedState
	moved.Sessions.GetMut(parent).Target = core.TargetLocatorLocalDocker{ContainerID: "destination-container"}
	dashboard.SetState(moved)
	golden += "\n\nOwner moved; source child sampled independently\n" + report()
	if hourly := dashboard.cpuRollup(parent).hourlyPermille; hourly != 420 {
		t.Fatalf("hourly %d", hourly)
	}
	if _, included := dashboard.cpuIncludedIn("kid-a"); included {
		t.Fatal("kid-a is sampled on its own")
	}
	remote := *dashboard.state.Get()
	remote.Sessions.GetMut("kid-b").TargetTemplateID = "other-machine"
	dashboard.SetState(remote)
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{{
		ID:        "other-machine",
		Host:      "other-machine",
		TargetIDs: []string{"other-machine"},
		Kind:      core.DeploymentCapacityKindHost,
	}})
	golden += "\n\nIndependent child on another machine\n" + report()
	groups := reportGroups(dashboard, CpuMetricHourly)
	remoteGroup := slices.IndexFunc(groups, func(group MachineReport) bool { return group.key == "other-machine" })
	if remoteGroup < 0 {
		t.Fatal("the other machine's group")
	}
	if groups[remoteGroup].totalPermille != 100 || !slices.Equal(groups[remoteGroup].sessionIDs, []string{"kid-b"}) {
		t.Fatalf("%#v", groups[remoteGroup])
	}
	dashboard.setDeploymentCapacityTargets(nil)

	dashboard.SetState(sharedState)
	delete(cpu, parent)
	publish()
	golden += "\n\nBorrower supplies shared container sample\n" + report()
	if hourly := dashboard.cpuRollup(parent).hourlyPermille; hourly != 400 {
		t.Fatalf("hourly %d", hourly)
	}
	parked := sharedState
	parked.Sessions.GetMut(parent).State = core.SessionStateParked
	dashboard.SetState(parked)
	golden += "\n\nOwner parked; borrower still sampled\n" + report()
	dashboard.SetState(sharedState)
	cpu[parent] = measuredCPU(30, 20)
	delete(cpu, "kid-a")
	setScope(parent, core.CpuSampleScopeProcessTree)
	publish()
	golden += "\n\nLegacy owner does not cover borrowed child\n" + report()
	clear(cpu)
	publish()
	golden += "\n\nUnknown measurements\n" + report() + "\n"
	core.AssertGolden(t, "testdata", "session-cpu-scopes", golden)
}

// Hard-won: c17d3225: A stale table offset hid fitting session rows after a list shrank.
func TestSessionsPaneShowsEveryRowFromTheTopAfterTheListShrank(t *testing.T) {
	dashboard, _ := dashboardWithSubagents([]string{"worker-a", "worker-b"})
	// An earlier, longer list had scrolled the pane down.
	dashboard.sessionsScroll = 3
	text := strings.Join(drawn(dashboard, 120, 50), "\n")
	if !strings.Contains(text, "ACP pretty name") {
		t.Fatal(text)
	}
	if dashboard.sessionsScroll != 0 {
		t.Fatalf("scroll %d", dashboard.sessionsScroll)
	}
}

// The Sub-agents view is the Sessions pane listing only a parent's
// children. A render that kept an offset from a longer list hid the first
// children although all of them fit.
//
// Hard-won: c17d3225: A stale parent-list offset hid children and made a running child look parked after switching to Sub-agents.
func TestSubagentsPaneShowsEveryChildFromTheTopWithAStaleOffset(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"worker-a", "worker-b", "worker-c"})
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	dashboard.SelectActiveSession("worker-c")
	dashboard.sessionsScroll = 2
	text := strings.Join(drawn(dashboard, 120, 60), "\n")
	for _, id := range []string{"worker-a", "worker-b", "worker-c"} {
		if !strings.Contains(text, id) {
			t.Fatalf("%s hidden:\n%s", id, text)
		}
	}
	if dashboard.sessionsScroll != 0 {
		t.Fatalf("scroll %d", dashboard.sessionsScroll)
	}
}

// rowOfTitle is the screen row of the line that draws title in the
// Sessions pane; -1 when none does.
func rowOfTitle(lines []string, title string) int {
	return slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, title) })
}

// childNames is child-00 through child-(count-1).
func childNames(count int) []string {
	names := make([]string, count)
	for n := range names {
		names[n] = fmt.Sprintf("child-%02d", n)
	}
	return names
}

func TestSessionsPaneKeepsTheKeyboardSelectionCentredAndPinsTheEnds(t *testing.T) {
	dashboard, parent := dashboardWithSubagents(childNames(30))
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	dashboard.SelectActiveSession("child-00")
	dashboard.focus = FocusSessions
	lines := drawn(dashboard, 120, 40)
	first := rowOfTitle(lines, "child-00")
	if first < 0 {
		t.Fatal("first child drawn at the top")
	}
	if dashboard.sessionsScroll != 0 {
		t.Fatalf("scroll %d", dashboard.sessionsScroll)
	}

	// Walk down one key at a time. Once the selection passes the middle it
	// holds one screen row until the list pins to its end.
	var rows, offsets []int
	for range 29 {
		dashboard.moveSelection(1)
		lines := drawn(dashboard, 120, 40)
		offsets = append(offsets, dashboard.sessionsScroll)
		selected, ok := dashboard.SelectedSessionID()
		if !ok {
			t.Fatal("selection")
		}
		session, _ := dashboard.state.Get().Sessions.Get(selected)
		row := rowOfTitle(lines, session.Title)
		if row < 0 {
			t.Fatal("selected child stays visible")
		}
		rows = append(rows, row)
	}
	for i := 1; i < len(offsets); i++ {
		if offsets[i-1] > offsets[i] {
			t.Fatalf("%v", offsets)
		}
	}
	// The most frequent row; Rust's max_by_key keeps the last of equals.
	middle, held := 0, 0
	for _, candidate := range rows {
		count := 0
		for _, row := range rows {
			if row == candidate {
				count++
			}
		}
		if count >= held {
			middle, held = candidate, count
		}
	}
	if held < 10 {
		t.Fatalf("selection holds the centre row: %v", rows)
	}
	if middle <= first+3 {
		t.Fatalf("%v", rows)
	}
	// At the end the last child is the last row, so the selection has moved
	// below the centre.
	if rows[len(rows)-1] <= middle {
		t.Fatalf("%v", rows)
	}
	// Moving back up to the first child returns the list to its top.
	for range 29 {
		dashboard.moveSelection(-1)
		drawn(dashboard, 120, 40)
	}
	if dashboard.sessionsScroll != 0 {
		t.Fatalf("scroll %d", dashboard.sessionsScroll)
	}
}

// The wheel scrolls the Sessions list and leaves the selection where it
// was, even when that scrolls the selected row out of view.
func TestSessionsWheelScrollsPastTheSelectionUntilItChanges(t *testing.T) {
	dashboard, parent := dashboardWithSubagents(childNames(30))
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	dashboard.SelectActiveSession("child-00")
	dashboard.focus = FocusSessions
	drawn(dashboard, 120, 40)

	for range 8 {
		dashboard.scrollSessionsView(1)
	}
	lines := drawn(dashboard, 120, 40)
	renderRequireSelected(t, dashboard, "child-00")
	if rowOfTitle(lines, "child-00") >= 0 {
		t.Fatalf("%#v", lines)
	}
	scrolled := dashboard.sessionsScroll
	if scrolled <= 0 {
		t.Fatal("the wheel scrolled")
	}

	// Later frames keep the wheel's offset.
	drawn(dashboard, 120, 40)
	if dashboard.sessionsScroll != scrolled {
		t.Fatalf("scroll %d, was %d", dashboard.sessionsScroll, scrolled)
	}

	// Choosing the row that is already selected, as Home does, brings it
	// back.
	dashboard.setSelectionFor(FocusSessions, 0)
	if lines := drawn(dashboard, 120, 40); rowOfTitle(lines, "child-00") < 0 {
		t.Fatalf("%#v", lines)
	}

	// Moving the selection brings it back into view.
	for range 8 {
		dashboard.scrollSessionsView(1)
	}
	drawn(dashboard, 120, 40)
	dashboard.moveSelection(1)
	lines = drawn(dashboard, 120, 40)
	renderRequireSelected(t, dashboard, "child-01")
	if rowOfTitle(lines, "child-01") < 0 {
		t.Fatalf("%#v", lines)
	}
}

// Hard-won: review of the wheel change: the table scrolled a partly visible
// selected row into view, so click areas pointed at other sessions.
func TestSessionsWheelScrollKeepsClickAreasOnTheRowsDrawn(t *testing.T) {
	dashboard, parent := dashboardWithSubagents(childNames(30))
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	dashboard.SelectActiveSession("child-29")
	dashboard.focus = FocusSessions
	// Heights that leave the selected row cut off at the bottom for some
	// offsets.
	for _, height := range []int{33, 34, 35, 36, 37} {
		dashboard.sessionsWheelScrolledFrom = nil
		dashboard.SelectActiveSession("child-29")
		drawn(dashboard, 120, height)
		for range 28 {
			dashboard.scrollSessionsView(-1)
			lines := drawn(dashboard, 120, height)
			for _, area := range dashboard.sessionRowAreas {
				title := dashboard.orderedSessions()[area.First].Title
				if !strings.Contains(lines[area.Second.Min.Y], title) {
					t.Fatalf("height %d: area for %s shows %q", height, title, lines[area.Second.Min.Y])
				}
			}
		}
	}
}

// drawSessionsAlone draws the Sessions pane over the whole terminal, laid
// out for its content width.
func drawSessionsAlone(terminal *renderTerminal, dashboard *DashboardState) string {
	terminal.draw(func(frame *chat.Frame) {
		layout := newSessionsLayout(dashboard, frame.Area().Dx()-2)
		renderSessions(frame, frame.Area(), dashboard, &layout)
	})
	return strings.Join(bufferLines(terminal.buffer), "\n")
}

// Hard-won: review of the wheel change: with a pane shorter than the last
// session row the wheel scrolled past every session and drew an empty list.
func TestSessionsWheelStopsAtTheLastSessionInAShortPane(t *testing.T) {
	dashboard, parent := dashboardWithSubagents([]string{"worker-a", "worker-b"})
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.clearSessionsFilter()
	dashboard.SelectActiveSession("worker-a")
	dashboard.focus = FocusSessions
	// Room for the borders, the action row and two lines of a row.
	terminal := newRenderTerminal(60, 5)
	var rows string
	for range 10 {
		dashboard.scrollSessionsView(1)
		rows = drawSessionsAlone(terminal, dashboard)
	}
	if !strings.Contains(rows, "worker-b") || strings.Contains(rows, "No sessions") {
		t.Fatal(rows)
	}

	// With the last session selected and taller than the pane, the wheel
	// must not hand the table a selection it would scroll past.
	dashboard.SelectActiveSession("worker-b")
	for _, delta := range []int{-1, 1, 1, -1} {
		dashboard.scrollSessionsView(delta)
		rows := drawSessionsAlone(terminal, dashboard)
		if strings.Contains(rows, "No sessions") || (!strings.Contains(rows, "worker-a") && !strings.Contains(rows, "worker-b")) {
			t.Fatal(rows)
		}
	}
}

// renderDebugText is text already in Rust's Debug form.
type renderDebugText string

func (text renderDebugText) GoString() string { return string(text) }

// renderRectDebug is ratatui's Rect Debug.
func renderRectDebug(rect uv.Rectangle) renderDebugText {
	return renderDebugText(fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }",
		rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy()))
}

// renderVariantDebug is the Debug of a unit enum variant ported as an empty
// struct: its type name without the enum's prefix (DashboardActionNone is
// `None`, KeyRouteConsumed `Consumed`).
func renderVariantDebug(value any, enum string) renderDebugText {
	name := reflect.TypeOf(value).Name()
	return renderDebugText(strings.TrimPrefix(name, enum))
}

func TestGoldenDashboardSessionRender(t *testing.T) {
	var output strings.Builder
	queuedPrompt := func(id, text string) core.QueuedPrompt {
		return core.QueuedPrompt{
			ID:          id,
			Kind:        core.QueuedCommandKindPrompt{},
			Text:        text,
			Attachments: []core.Attachment{},
			CreatedAtMs: 1,
		}
	}

	dashboard := dashboardWithSession(runningSession())
	applyMaterializedTranscript(dashboard, numberedConversation(2))
	clearSessionActivity(dashboard, "session-1")
	dashboard.SetSessionAttentionFor("session-1", core.SessionAttentionUnread)
	detail := (*dashboard.sessionDetails.Mut())["session-1"]
	detail.queuedPrompts = append(detail.queuedPrompts, queuedPrompt("queued-1", "later"))
	renderAppendDashboardGolden(&output, "grouped session summary", 120, 30, dashboard)

	for _, symbolCase := range []struct {
		symbols   core.SymbolSet
		stateName string
	}{{core.SymbolSetUnicode, "Unicode"}, {core.SymbolSetAscii, "ASCII"}} {
		close, dot := "x", "-"
		if symbolCase.symbols == core.SymbolSetUnicode {
			close, dot = "×", "·"
		}
		dashboard := dashboardWithSession(runningSession())
		config := *dashboard.config.Get()
		symbols := symbolCase.symbols
		config.Advanced.Symbols = &symbols
		dashboard.SetConfig(config)
		clearSessionActivity(dashboard, "session-1")
		dashboard.FocusSessions()
		name := symbolCase.stateName
		renderAppendDashboardGolden(&output, name+" unfiltered Sessions title", 120, 40, dashboard)
		dashboard.HandleKey(key('w'))
		renderAppendDashboardGolden(&output, name+" working filter", 120, 40, dashboard)
		dashboard.HandleKey(key('b'))
		renderAppendDashboardGolden(&output, name+" blocked filter at wide size", 240, 40, dashboard)
		dashboard.HandleKey(key(tea.KeyEscape))
		renderAppendDashboardGolden(&output, name+" cleared filter", 120, 40, dashboard)
		renderAppendGoldenValue(&output, name+" title markers", renderDebugText(fmt.Sprintf("('%s', '%s')", close, dot)))
	}

	for _, sideCase := range []struct {
		width int
		side  core.SessionsSide
		label string
	}{
		{80, core.SessionsSideLeft, "80 left"},
		{80, core.SessionsSideRight, "80 right"},
		{120, core.SessionsSideLeft, "120 left"},
		{120, core.SessionsSideRight, "120 right"},
		{180, core.SessionsSideLeft, "180 left"},
		{180, core.SessionsSideRight, "180 right"},
	} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.config.Mut().SessionsSide = sideCase.side
		detail, ok := (*dashboard.sessionDetails.Mut())["session-1"]
		if !ok {
			t.Fatal("fixture detail")
		}
		detail.queuedPrompts = append(detail.queuedPrompts, queuedPrompt("queued", "follow-up"))
		renderAppendDashboardGolden(&output, "expanded rows, "+sideCase.label, sideCase.width, 40, dashboard)
	}
	dashboard = dashboardWithSession(runningSession())
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	renderAppendDashboardGolden(&output, "minimized rows at 80 columns", 80, 40, dashboard)

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	renderAppendDashboardGolden(&output, "expanded session row", 120, 44, dashboard)
	dashboard.sessionsFilter.Set(&SessionsFilter{Query: chat.TextInputFromValue("quokka"), Editing: true})
	dashboard.NextSessionsTextSearch()
	episode, ok := dashboard.WikiSyncEpisode(false)
	if !ok {
		t.Fatal("a sync episode")
	}
	target, total := uint64(1), 100
	dashboard.ApplyWikiSync(false, episode, client.WikiSyncStatus{
		Target:    &target,
		Requested: 1,
		Started:   1,
		Running: &client.WikiSyncProgress{
			Phase:    "Native",
			Tool:     "Codex",
			Adapter:  2,
			Adapters: 3,
			Done:     50,
			Total:    &total,
		},
	}, nil)
	renderAppendDashboardGolden(&output, "Sessions filter sync progress", 120, 44, dashboard)
	queued := dashboard.sessionsText.Get().sync.Status.Ok
	queuedTarget := uint64(2)
	queued.Target = &queuedTarget
	queued.Requested = 2
	queued.Started = 1
	// Hard-won: 18753da4d: earlier-pass counts were displayed for a queued successor.
	dashboard.ApplyWikiSync(false, episode, queued, nil)
	renderAppendDashboardGolden(&output, "Queued Sessions filter sync", 120, 44, dashboard)
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	renderAppendDashboardGolden(&output, "Minimized queued Sessions filter sync", 120, 44, dashboard)
	queued.Started = 2
	dashboard.ApplyWikiSync(false, episode, queued, nil)
	// Hard-won: 538a5336e: minimized Sessions clipped the bar, counts and error cause.
	renderAppendDashboardGolden(&output, "Minimized Sessions filter sync progress", 120, 44, dashboard)
	dashboard.ApplyWikiSync(false, episode, client.WikiSyncStatus{}, fmt.Errorf("database is locked"))
	renderAppendDashboardGolden(&output, "Minimized Sessions filter sync failure", 120, 44, dashboard)

	finishedTarget := uint64(2)
	dashboard.ApplyWikiSync(false, episode, client.WikiSyncStatus{
		Epoch:      "fixture",
		Target:     &finishedTarget,
		Requested:  2,
		Started:    2,
		Finished:   2,
		Successful: 2,
	}, nil)
	refresh, _, ok := dashboard.RefreshSessionsTextSearch()
	if !ok {
		t.Fatal("a final read")
	}
	renderAppendDashboardGolden(&output, "Minimized Sessions filter final read", 120, 44, dashboard)
	dashboard.ApplySessionsText(refresh, nil, fmt.Errorf("final index read failed"))
	renderAppendDashboardGolden(&output, "Minimized Sessions filter final read failure", 120, 44, dashboard)

	dashboard = minimizedSessionsDashboard(3, 2)
	renderAppendDashboardGolden(&output, "minimized session summaries", 120, 44, dashboard)

	dashboard = dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	usage := hostUsage(42)
	dashboard.applyDeploymentCapacity("local", &usage, nil, renderNowSeconds())
	dashboard.ApplyQuota(weeklyQuota("claude-1", 63))
	minimizeAllPanes(dashboard)
	renderAppendDashboardGolden(&output, "minimized CPU and weekly quota", 120, 44, dashboard)

	dashboard = dashboardWithSession(runningSession())
	dashboard.ApplyQuota(weeklyAndFiveHourQuota("claude-1", 96, 40))
	dashboard.ApplyQuota(weeklyAndFiveHourQuota("codex-1", 100, 40))
	dashboard.ApplyQuota(weeklyQuota("codex-2", 63))
	minimizeAllPanes(dashboard)
	renderAppendDashboardGolden(&output, "paired weekly and five-hour quota", 160, 44, dashboard)

	core.AssertGolden(t, "testdata", "dashboard-session-render", output.String())
}

func TestGoldenDashboardLayoutRender(t *testing.T) {
	var output strings.Builder
	notifications := publishedDashboard()
	renderAppendDashboardGolden(&output, "older important card before a newer question", 120, 44, notifications)
	notifications.notificationSkip()
	for _, character := range "I'll investigate." {
		notifications.HandleKey(key(character))
	}
	renderAppendDashboardGolden(&output, "multi-entry question notification card", 120, 44, notifications)
	terminal := newRenderTerminal(120, 44)
	terminal.draw(func(frame *chat.Frame) { renderRender(frame, notifications) })
	lines := chat.BufferLines(terminal.buffer)
	headerRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "? Newest question ·") })
	if headerRow < 0 {
		t.Fatal("notification header")
	}
	headerColumn := cellColumn(lines[headerRow], "? Newest question ·")
	renderAppendGoldenValue(&output, "notification header color", renderCellStyle(terminal.buffer, headerColumn, headerRow).Fg)

	dashboard := dashboardWithSession(runningSession())
	dashboard.SetWorkspaceName("UNDERLYING DASHBOARD SENTINEL")
	dashboard.FocusSessions()
	openPalette(dashboard)
	for _, character := range "rename" {
		dashboard.HandleKey(key(character))
	}
	action := dashboard.HandleKey(key(tea.KeyEnter))
	renderAppendGoldenValue(&output, "open Rename action", renderVariantDebug(action, "DashboardAction"))
	renderAppendDashboardGolden(&output, "Rename modal over dashboard", 120, 30, dashboard)

	transcriptArea := func(dashboard *DashboardState) renderDebugText {
		area, ok := dashboard.focusedTranscriptArea()
		if !ok {
			t.Fatal("a transcript area")
		}
		return renderRectDebug(area)
	}
	for _, height := range []int{32, 44} {
		dashboard := minimizedSessionsDashboard(3, 2)
		if err := dashboard.RestorePaneSizes(core.NewPaneSizes()); err != nil {
			t.Fatal(err)
		}
		dashboard.FocusSessions()
		renderAppendDashboardGolden(&output, fmt.Sprintf("standard panes at height %d", height), 120, height, dashboard)
		renderAppendGoldenValue(&output, fmt.Sprintf("standard transcript area at height %d", height), transcriptArea(dashboard))
		action := chord(dashboard, CommandIdTogglePanePreset)
		renderAppendGoldenValue(&output, "compact pane preset action", renderVariantDebug(action, "DashboardAction"))
		renderAppendDashboardGolden(&output, fmt.Sprintf("compact panes at height %d", height), 120, height, dashboard)
		renderAppendGoldenValue(&output, fmt.Sprintf("compact transcript area at height %d", height), transcriptArea(dashboard))
		action = chord(dashboard, CommandIdTogglePanePreset)
		renderAppendGoldenValue(&output, "restore pane preset action", renderVariantDebug(action, "DashboardAction"))
		renderAppendDashboardGolden(&output, fmt.Sprintf("restored panes at height %d", height), 120, height, dashboard)
		renderAppendGoldenValue(&output, fmt.Sprintf("restored transcript area at height %d", height), transcriptArea(dashboard))
	}

	dashboard = dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	for _, width := range []int{59, 70, 80} {
		renderAppendDashboardGolden(&output, fmt.Sprintf("dashboard at %d columns", width), width, 30, dashboard)
		panes := "None"
		if dashboard.paneAreas != nil {
			var rects []string
			for _, pane := range dashboard.paneAreas {
				rects = append(rects, string(renderRectDebug(pane)))
			}
			panes = "Some([" + strings.Join(rects, ", ") + "])"
		}
		conversation := "None"
		if dashboard.conversationAreaField != nil {
			conversation = "Some(" + string(renderRectDebug(*dashboard.conversationAreaField)) + ")"
		}
		renderAppendGoldenValue(&output, fmt.Sprintf("dashboard geometry at %d columns", width),
			renderDebugText(fmt.Sprintf("(%s, %s, %v)", panes, conversation, dashboard.SessionsMinimized())))
	}

	core.AssertGolden(t, "testdata", "dashboard-layout-render", output.String())
}

func TestGoldenDashboardFooterRender(t *testing.T) {
	var output strings.Builder

	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.SetWorkspaceName("personal")
	dashboard.SetNotice("Transient dashboard message")
	renderAppendDashboardGolden(&output, "notice takes over the one-row footer", 120, 24, dashboard)
	dashboard.notices.Clear()
	renderAppendDashboardGolden(&output, "footer hints after notice", 120, 24, dashboard)

	focusNames := map[Focus]string{
		FocusSessions: "Sessions",
		FocusTargets:  "Targets",
		FocusQuota:    "Quota",
		FocusPrompt:   "Prompt",
	}
	for _, focus := range []Focus{FocusSessions, FocusTargets, FocusQuota, FocusPrompt} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
		dashboard.focus = focus
		renderAppendDashboardGolden(&output, fmt.Sprintf("footer at %s focus", focusNames[focus]), 200, 24, dashboard)
	}

	dashboard = dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.FocusSessions()
	for width := 0; width <= 80; width++ {
		renderAppendDashboardGolden(&output, fmt.Sprintf("footer at %d columns", width), width, 30, dashboard)
	}

	for _, focus := range []Focus{FocusSessions, FocusPrompt} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.focus = focus
		action := dashboard.RouteBoundKey(prefixKey())
		renderAppendGoldenValue(&output, focusNames[focus]+" prefix action", renderVariantDebug(action, "KeyRoute"))
		renderAppendDashboardGolden(&output, focusNames[focus]+" pending chord", 120, 40, dashboard)
		dashboard.CancelPrefix()
		renderAppendDashboardGolden(&output, focusNames[focus]+" after cancelling chord", 120, 40, dashboard)
	}

	core.AssertGolden(t, "testdata", "dashboard-footer-render", output.String())
}

// Hard-won: 3747d578c: Windows omits the default localhost permission metadata.
func TestGoldenDashboardResourcePanels(t *testing.T) {
	var output strings.Builder
	noQuotas := map[string]client.ProfileQuota{}

	targetConfig := core.DefaultConfig()
	// Pin the permission metadata even on hosts that cannot run local
	// workers.
	targetConfig.Targets["localhost"] = core.TargetTemplateLocalBare{}
	sandbox := core.DefaultConfig().WithLocalTargets().Targets["docker"]
	if local, ok := sandbox.(core.TargetTemplateLocalDocker); ok {
		local.ContainerTemplate.Image = "example.test/own:latest"
		sandbox = local
	}
	targetConfig.Targets["sandbox"] = sandbox
	dashboard := NewDashboardState(targetConfig.WithLocalTargets(), core.NewStateState(), noQuotas)
	target := testCapacityTarget()
	target.TargetIDs = []string{"docker", "localhost", "podman", "sandbox"}
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{target})
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"docker", "sandbox"}, nil)
	})
	renderAppendDashboardGolden(&output, "Targets hides unavailable defaults", 160, 40, dashboard)

	dashboard = NewDashboardState(config(), core.NewStateState(), noQuotas)
	target = testCapacityTarget()
	target.TargetIDs = []string{"podman", "mac-container"}
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{target})
	usage := hostCapacityUsage()
	dashboard.applyDeploymentCapacity("local", &usage, nil, renderNowEpochSeconds())
	dashboard.SetContainerCapacities(map[string]client.RuntimeContainerCapacity{
		"local": {
			TargetIDs: []string{"podman", "mac-container"},
			Capacity: core.HostContainerCapacity{
				Total:     core.HostContainerSize{Cpus: 8, MemoryBytes: 4},
				Reserved:  core.HostContainerSize{Cpus: 0, MemoryBytes: 2 << 30},
				Committed: core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
				Remaining: core.HostContainerSize{Cpus: 8, MemoryBytes: 0},
				Complete:  true,
			},
		},
	})
	renderAppendDashboardGolden(&output, "grouped host load without sample clock", 120, 40, dashboard)

	// Why a reading cannot be trusted takes its own line under the Targets
	// column and runs on across the columns after it, cut with an ellipsis
	// rather than widening the table.
	twenty := "probe timed out after twenty seconds"
	dashboard.applyDeploymentCapacity("local", nil, &twenty, renderNowEpochSeconds())
	var capacities []client.RuntimeHostCapacity
	for _, detail := range capacityDetailsInKeyOrder(dashboard) {
		capacities = append(capacities, *detail)
	}
	sent := client.RuntimeMetadata{HostCapacities: core.NewSharedSnapshot(capacities)}
	encoded, err := json.Marshal(sent)
	if err != nil {
		t.Fatal(err)
	}
	var received client.RuntimeMetadata
	if err := json.Unmarshal(encoded, &received); err != nil {
		t.Fatal(err)
	}
	dashboard.SetHostCapacities(nil)
	dashboard.SetHostCapacities(received.HostCapacities.IntoInner())
	renderAppendDashboardGolden(&output, "stale reading notice under the Targets column", 100, 40, dashboard)

	erroredQuota := func(profileID string, harness core.HarnessKind, message string) map[string]client.ProfileQuota {
		return map[string]client.ProfileQuota{profileID: {
			ProfileID: profileID,
			Harness:   harness,
			Windows:   []client.QuotaWindow{},
			Error:     &message,
		}}
	}
	dashboard = NewDashboardState(config(), core.NewStateState(), erroredQuota("codex-1", core.HarnessKindCodex, "offline"))
	renderAppendDashboardGolden(&output, "quota error with refresh age", 120, 28, dashboard)

	dashboard = NewDashboardState(config(), core.NewStateState(), erroredQuota("claude-1", core.HarnessKindClaude, "login expired"))
	renderAppendDashboardGolden(&output, "expired login quota", 120, 28, dashboard)

	apiConfig := config()
	bedrock := apiConfig.Profiles["claude-1"]
	delete(apiConfig.Profiles, "claude-1")
	bedrock.Home = "/profiles/bedrock-claude"
	bedrock.Environment.Insert("CLAUDE_CODE_USE_BEDROCK", "1")
	apiConfig.Profiles["bedrock-claude"] = bedrock
	dashboard = NewDashboardState(apiConfig, core.NewStateState(), noQuotas)
	bedrockQuota := renderAPIQuota("bedrock-claude")
	bedrockQuota.Harness = core.HarnessKindClaude
	bedrockQuota.RefreshedAtEpochSeconds = 0
	dashboard.ApplyQuota(bedrockQuota)
	codexQuota := renderAPIQuota("codex-1")
	codexQuota.RefreshedAtEpochSeconds = 0
	dashboard.ApplyQuota(codexQuota)
	renderAppendDashboardGolden(&output, "usage-priced API quota", 120, 28, dashboard)

	opencodeConfig := config()
	opencode := opencodeConfig.Profiles["codex-1"]
	opencode.Kind = core.HarnessKindOpenCode
	opencode.Home = "/profiles/opencode"
	opencodeConfig.Profiles["opencode-1"] = opencode
	dashboard = NewDashboardState(opencodeConfig, core.NewStateState(), noQuotas)
	opencodeQuota := renderAPIQuota("opencode-1")
	opencodeQuota.Harness = core.HarnessKindOpenCode
	notReported := client.NotReportedLabel
	opencodeQuota.Extra = &notReported
	opencodeQuota.RefreshedAtEpochSeconds = 0
	dashboard.ApplyQuota(opencodeQuota)
	renderAppendDashboardGolden(&output, "quota a harness never shares", 120, 28, dashboard)

	now := int64(renderNowEpochSeconds())
	percent := func(value uint8) *uint8 { return &value }
	seconds := func(value int64) *int64 { return &value }
	text := func(value string) *string { return &value }
	claudeConfig := config()
	claudeConfig.Profiles["claude-2"] = claudeConfig.Profiles["claude-1"]
	claudeQuota := func(profileID string, fable *uint8) client.ProfileQuota {
		var windows []client.QuotaWindow
		// Fable deliberately precedes the master weekly window: ordering
		// must not change which value fills the weekly bar.
		if fable != nil {
			windows = append(windows, client.QuotaWindow{
				Label:            "Week (Fable)",
				RemainingPercent: fable,
				DurationSeconds:  seconds(604_800),
				ModelScope:       text("fable"),
			})
		}
		windows = append(windows,
			client.QuotaWindow{
				Label:                "Week",
				RemainingPercent:     percent(21),
				DurationSeconds:      seconds(604_800),
				ResetsAtEpochSeconds: seconds(now + 2*24*60*60 + 30),
			},
			client.QuotaWindow{
				Label:                "5h",
				RemainingPercent:     percent(40),
				DurationSeconds:      seconds(18_000),
				ResetsAtEpochSeconds: seconds(now + 60*60 + 30),
			})
		return client.ProfileQuota{
			ProfileID: profileID,
			Harness:   core.HarnessKindClaude,
			Windows:   windows,
		}
	}
	claudeReports := map[string]client.ProfileQuota{
		"claude-1": claudeQuota("claude-1", percent(17)),
		"claude-2": claudeQuota("claude-2", nil),
	}
	codexFable := claudeQuota("codex-1", percent(17))
	codexFable.Harness = core.HarnessKindCodex
	codexFable.Windows = []client.QuotaWindow{{
		Label:            "Week (Fable)",
		RemainingPercent: percent(17),
		DurationSeconds:  seconds(604_800),
		ModelScope:       text("fable"),
	}}
	claudeReports["codex-1"] = codexFable
	dashboard = NewDashboardState(claudeConfig, core.NewStateState(), claudeReports)
	renderAppendDashboardGolden(&output, "Claude Fable weekly quota", 140, 28, dashboard)

	weeklyAndShort := func(weeklyResets, shortResets *string, banked *uint64) client.ProfileQuota {
		return client.ProfileQuota{
			BankedResets: banked,
			ProfileID:    "codex-1",
			Harness:      core.HarnessKindCodex,
			Windows: []client.QuotaWindow{
				{
					Label:                "Week",
					RemainingPercent:     percent(73),
					Resets:               weeklyResets,
					ResetsAtEpochSeconds: seconds(now + 2*24*60*60 + 30),
				},
				{
					Label:                "5H",
					RemainingPercent:     percent(70),
					Resets:               shortResets,
					ResetsAtEpochSeconds: seconds(now + 60*60 + 5*60 + 30),
				},
			},
		}
	}
	dashboard = NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{
		"codex-1": weeklyAndShort(text("09:00 Aug 20"), text("14:00 Aug 13"), nil),
	})
	renderAppendDashboardGolden(&output, "weekly and five-hour quota columns", 140, 28, dashboard)

	one := uint64(1)
	dashboard = NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{
		"codex-1": weeklyAndShort(nil, nil, &one),
	})
	renderAppendDashboardGolden(&output, "weekly and five-hour quota at 80 columns", 80, 28, dashboard)

	agyConfig := config()
	agyConfig.Profiles["agy"] = core.NewHarnessProfile(core.HarnessKindAntigravity, "/profiles/agy")
	agyQuota := weeklyAndFiveHourQuota("agy", 73, 48)
	agyQuota.Harness = core.HarnessKindAntigravity
	for i := range agyQuota.Windows {
		agyQuota.Windows[i].ModelScope = text(core.AntigravityQuotaPoolGemini.ModelScope())
	}
	claudePool := weeklyAndFiveHourQuota("agy", 0, 81)
	for i := range claudePool.Windows {
		claudePool.Windows[i].ModelScope = text(core.AntigravityQuotaPoolClaude.ModelScope())
	}
	agyQuota.Windows = append(agyQuota.Windows, claudePool.Windows...)
	dashboard = NewDashboardState(agyConfig, core.NewStateState(), map[string]client.ProfileQuota{"agy": agyQuota})
	dashboard.focus = FocusQuota
	dashboard.setSelectionFor(FocusQuota, 1)
	renderAppendDashboardGolden(&output, "Antigravity Gemini and Claude quota pools", 140, 28, dashboard)
	dashboard.beginProfileRename()
	if editor, ok := dashboard.mode.(ModeConfigId); !ok || editor.oldID != "agy" {
		t.Fatal("the Claude quota row still selects the agy profile")
	}

	core.AssertGolden(t, "testdata", "dashboard-resource-panels", output.String())
}

// Long disk lists stay in the anchored dropdown and can be navigated
// without increasing the Targets table's row height. A notice line above
// the Disks cell belongs to its own row and must not shift the cell's hit
// region.
//
// Hard-won: 371d4652: Disks hit regions drifted from the text when table columns shrank.
func TestALongDisksDropdownKeepsTheTargetRowOneLineAndScrolls(t *testing.T) {
	dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	// The stale target sorts first, so its notice line sits between the
	// heading and the row whose Disks cell the clicks below land on.
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{
		{
			ID:        "a:stale",
			Host:      "stale-host",
			TargetIDs: []string{"podman"},
			Kind:      core.DeploymentCapacityKindHost,
		},
		precisionCapacityTarget(),
	})
	usage := hostUsage(20)
	dashboard.applyDeploymentCapacity("a:stale", &usage, nil, renderNowEpochSeconds())
	timedOut := "probe timed out"
	dashboard.applyDeploymentCapacity("a:stale", nil, &timedOut, renderNowEpochSeconds())
	filesystems := make([]core.FilesystemSpace, 30)
	for index := range filesystems {
		mount := fmt.Sprintf("/srv/volume-%02d", index)
		filesystems[index] = core.FilesystemSpace{
			Mount:          mount,
			AvailableBytes: 40 << 30,
			TotalBytes:     100 << 30,
			Paths:          []string{mount},
		}
	}
	home := "/home/jonathan"
	sampled := uint64(1)
	dashboard.setTestCapacityStorage([]core.TargetStorageView{core.TargetStorageViewEvaluate(
		"precision-3260",
		&home,
		filesystems,
		&sampled,
		func(*core.FilesystemSpace) *string { return nil },
		nil,
	)})
	tableWidth := capacityTableWidth(dashboard)
	for _, width := range []int{tableWidth - 5, tableWidth, tableWidth + 20} {
		for character := range utf8.RuneCountInString("Disks ▾") {
			terminal := newRenderTerminal(width+6, 10)
			terminal.draw(func(frame *chat.Frame) {
				dashboard.beginSurfaceFrame()
				renderCapacity(frame, uv.Rect(3, 2, width, 6), dashboard, nil)
				dashboard.endSurfaceFrame()
			})
			lines := bufferLines(terminal.buffer)
			cell := point(lines, "Disks ▾")
			position := uv.Pos(cell.X+character, cell.Y)
			dashboard.handleSurfaceMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, position))
			dashboard.handleSurfaceMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, position))
			if !dashboard.capacityDisksMenuOpen("ssh:precision-3260") {
				t.Fatalf("Disks character %d at pane width %d: %q, position %v, regions %v",
					character, width, lines[cell.Y], position, dashboard.capacityDisksAreas)
			}
			dashboard.HandleKey(key(tea.KeyEscape))
		}
	}
	rendered := drawnDashboard(dashboard, 160)
	diskCell := point(strings.Split(rendered, "\n"), "Disks ▾")
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, diskCell))
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, diskCell))
	if dashboard.paneMenu == nil {
		t.Fatal("the dropdown opened")
	}
	// The stale target's reading and notice, plus the row under the
	// dropdown.
	if lines := capacityTableLines(dashboard); lines != 3 {
		t.Fatalf("%d table lines", lines)
	}

	rendered = drawnDashboard(dashboard, 160)
	for _, expected := range []string{"precision-3260", "Disks ▴", "/srv/volume-00 40.0G free"} {
		if !strings.Contains(rendered, expected) {
			t.Fatalf("%q: %s", expected, rendered)
		}
	}
	if strings.Contains(rendered, "/srv/volume-29") {
		t.Fatal(rendered)
	}

	for range 29 {
		dashboard.HandleKey(key(tea.KeyDown))
	}
	if rendered := drawnDashboard(dashboard, 160); !strings.Contains(rendered, "/srv/volume-29 40.0G free") {
		t.Fatal(rendered)
	}
	if lines := capacityTableLines(dashboard); lines != 3 {
		t.Fatalf("%d table lines", lines)
	}
}

// Hard-won: b54b2449a8: AFK omitted classifier intervention after failed turns.
func TestGoldenOperatorAwayDialog(t *testing.T) {
	var output strings.Builder
	type classifiedSession struct {
		materialized core.MaterializedSession
		assessment   *core.TurnAssessment
	}
	for _, awayCase := range []struct {
		symbols       core.SymbolSet
		label         string
		width, height int
	}{
		{core.SymbolSetUnicode, "AFK", 100, 32},
		{core.SymbolSetAscii, "AFK ASCII", 100, 32},
		{core.SymbolSetUnicode, "AFK small", 32, 12},
		{core.SymbolSetUnicode, "AFK tall narrow", 56, 32},
		{core.SymbolSetUnicode, "AFK tiny", 10, 5},
	} {
		testConfig := config()
		symbols := awayCase.symbols
		testConfig.Advanced.Symbols = &symbols
		dashboard := NewDashboardState(testConfig, core.NewStateState(), map[string]client.ProfileQuota{})
		var classified []classifiedSession
		for index, title := range []string{"Explicit question", "Classifier question", "Classifier failure", "Ordinary idle"} {
			session := runningSession()
			session.ID = fmt.Sprintf("waiting-%d", index)
			session.Title = title
			session.ACPSessionTitle = nil
			session.ViewedThroughEventOrdinal = 3
			if index == 1 {
				session.WorkspaceID = "other"
			}
			materialized := core.MaterializedSessionEmpty(session.ID)
			materialized.AppliedEventOrdinal = 4
			materialized.Transcript = []*core.TranscriptItem{agentMessage(2, "I need your decision.")}
			stopReason := "EndTurn"
			if index == 2 {
				stopReason = "provider_error"
			}
			accepted, turnStart := uint64(1), uint64(2)
			materialized.LastTurnOutcome = &core.MaterializedTurnOutcome{
				CommandID:         fmt.Sprintf("prompt-%d", index),
				AcceptedOrdinal:   &accepted,
				TurnStartPosition: &turnStart,
				CompletedOrdinal:  3,
				CompletedAtMs:     1_000,
				Outcome:           core.TurnOutcomeKindCompleted{StopReason: stopReason},
			}
			var assessment *core.TurnAssessment
			switch index {
			case 0:
				materialized.PendingElicitations = []core.ElicitationRequest{question(session.ID)}
			case 3:
			default:
				pending := core.TurnAssessmentPending(fmt.Sprintf("prompt-%d", index), 3, 1_000,
					core.CompletionEvidence{StopReason: stopReason})
				pending.Status = core.StatusAssessed
				action := core.AssessmentActionAwaitInput
				pending.Action = &action
				assessment = &pending
			}
			dashboard.state.Mut().Sessions.Insert(session.ID, session)
			dashboard.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromAssessedMaterialized(
				materialized, assessment, 3, MaterializedProjectionCache{}))
			classified = append(classified, classifiedSession{materialized, assessment})
			attention := core.SessionAttentionNeedsInput
			if index == 3 {
				attention = core.SessionAttentionIdle
			}
			serveAttention(dashboard, []string{session.ID}, attention)
		}
		publishFixtureVerdicts(dashboard)
		dashboard.SetOperatorPresence(core.OperatorPresence{Away: true, Revision: 1})
		renderAppendDashboardGolden(&output, awayCase.label, awayCase.width, awayCase.height, dashboard)
		if awayCase.label == "AFK" {
			(*dashboard.sessionDetails.Mut())["waiting-0"].pendingElicitations = nil
			serveAttention(dashboard, []string{"waiting-0"}, core.SessionAttentionIdle)
			renderAppendDashboardGolden(&output, "AFK classifier intervention only", awayCase.width, awayCase.height, dashboard)
			for _, session := range classified {
				materialized := session.materialized
				serveAttention(dashboard, []string{materialized.SessionID}, core.SessionAttentionIdle)
				materialized.AppliedEventOrdinal++
				materialized.PendingElicitations = nil
				var assessment *core.TurnAssessment
				if session.assessment != nil {
					superseded := *session.assessment
					superseded.Status = core.StatusSuperseded
					superseded.Reason = "question_dismissed"
					assessment = &superseded
				}
				dashboard.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromAssessedMaterialized(
					materialized, assessment, 3, MaterializedProjectionCache{}))
			}
			renderAppendDashboardGolden(&output, "AFK waiting requests cleared", awayCase.width, awayCase.height, dashboard)
		}
		if awayCase.label == "AFK small" {
			dashboard.HandleKey(key(tea.KeyEnd))
			renderAppendDashboardGolden(&output, "AFK small scrolled", awayCase.width, awayCase.height, dashboard)
		}
	}
	core.AssertGolden(t, "testdata", "operator-away-dialog", output.String())
}
