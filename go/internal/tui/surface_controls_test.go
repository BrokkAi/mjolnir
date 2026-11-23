// Port of mj-tui/src/surface_controls.rs tests.

package tui

import (
	"fmt"
	"image/color"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// surfaceControlsDrawBuffer draws the combined surface with no conversation
// on it, as Rust's test-only render::render does, into a fresh buffer.
func surfaceControlsDrawBuffer(dashboard *DashboardState, width, height int) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	withNoColorOverrideForTest(true, func() []string {
		return renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
	})
	return buffer
}

// surfaceControlsDraw is the drawn surface, one string per row, with the
// spinner frames pinned.
func surfaceControlsDraw(dashboard *DashboardState, width, height int) []string {
	lines := bufferLines(surfaceControlsDrawBuffer(dashboard, width, height))
	pinSpinnerGlyphs(lines)
	return lines
}

func surfaceControlsAppendState(output *strings.Builder, label string, dashboard *DashboardState, width, height int) []string {
	lines := surfaceControlsDraw(dashboard, width, height)
	if output.Len() > 0 {
		output.WriteString("\n")
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteString("\n")
	return lines
}

// surfaceControlsColorDebug is ratatui's Debug of a drawn cell's colour: a
// cell with no colour is Reset.
func surfaceControlsColorDebug(value color.Color) string {
	switch value := value.(type) {
	case nil:
		return "Reset"
	case ansi.IndexedColor:
		return fmt.Sprintf("Indexed(%d)", uint8(value))
	case ansi.BasicColor:
		return (chat.ColorBlack + chat.Color(value)).GoString()
	default:
		r, g, b, _ := value.RGBA()
		return fmt.Sprintf("Rgb(%d, %d, %d)", r>>8, g>>8, b>>8)
	}
}

// surfaceControlsModifierDebug is ratatui's Debug of a drawn cell's
// modifiers.
func surfaceControlsModifierDebug(style uv.Style) string {
	var names []string
	for _, flag := range []struct {
		set  bool
		name string
	}{
		{style.Attrs&uv.AttrBold != 0, "BOLD"},
		{style.Attrs&uv.AttrFaint != 0, "DIM"},
		{style.Attrs&uv.AttrItalic != 0, "ITALIC"},
		{style.Underline != uv.UnderlineStyleNone, "UNDERLINED"},
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

// surfaceControlsActionDebug is DashboardAction's derived Debug for the
// actions these tests print.
func surfaceControlsActionDebug(action DashboardAction) string {
	switch action := action.(type) {
	case DashboardActionNone:
		return "None"
	case DashboardActionOpenResumeDialog:
		return "OpenResumeDialog"
	case DashboardActionRefreshAll:
		return "RefreshAll"
	case DashboardActionOpen:
		return fmt.Sprintf("Open { session_id: %s }", core.RustDebugString(action.SessionID))
	case DashboardActionOpenFilteredResumeDialog:
		state := "None"
		if action.State != nil {
			state = fmt.Sprintf("Some(%s)", surfaceControlsStateFilterDebug(*action.State))
		}
		return fmt.Sprintf("OpenFilteredResumeDialog { query: %s, state: %s }", core.RustDebugString(action.Query), state)
	default:
		return fmt.Sprintf("%#v", action)
	}
}

func surfaceControlsStateFilterDebug(filter SessionStateFilter) string {
	switch filter {
	case SessionStateFilterBlocked:
		return "Blocked"
	case SessionStateFilterWorking:
		return "Working"
	case SessionStateFilterIdle:
		return "Idle"
	case SessionStateFilterDone:
		return "Done"
	default:
		return "NotIdle"
	}
}

func surfaceControlsAppendButtonStyles(output *strings.Builder, dashboard *DashboardState, width, height int) {
	buffer := surfaceControlsDrawBuffer(dashboard, width, height)
	lines := bufferLines(buffer)
	for _, label := range []string{"Create", "Open"} {
		at := point(lines, label)
		cell := buffer.CellAt(at.X, at.Y)
		fmt.Fprintf(output, "button %s: fg=%s bg=%s modifiers=%s\n",
			label,
			surfaceControlsColorDebug(cell.Style.Fg),
			surfaceControlsColorDebug(cell.Style.Bg),
			surfaceControlsModifierDebug(cell.Style),
		)
	}
}

func surfaceControlsAppendActionFocus(output *strings.Builder, dashboard *DashboardState) {
	focus := "None"
	if dashboard.sessionActionFocus != nil {
		focus = fmt.Sprintf("Some(%s)", dashboard.sessionActionFocus.String())
	}
	fmt.Fprintf(output, "action focus: %s; Create enabled: %t; Open enabled: %t\n",
		focus,
		sessionActionEnabled(dashboard, CommandIdNewSessionWizard),
		sessionActionEnabled(dashboard, CommandIdResumeDialog),
	)
}

var (
	surfaceControlsLeftDown = tea.MouseClickMsg{Button: tea.MouseLeft}
	surfaceControlsLeftUp   = tea.MouseReleaseMsg{Button: tea.MouseLeft}
)

// surfaceControlsClick is a press and release at one cell; the press itself
// must do nothing.
func surfaceControlsClick(t *testing.T, dashboard *DashboardState, at uv.Position) DashboardAction {
	t.Helper()
	if action := dashboard.HandleMouse(mouseAt(surfaceControlsLeftDown, at)); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
		t.Fatalf("press at %v = %s, want None", at, surfaceControlsActionDebug(action))
	}
	return dashboard.HandleMouse(mouseAt(surfaceControlsLeftUp, at))
}

func surfaceControlsAnyLineContains(lines []string, text string) bool {
	return slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, text) })
}

