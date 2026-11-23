// Port of mj-tui/src/palette.rs tests.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func paletteTypeQuery(dashboard *DashboardState, query string) {
	for _, character := range query {
		dashboard.HandleKey(key(character))
	}
}

// paletteRowOf is the row of a drawn palette, or -1 when the text is not on
// screen.
func paletteRowOf(lines []string, needle string) int {
	return slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, needle) })
}

// paletteSelectedCommand is the highlighted palette command, if the palette is
// open and has one.
func paletteSelectedCommand(dashboard *DashboardState) (CommandId, bool) {
	palette, ok := dashboard.mode.(ModePalette)
	if !ok || palette.selected < 0 || palette.selected >= len(palette.entries) {
		return 0, false
	}
	return palette.entries[palette.selected].id, true
}

// paletteSelectedCommandDebug is Rust's Debug of selected_command's
// `Option<CommandId>`.
func paletteSelectedCommandDebug(dashboard *DashboardState) string {
	id, ok := paletteSelectedCommand(dashboard)
	if !ok {
		return "None"
	}
	return "Some(" + id.String() + ")"
}

func paletteGoldenState(output *strings.Builder, label string, width, height int, lines []string) {
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
}

// paletteGoldenValue is Rust's append_golden_value; value is already its
// Debug text.
func paletteGoldenValue(output *strings.Builder, label, value string) {
	fmt.Fprintf(output, "%s: %s\n", label, value)
}

// The session menu names a full disk on the session's host, with the
// figures the daemon's storage owner measured.
//
// Hard-won: 540c9202494a: a full target disk was reported only as unreachable.
func TestSessionFactsNameAFullDiskOnTheSessionsHost(t *testing.T) {
	session := precisionSession()
	dashboard := dashboardWithSession(session)
	text := func(dashboard *DashboardState) string {
		facts := paletteSessionFacts(dashboard, &session)
		lines := make([]string, 0, len(facts))
		for _, fact := range facts {
			lines = append(lines, fact.String())
		}
		return strings.Join(lines, "\n")
	}
	if strings.Contains(text(dashboard), "Disk") {
		t.Fatal(text(dashboard))
	}
	dashboard.SetTargetStorage([]core.TargetStorageView{precisionStorage(40<<30, 0)})
	facts := text(dashboard)
	if !strings.Contains(facts, "Disk       disk full: precision-3260 has 0 B free on /home/jonathan/Projects") {
		t.Fatal(facts)
	}
	// A healthy filesystem the session writes to says nothing.
	if strings.Count(facts, "Disk ") != 1 {
		t.Fatal(facts)
	}
}

// The query is a text field, so readline's Ctrl-U and Ctrl-D keep editing
// it while there is text to edit. The list borrows them for paging only once
// the query is empty, where they would otherwise do nothing.
func TestPaletteCtrlUAndCtrlDEditTheQueryUntilItIsEmpty(t *testing.T) {
	ctrl := func(character rune) tea.KeyPressMsg {
		return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
	}
	query := func(dashboard *DashboardState) (string, int) {
		palette, ok := dashboard.mode.(ModePalette)
		if !ok {
			t.Fatal("the palette stays open")
		}
		return palette.query.Value(), palette.selected
	}

	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteTypeQuery(dashboard, "rename")
	drawn(dashboard, 120, 30)
	_, selected := query(dashboard)

	// Ctrl-D deletes forward within the text rather than paging the list.
	dashboard.HandleKey(key(tea.KeyHome))
	dashboard.HandleKey(ctrl('d'))
	if text, now := query(dashboard); text != "ename" || now != selected {
		t.Fatalf("after Ctrl-D: %q, %d", text, now)
	}

	// Ctrl-U kills back to the start of the line, and the selection stays
	// where the shortened query puts it rather than jumping eight rows.
	dashboard.HandleKey(key(tea.KeyEnd))
	dashboard.HandleKey(ctrl('u'))
	text, selected := query(dashboard)
	if text != "" {
		t.Fatalf("after Ctrl-U: %q", text)
	}
	drawn(dashboard, 120, 30)

	// With nothing left to edit, the same chord pages the list.
	dashboard.HandleKey(ctrl('d'))
	text, paged := query(dashboard)
	if text != "" || paged == selected {
		t.Fatalf("ctrl+d must page an empty palette: %q, %d", text, paged)
	}
	dashboard.HandleKey(ctrl('u'))
	if text, now := query(dashboard); text != "" || now != selected {
		t.Fatalf("after paging back: %q, %d", text, now)
	}
}

