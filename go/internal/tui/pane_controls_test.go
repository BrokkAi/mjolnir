// Port of mj-tui/src/pane_controls.rs tests.

package tui

import (
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestPaneMenuTargetsTheClickedPaneAndSwapsWithoutMovingBrowseRole(t *testing.T) {
	d := dashboardWithSession(runningSession())
	area := uv.Rect(0, 0, 120, 40)
	d.conversationAreaField = &area
	pin := d.BrowsePane()
	session := "session-1"
	d.SetPaneSession(pin, &session)
	browse, ok := d.SplitFocusedPane(chat.DirectionHorizontal, nil)
	if !ok {
		t.Fatal("the pane splits")
	}
	d.beginPaneMenu(pin)
	want := DashboardAction(DashboardActionSplitConversation{Pane: pin, Direction: chat.DirectionHorizontal})
	if action := d.HandleKey(key(tea.KeyEnter)); !reflect.DeepEqual(action, want) {
		t.Fatalf("action = %#v, want %#v", action, want)
	}
	d.beginPaneMenu(pin)
	d.HandleKey(key('3'))
	if d.BrowsePane() != browse {
		t.Fatalf("browse pane = %v, want %v", d.BrowsePane(), browse)
	}
	if got, ok := d.PaneSession(pin); !ok || got != "session-1" {
		t.Fatalf("pinned pane = %q, want session-1", got)
	}
	if id, ok := d.PinID("session-1"); !ok || id != 0 {
		t.Fatalf("pin id = %d, %v, want 0", id, ok)
	}
}

// paneControlsMenuLabels is the labels of the open menu's entries.
func paneControlsMenuLabels(t *testing.T, d *DashboardState) []string {
	t.Helper()
	if d.paneMenu == nil {
		t.Fatal("the menu should be open")
	}
	labels := []string{}
	for _, entry := range d.paneMenu.entries {
		labels = append(labels, entry.label)
	}
	return labels
}

// paneControlsRenderMenu draws only the pane menu into a buffer of the
// given size.
func paneControlsRenderMenu(d *DashboardState, width, height int) (*uv.Buffer, []string) {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	renderPaneMenu(frame, frame.Area(), d)
	return buffer, chat.BufferLines(buffer)
}

// Right-clicking a Quota profile row offers Refresh, Usage…, Prefer and
// Clear; Clear is disabled while no preference is set.
func TestQuotaRowRightClickOffersLoadBalancingPreferenceAndClear(t *testing.T) {
	d := dashboardWithSession(runningSession())
	drawn(d, 140, 40)
	index := slices.IndexFunc(d.quotaRowAreas, func(entry client.DaemonPair[string, uv.Rectangle]) bool {
		return entry.First == "codex-1"
	})
	if index < 0 {
		t.Fatal("codex-1 row")
	}
	profile := d.quotaRowAreas[index].First
	rowArea := d.quotaRowAreas[index].Second
	at := uv.Pos(rowArea.Min.X+1, rowArea.Min.Y)
	if action := d.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseRight}, at)); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
		t.Fatalf("right click = %#v, want None", action)
	}
	if labels := paneControlsMenuLabels(t, d); !slices.Equal(labels, []string{
		"Refresh",
		"Usage…",
		"Prefer codex-1 in load balancing",
		"Clear load balancing preferences",
	}) {
		t.Fatalf("labels = %q", labels)
	}
	// Refresh probes just this profile.
	want := DashboardAction(DashboardActionProbeQuotaProfile{ProfileID: "codex-1"})
	if action := d.handlePaneMenuEvent(key(tea.KeyEnter)); !reflect.DeepEqual(action, want) {
		t.Fatalf("Refresh = %#v, want %#v", action, want)
	}
	// Usage… opens the usage dialog with this profile preselected.
	d.beginQuotaRowMenu(profile, at)
	d.handlePaneMenuEvent(key(tea.KeyDown))
	if action := d.handlePaneMenuEvent(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
		t.Fatalf("Usage… = %#v, want None", action)
	}
	usage, ok := d.mode.(ModeSessionUsageReport)
	if !ok {
		t.Fatalf("Usage… did not open the usage dialog: %T", d.mode)
	}
	if usage.query.Profile == nil || *usage.query.Profile != "codex-1" {
		t.Fatalf("usage profile = %v, want codex-1", usage.query.Profile)
	}
	d.CancelModal()
	// Prefer runs from the row's menu.
	d.beginQuotaRowMenu(profile, at)
	d.handlePaneMenuEvent(key(tea.KeyDown))
	d.handlePaneMenuEvent(key(tea.KeyDown))
	want = DashboardActionPreferLoadBalancingProfile{ProfileID: "codex-1"}
	if action := d.handlePaneMenuEvent(key(tea.KeyEnter)); !reflect.DeepEqual(action, want) {
		t.Fatalf("Prefer = %#v, want %#v", action, want)
	}
	// With nothing preferred, Clear is disabled: clicking it keeps the menu.
	d.beginQuotaRowMenu(profile, at)
	_, rows := paneControlsRenderMenu(d, 140, 40)
	clearY := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "Clear load balancing preferences") })
	if clearY < 0 {
		t.Fatal("Clear row")
	}
	clearX := utf8.RuneCountInString(rows[clearY][:strings.Index(rows[clearY], "Clear")])
	for _, kind := range []tea.MouseMsg{tea.MouseClickMsg{Button: tea.MouseLeft}, tea.MouseReleaseMsg{Button: tea.MouseLeft}} {
		if action := d.handlePaneMenuEvent(mouseAt(kind, uv.Pos(clearX, clearY))); !reflect.DeepEqual(action, DashboardAction(DashboardActionNone{})) {
			t.Fatalf("Clear with no preference set = %#v, want None", action)
		}
	}
	if d.paneMenu == nil {
		t.Fatal("the menu stayed open")
	}
	// With a preference set, Clear empties it.
	d.config.Mut().Quota.Prefer = []string{"codex-1"}
	d.beginQuotaRowMenu(profile, at)
	for range 3 {
		d.handlePaneMenuEvent(key(tea.KeyDown))
	}
	if action := d.handlePaneMenuEvent(key(tea.KeyEnter)); !reflect.DeepEqual(action, DashboardAction(DashboardActionClearLoadBalancingPreferences{})) {
		t.Fatalf("Clear = %#v, want ClearLoadBalancingPreferences", action)
	}
}