func TestPointerReleaseOutsideOrAfterDisablingNeverCreatesASession(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	lines := surfaceControlsDraw(dashboard, 120, 40)
	create := point(lines, "Create")
	dashboard.HandleMouse(mouseAt(surfaceControlsLeftDown, create))
	outside := mouseAt(surfaceControlsLeftUp, uv.Pos(119, 1))
	if !dashboard.ComponentHandlesMouse(outside) {
		t.Fatal("the surface owns the release of its own press")
	}
	if action := dashboard.HandleMouse(outside); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
		t.Fatalf("release outside = %s, want None", surfaceControlsActionDebug(action))
	}
	dashboard.HandleMouse(mouseAt(surfaceControlsLeftDown, create))
	dashboard.config.Mut().Profiles = map[string]core.HarnessProfile{}
	surfaceControlsDraw(dashboard, 120, 40)
	if action := dashboard.HandleMouse(mouseAt(surfaceControlsLeftUp, create)); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
		t.Fatalf("release after disabling = %s, want None", surfaceControlsActionDebug(action))
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %T, want the dashboard", dashboard.mode)
	}
}

// surfaceControlsRowArea is the row index and area of the session row that
// shows sessionID.
func surfaceControlsRowArea(t *testing.T, dashboard *DashboardState, sessionID string) (int, uv.Rectangle) {
	t.Helper()
	for _, entry := range dashboard.sessionRowAreas {
		if dashboard.sessionMenuIDs[entry.First] == sessionID {
			return entry.First, entry.Second
		}
	}
	t.Fatalf("no row for %s", sessionID)
	return 0, uv.Rectangle{}
}

// Hard-won: 213602c916: opening a row menu switched the visible conversation.
func TestSessionActionsTargetTheClickedRowWithoutSwitchingConversations(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	second := runningSession()
	second.ID = "another-session"
	title := "Menu target B"
	second.SessionTitleOverride = &title
	dashboard.state.Mut().Sessions.Insert(second.ID, second)
	publishFixtureVerdicts(dashboard)
	area := uv.Rect(0, 0, 120, 40)
	dashboard.conversationAreaField = &area
	pinned := dashboard.BrowsePane()
	sessionOne := "session-1"
	dashboard.SetPaneSession(pinned, &sessionOne)
	if _, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the pane splits")
	}
	dashboard.FocusPane(pinned)
	dashboard.focus = FocusPrompt
	surfaceControlsDraw(dashboard, 120, 40)
	index, row := surfaceControlsRowArea(t, dashboard, "another-session")
	menu := uv.Pos(row.Max.X-2, row.Min.Y)
	surfaceControlsClick(t, dashboard, menu)
	surfaceControlsAssertSelection := func(stage string) {
		t.Helper()
		if selected, ok := dashboard.SelectedSessionID(); !ok || selected != "session-1" {
			t.Fatalf("%s: selected = %q, want session-1", stage, selected)
		}
		if dashboard.FocusedPane() != pinned {
			t.Fatalf("%s: focused pane = %v, want %v", stage, dashboard.FocusedPane(), pinned)
		}
		if dashboard.focus != FocusPrompt {
			t.Fatalf("%s: focus = %v, want the prompt", stage, dashboard.focus)
		}
		if dashboard.commandSessionOverride != nil {
			t.Fatalf("%s: command override = %q, want none", stage, *dashboard.commandSessionOverride)
		}
	}
	surfaceControlsAssertSelection("menu opened")
	if session, ok := dashboard.PaneSession(pinned); !ok || session != "session-1" {
		t.Fatalf("pinned pane = %q, want session-1", session)
	}
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatalf("mode = %T, want the palette", dashboard.mode)
	}
	if !strings.Contains(strings.Join(surfaceControlsDraw(dashboard, 120, 40), "\n"), "Menu target B") {
		t.Fatal("the menu names the clicked session")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %T, want the dashboard", dashboard.mode)
	}
	surfaceControlsAssertSelection("menu closed")

	// A different pointer position clears the modal click transition.
	surfaceControlsDraw(dashboard, 120, 40)
	otherIndex, otherRow := surfaceControlsRowArea(t, dashboard, "session-1")
	if index == otherIndex {
		t.Fatal("the two sessions have different rows")
	}
	otherMenu := uv.Pos(otherRow.Max.X-2, otherRow.Min.Y)
	surfaceControlsClick(t, dashboard, otherMenu)
	dashboard.HandleKey(key(tea.KeyEscape))
	surfaceControlsDraw(dashboard, 120, 40)
	surfaceControlsClick(t, dashboard, menu)
	lines := surfaceControlsDraw(dashboard, 120, 40)
	surfaceControlsClick(t, dashboard, point(lines, "Rename…"))
	editor, ok := dashboard.mode.(ModeRename)
	if !ok {
		t.Fatalf("rename editor should be open, mode = %T", dashboard.mode)
	}
	if editor.sessionID != "another-session" {
		t.Fatalf("rename target = %q, want another-session", editor.sessionID)
	}
	surfaceControlsAssertSelection("rename opened")
}

func TestPaneQuestionMarkOpensPaletteAndModalChordKeepsItsDialog(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.focus = FocusSessions
	dashboard.HandleKey(key('?'))
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatalf("mode = %T, want the palette", dashboard.mode)
	}
	if !strings.Contains(strings.Join(surfaceControlsDraw(dashboard, 120, 40), "\n"), "Search commands") {
		t.Fatal("the palette's search should show")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode = %T, want the dashboard", dashboard.mode)
	}

	chord(dashboard, CommandIdOpenConfig)
	// Rust compares a clone of the mode; Go keeps the dialog by pointer, so
	// the same dialog with the same top-level state stands for equality.
	previous := dashboard.mode
	setup, ok := previous.(ModeSetup)
	if !ok {
		t.Fatalf("mode = %T, want setup", previous)
	}
	snapshot := *setup.SetupDialog
	route(dashboard, []tea.KeyMsg{prefixKey(), key('?')})
	after, ok := dashboard.mode.(ModeSetup)
	if !ok || after.SetupDialog != setup.SetupDialog || !reflect.DeepEqual(*after.SetupDialog, snapshot) {
		t.Fatalf("mode = %T, want the same setup dialog", dashboard.mode)
	}
}