// A-3, palette half: a blocked row gives its reason as a dimmed,
// capitalised sentence after " — ", as help does, not as a lowercase
// fragment in brackets.
//
// Hard-won: 7ea16bf8220e: blocked palette reasons appeared as raw lowercase bracket fragments.
func TestPaletteRowsSetUnavailabilityReasonsApartLikeHelp(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.beginPalette()
	paletteTypeQuery(dashboard, "dismiss")
	joined := strings.Join(drawn(dashboard, 120, 30), "\n")
	if !strings.Contains(joined, "Dismiss notification — No dismissible notification is active.") {
		t.Fatal(joined)
	}
	if strings.Contains(joined, "[No dismissible") {
		t.Fatal(joined)
	}
}

// Hard-won: e063b7f41d77: palette command availability stayed stale after session creation finished.
func TestOpenPaletteEnablesRenameWhenSessionCreationFinishes(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.beginSessionPalette()
	drawn(dashboard, 120, 30)
	if id, ok := paletteSelectedCommand(dashboard); !ok || id != CommandIdIgnoreNotifications {
		t.Fatalf("selected %s", paletteSelectedCommandDebug(dashboard))
	}
	dashboard.FinishSessionOperation("session-1")
	drawn(dashboard, 120, 30)
	dashboard.HandleKey(key(tea.KeyDown))
	if id, ok := paletteSelectedCommand(dashboard); !ok || id != CommandIdRenameSession {
		t.Fatalf("selected %s", paletteSelectedCommandDebug(dashboard))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeRename); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// measured is a measured CPU view of recent and hourly permille.
func paletteMeasuredCPU(recent, hourly uint16) client.SessionCpuView {
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

// paletteSessionMenuLayout is the open session menu as its lines: a named divider
// as `[name]`, the unnamed divider as an empty line, and each command by its
// menu label.
func paletteSessionMenuLayout(t *testing.T, dashboard *DashboardState) []string {
	t.Helper()
	palette, ok := dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the session menu is open")
	}
	var layout []string
	for _, line := range paletteLines(dashboard, palette.CommandPalette) {
		switch line := line.(type) {
		case PaletteLineHeading:
			layout = append(layout, "["+string(line)+"]")
		case PaletteLineBlank:
			layout = append(layout, "")
		case PaletteLineCommand:
			layout = append(layout, sessionMenuLabel(dashboard, palette.CommandPalette, palette.entries[int(line)].id))
		}
	}
	return layout
}

// The menu opens with the session's model, effort, and profile as one bare
// line above the labeled facts.
func TestTheSessionMenuOpensWithModelEffortAndProfile(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	model, effort := "gpt-6.1-sol", "high"
	dashboard.SetSessionCurrentConfig("session-1", &model, &effort)
	dashboard.DispatchCommand(CommandIdSessionActions)
	lines := drawn(dashboard, 120, 40)
	info := point(lines, "gpt-6.1-sol · high · codex-1")
	content := point(lines, "Content")
	if info.Y >= content.Y {
		t.Fatal("the info line sits above the sections")
	}
}

// The menu's Ignore row is one option that says which way it flips.
func TestTheIgnoreRowReadsTheSessionsNotificationPreference(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.DispatchCommand(CommandIdSessionActions)
	if layout := paletteSessionMenuLayout(t, dashboard); !slices.Contains(layout, "Ignore") {
		t.Fatalf("%#v", layout)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatal("Esc closes the menu")
	}
	record := dashboard.state.Mut().Sessions.GetMut("session-1")
	if record == nil {
		t.Fatal("the fixture session")
	}
	record.NotificationsIgnored = true
	dashboard.DispatchCommand(CommandIdSessionActions)
	if layout := paletteSessionMenuLayout(t, dashboard); !slices.Contains(layout, "Stop ignoring") {
		t.Fatalf("%#v", layout)
	}
}

// R10-2: a native child's menu offered Rename, Pin, Restart and Destroy,
// none of which a harness-owned child can take: its parent's harness owns
// it, and Mjolnir has no record of its own to rename or destroy. The menu
// offers what does apply, which is opening its conversation.
//
// Hard-won: 936d6ee93c8a: native child menus offered unsupported record actions.
func TestANativeChildsMenuOffersOnlyWhatAppliesToIt(t *testing.T) {
	dashboard, parentID, id := dashboardWithFinishedNativeChild()
	dashboard.OpenSubagentWorkspace(parentID)
	dashboard.HandleKey(key('a'))
	dashboard.SelectActiveSession(id)
	if selected, ok := dashboard.SelectedSessionID(); !ok || selected != id {
		t.Fatalf("selected %q", selected)
	}
	dashboard.FocusSessions()
	dashboard.DispatchCommand(CommandIdSessionActions)
	if layout := paletteSessionMenuLayout(t, dashboard); !slices.Equal(layout, []string{"[Content]", "Open"}) {
		t.Fatalf("%#v", layout)
	}
	lines := drawn(dashboard, 120, 40)
	if paletteRowOf(lines, "Review calc · completed") < 0 {
		t.Fatalf("the menu is titled with the child's name: %#v", lines)
	}
	if action := dashboard.HandleKey(key(tea.KeyEnter)); action != DashboardAction(DashboardActionOpen{SessionID: id}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// Launch finding R11-4: a native child's Open said "Show the selected
// session's conversation and type in it.", but its pane is read-only
// ("controlled by parent"), and so is a stopped sub-agent's. Open says so
// for both, and keeps its description for a session you can type in.
//
// Hard-won: e5b2818d4996: Open described a read-only child conversation as writable.
func TestOpenDescribesAReadOnlyConversationAsReadOnly(t *testing.T) {
	readOnly := "Show the selected agent's conversation (read-only)."
	dashboard, parentID, id := dashboardWithFinishedNativeChild()
	if got := commandDescription(dashboard, CommandIdOpenSession); got != spec(CommandIdOpenSession).description {
		t.Fatalf("the parent itself is a session you can type in: %q", got)
	}
	dashboard.OpenSubagentWorkspace(parentID)
	dashboard.HandleKey(key('a'))
	dashboard.SelectActiveSession(id)
	dashboard.FocusSessions()
	dashboard.DispatchCommand(CommandIdSessionActions)
	screen := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(screen, readOnly) || strings.Contains(screen, "type in it") {
		t.Fatal(screen)
	}

	dashboard, parentID = dashboardWithOneSubagent()
	state := *dashboard.state.Get()
	child := state.Sessions.GetMut("child-session")
	child.State = core.SessionStateStopped
	dashboard.SetState(state)
	dashboard.OpenSubagentWorkspace(parentID)
	dashboard.HandleKey(key('a'))
	dashboard.SelectActiveSession("child-session")
	dashboard.FocusSessions()
	if selected, ok := dashboard.SelectedSessionID(); !ok || selected != "child-session" {
		t.Fatalf("selected %q", selected)
	}
	if got := commandDescription(dashboard, CommandIdOpenSession); got != readOnly {
		t.Fatalf("description %q", got)
	}
}

// The palette's whole point: the commands for the session you are looking
// at come first, under a heading saying which session that is.
func TestPaletteShowsTheSelectedRowAndScrollsTheListOnAShortTerminal(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	openPalette(dashboard)

	// Focus the list and select its final command before drawing the
	// constrained viewport. The shared list renderer must reveal it and
	// publish the same offset used by the scrollbar.
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(key(tea.KeyEnd))
	buffer, _ := combinedDraw(100, 18, func(frame *chat.Frame) { combinedRender(frame, dashboard) })

	lines := bufferLines(buffer)
	palette, ok := dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the palette chord should leave the palette open")
	}
	selectedLabel := spec(palette.entries[palette.selected].id).label
	if palette.selected != max(len(palette.entries)-1, 0) {
		t.Fatal("End selects the final command")
	}
	if palette.form.ListOffset(PaletteControlCommands) <= 0 {
		t.Fatalf("the selected final command requires scrolling: %#v", lines)
	}
	if paletteRowOf(lines, selectedLabel) < 0 {
		t.Fatal("the selected row is visible")
	}
}

// From the composer the selection is the conversation on screen, so the
// palette still leads with that session's commands. Launch campaign finding
// A-6: a command run from the palette appears under Recent even when it also
// has a pane key, as Create session does, and even when it opens a wizard.
//
// Hard-won: a93d64c805c9: palette commands with pane keys were missing from Recent.
func TestACommandRunFromThePaletteIsListedUnderRecent(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if len(spec(CommandIdNewSessionWizard).paneKeys) == 0 {
		t.Fatal("Create session has a pane key")
	}

	openPalette(dashboard)
	paletteTypeQuery(dashboard, "create session")
	if id, ok := paletteSelectedCommand(dashboard); !ok || id != CommandIdNewSessionWizard {
		t.Fatalf("selected %s", paletteSelectedCommandDebug(dashboard))
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeNew); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.HandleKey(key(tea.KeyEscape))

	openPalette(dashboard)
	palette, ok := dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the palette stays open")
	}
	if palette.entries[0].id != CommandIdNewSessionWizard || !palette.entries[0].recent {
		t.Fatalf("%#v", palette.entries[0])
	}
}

// Enter runs the row the cursor is on, so the cursor has to follow the
// ranking. A command run earlier leads the unfiltered list under Recent, and
// the cursor used to ride that command down into the results of the next
// search: the screen pointed at the top match while Enter ran the command
// from last time.
//
// Hard-won: fb0622d0f159: Enter ran a stale Recent command instead of the highlighted match.
func TestANewPaletteQueryPutsTheCursorOnItsTopMatch(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()

	openPalette(dashboard)
	paletteTypeQuery(dashboard, "workspaces")
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeWorkspaceManager); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}

	// The unfiltered list leads with what was just run.
	openPalette(dashboard)
	palette, ok := dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the palette stays open")
	}
	if palette.entries[0].id != CommandIdWorkspaces || !palette.entries[0].recent {
		t.Fatalf("%#v", palette.entries[0])
	}

	// "Workspaces" carries "rename" in its description, so it still matches
	// the query — at the bottom of the results, where the cursor must not
	// follow it.
	paletteTypeQuery(dashboard, "rename")
	palette, ok = dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the palette stays open")
	}
	if palette.entries[0].id != CommandIdRenameSession {
		t.Fatalf("%#v", palette.entries)
	}
	if id, ok := paletteSelectedCommand(dashboard); !ok || id != CommandIdRenameSession {
		t.Fatalf("the cursor sits on the top match: %#v", palette.entries)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeRename); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// A run of the query inside a word is what a reader means by a match.