// Launch campaign finding D-10: the empty-pane chooser opens in the empty
// pane, under its "[1] Pin here" marker, not at the screen's top-left corner
// over the Workspaces pane.
//
// Hard-won: 541cba84d488: the empty-pane chooser covered Workspaces from the screen origin.
func TestTheEmptyPaneChooserOpensInsideTheEmptyPane(t *testing.T) {
	d := dashboardWithSession(runningSession())
	area := uv.Rect(0, 0, 120, 40)
	d.conversationAreaField = &area
	empty := d.BrowsePane()
	if _, ok := d.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the pane splits")
	}
	d.SetPaneSession(empty, nil)
	d.conversationPaneAreas = []conversationPaneArea{
		{pane: empty, transcript: uv.Rect(46, 0, 74, 30), prompt: uv.Rect(46, 30, 74, 10)},
	}
	d.beginPinMenu("session-1")
	d.HandleKey(key('3'))
	buffer, rows := paneControlsRenderMenu(d, 120, 40)
	marker := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "[1] Pin here") })
	if marker < 0 {
		t.Fatal("marker")
	}
	title := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "Choose empty pane") })
	if title < 0 {
		t.Fatal("chooser")
	}
	if title <= marker {
		t.Fatalf("the chooser opens under its marker:\n%s", strings.Join(rows, "\n"))
	}
	column := -1
	for x := range 120 {
		if buffer.CellAt(x, title).Content != " " {
			column = x
			break
		}
	}
	if column < 0 {
		t.Fatal("chooser border")
	}
	if column < 46 {
		t.Fatalf("the chooser sits in the empty pane:\n%s", strings.Join(rows, "\n"))
	}
	// A click on the chooser's own entry picks it through the menu even
	// though the chooser lies inside the pane.
	entry := uv.Pos(column+3, title+1)
	action := d.handlePaneMenuEvent(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, entry))
	if d.paneMenu != nil && d.paneMenu.pressedDestination != nil {
		t.Fatalf("a click on the chooser is not a press on its pane: %#v", action)
	}
}