// surfaceControlsActionsRow is the Sessions action row as drawn inside the
// Sessions pane, without the panes beside it.
func surfaceControlsActionsRow(dashboard *DashboardState, lines []string) string {
	pane := dashboard.paneAreas[0]
	runes := []rune(lines[pane.Min.Y+1])
	start := min(pane.Min.X, len(runes))
	end := min(pane.Min.X+pane.Dx(), len(runes))
	return string(runes[start:end])
}

func surfaceControlsFilterState(t *testing.T, dashboard *DashboardState) (string, int) {
	t.Helper()
	filter := *dashboard.sessionsFilter.Get()
	if filter == nil {
		t.Fatal("filter")
	}
	return filter.Query.Value(), filter.Query.Cursor()
}

// surfaceControlsFilterCells is the drawn Filter row and the columns, from
// the label's start, that show the caret. User request 2026-09-29: the
// filter input takes the readline keys the composer does.
func surfaceControlsFilterCells(dashboard *DashboardState) (string, []int) {
	buffer := surfaceControlsDrawBuffer(dashboard, 140, 40)
	lines := bufferLines(buffer)
	at := point(lines, "Filter:")
	caret := chat.FocusControl()
	reversed := []int{}
	for column := at.X; column < at.X+len(filterLabel)+filterFieldMax; column++ {
		cell := buffer.CellAt(column, at.Y)
		var marked bool
		if chat.ReverseVideo() {
			marked = cell.Style.Attrs&uv.AttrReverse != 0
		} else {
			marked = chat.ColorFromTerminal(cell.Style.Bg) == caret.Bg
		}
		if marked {
			reversed = append(reversed, column-at.X)
		}
	}
	return lines[at.Y], reversed
}

// The caret is a reversed cell at the cursor, and a field narrower than its
// text scrolls to keep the cursor in view.
func TestTheFilterCaretFollowsTheCursorAndTheFieldScrolls(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.HandleKey(key('/'))
	for _, character := range "abcdef" {
		dashboard.HandleKey(key(character))
	}
	label := len(filterLabel)
	// At the end the caret sits on the cell after the text.
	row, reversed := surfaceControlsFilterCells(dashboard)
	if !strings.Contains(row, "Filter: abcdef") {
		t.Fatalf("row = %q", row)
	}
	if !slices.Equal(reversed, []int{label + 6}) {
		t.Fatalf("caret = %v, want [%d]", reversed, label+6)
	}
	// Moved left, it sits on the letter it would delete.
	dashboard.HandleKey(key(tea.KeyLeft))
	dashboard.HandleKey(key(tea.KeyLeft))
	row, reversed = surfaceControlsFilterCells(dashboard)
	if !strings.Contains(row, "Filter: abcdef") {
		t.Fatalf("row = %q", row)
	}
	if !slices.Equal(reversed, []int{label + 4}) {
		t.Fatalf("caret = %v, want [%d]", reversed, label+4)
	}

	// Text wider than the field shows its tail with the caret on the last
	// cell, then its head when the cursor goes home.
	dashboard.HandleKey(key(tea.KeyEnd))
	for _, character := range "ghijklmnopqrstuvwxyz0123456789" {
		dashboard.HandleKey(key(character))
	}
	row, reversed = surfaceControlsFilterCells(dashboard)
	if !strings.Contains(row, "789 ") {
		t.Fatalf("row = %q", row)
	}
	if strings.Contains(row, "abc") {
		t.Fatalf("row = %q", row)
	}
	if len(reversed) != 1 {
		t.Fatalf("one caret cell: %v in %q", reversed, row)
	}
	if reversed[0] <= label+6 {
		t.Fatalf("row = %q", row)
	}
	dashboard.HandleKey(key(tea.KeyHome))
	row, reversed = surfaceControlsFilterCells(dashboard)
	if !strings.Contains(row, "Filter: abcdefghij") {
		t.Fatalf("row = %q", row)
	}
	if strings.Contains(row, "789") {
		t.Fatalf("row = %q", row)
	}
	if !slices.Equal(reversed, []int{label}) {
		t.Fatalf("caret = %v, want [%d]", reversed, label)
	}
}

func surfaceControlsTitled(id, title string) core.SessionRecord {
	session := runningSession()
	session.ID = id
	session.Title = title
	session.SessionTitleOverride = &title
	return session
}

func surfaceControlsFound(id string, kind client.SessionTextMatchKind) client.SessionTextMatch {
	return client.SessionTextMatch{SessionID: id, Kind: kind}
}

func surfaceControlsOrderedIDs(dashboard *DashboardState) []string {
	ids := []string{}
	for _, session := range dashboard.orderedSessions() {
		ids = append(ids, session.ID)
	}
	return ids
}