// Letters merely found in order, which any long label can supply, must not
// outrank it, and a label that starts with the query still wins.
//
// Hard-won: fb0622d0f159: scattered letters outranked a contiguous query match.
func TestAContiguousLabelMatchOutranksAScatteredOne(t *testing.T) {
	contiguous, ok := matchScore("Toggle transcript rendering", "", "rend")
	if !ok {
		t.Fatal("a contiguous match")
	}
	scattered, ok := matchScore("Resize pane down", "", "rend")
	if !ok {
		t.Fatal("a scattered match")
	}
	if contiguous <= scattered {
		t.Fatalf("'rend' in 'rendering' beats R-e-n-d across three words: %d vs %d", contiguous, scattered)
	}
	prefix, ok := matchScore("Rendering", "", "rend")
	if !ok {
		t.Fatal("a prefix match")
	}
	if prefix <= contiguous {
		t.Fatalf("%d vs %d", prefix, contiguous)
	}
}

// The same ranking through the palette, on the two commands that showed the
// defect.
//
// Hard-won: fb0622d0f159: searching “rend” ranked Resize above the literal rendering match.
func TestPaletteRanksToggleRenderingAboveResizePaneDownForRend(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	var ranked []CommandId
	for _, entry := range paletteEntries(dashboard, "rend") {
		ranked = append(ranked, entry.id)
	}
	rendering := slices.Index(ranked, CommandIdToggleTranscriptRendering)
	resize := slices.Index(ranked, CommandIdResizePaneDown)
	if rendering < 0 || resize < 0 {
		t.Fatalf("%v", ranked)
	}
	if rendering >= resize {
		t.Fatalf("%v", ranked)
	}
}