func TestPromptCommandsAndListSelectionFollowTheActivePin(t *testing.T) {
	d := dashboardWithSession(runningSession())
	pin := d.BrowsePane()
	if _, ok := d.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the pane splits")
	}
	other := runningSession()
	other.ID = "session-2"
	d.state.Mut().Sessions.Insert(other.ID, other)
	d.SetState(*d.state.Get())
	d.SelectActiveSession("session-2")
	d.FocusPane(pin)
	d.FocusPrompt()
	want := DashboardAction(DashboardActionUnpinSession{SessionID: "session-1"})
	if action := d.DispatchCommand(CommandIdPinSession); !reflect.DeepEqual(action, want) {
		t.Fatalf("action = %#v, want %#v", action, want)
	}
	if selected, ok := d.SelectedSessionID(); !ok || selected != "session-1" {
		t.Fatalf("selected = %q, want session-1", selected)
	}
}

func TestUnpinGlyphDoesNotSelectTheSessionOrOpenItsComposer(t *testing.T) {
	d := dashboardWithSession(runningSession())
	pin := d.BrowsePane()
	session := "session-1"
	d.SetPaneSession(pin, &session)
	if _, ok := d.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the pane splits")
	}
	d.SetCurrentSession(nil)
	want := DashboardAction(DashboardActionUnpinSession{SessionID: "session-1"})
	if action := d.toggleSessionPin("session-1"); !reflect.DeepEqual(action, want) {
		t.Fatalf("action = %#v, want %#v", action, want)
	}
	if selected, ok := d.SelectedSessionID(); ok {
		t.Fatalf("selected = %q, want none", selected)
	}
	if d.paneMenu != nil {
		t.Fatal("no menu opens")
	}
}

// paneControlsClick is a press and release at one cell, as a click arrives
// from the terminal.
func paneControlsClick(d *DashboardState, at uv.Position) DashboardAction {
	d.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	return d.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
}

// paneControlsDrawnMenu is where the open dropdown was drawn, after drawing
// the surface once.
func paneControlsDrawnMenu(t *testing.T, d *DashboardState) ([]string, uv.Rectangle) {
	t.Helper()
	lines := drawn(d, 140, 40)
	if d.paneMenu == nil {
		t.Fatal("the menu is open")
	}
	return lines, d.paneMenu.popup
}

func paneControlsAppendGoldenState(output *strings.Builder, label string, width, height int, lines []string) {
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteString("\n")
}

func paneControlsAppendGoldenValue(output *strings.Builder, label string, value string) {
	fmt.Fprintf(output, "%s: %s\n", label, value)
}

// paneControlsEntry is the cell of a menu entry's first letter: inside the
// border, one row per entry.
func paneControlsEntry(popup uv.Rectangle, index int) uv.Position {
	return uv.Pos(popup.Min.X+1, popup.Min.Y+1+index)
}

// paneControlsCellText is the text drawn in width cells from (x, y).
func paneControlsCellText(lines []string, x, y, width int) string {
	runes := []rune(lines[y])
	start := min(x, len(runes))
	end := min(x+width, len(runes))
	return string(runes[start:end])
}

// paneControlsFocusDebug is Focus's derived Debug.
func paneControlsFocusDebug(focus Focus) string {
	switch focus {
	case FocusWorkspaces:
		return "Workspaces"
	case FocusSessions:
		return "Sessions"
	case FocusQuota:
		return "Quota"
	case FocusTargets:
		return "Targets"
	case FocusNotifications:
		return "Notifications"
	default:
		return "Prompt"
	}
}

// paneControlsSizeDebug is PaneSize's derived Debug.
func paneControlsSizeDebug(size core.PaneSize) string {
	switch size {
	case core.PaneSizeMinimized:
		return "Minimized"
	case core.PaneSizeMaximized:
		return "Maximized"
	default:
		return "Standard"
	}
}

// paneControlsSetupEntry is a Setup entry a title menu lists after Refresh:
// the label, the command it runs, and the Setup page (config section) that
// opens.
type paneControlsSetupEntry struct {
	label   string
	command CommandId
	page    string
}

// paneControlsTitleMenus are the two panes with a title menu: the pane, its
// index in paneAreas, the name on its title, the focus that owns it, and its
// entries after Refresh.
var paneControlsTitleMenus = []struct {
	pane     SupportPane
	index    int
	name     string
	focus    Focus
	settings []paneControlsSetupEntry
}{
	{
		SupportPaneTargets,
		1,
		"Targets",
		FocusTargets,
		[]paneControlsSetupEntry{
			{"Runtimes…", CommandIdManageTargets, "targets"},
			{"Machines…", CommandIdManageMachines, "machines"},
		},
	},
	{
		SupportPaneQuota,
		2,
		"Profiles",
		FocusQuota,
		[]paneControlsSetupEntry{{"Settings…", CommandIdManageProfiles, "profiles"}},
	},
}

