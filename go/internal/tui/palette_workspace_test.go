// Port of mj-tui/src/palette/workspace.rs tests.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// paletteDashboardWithWorkspaces is a dashboard with one running session in
// "Default" and two other workspaces, focused on its row.
func paletteDashboardWithWorkspaces() *DashboardState {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetWorkspaceNames(map[string]string{
		"default": "Default",
		"other":   "Other",
		"third":   "Third",
	})
	publishFixtureVerdicts(dashboard)
	dashboard.FocusSessions()
	return dashboard
}

// paletteWorkspaceRow is the row of the first drawn line containing needle.
func paletteWorkspaceRow(t *testing.T, lines []string, needle string) int {
	t.Helper()
	row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, needle) })
	if row < 0 {
		t.Fatalf("%q is drawn in %#v", needle, lines)
	}
	return row
}

// paletteMenuOnChangeWorkspace opens the session menu and moves to its Change
// Workspace item.
func paletteMenuOnChangeWorkspace(dashboard *DashboardState, width, height int) {
	dashboard.beginSessionPalette()
	drawn(dashboard, width, height)
	for {
		if id, ok := paletteSelectedCommand(dashboard); ok && id == CommandIdChangeWorkspace {
			return
		}
		dashboard.HandleKey(key(tea.KeyDown))
		drawn(dashboard, width, height)
	}
}

// paletteDashboardWithHandoffTitle is a session titled by the prompt it started
// with: a long handoff, over many lines.
func paletteDashboardWithHandoffTitle(t *testing.T) *DashboardState {
	session := runningSession()
	session.SessionTitleOverride = nil
	session.ACPSessionTitle = nil
	title := []rune(strings.Repeat("Continue the handoff below and finish the migration.\n\n", 2000/50+1))
	session.Title = string(title[:2000])
	if count := len([]rune(session.Title)); count != 2000 {
		t.Fatalf("title has %d characters", count)
	}
	dashboard := dashboardWithSession(session)
	dashboard.SetWorkspaceNames(map[string]string{
		"default": "Default",
		"other":   "Other",
	})
	dashboard.FocusSessions()
	return dashboard
}

func TestOtherConfirmationsTruncateALongTitleTheSameWay(t *testing.T) {
	dashboard := paletteDashboardWithHandoffTitle(t)
	dashboard.DispatchCommand(CommandIdDestroySession)
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	lines := drawn(dashboard, 80, 40)
	at := paletteWorkspaceRow(t, lines, "Session: Continue the handoff")
	if !strings.Contains(lines[at], chat.ThemeGlyphs().Ellipsis) {
		t.Fatal(lines[at])
	}
	if strings.Contains(lines[at+1], "Continue") {
		t.Fatalf("%#v", lines)
	}
}

func TestGoldenChangeWorkspaceCombobox(t *testing.T) {
	var output strings.Builder

	dashboard := paletteDashboardWithWorkspaces()
	dashboard.beginSessionPalette()
	paletteGoldenState(&output, "session menu", 120, 40, drawn(dashboard, 120, 40))

	for _, width := range []int{140, 80} {
		dashboard := paletteDashboardWithWorkspaces()
		paletteMenuOnChangeWorkspace(dashboard, width, 40)
		drawn(dashboard, width, 40)
		action := dashboard.HandleKey(key(tea.KeyEnter))
		paletteGoldenValue(&output, "menu popup action", combinedActionDebug(action))
		paletteGoldenState(&output, "session menu workspace list", width, 40, drawn(dashboard, width, 40))
	}

	dashboard = paletteDashboardWithWorkspaces()
	openPalette(dashboard)
	drawn(dashboard, 100, 40)
	for _, character := range "change workspace" {
		dashboard.HandleKey(key(character))
	}
	drawn(dashboard, 100, 40)
	paletteGoldenValue(&output, "selected palette command", paletteSelectedCommandDebug(dashboard))
	action := dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "palette popup action", combinedActionDebug(action))
	paletteGoldenState(&output, "palette workspace list", 100, 40, drawn(dashboard, 100, 40))

	dashboard = paletteDashboardWithWorkspaces()
	dashboard.DispatchCommand(CommandIdChangeWorkspace)
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key(tea.KeyDown))
	drawn(dashboard, 120, 40)
	action = dashboard.HandleKey(key(tea.KeyEnter))
	paletteGoldenValue(&output, "select Other action", combinedActionDebug(action))
	paletteGoldenState(&output, "dashboard after selecting Other", 120, 40, drawn(dashboard, 120, 40))

	// More workspaces than the list's six rows: it scrolls rather than
	// covering more of the palette.
	dashboard = dashboardWithSession(runningSession())
	names := map[string]string{"default": "Workspace default"}
	for index := 2; index <= 9; index++ {
		id := fmt.Sprintf("w%d", index)
		names[id] = "Workspace " + id
	}
	dashboard.SetWorkspaceNames(names)
	publishFixtureVerdicts(dashboard)
	dashboard.FocusSessions()
	dashboard.DispatchCommand(CommandIdChangeWorkspace)
	paletteGoldenState(&output, "nine workspaces in a six-row list", 120, 40, drawn(dashboard, 120, 40))

	dashboard = paletteDashboardWithWorkspaces()
	dashboard.DispatchCommand(CommandIdChangeWorkspace)
	drawn(dashboard, 120, 40)
	action = dashboard.HandleKey(key(tea.KeyEscape))
	paletteGoldenValue(&output, "first Escape action", combinedActionDebug(action))
	paletteGoldenState(&output, "menu after closing workspace list", 120, 40, drawn(dashboard, 120, 40))
	action = dashboard.HandleKey(key(tea.KeyEscape))
	paletteGoldenValue(&output, "second Escape action", combinedActionDebug(action))
	paletteGoldenState(&output, "dashboard after closing menu", 120, 40, drawn(dashboard, 120, 40))

	core.AssertGolden(t, "testdata", "change-workspace-combobox", output.String())
}