// Container settings make no sense for a session that is not on a
// container, so the palette leaves the row out rather than greying it. A
// command that is blocked rather than meaningless stays visible and says
// why, and pressing Enter on it explains instead of acting.
func TestPaletteGreysStopWhileAnOperationIsInFlightAndExplainsWhy(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	display := operation(SessionOperationKindLaunching, nil)
	operations := dashboard.sessionOperations.Mut()
	if *operations == nil {
		*operations = map[string]*SessionOperationDisplay{}
	}
	(*operations)["session-1"] = &display

	entries := paletteEntries(dashboard, "rename")
	if entries[0].id != CommandIdRenameSession {
		t.Fatalf("%#v", entries)
	}
	if entries[0].availability != Availability(AvailabilityBlocked{Reason: "a session transition is in progress"}) {
		t.Fatalf("%#v", entries[0].availability)
	}

	openPalette(dashboard)
	paletteTypeQuery(dashboard, "rename")
	lines := strings.Join(drawn(dashboard, 120, 44), "\n")
	if !strings.Contains(lines, "Rename session — A session transition is in progress.") {
		t.Fatal(lines)
	}

	if action := dashboard.HandleKey(key(tea.KeyEnter)); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatal("the palette stays open")
	}
	if notice, ok := dashboard.Notice(); !ok || notice != "Rename session is unavailable: a session transition is in progress." {
		t.Fatalf("notice %q", notice)
	}
}