// User request 2026-09-29: the Filter searches user and agent messages as
// well as the name and branch, and lists name matches first, then
// user-message matches, then agent-message matches. A session whose only
// match is in tool output is not listed. The list keeps its own order
// inside each group.
func TestFilterListsNameMatchesThenUserThenAgentMessages(t *testing.T) {
	agent, user := client.SessionTextMatchKindAgent, client.SessionTextMatchKindUser

	dashboard := dashboardWithSession(surfaceControlsTitled("a-agent", "agent talks"))
	for _, session := range []core.SessionRecord{
		surfaceControlsTitled("b-user", "user talks"),
		surfaceControlsTitled("c-name", "the Zebra project"),
		surfaceControlsTitled("d-tool", "tool only"),
		surfaceControlsTitled("e-name", "zebra again"),
		surfaceControlsTitled("f-user", "second user"),
	} {
		dashboard.state.Mut().Sessions.Insert(session.ID, session)
	}
	publishFixtureVerdicts(dashboard)
	dashboard.SelectActiveSession("c-name")
	dashboard.FocusSessions()
	baseline := surfaceControlsOrderedIDs(dashboard)
	if len(baseline) != 6 {
		t.Fatalf("baseline = %v, want 6 sessions", baseline)
	}

	dashboard.HandleKey(key('/'))
	for _, character := range "zebra" {
		dashboard.HandleKey(key(character))
	}
	// The daemon is asked once for the text, and the list says so until it
	// answers. Meanwhile the name matches are already there.
	requestID, query, ok := dashboard.NextSessionsTextSearch()
	if !ok {
		t.Fatal("a search")
	}
	if query != "zebra" {
		t.Fatalf("query = %q", query)
	}
	if _, _, ok := dashboard.NextSessionsTextSearch(); ok {
		t.Fatal("the same text is asked about once")
	}
	if !strings.Contains(dashboard.sessionsFilterLabel(), "searching") {
		t.Fatalf("label = %q", dashboard.sessionsFilterLabel())
	}
	if got := surfaceControlsOrderedIDs(dashboard); !slices.Equal(got, []string{"c-name", "e-name"}) {
		t.Fatalf("ids = %v", got)
	}
	if got := dashboard.sessionsHiddenCount(); got != 4 {
		t.Fatalf("hidden = %d, want 4", got)
	}

	// An answer to an older search changes nothing.
	dashboard.ApplySessionsText(requestID+9, []client.SessionTextMatch{surfaceControlsFound("a-agent", agent)}, nil)
	if !strings.Contains(dashboard.sessionsFilterLabel(), "searching") {
		t.Fatalf("label = %q", dashboard.sessionsFilterLabel())
	}

	dashboard.ApplySessionsText(requestID, []client.SessionTextMatch{
		surfaceControlsFound("a-agent", agent),
		surfaceControlsFound("f-user", user),
		surfaceControlsFound("b-user", user),
	}, nil)
	if strings.Contains(dashboard.sessionsFilterLabel(), "searching") {
		t.Fatalf("label = %q", dashboard.sessionsFilterLabel())
	}
	if got := surfaceControlsOrderedIDs(dashboard); !slices.Equal(got, []string{"c-name", "e-name", "b-user", "f-user", "a-agent"}) {
		t.Fatalf("ids = %v", got)
	}
	// Only the tool-only session is held back, and the title says so.
	if got := dashboard.sessionsHiddenCount(); got != 1 {
		t.Fatalf("hidden = %d, want 1", got)
	}

	episode, ok := dashboard.WikiSyncEpisode(false)
	if !ok {
		t.Fatal("a sync episode")
	}
	target := uint64(1)
	if !dashboard.ApplyWikiSync(false, episode, client.WikiSyncStatus{
		Epoch:      "fixture",
		Target:     &target,
		Finished:   1,
		Successful: 1,
	}, nil) {
		t.Fatal("the sync status applies")
	}

	refresh, _, ok := dashboard.RefreshSessionsTextSearch()
	if !ok {
		t.Fatal("a refresh")
	}
	if label, ok := dashboard.sessionsSyncLabel(); !ok || label != "Searching…" {
		t.Fatalf("sync label = %q, %v", label, ok)
	}
	dashboard.ApplySessionsText(refresh, []client.SessionTextMatch{}, nil)
	// Hard-won: 7d06e3b: ordinary query failures incorrectly became permanent sync failures.
	(*dashboard.sessionsFilter.Mut()).Query = chat.TextInputFromValue("new query")
	typed, _, ok := dashboard.NextSessionsTextSearch()
	if !ok {
		t.Fatal("a typed search")
	}
	dashboard.ApplySessionsText(typed, nil, fmt.Errorf("ordinary query failed"))
	if label, ok := dashboard.sessionsSyncLabel(); ok {
		t.Fatalf("sync label = %q, want none", label)
	}
	dashboard.ApplySessionsText(typed, []client.SessionTextMatch{}, nil)
	if label, ok := dashboard.sessionsSyncLabel(); ok {
		t.Fatalf("sync label = %q, want none", label)
	}
	refresh, _, ok = dashboard.RefreshSessionsTextSearch()
	if !ok {
		t.Fatal("a refresh")
	}
	dashboard.ApplySessionsText(refresh, nil, fmt.Errorf("final read failed"))
	if label, ok := dashboard.sessionsSyncLabel(); !ok || label != "Index sync failed: final read failed" {
		t.Fatalf("sync label = %q, %v", label, ok)
	}

	// Clearing the filter restores the list and forgets the matches.
	dashboard.clearSessionsFilter()
	if _, _, ok := dashboard.NextSessionsTextSearch(); ok {
		t.Fatal("no search without text")
	}
	if got := surfaceControlsOrderedIDs(dashboard); !slices.Equal(got, baseline) {
		t.Fatalf("ids = %v, want %v", got, baseline)
	}
}