// User request 2026-09-25: the Targets and Profiles titles are dropdowns.
// Clicking one opens a small menu hanging under that title, with Refresh and
// then Runtimes… and Machines… (Targets) or Settings… (Profiles), and leaves
// the keyboard where it was. Refresh runs the refresh `prefix+shift+r` runs.
// Each later entry opens Setup on the page its command opens: Runtimes… as
// "Manage runtimes" and Machines… as "Manage machines" (Targets), Settings…
// as "Manage agent profiles" (Profiles). The number keys pick a title-menu
// entry directly: 1 is Refresh, and 2 and 3 are Runtimes… and Machines… on
// Targets. The keyboard path: `.` on the focused pane opens the same menu,
// Enter runs the entry under the cursor, and Esc closes it without running
// anything. Minimized, Targets and Profiles are the last two rows above the
// footer, so neither has room for the menu under its title and the menu
// opens upward, still starting at the title's first column.
func TestAMinimizedPaneMenuOpensAboveItsRow(t *testing.T) {
	for _, menu := range paneControlsTitleMenus {
		d := dashboardWithSession(runningSession())
		for _, pane := range []SupportPane{SupportPaneTargets, SupportPaneQuota} {
			d.SetPaneSize(pane, core.PaneSizeMinimized)
		}
		drawn(d, 140, 40)
		d.focus = menu.focus
		d.HandleKey(key('.'))
		lines, popup := paneControlsDrawnMenu(t, d)
		pane := d.paneAreas[menu.index]
		if pane.Dy() != 1 {
			t.Fatalf("%s: pane height = %d, want 1", menu.name, pane.Dy())
		}
		if popup.Max.Y > pane.Min.Y {
			t.Fatalf("%s: the menu opens above its row:\n%s", menu.name, strings.Join(lines, "\n"))
		}
		if popup.Min.X != pane.Min.X+1 {
			t.Fatalf("%s: menu x = %d, want %d", menu.name, popup.Min.X, pane.Min.X+1)
		}
		entry := paneControlsEntry(popup, 0)
		if got := paneControlsCellText(lines, entry.X, entry.Y, 7); got != "Refresh" {
			t.Fatalf("%s: first entry = %q:\n%s", menu.name, got, strings.Join(lines, "\n"))
		}
	}
}