// paletteRightClickFirstRow right-clicks the first session row and returns the
// screen with the row's context menu open.
func paletteRightClickFirstRow(dashboard *DashboardState) []string {
	drawn(dashboard, 120, 40)
	row := dashboard.sessionRowAreas[0].Second
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseRight}, uv.Pos(row.Min.X+1, row.Min.Y)))
	return drawn(dashboard, 120, 40)
}

// paletteClickAt presses and releases the left button at one cell.
func paletteClickAt(dashboard *DashboardState, at uv.Position) DashboardAction {
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	return dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
}

// paletteNoticeDebug is Rust's Debug of an `Option<String>` notice.
func paletteNoticeDebug(notice string, ok bool) string {
	return combinedOptionString(notice, ok)
}

// Hard-won: 92684ad3b: dismissing a question must also acknowledge its unread turn.
func TestGoldenSessionActionMenu(t *testing.T) {
	var output strings.Builder

	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "session action menu", 120, 40, drawn(dashboard, 120, 40))

	session := runningSession()
	session.ResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 8 << 30}
	session.Subagents = &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	dashboard = dashboardWithSession(session)
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "session facts", 120, 40, drawn(dashboard, 120, 40))

	dashboard, parent := dashboardWithOneSubagent()
	var cpu core.SnapshotMap[string, client.SessionCpuView]
	cpu.Insert(parent, paletteMeasuredCPU(30, 20))
	dashboard.SetSessionCPU(cpu)
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "sub-agent CPU coverage", 120, 40, drawn(dashboard, 120, 40))

	dashboard = dashboardWithSession(runningSession())
	cpu.Insert(runningSession().ID, paletteMeasuredCPU(230, 100))
	dashboard.SetSessionCPU(cpu)
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "single-session CPU", 120, 40, drawn(dashboard, 120, 40))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "CPU not measured", 120, 40, drawn(dashboard, 120, 40))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	lines := paletteRightClickFirstRow(dashboard)
	paletteGoldenState(&output, "row context menu, nothing to dismiss", 120, 40, lines)
	dismiss := point(lines, "Dismiss notification")
	for _, kind := range []tea.MouseMsg{
		tea.MouseClickMsg{Button: tea.MouseLeft},
		tea.MouseReleaseMsg{Button: tea.MouseLeft},
	} {
		action := dashboard.HandleMouse(mouseAt(kind, dismiss))
		paletteGoldenValue(&output, "disabled context Dismiss click", combinedActionDebug(action))
	}
	action := dashboard.HandleKey(key('1'))
	paletteGoldenValue(&output, "disabled context Dismiss digit", combinedActionDebug(action))
	action = dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "context menu Enter opens on Copy", combinedActionDebug(action))
	paletteGoldenState(&output, "after the context menu copy", 120, 40, drawn(dashboard, 120, 40))

	// The workspace context menu uses the rendered tab hitbox.
	dashboard = dashboardWithSession(runningSession())
	drawn(dashboard, 120, 40)
	workspace := dashboard.workspaceTabAreas[0].Second
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseRight}, uv.Pos(workspace.Min.X, workspace.Min.Y)))
	paletteGoldenState(&output, "workspace menu, no dismissible notifications", 120, 40, drawn(dashboard, 120, 40))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	lines = drawn(dashboard, 120, 40)
	copyButton := point(lines, "Copy session ID")
	paletteGoldenState(&output, "copy session ID menu row", 120, 40, lines)
	action = paletteClickAt(dashboard, copyButton)
	paletteGoldenValue(&output, "action", combinedActionDebug(action))
	paletteGoldenState(&output, "after copying the session ID", 120, 40, drawn(dashboard, 120, 40))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.beginSessionPalette()
	lines = drawn(dashboard, 120, 40)
	dismiss = point(lines, "Dismiss notification")
	dashboard.HandleKey(key(tea.KeyDown))
	paletteGoldenValue(&output, "keyboard selection after skipping disabled Dismiss", paletteSelectedCommandDebug(dashboard))
	down := dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, dismiss))
	paletteGoldenValue(&output, "disabled Dismiss mouse-down action", combinedActionDebug(down))
	up := dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, dismiss))
	paletteGoldenValue(&output, "disabled Dismiss mouse-up action", combinedActionDebug(up))
	paletteGoldenValue(&output, "selection after disabled click", paletteSelectedCommandDebug(dashboard))
	paletteGoldenState(&output, "disabled Dismiss remains open", 120, 40, drawn(dashboard, 120, 40))

	session = runningSession()
	dashboard = dashboardWithSession(session)
	dashboard.SetSessionActivity(session.ID, client.SessionActivity{DismissQuestionSupported: true})
	materialized := materializedSessionFor(session.ID, []*core.TranscriptItem{
		agentMessage(2, "Waiting on you to choose a target."),
	})
	materialized.AppliedEventOrdinal = 4
	materialized.Execution = core.MaterializedExecutionStateIdle{}
	accepted, start := uint64(1), uint64(1)
	materialized.LastTurnOutcome = &core.MaterializedTurnOutcome{
		WorkFacts:         nil,
		AnsweredThrough:   nil,
		CommandID:         "prompt-1",
		AcceptedOrdinal:   &accepted,
		TurnStartPosition: &start,
		CompletedOrdinal:  3,
		CompletedAtMs:     100_000,
		Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
		Diagnostic:        nil,
		Usage:             nil,
	}
	assessment := core.TurnAssessmentPending("prompt-1", 3, 100_000, core.CompletionEvidence{StopReason: "EndTurn"})
	assessment.Status = core.StatusAssessed
	awaitInput := core.AssessmentActionAwaitInput
	assessment.Action = &awaitInput
	serveAttention(dashboard, []string{session.ID}, core.SessionAttentionNeedsInput)

	project := func(dashboard *DashboardState, materialized core.MaterializedSession, assessment core.TurnAssessment) {
		verdict := dashboard.verdict(materialized.SessionID)
		verdict.Unfinished = assessment.InferredQuestion() != nil
		verdict.InterruptAll = verdict.Unfinished
		verdict.InterruptCount = combinedFlag(verdict.Unfinished)
		dashboard.setSessionVerdict(materialized.SessionID, verdict)
		var dismissals core.SnapshotMap[string, core.NotificationDismissal]
		if question := assessment.InferredQuestion(); question != nil {
			dismissals.Insert(materialized.SessionID, core.NotificationDismissalQuestion{Value: *question})
		}
		dashboard.SetNotificationDismissals(dismissals)
		dashboard.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromAssessedMaterialized(
			materialized,
			&assessment,
			0,
			MaterializedProjectionCache{},
		))
	}
	project(dashboard, materialized, assessment)
	drawn(dashboard, 120, 40)
	workspace = dashboard.workspaceTabAreas[0].Second
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseRight}, uv.Pos(workspace.Min.X, workspace.Min.Y)))
	paletteGoldenState(&output, "workspace menu, dismissible notification", 120, 40, drawn(dashboard, 120, 40))
	// Hard-won: workspace Clear notifications: results of dismissals the
	// batch never marked as submitting were silently discarded.
	clearAction := dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "workspace clear notifications", combinedActionDebug(clearAction))
	clearing, ok := clearAction.(DashboardActionDismissNotifications)
	if !ok {
		t.Fatal("Clear notifications dismisses the workspace's notifications")
	}
	results := make([]NotificationDismissalResult, 0, len(clearing.Dismissals))
	for _, dismissal := range clearing.Dismissals {
		results = append(results, NotificationDismissalResult{
			SessionID: dismissal.First,
			Target:    dismissal.Second,
			Err:       fmt.Errorf("daemon refused"),
		})
	}
	dashboard.NotificationsDismissed(results)
	paletteGoldenValue(&output, "notice after a refused workspace clear", paletteNoticeDebug(dashboard.notices.Current()))
	dashboard.ClearNotice()
	dashboard.FocusSessions()
	lines = paletteRightClickFirstRow(dashboard)
	paletteGoldenState(&output, "row context menu, dismissible notification", 120, 40, lines)
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.beginSessionPalette()
	lines = drawn(dashboard, 120, 40)
	paletteGoldenState(&output, "active prose question", 120, 40, lines)
	dismiss = point(lines, "Dismiss notification")
	action = paletteClickAt(dashboard, dismiss)
	paletteGoldenValue(&output, "dismiss exact question", combinedActionDebug(action))
	dismissing, ok := action.(DashboardActionDismissNotification)
	if !ok {
		t.Fatal("dismissal action")
	}
	sessionID, question := dismissing.SessionID, dismissing.Target
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "dismissal in flight", 120, 40, drawn(dashboard, 120, 40))
	dashboard.NotificationDismissalFinished(sessionID, question, 0, fmt.Errorf("Worker disconnected"))
	paletteGoldenValue(&output, "dismissal failure", paletteNoticeDebug(dashboard.Notice()))
	if dashboard.AttentionLevel(sessionID) != AttentionLevelWaiting {
		t.Fatal("the failed dismissal leaves the question waiting")
	}
	if spec(CommandIdDismissNotification).available(dashboard) != Availability(AvailabilityReady{}) {
		t.Fatal("dismissal is ready again")
	}
	if _, ok := dashboard.DispatchCommand(CommandIdDismissNotification).(DashboardActionDismissNotification); !ok {
		t.Fatal("dismissal dispatches")
	}
	dashboard.NotificationDismissalFinished(sessionID, question, 5, nil)
	if spec(CommandIdDismissNotification).available(dashboard) == Availability(AvailabilityReady{}) {
		t.Fatal("an acknowledgement alone must not enable another request before publication")
	}
	assessment.Status = core.StatusSuperseded
	assessment.Reason = "question_dismissed"
	materialized.AppliedEventOrdinal = 6
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionIdle)
	dashboard.state.Mut().Sessions.GetMut(sessionID).ViewedThroughEventOrdinal = 3
	project(dashboard, materialized, assessment)
	if dashboard.AttentionLevel(sessionID) == AttentionLevelWaiting {
		t.Fatal("the dismissed question no longer waits")
	}
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "question dismissed", 120, 40, drawn(dashboard, 120, 40))

	// A later question is eligible again. Its publication can overtake the
	// acknowledgement without leaving dismissal stuck in flight.
	outcome := *materialized.LastTurnOutcome
	outcome.CommandID = "prompt-2"
	outcome.CompletedOrdinal = 8
	materialized.LastTurnOutcome = &outcome
	materialized.AppliedEventOrdinal = 9
	assessment.TurnID = "prompt-2"
	assessment.CompletedOrdinal = 8
	assessment.Status = core.StatusAssessed
	assessment.Reason = "user_input_required"
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionNeedsInput)
	project(dashboard, materialized, assessment)
	if spec(CommandIdDismissNotification).available(dashboard) != Availability(AvailabilityReady{}) {
		t.Fatal("the later question can be dismissed")
	}
	next, ok := dashboard.DispatchCommand(CommandIdDismissNotification).(DashboardActionDismissNotification)
	if !ok {
		t.Fatal("new dismissal")
	}
	dashboard.NotificationDismissalFinished(sessionID, question, 5, nil)
	if !dashboard.notificationDismissalPending(sessionID) {
		t.Fatal("an old callback cannot finish the new request")
	}
	assessment.Status = core.StatusSuperseded
	assessment.Reason = "question_dismissed"
	materialized.AppliedEventOrdinal = 11
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionIdle)
	project(dashboard, materialized, assessment)
	dashboard.NotificationDismissalFinished(sessionID, next.Target, 10, nil)
	if dashboard.notificationDismissalPending(sessionID) {
		t.Fatal("the publication finished the new request")
	}
	paletteGoldenValue(&output, "publication before acknowledgement", paletteNoticeDebug(dashboard.Notice()))

	var readThrough core.SnapshotMap[string, core.NotificationDismissal]
	readThrough.Insert(sessionID, core.NotificationDismissalReadThrough{Value: 12})
	dashboard.SetNotificationDismissals(readThrough)
	dashboard.SetSessionAttentionFor(sessionID, core.SessionAttentionUnread)
	detail := (*dashboard.sessionDetails.Mut())[sessionID]
	detail.agentMessageLatestContentOrdinals = []uint64{12}
	detail.unreadAgentMessages = 1
	if dashboard.AttentionLevel(sessionID) != AttentionLevelUnread {
		t.Fatal("the unread message shows")
	}
	unread, ok := dashboard.DispatchCommand(CommandIdDismissNotification).(DashboardActionDismissNotification)
	if !ok {
		t.Fatal("unread dismissal")
	}
	if unread.Target != core.NotificationDismissal(core.NotificationDismissalReadThrough{Value: 12}) {
		t.Fatalf("target %s", combinedDismissalDebug(unread.Target))
	}
	// A parked session has no worker projection to clear the request.
	(*dashboard.sessionDetails.Mut())[sessionID].materializedAppliedEventOrdinal = nil
	dashboard.NotificationDismissalFinished(sessionID, unread.Target, 12, nil)
	// Hard-won: 6a0981b02: a read dismissal on a parked session stayed pending forever.
	if dashboard.notificationDismissalPending(sessionID) {
		t.Fatal("the parked session's read dismissal finished")
	}
	dashboard.SetSessionAttentionFor(sessionID, core.SessionAttentionIdle)
	dashboard.SetNotificationDismissals(core.SnapshotMap[string, core.NotificationDismissal]{})
	if dashboard.AttentionLevel(sessionID) != AttentionLevelIdle {
		t.Fatal("the session settles idle")
	}
	paletteGoldenValue(&output, "unread dismissed", paletteNoticeDebug(dashboard.Notice()))

	// A parent marked only by its sub-agent's open form has nothing of its
	// own to dismiss; the row in Dismiss's place goes to the child.
	dashboard, parent = dashboardWithOneSubagent()
	setWorking(dashboard, parent)
	setWorking(dashboard, "child-session")
	// The child has only summary state; the daemon supplies the holder.
	var holders core.SnapshotMap[string, string]
	holders.Insert(parent, "child-session")
	holders.Insert("child-session", "child-session")
	dashboard.SetFormHolders(holders)
	serveAttention(dashboard, []string{parent, "child-session"}, core.SessionAttentionNeedsInput)
	dashboard.FocusSessions()
	lines = paletteRightClickFirstRow(dashboard)
	paletteGoldenState(&output, "row context menu, sub-agent approval", 120, 40, lines)
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.beginSessionPalette()
	lines = drawn(dashboard, 120, 40)
	paletteGoldenState(&output, "sub-agent question menu", 120, 40, lines)
	open := point(lines, "Open sub-agent question")
	action = paletteClickAt(dashboard, open)
	paletteGoldenValue(&output, "open sub-agent question action", combinedActionDebug(action))
	paletteGoldenValue(&output, "selected after opening the sub-agent question", combinedOptionString(dashboard.SelectedSessionID()))

	core.AssertGolden(t, "testdata", "session-action-menu", output.String())
}