func TestGoldenSessionsPaneActions(t *testing.T) {
	var output strings.Builder
	for _, size := range [][2]int{{80, 20}, {100, 24}, {120, 30}, {140, 40}, {200, 60}} {
		width, height := size[0], size[1]
		dashboard := dashboardWithSession(runningSession())
		dashboard.SetPaneSize(SupportPaneTargets, core.PaneSizeMinimized)
		dashboard.SetPaneSize(SupportPaneQuota, core.PaneSizeMinimized)
		dashboard.FocusPrompt()
		dashboard.SetNotice("Background work finished")
		lines := surfaceControlsAppendState(&output, "Create and Open remain clickable", dashboard, width, height)
		action := surfaceControlsClick(t, dashboard, point(lines, "Create"))
		surfaceControlsAppendState(&output, "Create opens the session wizard", dashboard, width, height)
		_, isNew := dashboard.mode.(ModeNew)
		fmt.Fprintf(&output, "action: %s; mode: New=%t\n", surfaceControlsActionDebug(action), isNew)
		dashboard.CancelModal()
		lines = surfaceControlsDraw(dashboard, width, height)
		action = surfaceControlsClick(t, dashboard, point(lines, "Open"))
		surfaceControlsAppendState(&output, "Open launches the session picker", dashboard, width, height)
		fmt.Fprintf(&output, "action: %s\n", surfaceControlsActionDebug(action))
	}

	const width, height = 120, 40
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	surfaceControlsAppendState(&output, "action row in the Sessions pane", dashboard, width, height)
	surfaceControlsAppendActionFocus(&output, dashboard)
	surfaceControlsAppendButtonStyles(&output, dashboard, width, height)
	dashboard.HandleKey(key(tea.KeyUp))
	surfaceControlsAppendState(&output, "keyboard focus on Create", dashboard, width, height)
	surfaceControlsAppendActionFocus(&output, dashboard)
	surfaceControlsAppendButtonStyles(&output, dashboard, width, height)
	dashboard.HandleKey(key(tea.KeyRight))
	surfaceControlsAppendState(&output, "keyboard focus moves to Open", dashboard, width, height)
	surfaceControlsAppendActionFocus(&output, dashboard)
	surfaceControlsAppendButtonStyles(&output, dashboard, width, height)
	action := dashboard.HandleKey(key(tea.KeyEnter))
	surfaceControlsAppendState(&output, "Enter activates the focused Open button", dashboard, width, height)
	fmt.Fprintf(&output, "action: %s\n", surfaceControlsActionDebug(action))

	monochrome := dashboardWithSession(runningSession())
	config := *monochrome.config.Get()
	config.Theme = core.ThemeChoiceFixed{Value: core.UiThemeMono}
	monochrome.SetConfig(config)
	monochrome.FocusSessions()
	surfaceControlsAppendState(&output, "monochrome action emphasis at rest", monochrome, width, height)
	surfaceControlsAppendButtonStyles(&output, monochrome, width, height)
	monochrome.HandleKey(key(tea.KeyUp))
	surfaceControlsAppendState(&output, "monochrome emphasis on Create", monochrome, width, height)
	surfaceControlsAppendButtonStyles(&output, monochrome, width, height)
	monochrome.HandleKey(key(tea.KeyRight))
	surfaceControlsAppendState(&output, "monochrome emphasis on Open", monochrome, width, height)
	surfaceControlsAppendButtonStyles(&output, monochrome, width, height)
	action = monochrome.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "monochrome Enter action: %s\n", surfaceControlsActionDebug(action))

	navigation := dashboardWithSession(runningSession())
	second := runningSession()
	second.ID = "another-session"
	navigation.state.Mut().Sessions.Insert(second.ID, second)
	publishFixtureVerdicts(navigation)
	navigation.FocusSessions()
	navigation.setSelectionFor(FocusSessions, 0)
	surfaceControlsAppendState(&output, "navigation starts at first session", navigation, width, height)
	for _, step := range []struct {
		label string
		code  rune
	}{
		{"Down selects second session", tea.KeyDown},
		{"Up returns to first session", tea.KeyUp},
		{"Up reaches Create", tea.KeyUp},
		{"Right reaches Open", tea.KeyRight},
		{"Left returns to Create", tea.KeyLeft},
		{"Down returns to first session", tea.KeyDown},
	} {
		navigation.HandleKey(key(step.code))
		surfaceControlsAppendState(&output, step.label, navigation, width, height)
		surfaceControlsAppendActionFocus(&output, navigation)
	}
	action = navigation.HandleKey(key(tea.KeyEnter))
	surfaceControlsAppendState(&output, "Enter opens the first session", navigation, width, height)
	fmt.Fprintf(&output, "action: %s\n", surfaceControlsActionDebug(action))

	noSessions := dashboardWithSession(runningSession())
	noSessions.state.Mut().Sessions.Clear()
	*noSessions.sessionDetails.Mut() = map[string]*SessionDetail{}
	noSessions.FocusSessions()
	surfaceControlsAppendState(&output, "empty list selects Create", noSessions, width, height)
	surfaceControlsAppendActionFocus(&output, noSessions)
	noSessions.HandleKey(key(tea.KeyRight))
	surfaceControlsAppendState(&output, "empty list can select Open", noSessions, width, height)
	action = noSessions.HandleKey(key(tea.KeyEnter))
	surfaceControlsAppendState(&output, "empty list opens the picker", noSessions, width, height)
	fmt.Fprintf(&output, "action: %s\n", surfaceControlsActionDebug(action))

	disabled := dashboardWithSession(runningSession())
	disabled.config.Mut().Profiles = map[string]core.HarnessProfile{}
	disabled.FocusSessions()
	lines := surfaceControlsAppendState(&output, "Create disabled without profiles", disabled, width, height)
	surfaceControlsAppendButtonStyles(&output, disabled, width, height)
	disabled.HandleKey(key(tea.KeyUp))
	surfaceControlsAppendState(&output, "keyboard skips disabled Create", disabled, width, height)
	surfaceControlsAppendActionFocus(&output, disabled)
	disabled.HandleKey(key(tea.KeyLeft))
	surfaceControlsAppendState(&output, "Left stays on enabled Open", disabled, width, height)
	surfaceControlsAppendActionFocus(&output, disabled)
	action = surfaceControlsClick(t, disabled, point(lines, "Open"))
	surfaceControlsAppendState(&output, "mouse still activates Open", disabled, width, height)
	fmt.Fprintf(&output, "action: %s\n", surfaceControlsActionDebug(action))

	core.AssertGolden(t, "testdata", "sessions-pane-actions", output.String())
}