// The size chips share the title row with the dropdown and keep their own
// clicks: a chip resizes the pane and opens no menu.
func TestGoldenSupportPaneTitleMenu(t *testing.T) {
	var output strings.Builder

	for _, menu := range paneControlsTitleMenus {
		dashboard := dashboardWithSession(runningSession())
		dashboard.FocusPrompt()
		lines := drawn(dashboard, 140, 40)
		pane := dashboard.paneAreas[menu.index]
		title := point(lines, menu.name+" ▾")
		dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, title))
		action := dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, title))
		paneControlsAppendGoldenValue(&output, "title click action", surfaceControlsActionDebug(action))
		paneControlsAppendGoldenValue(&output, "focus after title click", paneControlsFocusDebug(dashboard.Focus()))
		lines, popup := paneControlsDrawnMenu(t, dashboard)
		paneControlsAppendGoldenValue(&output, "menu anchor",
			fmt.Sprintf("(%d, %d, %d, %d)", popup.Min.X, popup.Min.Y, pane.Min.X+1, pane.Min.Y+1))
		paneControlsAppendGoldenState(&output, menu.name+" title menu", 140, 40, lines)
	}

	for _, menu := range paneControlsTitleMenus {
		dashboard := dashboardWithSession(runningSession())
		lines := drawn(dashboard, 140, 40)
		title := point(lines, menu.name+" ▾")
		paneControlsClick(dashboard, title)
		_, popup := paneControlsDrawnMenu(t, dashboard)
		action := paneControlsClick(dashboard, paneControlsEntry(popup, 0))
		paneControlsAppendGoldenValue(&output, menu.name+" Refresh action", surfaceControlsActionDebug(action))
		lines = drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" after Refresh", 140, 40, lines)

		for row, setting := range menu.settings {
			dashboard := dashboardWithSession(runningSession())
			lines := drawn(dashboard, 140, 40)
			title := point(lines, menu.name+" ▾")
			paneControlsClick(dashboard, title)
			_, popup := paneControlsDrawnMenu(t, dashboard)
			action := paneControlsClick(dashboard, paneControlsEntry(popup, row+1))
			paneControlsAppendGoldenValue(&output, fmt.Sprintf("%s %s action", menu.name, setting.label), surfaceControlsActionDebug(action))
			lines = drawn(dashboard, 140, 40)
			paneControlsAppendGoldenState(&output, fmt.Sprintf("%s %s setup page", menu.name, setting.page), 140, 40, lines)
		}
	}

	for _, menu := range paneControlsTitleMenus {
		dashboard := dashboardWithSession(runningSession())
		drawn(dashboard, 140, 40)
		dashboard.focus = menu.focus
		action := dashboard.HandleKey(key('.'))
		paneControlsAppendGoldenValue(&output, menu.name+" dot action", surfaceControlsActionDebug(action))
		lines := drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" opened by dot", 140, 40, lines)
		for index, setting := range menu.settings {
			digit := rune('2' + index)
			dashboard := dashboardWithSession(runningSession())
			drawn(dashboard, 140, 40)
			dashboard.focus = menu.focus
			dashboard.HandleKey(key('.'))
			action := dashboard.HandleKey(key(digit))
			paneControlsAppendGoldenValue(&output, fmt.Sprintf("%s number %c action", menu.name, digit), surfaceControlsActionDebug(action))
			lines := drawn(dashboard, 140, 40)
			paneControlsAppendGoldenState(&output, fmt.Sprintf("%s number %c: %s, %s", menu.name, digit, setting.label, setting.page), 140, 40, lines)
		}

		dashboard = dashboardWithSession(runningSession())
		drawn(dashboard, 140, 40)
		dashboard.focus = menu.focus
		dashboard.HandleKey(key('.'))
		openLines := drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" keyboard menu", 140, 40, openLines)
		action = dashboard.HandleKey(key(tea.KeyEscape))
		paneControlsAppendGoldenValue(&output, menu.name+" Escape action", surfaceControlsActionDebug(action))
		paneControlsAppendGoldenValue(&output, menu.name+" focus after Escape", paneControlsFocusDebug(dashboard.Focus()))
		paneControlsAppendGoldenValue(&output, menu.name+" menu after Escape", fmt.Sprint(dashboard.paneMenu != nil))
		lines = drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" after Escape", 140, 40, lines)

		dashboard.HandleKey(key('.'))
		refresh := dashboard.HandleKey(key(tea.KeyEnter))
		paneControlsAppendGoldenValue(&output, menu.name+" Enter Refresh action", surfaceControlsActionDebug(refresh))
		lines = drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" after Enter Refresh", 140, 40, lines)

		dashboard.HandleKey(key('.'))
		dashboard.HandleKey(key(tea.KeyDown))
		openSetup := dashboard.HandleKey(key(tea.KeyEnter))
		paneControlsAppendGoldenValue(&output, menu.name+" Enter setup action", surfaceControlsActionDebug(openSetup))
		lines = drawn(dashboard, 140, 40)
		paneControlsAppendGoldenState(&output, menu.name+" setup from keyboard", 140, 40, lines)
	}

	for _, menu := range paneControlsTitleMenus {
		dashboard := dashboardWithSession(runningSession())
		drawn(dashboard, 140, 40)
		for _, size := range []core.PaneSize{core.PaneSizeMinimized, core.PaneSizeStandard} {
			index := slices.IndexFunc(dashboard.paneSizeControlAreas, func(area paneSizeControlArea) bool {
				return area.pane == menu.pane && area.size == size
			})
			if index < 0 {
				t.Fatal("the size chip is drawn")
			}
			chip := dashboard.paneSizeControlAreas[index].area
			action := paneControlsClick(dashboard, uv.Pos(chip.Min.X+1, chip.Min.Y))
			paneControlsAppendGoldenValue(&output, fmt.Sprintf("%s %s size action", menu.name, paneControlsSizeDebug(size)), surfaceControlsActionDebug(action))
			paneControlsAppendGoldenValue(&output, menu.name+" menu after size click", fmt.Sprint(dashboard.paneMenu != nil))
			lines := drawn(dashboard, 140, 40)
			paneControlsAppendGoldenState(&output, fmt.Sprintf("%s %s pane", menu.name, paneControlsSizeDebug(size)), 140, 40, lines)
		}
	}

	core.AssertGolden(t, "testdata", "support-pane-title-menu", output.String())
}