func TestGoldenCommandPalette(t *testing.T) {
	var output strings.Builder

	for _, alias := range []struct {
		label    string
		question bool
	}{{"prefix+? alias", true}, {"prefix+: alias", false}} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.FocusSessions()
		character := ':'
		if alias.question {
			character = '?'
		}
		route(dashboard, []tea.KeyMsg{prefixKey(), key(character)})
		if _, ok := dashboard.mode.(ModePalette); !ok {
			t.Fatalf("%s: mode %T", alias.label, dashboard.mode)
		}
		paletteGoldenState(&output, alias.label, 120, 40, drawn(dashboard, 120, 40))
	}

	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteGoldenState(&output, "command palette", 120, 100, drawn(dashboard, 120, 100))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.HandleKey(key(tea.KeyEnter))
	openPalette(dashboard)
	paletteGoldenState(&output, "palette opened from the composer", 120, 90, drawn(dashboard, 120, 90))

	for _, search := range []struct{ label, query string }{
		{"label prefix match", "suspend"},
		{"description match", "unread marker"},
	} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.FocusSessions()
		openPalette(dashboard)
		paletteTypeQuery(dashboard, search.query)
		paletteGoldenState(&output, search.label, 120, 44, drawn(dashboard, 120, 44))
	}

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteTypeQuery(dashboard, "open settings")
	paletteGoldenState(&output, "search for Open settings", 120, 30, drawn(dashboard, 120, 30))
	action := dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "action after selecting Open settings", combinedActionDebug(action))
	paletteGoldenValue(&output, "mode after selecting Open settings", core.RustDebugString("Setup"))
	paletteGoldenState(&output, "Setup opened from the palette", 120, 30, drawn(dashboard, 120, 30))

	session := stoppedSession()
	session.TargetTemplateID = "local"
	dashboard = dashboardWithSession(session)
	config := *dashboard.config.Get()
	targets := make(map[string]core.TargetTemplate, len(config.Targets)+1)
	for id, target := range config.Targets {
		targets[id] = target
	}
	targets["local"] = core.TargetTemplateLocalBare{}
	config.Targets = targets
	*dashboard.config.Mut() = config
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteGoldenState(&output, "non-container session commands", 120, 44, drawn(dashboard, 120, 44))

	session = runningSession()
	project := "/srv/project"
	session.ProjectDirectory = &project
	dashboard = dashboardWithSession(session)
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteTypeQuery(dashboard, "suspend")
	paletteGoldenState(&output, "search for Suspend", 120, 44, drawn(dashboard, 120, 44))
	action = dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "suspend action", combinedActionDebug(action))
	paletteGoldenState(&output, "after Suspend", 120, 44, drawn(dashboard, 120, 44))

	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	openPalette(dashboard)
	paletteTypeQuery(dashboard, "stop")
	action = dashboard.HandleKey(key(tea.KeyEscape))
	paletteGoldenValue(&output, "Escape action", combinedActionDebug(action))
	paletteGoldenValue(&output, "notice after Escape", paletteNoticeDebug(dashboard.Notice()))
	commandSession, hasCommandSession := "", false
	if session := dashboard.commandSession(); session != nil {
		commandSession, hasCommandSession = session.ID, true
	}
	paletteGoldenValue(&output, "command session id after Escape", combinedOptionString(commandSession, hasCommandSession))
	paletteGoldenState(&output, "dashboard after Escape", 120, 44, drawn(dashboard, 120, 44))

	core.AssertGolden(t, "testdata", "command-palette", output.String())
}