func TestGoldenSessionsFilter(t *testing.T) {
	var output strings.Builder
	for _, variant := range []struct {
		symbols core.SymbolSet
		close   rune
		dot     rune
		name    string
	}{
		{core.SymbolSetUnicode, '×', '·', "Unicode"},
		{core.SymbolSetAscii, 'x', '-', "ASCII"},
	} {
		dashboard := dashboardWithSession(runningSession())
		config := *dashboard.config.Get()
		symbols := variant.symbols
		config.Advanced.Symbols = &symbols
		dashboard.SetConfig(config)
		dashboard.FocusSessions()
		dashboard.HandleKey(key('/'))
		for _, character := range "ses" {
			dashboard.HandleKey(key(character))
		}
		dashboard.HandleKey(key(tea.KeyEnter))
		dashboard.HandleKey(key('b'))
		filter := **dashboard.sessionsFilter.Get()
		label := fmt.Sprintf("/ses %c blocked", variant.dot)
		lines := surfaceControlsAppendState(&output, variant.name+" state filter and title clear chip", dashboard, 120, 40)
		at := point(lines, fmt.Sprintf("%s %c ", label, variant.close))
		last := at.X + utf8.RuneCountInString(label) - 1
		for _, cell := range []struct {
			which  string
			column int
		}{{"first label cell", at.X}, {"last label cell", last}} {
			action := surfaceControlsClick(t, dashboard, uv.Pos(cell.column, at.Y))
			current := *dashboard.sessionsFilter.Get()
			if current == nil || !reflect.DeepEqual(*current, filter) {
				t.Fatalf("%s: %s should leave the filter", variant.name, cell.which)
			}
			surfaceControlsAppendState(&output, fmt.Sprintf("%s click on %s leaves filter active", variant.name, cell.which), dashboard, 120, 40)
			fmt.Fprintf(&output, "click=(%d,%d); action=%s; filter retained=true\n", cell.column, at.Y, surfaceControlsActionDebug(action))
		}
		chip := last + 2
		action := surfaceControlsClick(t, dashboard, uv.Pos(chip, at.Y))
		if *dashboard.sessionsFilter.Get() != nil {
			t.Fatalf("%s: chip clears filter", variant.name)
		}
		surfaceControlsAppendState(&output, variant.name+" clear chip drops search and state filters", dashboard, 120, 40)
		fmt.Fprintf(&output, "chip=(%d,%d) glyph='%c'; action=%s; filter active=false\n", chip, at.Y, variant.close, surfaceControlsActionDebug(action))
	}

	for _, width := range []int{140, 80} {
		dashboard := dashboardWithSession(runningSession())
		lines := surfaceControlsAppendState(&output, fmt.Sprintf("inactive filter input follows buttons at %d columns", width), dashboard, width, 40)
		row := surfaceControlsActionsRow(dashboard, lines)
		fmt.Fprintf(&output, "Sessions action row: %s; filter active: false\n", core.RustDebugString(row))
	}

	compact := dashboardWithSession(runningSession())
	compact.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	compact.FocusSessions()
	compact.HandleKey(key('/'))
	for _, character := range "zzz" {
		compact.HandleKey(key(character))
	}
	lines := surfaceControlsAppendState(&output, "minimized pane applies the filter while hiding the input", compact, 140, 40)
	fmt.Fprintf(&output, "action row: %s; hidden sessions: %d\n",
		core.RustDebugString(surfaceControlsActionsRow(compact, lines)), compact.sessionsHiddenCount())

	typing := dashboardWithSession(runningSession())
	lines = surfaceControlsDraw(typing, 140, 40)
	at := point(lines, "Filter:")
	action := surfaceControlsClick(t, typing, uv.Pos(at.X+2, at.Y))
	editing := func(dashboard *DashboardState) bool {
		filter := *dashboard.sessionsFilter.Get()
		return filter != nil && filter.Editing
	}
	fmt.Fprintf(&output, "click Filter input: %s; editing: %t\n", surfaceControlsActionDebug(action), editing(typing))
	for _, character := range "nomatch" {
		typing.HandleKey(key(character))
	}
	lines = surfaceControlsAppendState(&output, "typed filter narrows the Sessions list", typing, 140, 40)
	query, cursor := surfaceControlsFilterState(t, typing)
	fmt.Fprintf(&output, "query/cursor: (%s, %d); hidden sessions: %d; row: %s\n",
		core.RustDebugString(query), cursor, typing.sessionsHiddenCount(),
		core.RustDebugString(surfaceControlsActionsRow(typing, lines)))
	typing.HandleKey(key(tea.KeyEscape))
	if *typing.sessionsFilter.Get() != nil {
		t.Fatal("Esc clears the filter")
	}
	surfaceControlsAppendState(&output, "Esc clears the filter input", typing, 140, 40)
	output.WriteString("filter active: false\n")

	elsewhere := runningSession()
	elsewhere.ID = "other-workspace"
	elsewhere.WorkspaceID = "elsewhere"
	elsewhere.Title = "remote needle"
	elsewhere.ACPSessionTitle = nil
	cross := dashboardWithSession(elsewhere)
	current := "current"
	*cross.activeWorkspaceID.Mut() = &current
	*cross.sessionsFilter.Mut() = &SessionsFilter{Query: chat.TextInputFromValue("needle"), Editing: true}
	lines = surfaceControlsAppendState(&output, "selected session from another workspace is already displayed, not more", cross, 140, 40)
	if surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("the selected session is already displayed")
	}
	cross.navigation.assign(cross.FocusedPane(), nil)
	if got := cross.sessionsOtherWorkspaceMatches(); got != 1 {
		t.Fatalf("other workspace matches = %d, want 1", got)
	}
	lines = surfaceControlsAppendState(&output, "matching sessions in another workspace offer Open with the same search", cross, 140, 40)
	if !surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatalf("the button should offer the match:\n%s", strings.Join(lines, "\n"))
	}
	for index := 1; index < 10; index++ {
		extra := cross.state.Get().Sessions.Index("other-workspace")
		copied := *extra
		copied.ID = fmt.Sprintf("extra-%d", index)
		cross.state.Mut().Sessions.Insert(copied.ID, copied)
	}
	publishFixtureVerdicts(cross)
	lines = surfaceControlsAppendState(&output, "two digit match count keeps the filter input visible at 80 columns", cross, 80, 40)
	point(lines, "10 more")
	point(lines, "Filter:")
	for index := 1; index < 10; index++ {
		cross.state.Mut().Sessions.Remove(fmt.Sprintf("extra-%d", index))
	}
	publishFixtureVerdicts(cross)
	cross.state.Mut().Sessions.GetMut("other-workspace").Title = "unrelated title"
	lines = surfaceControlsAppendState(&output, "no other workspace matches hides the button", cross, 140, 40)
	if surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("nothing else matches")
	}
	requestID, _, ok := cross.NextSessionsTextSearch()
	if !ok {
		t.Fatal("a search")
	}
	matches := []client.SessionTextMatch{{SessionID: "other-workspace", Kind: client.SessionTextMatchKindUser}}
	cross.ApplySessionsText(requestID, slices.Clone(matches), nil)
	lines = surfaceControlsAppendState(&output, "conversation-only match in another workspace offers Open", cross, 140, 40)
	if !surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("the conversation match is offered")
	}
	derivations, orderDerivations := cross.sessionViewDerivations()
	if got := cross.sessionsOtherWorkspaceMatches(); got != 1 {
		t.Fatalf("other workspace matches = %d, want 1", got)
	}
	if got := cross.sessionsOtherWorkspaceMatches(); got != 1 {
		t.Fatalf("other workspace matches = %d, want 1", got)
	}
	if facts, order := cross.sessionViewDerivations(); facts != derivations || order != orderDerivations {
		t.Fatal("reading the count must not derive the view again")
	}
	(*cross.sessionsFilter.Mut()).Query = chat.TextInputFromValue("needlex")
	if got := cross.sessionsOtherWorkspaceMatches(); got != 0 {
		t.Fatalf("other workspace matches = %d, want 0", got)
	}
	if _, _, ok := cross.NextSessionsTextSearch(); !ok {
		t.Fatal("a search")
	}
	if _, _, ok := cross.RefreshSessionsTextSearch(); !ok {
		t.Fatal("a refresh")
	}
	lines = surfaceControlsAppendState(&output, "prefix conversation matches do not count when sync refreshes the longer query", cross, 140, 40)
	if surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("prefix matches do not count")
	}
	(*cross.sessionsFilter.Mut()).Query = chat.TextInputFromValue("needle")
	requestID, _, ok = cross.NextSessionsTextSearch()
	if !ok {
		t.Fatal("a search")
	}
	cross.ApplySessionsText(requestID, slices.Clone(matches), nil)
	moving := core.SessionTransitionKindMoving
	verdict := cross.sessionVerdicts.Mut().GetMut("other-workspace")
	verdict.Transition = &moving
	lines = surfaceControlsAppendState(&output, "transitioning match is unavailable in Open and not counted", cross, 140, 40)
	if surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("a transitioning match is not counted")
	}
	cross.sessionVerdicts.Mut().GetMut("other-workspace").Transition = nil
	cross.state.Mut().Sessions.GetMut("other-workspace").State = core.SessionStateStopped
	serveAttention(cross, []string{"other-workspace"}, core.SessionAttentionNeedsInput)
	publishFixtureVerdicts(cross)
	lines = surfaceControlsAppendState(&output, "stopped session needing attention remains an additional workspace match", cross, 140, 40)
	if !surfaceControlsAnyLineContains(lines, "1 more") {
		t.Fatal("the stopped session needing attention is counted")
	}
	cross.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	lines = surfaceControlsAppendState(&output, "minimized pane retains the more button and its search handoff", cross, 140, 40)
	open := point(lines, "Open")
	if _, ok := surfaceControlsClick(t, cross, open).(DashboardActionOpenResumeDialog); !ok {
		t.Fatal("Open opens the resume dialog")
	}
	if query := (*cross.sessionsFilter.Get()).Query.Value(); query != "needle" {
		t.Fatalf("query = %q, want needle", query)
	}
	position := point(lines, "1 more")
	filtered, ok := surfaceControlsClick(t, cross, position).(DashboardActionOpenFilteredResumeDialog)
	if !ok {
		t.Fatal("expected filtered Open action")
	}
	cross.ShowResumeDialog(1, nil)
	cross.SetResumeFilter(filtered.Query, filtered.State)
	wikiRequest, wikiQuery, ok := cross.NextWikiSearch()
	if !ok {
		t.Fatal("a wiki search")
	}
	if wikiQuery != "needle" {
		t.Fatalf("wiki query = %q, want needle", wikiQuery)
	}
	cross.ApplyResumeTextSearchResult(wikiRequest, matches, nil)
	surfaceControlsAppendState(&output, "Open keeps the workspace search and shows the other workspace match", cross, 140, 40)

	resumeDialogMut(&cross.mode).form.Focus(ResumeFocusSessions)
	if _, ok := cross.HandleKey(key(tea.KeyEnter)).(DashboardActionNone); !ok {
		t.Fatal("Enter opens the wizard and returns no action")
	}
	wizard, ok := cross.mode.(ModeResume)
	if !ok {
		t.Fatalf("stopped match must open the resume wizard: %T", cross.mode)
	}
	if wizard.workspaceID != "elsewhere" {
		t.Fatalf("wizard workspace = %q, want elsewhere", wizard.workspaceID)
	}
	surfaceControlsAppendState(&output, "Open on a stopped remote match starts resume in its own workspace", cross, 140, 40)

	single := *cross.config.Get()
	single.Profiles = map[string]core.HarnessProfile{"codex-1": single.Profiles["codex-1"]}
	single.Targets = map[string]core.TargetTemplate{"machine": core.TargetTemplateLocalBare{}}
	cross.SetConfig(single)
	{
		session := cross.state.Mut().Sessions.GetMut("other-workspace")
		session.TargetTemplateID = "machine"
		directory := "/srv/project"
		session.ProjectDirectory = &directory
	}
	cross.ShowResumeDialog(2, nil)
	cross.SetResumeFilter("", nil)
	preflight, ok := cross.HandleKey(key(tea.KeyEnter)).(DashboardActionPreflightResumeRepositories)
	if !ok {
		t.Fatal("single bare target must preflight immediately")
	}
	if resume, ok := preflight.Launch.(DashboardActionResumeSession); !ok || resume.WorkspaceID != "elsewhere" {
		t.Fatalf("launch = %#v, want a resume in elsewhere", preflight.Launch)
	}
	output.WriteString("single bare target preflight preserves matched workspace: elsewhere\n")

	readline := dashboardWithSession(runningSession())
	readline.FocusSessions()
	readline.HandleKey(key('/'))
	for _, character := range "alpha beta gamma" {
		readline.HandleKey(key(character))
	}
	control := func(character rune) tea.KeyPressMsg { return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl} }
	alt := func(character rune) tea.KeyPressMsg { return tea.KeyPressMsg{Code: character, Mod: tea.ModAlt} }
	for _, step := range []struct {
		label  string
		event  tea.KeyPressMsg
		text   string
		cursor int
	}{
		{"Left", key(tea.KeyLeft), "alpha beta gamma", 15},
		{"Right", key(tea.KeyRight), "alpha beta gamma", 16},
		{"Home", key(tea.KeyHome), "alpha beta gamma", 0},
		{"End", key(tea.KeyEnd), "alpha beta gamma", 16},
		{"Ctrl-A", control('a'), "alpha beta gamma", 0},
		{"Ctrl-E", control('e'), "alpha beta gamma", 16},
		{"Ctrl-B", control('b'), "alpha beta gamma", 15},
		{"Ctrl-F", control('f'), "alpha beta gamma", 16},
		{"Alt-B", alt('b'), "alpha beta gamma", 11},
		{"Alt-B again", alt('b'), "alpha beta gamma", 6},
		{"Alt-F", alt('f'), "alpha beta gamma", 10},
		{"Delete", key(tea.KeyDelete), "alpha betagamma", 10},
		{"Backspace", key(tea.KeyBackspace), "alpha betgamma", 9},
		{"Ctrl-H", control('h'), "alpha begamma", 8},
		{"Ctrl-W", control('w'), "alpha gamma", 6},
		{"Ctrl-K", control('k'), "alpha ", 6},
		{"Ctrl-A before insert", control('a'), "alpha ", 0},
		{"insert x", key('x'), "xalpha ", 1},
		{"Ctrl-U", control('u'), "alpha ", 0},
	} {
		readline.HandleKey(step.event)
		text, cursor := surfaceControlsFilterState(t, readline)
		if text != step.text || cursor != step.cursor {
			t.Fatalf("%s: input = %q@%d, want %q@%d", step.label, text, cursor, step.text, step.cursor)
		}
		surfaceControlsAppendState(&output, "readline "+step.label, readline, 140, 40)
		fmt.Fprintf(&output, "input/cursor: %s@%d\n", core.RustDebugString(text), cursor)
	}
	readline.HandleKey(key(tea.KeyUp))
	surfaceControlsAppendState(&output, "Up leaves text editing for the Sessions pane", readline, 140, 40)
	query, cursor = surfaceControlsFilterState(t, readline)
	fmt.Fprintf(&output, "query/cursor after Up: (%s, %d)\n", core.RustDebugString(query), cursor)

	slash := dashboardWithSession(runningSession())
	slash.FocusSessions()
	slash.HandleKey(key('/'))
	slash.HandleKey(key('q'))
	surfaceControlsAppendState(&output, "slash opens filter input", slash, 140, 40)
	fmt.Fprintf(&output, "editing after slash: %t\n", editing(slash))
	slash.HandleKey(key(tea.KeyEnter))
	surfaceControlsAppendState(&output, "Enter keeps text and leaves editing", slash, 140, 40)
	query, cursor = surfaceControlsFilterState(t, slash)
	fmt.Fprintf(&output, "query after Enter: (%s, %d); editing: %t\n", core.RustDebugString(query), cursor, editing(slash))
	slash.HandleKey(key('/'))
	surfaceControlsAppendState(&output, "slash returns focus to the filter input", slash, 140, 40)
	fmt.Fprintf(&output, "editing after slash: %t\n", editing(slash))
	slash.HandleKey(key(tea.KeyEscape))
	surfaceControlsAppendState(&output, "Esc after slash removes filter", slash, 140, 40)
	fmt.Fprintf(&output, "filter active: %t\n", *slash.sessionsFilter.Get() != nil)

	ascii := dashboardWithSession(runningSession())
	asciiConfig := *ascii.config.Get()
	asciiSymbols := core.SymbolSetAscii
	asciiConfig.Advanced.Symbols = &asciiSymbols
	ascii.SetConfig(asciiConfig)
	ascii.FocusSessions()
	ascii.HandleKey(key('/'))
	ascii.HandleKey(key('q'))
	lines = surfaceControlsAppendState(&output, "ASCII symbols keep the filter row ASCII", ascii, 140, 40)
	row := surfaceControlsActionsRow(ascii, lines)
	for _, character := range row {
		if character > unicode.MaxASCII {
			t.Fatalf("filter row %q is not ASCII", row)
		}
	}
	fmt.Fprintf(&output, "filter row: %s; ASCII: true\n", core.RustDebugString(row))

	branch := dashboardWithSession(runningSession())
	branchSession := runningSession()
	branchSession.ID = "branchy-session"
	launchBranch := "feature/octopus"
	branchSession.LaunchBranch = &launchBranch
	branch.state.Mut().Sessions.Insert(branchSession.ID, branchSession)
	publishFixtureVerdicts(branch)
	branch.FocusSessions()
	branch.HandleKey(key('/'))
	for _, character := range "OCTOPUS" {
		branch.HandleKey(key(character))
	}
	if got := branch.sessionsHiddenCount(); got != 0 {
		t.Fatalf("hidden = %d, want 0", got)
	}
	surfaceControlsAppendState(&output, "branch name matches without regard to case", branch, 140, 40)
	output.WriteString("hidden sessions: 0\n")
	branch.HandleKey(key('x'))
	if got := branch.sessionsHiddenCount(); got != 1 {
		t.Fatalf("hidden = %d, want 1", got)
	}
	surfaceControlsAppendState(&output, "additional text hides the branch match", branch, 140, 40)
	output.WriteString("hidden sessions: 1\n")

	core.AssertGolden(t, "testdata", "sessions-filter", output.String())
}
