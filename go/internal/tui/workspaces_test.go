// Port of mj-tui/src/workspaces/tests.rs.

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

func workspacesEntry(id, name string) WorkspaceManagementEntry {
	return WorkspaceManagementEntry{
		Workspace: core.WorkspaceRecord{
			ID:           id,
			Name:         name,
			CreatedAt:    "",
			LastOpenedAt: "",
			SessionCount: 0,
		},
		Drafts: nil,
	}
}

// workspacesDrawManager is the open workspace manager drawn alone into a
// 100x28 terminal.
func workspacesDrawManager(dashboard *DashboardState) []string {
	surfaces := chat.NewFrameSurfaces()
	buffer, _ := combinedDraw(100, 28, func(frame *chat.Frame) {
		if manager := workspaceManagerRef(dashboard.mode); manager != nil {
			renderWorkspaceManager(frame, frame.Area(), manager, &surfaces)
		}
	})
	return bufferLines(buffer)
}

func workspacesAppendState(output *strings.Builder, label string, lines []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (110x32) ===\n", label)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
}

func workspacesClickLabel(dashboard *DashboardState, lines []string, label string) DashboardAction {
	position := point(lines, label)
	var action DashboardAction = DashboardActionNone{}
	for _, kind := range []tea.MouseMsg{
		tea.MouseClickMsg{Button: tea.MouseLeft},
		tea.MouseReleaseMsg{Button: tea.MouseLeft},
	} {
		action = dashboard.HandleMouse(mouseAt(kind, position))
	}
	return action
}

// workspacesManagerOf is the open workspace manager; the test fails when none is.
func workspacesManagerOf(t *testing.T, dashboard *DashboardState) *WorkspaceManager {
	t.Helper()
	manager := workspaceManagerRef(dashboard.mode)
	if manager == nil {
		t.Fatalf("manager open: mode %T", dashboard.mode)
	}
	return manager
}

// workspacesLoadGeneration is the generation a LoadWorkspaceManagement action asks
// for; the test fails on any other action.
func workspacesLoadGeneration(t *testing.T, action DashboardAction) uint64 {
	t.Helper()
	load, ok := action.(DashboardActionLoadWorkspaceManagement)
	if !ok {
		t.Fatalf("manager did not request a load: %s", combinedActionDebug(action))
	}
	return load.Generation
}

func workspacesEnter() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: tea.KeyEnter}
}

func workspacesStringPtr(id string) *string {
	return &id
}

func TestGoldenWorkspaceManagerDrafts(t *testing.T) {
	var output strings.Builder
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetWorkspaceNames(map[string]string{
		"default": "Default",
		"alpha":   "Echo",
		"beta":    "Foxtrot",
	})
	dashboard.SetActiveWorkspace(workspacesStringPtr("default"))

	action := chord(dashboard, CommandIdFocusWorkspaces)
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	action = dashboard.HandleKey(key(tea.KeyRight))
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	dashboard.SetActiveWorkspace(workspacesStringPtr("alpha"))
	workspacesAppendState(&output, "dashboard after selecting the next workspace", drawn(dashboard, 110, 32))
	action = route(dashboard, []tea.KeyMsg{prefixKey(), tea.KeyPressMsg{Code: '3', Text: "3"}})
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	dashboard.SetActiveWorkspace(workspacesStringPtr("beta"))
	workspacesAppendState(&output, "dashboard after selecting a numbered workspace", drawn(dashboard, 110, 32))

	action = chord(dashboard, CommandIdWorkspaces)
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	generation := workspacesLoadGeneration(t, action)
	beta := workspacesEntry("beta", "Beta")
	beta.Workspace.SessionCount = 2
	ownerPID := uint32(4100)
	beta.Drafts = []WorkspaceDraftEntry{
		{
			ID:        "draft-terminal",
			SessionID: workspacesStringPtr("session-1"),
			Source:    "terminal composer",
			SavedAt:   "2026-10-01 12:00",
			OwnerPID:  &ownerPID,
		},
		{
			ID:        "draft-web",
			SessionID: nil,
			Source:    "web viewer",
			SavedAt:   "2026-10-02 09:30",
			OwnerPID:  nil,
		},
	}
	alpha := workspacesEntry("alpha", "Alpha")
	alpha.Workspace.SessionCount = 1
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("default", "Default"),
		alpha.Clone(),
		beta.Clone(),
	}, nil, false)
	lines := drawn(dashboard, 110, 32)
	workspacesAppendState(&output, "workspace manager list", lines)
	action = workspacesClickLabel(dashboard, lines, "Drafts")
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	workspacesAppendState(&output, "drafts for the selected workspace", drawn(dashboard, 110, 32))
	action = dashboard.HandleKey(key(tea.KeyDown))
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))
	workspacesAppendState(&output, "second draft selected", drawn(dashboard, 110, 32))
	action = dashboard.HandleKey(key(tea.KeyEnter))
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))

	beta.Drafts = nil
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("default", "Default"),
		alpha,
		beta,
	}, nil, false)
	workspacesAppendState(&output, "draft recovery completed", drawn(dashboard, 110, 32))

	dashboard.mode = ModeDashboard{}
	names := map[string]string{}
	for index := range 24 {
		names[fmt.Sprintf("workspace-%d", index)] = fmt.Sprintf("Project %02d 界", index)
	}
	dashboard.SetWorkspaceNames(names)
	dashboard.SetActiveWorkspace(workspacesStringPtr("workspace-0"))
	for _, size := range []core.PaneSize{core.PaneSizeStandard, core.PaneSizeMaximized} {
		dashboard.SetPaneSize(SupportPaneSessions, size)
		lines := drawn(dashboard, 110, 32)
		if dashboard.workspacePaneArea.Dy() != 9 {
			t.Fatalf("%v: workspace pane height %d", size, dashboard.workspacePaneArea.Dy())
		}
		if _, ok := dashboard.workspaceScrollbar.Geometry(); !ok {
			t.Fatalf("%v: no scrollbar", size)
		}
		workspacesAppendState(&output, fmt.Sprintf("seven workspace rows in %s sidebar", combinedPaneSizeDebug(size)), lines)
	}
	area := *dashboard.workspacePaneArea
	if action := dashboard.HandleMouse(mouseAt(tea.MouseWheelMsg{Button: tea.MouseWheelDown}, uv.Pos(area.Min.X+1, area.Min.Y+1))); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("wheel = %s", combinedActionDebug(action))
	}
	lines = drawn(dashboard, 110, 32)
	if active, _ := dashboard.ActiveWorkspaceID(); active != "workspace-0" || dashboard.workspaceScroll != 1 {
		t.Fatalf("active %q, scroll %d", active, dashboard.workspaceScroll)
	}
	workspacesAppendState(&output, "wheel scrolls without switching workspaces", lines)
	scrollbar, _ := dashboard.workspaceScrollbar.Geometry()
	bottom := uv.Pos(scrollbar.Track.Min.X, scrollbar.Track.Max.Y-1)
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, bottom))
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, bottom))
	lines = drawn(dashboard, 110, 32)
	if dashboard.workspaceScroll != scrollbar.MaxScroll {
		t.Fatalf("scroll %d, max %d", dashboard.workspaceScroll, scrollbar.MaxScroll)
	}
	workspacesAppendState(&output, "scrollbar seeks to the final workspace rows", lines)
	scrollbar, _ = dashboard.workspaceScrollbar.Geometry()
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, uv.Pos(scrollbar.Thumb.Min.X, scrollbar.Thumb.Min.Y)))
	outside := uv.Pos(109, scrollbar.Track.Min.Y)
	drag := mouseAt(tea.MouseMotionMsg{Button: tea.MouseLeft}, outside)
	if !dashboard.ComponentHandlesMouse(drag) {
		t.Fatal("the scrollbar keeps the drag outside its track")
	}
	dashboard.HandleMouse(drag)
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, outside))
	if dashboard.workspaceScroll != 0 {
		t.Fatalf("scroll %d", dashboard.workspaceScroll)
	}
	dashboard.SetActiveWorkspace(workspacesStringPtr("workspace-23"))
	lines = drawn(dashboard, 110, 32)
	if !slices.ContainsFunc(dashboard.workspaceTabAreas, func(tab client.DaemonPair[string, uv.Rectangle]) bool { return tab.First == "workspace-23" }) {
		t.Fatal("the final workspace is revealed")
	}
	workspacesAppendState(&output, "selecting the final workspace reveals it", lines)

	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	lines = drawn(dashboard, 110, 32)
	if dashboard.workspacePaneArea.Dy() != 3 || len(dashboard.workspaceTabAreas) != 0 {
		t.Fatalf("tiny selector: height %d, %d tabs", dashboard.workspacePaneArea.Dy(), len(dashboard.workspaceTabAreas))
	}
	workspacesAppendState(&output, "tiny workspace selector", lines)
	dropdown := *dashboard.workspaceDropdownArea
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, uv.Pos(dropdown.Min.X+1, dropdown.Min.Y)))
	lines = drawn(dashboard, 110, 32)
	if dashboard.paneMenu == nil {
		t.Fatal("the tiny selector opens the workspace dropdown")
	}
	workspacesAppendState(&output, "tiny selector opens the workspace dropdown", lines)
	dashboard.HandleKey(key(tea.KeyHome))
	action = dashboard.HandleKey(key(tea.KeyEnter))
	if action != DashboardAction(DashboardActionSelectWorkspace{WorkspaceID: "workspace-0"}) {
		t.Fatalf("dropdown action %s", combinedActionDebug(action))
	}
	fmt.Fprintf(&output, "dropdown action: %s\n", combinedActionDebug(action))
	dashboard.SetActiveWorkspace(workspacesStringPtr("workspace-0"))
	workspacesAppendState(&output, "tiny selector after choosing a workspace", drawn(dashboard, 110, 32))
	dashboard.HandleKey(key(tea.KeyEnter))
	drawn(dashboard, 110, 32)
	if dashboard.paneMenu == nil {
		t.Fatal("Enter reopens the dropdown")
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	if dashboard.paneMenu != nil {
		t.Fatal("Escape dismisses the dropdown")
	}
	output.WriteString("Escape dismisses the dropdown without switching workspace\n")

	core.AssertGolden(t, "testdata", "workspace-manager-drafts", strings.TrimRight(output.String(), "\n"))
}

func TestManagerIgnoresStaleResultsAndSelectsWorkspaceOnEnter(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	generation := workspacesLoadGeneration(t, dashboard.BeginWorkspaceManager())
	dashboard.FinishWorkspaceManagement(generation-1, []WorkspaceManagementEntry{workspacesEntry("stale", "Stale")}, nil, false)
	if !workspacesManagerOf(t, dashboard).loading {
		t.Fatal("a stale reply is ignored")
	}
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("other", "Other")}, nil, false)
	if workspacesManagerOf(t, dashboard).loading {
		t.Fatal("the current reply loads")
	}
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionSelectWorkspace{WorkspaceID: "other"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
}

func TestManagerShowsTheSortedListWhileDetailsLoadAndMutationsWait(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("b"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdRenameWorkspace))
	// The names answer arrives first: the list shows, sorted by name, still
	// loading. The shortcut's pending rename waits for the details answer —
	// acting now would run on drafts it has not seen.
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("b", "Beta"),
		workspacesEntry("a", "Alpha"),
	}, nil, true)
	manager := workspacesManagerOf(t, dashboard)
	if !manager.loading {
		t.Fatal("still loading")
	}
	var ids []string
	for _, entry := range manager.entries {
		ids = append(ids, entry.Workspace.ID)
	}
	if !slices.Equal(ids, []string{"a", "b"}) {
		t.Fatalf("%v", ids)
	}
	// The names answer offers the active workspace, as the complete one
	// does.
	if manager.selected != 1 {
		t.Fatalf("selected %d", manager.selected)
	}
	if _, list := manager.view.(WorkspaceManagerViewList); !list {
		t.Fatalf("view %#v", manager.view)
	}
	// Opening the selected workspace needs no details: it works while the
	// details answer is still out.
	if !slices.ContainsFunc(manager.actions(), func(action chat.ButtonSpec[WorkspaceControl]) bool {
		return action.ID == WorkspaceControlOpen && action.Enabled
	}) {
		t.Fatal("Open is enabled")
	}
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionSelectWorkspace{WorkspaceID: "b"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	// Reopen for the rename: the pending command fires on the complete
	// answer.
	generation = workspacesLoadGeneration(t, chord(dashboard, CommandIdRenameWorkspace))
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("b", "Beta"),
		workspacesEntry("a", "Alpha"),
	}, nil, true)
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("b", "Beta"),
		workspacesEntry("a", "Alpha"),
	}, nil, false)
	manager = workspacesManagerOf(t, dashboard)
	if manager.loading {
		t.Fatal("loaded")
	}
	if view, ok := manager.view.(WorkspaceManagerViewRename); !ok || view.WorkspaceID != "b" {
		t.Fatalf("view %#v", manager.view)
	}
}

func TestAFailedDetailsAnswerKeepsTheListOpenableAndItsErrorVisible(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	generation := workspacesLoadGeneration(t, dashboard.BeginWorkspaceManager())
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("a", "Alpha"),
		workspacesEntry("b", "Beta"),
	}, nil, true)
	dashboard.FinishWorkspaceManagement(generation, nil, fmt.Errorf("drafts query failed"), false)
	manager := workspacesManagerOf(t, dashboard)
	if manager.error == nil || *manager.error != "drafts query failed" {
		t.Fatalf("error %v", manager.error)
	}
	if !slices.ContainsFunc(manager.actions(), func(action chat.ButtonSpec[WorkspaceControl]) bool {
		return action.ID == WorkspaceControlOpen && action.Enabled
	}) {
		t.Fatal("Open is enabled")
	}
	if slices.ContainsFunc(manager.actions(), func(action chat.ButtonSpec[WorkspaceControl]) bool {
		return action.ID == WorkspaceControlDelete && action.Enabled
	}) {
		t.Fatal("Delete waits for the details")
	}
	// Moving the selection keeps the failure visible, and Enter still opens.
	dashboard.HandleKey(tea.KeyPressMsg{Code: tea.KeyDown})
	manager = workspacesManagerOf(t, dashboard)
	if manager.error == nil || *manager.error != "drafts query failed" {
		t.Fatalf("error %v", manager.error)
	}
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionSelectWorkspace{WorkspaceID: "b"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
}

func TestTabsScrollToTheSelectedWorkspaceAndMouseHitsUnicodeLabels(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetWorkspaceNames(map[string]string{
		"a": "First workspace",
		"b": "界界",
		// Tabs sort by name, so this one must sort after "界界".
		"c": "界界 last workspace",
	})
	dashboard.SetActiveWorkspace(workspacesStringPtr("b"))
	buffer, _ := combinedDraw(24, 5, func(frame *chat.Frame) {
		renderWorkspaceTabs(frame, uv.Rect(0, 0, 24, 5), dashboard)
	})
	if tab := dashboard.workspaceTabAreas[1]; tab.First != "b" || tab.Second != uv.Rect(1, 2, 6, 1) {
		t.Fatalf("tab %v", tab)
	}
	symbol := func(x, y int) string { return buffer.CellAt(x, y).Content }
	if symbol(1, 2) != " " || symbol(2, 2) != "界" || symbol(4, 2) != "界" || symbol(6, 2) != " " {
		t.Fatalf("row 2: %q", chat.BufferLines(buffer)[2])
	}
	if chat.CellStyle(buffer.CellAt(1, 2)).Bg != chat.CellStyle(buffer.CellAt(6, 2)).Bg {
		t.Fatal("the tab's padding shares its background")
	}
	if action := workspaceTabClick(dashboard, 2, 3); action != DashboardAction(DashboardActionSelectWorkspace{WorkspaceID: "c"}) {
		t.Fatalf("click = %s", combinedActionDebug(action))
	}
	if dashboard.Focus() != FocusWorkspaces {
		t.Fatalf("focus %v", dashboard.Focus())
	}
	dashboard.SetActiveWorkspace(workspacesStringPtr("c"))
	buffer, _ = combinedDraw(24, 5, func(frame *chat.Frame) {
		renderWorkspaceTabs(frame, uv.Rect(0, 0, 24, 5), dashboard)
	})
	if !slices.ContainsFunc(dashboard.workspaceTabAreas, func(tab client.DaemonPair[string, uv.Rectangle]) bool { return tab.First == "c" }) {
		t.Fatal("the selected workspace is revealed")
	}
	var text strings.Builder
	for x := range 24 {
		text.WriteString(buffer.CellAt(x, 3).Content)
	}
	if !strings.Contains(text.String(), "last worksp") {
		t.Fatal(text.String())
	}
}

func TestManagerViewsKeepRenameIdentityAcrossReorderedRefresh(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	generation := workspacesLoadGeneration(t, dashboard.BeginWorkspaceManager())
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("a", "A"),
		workspacesEntry("b", "B"),
	}, nil, false)
	workspacesManagerOf(t, dashboard).form.Focus(WorkspaceControlRename)
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	if view, ok := workspacesManagerOf(t, dashboard).view.(WorkspaceManagerViewRename); !ok || view.WorkspaceID != "a" {
		t.Fatalf("view %#v", workspacesManagerOf(t, dashboard).view)
	}
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("b", "B"),
		workspacesEntry("a", "A renamed"),
	}, nil, false)
	if view, ok := workspacesManagerOf(t, dashboard).view.(WorkspaceManagerViewRename); !ok || view.WorkspaceID != "a" {
		t.Fatalf("view %#v", workspacesManagerOf(t, dashboard).view)
	}
}

func TestDeleteWithActiveSessionsUsesTheCancellableSuspensionWorkflow(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	generation := workspacesLoadGeneration(t, dashboard.BeginWorkspaceManager())
	busyEntry := workspacesEntry("a", "Project alpha")
	busyEntry.Workspace.SessionCount = 2
	busyEntry.Drafts = append(busyEntry.Drafts, WorkspaceDraftEntry{
		ID:        "draft-a",
		SessionID: workspacesStringPtr("session-a"),
		Source:    "composer",
		SavedAt:   "now",
		OwnerPID:  nil,
	})
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{busyEntry}, nil, false)
	workspacesManagerOf(t, dashboard).form.Focus(WorkspaceControlDelete)
	dashboard.HandleKey(workspacesEnter())
	if _, ok := workspacesManagerOf(t, dashboard).view.(WorkspaceManagerViewClose); !ok {
		t.Fatalf("view %#v", workspacesManagerOf(t, dashboard).view)
	}
	rendered := strings.Join(workspacesDrawManager(dashboard), "\n")
	if !strings.Contains(rendered, "Suspend 2 sessions") ||
		!strings.Contains(rendered, "Discard 1 saved draft and") ||
		strings.Contains(rendered, "Force delete") {
		t.Fatal(rendered)
	}
	workspacesManagerOf(t, dashboard).form.Focus(WorkspaceControlConfirmClose)
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionCloseWorkspace{Generation: generation, WorkspaceID: "a"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
}

func TestWorkspaceShortcutsLoadTheActiveWorkspaceAndIgnoreStaleReplies(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("b"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdRenameWorkspace))
	dashboard.FinishWorkspaceManagement(generation-1, []WorkspaceManagementEntry{workspacesEntry("b", "Wrong")}, nil, false)
	if !workspacesManagerOf(t, dashboard).loading {
		t.Fatal("a stale reply is ignored")
	}
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{
		workspacesEntry("a", "Other"),
		workspacesEntry("b", "Rename me"),
	}, nil, false)
	manager := workspacesManagerOf(t, dashboard)
	if view, ok := manager.view.(WorkspaceManagerViewRename); !ok || view.WorkspaceID != "b" || manager.name.Value() != "Rename me" {
		t.Fatalf("view %#v, name %q", manager.view, manager.name.Value())
	}
	manager.name = chat.TextInputFromValue("New name")
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionRenameWorkspace{
		Generation:  generation,
		WorkspaceID: "b",
		Name:        "New name",
	}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
}

// Hard-won: 25c6af83: Close used placeholder (s) pluralization
func TestClosingWorkspaceConfirmsCountsAndSupportsCancellationWhileBusy(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	workspace := workspacesEntry("a", "Example")
	workspace.Workspace.SessionCount = 2
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspace}, nil, false)
	rendered := strings.Join(workspacesDrawManager(dashboard), "\n")
	// Launch finding R5-10: the counts agree with their nouns, not "(s)".
	if !strings.Contains(rendered, "Suspend 2 sessions and") ||
		!strings.Contains(rendered, "Resumable histories") ||
		!strings.Contains(rendered, "Discard 0 saved drafts") ||
		strings.Contains(rendered, "(s)") {
		t.Fatal(rendered)
	}
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	// Cancel leaves the dialog for the dashboard the shortcut came from.
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	generation = workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	workspace = workspacesEntry("a", "Example")
	workspace.Workspace.SessionCount = 2
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspace}, nil, false)
	manager := workspacesManagerOf(t, dashboard)
	manager.syncForm()
	manager.form.Focus(WorkspaceControlConfirmClose)
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionCloseWorkspace{Generation: generation, WorkspaceID: "a"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	manager = workspacesManagerOf(t, dashboard)
	if manager.busy == nil || *manager.busy != WorkspaceMutationClose {
		t.Fatal("the delete is in flight")
	}
	manager.syncForm()
	manager.form.Focus(WorkspaceControlCancelClose)
	if action := dashboard.HandleKey(workspacesEnter()); action != DashboardAction(DashboardActionCancelWorkspaceClose{WorkspaceID: "a"}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
	if action := chord(dashboard, CommandIdQuitDetach); action != DashboardAction(DashboardActionQuitDetach{}) {
		t.Fatalf("quit = %s", combinedActionDebug(action))
	}
	dashboard.FinishWorkspaceManagement(generation, nil, fmt.Errorf("stop failed; retry"), false)
	manager = workspacesManagerOf(t, dashboard)
	if manager.busy != nil || manager.error == nil || *manager.error != "stop failed; retry" {
		t.Fatalf("busy %v, error %v", manager.busy, manager.error)
	}
}

func TestAWorkspaceCloseCanRunInBackgroundAndReopenForCancellation(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	workspace := workspacesEntry("a", "Example")
	workspace.Workspace.SessionCount = 1
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspace}, nil, false)
	if _, ok := dashboard.workspaceManagerMutation(WorkspaceMutationClose).(DashboardActionCloseWorkspace); !ok {
		t.Fatal("the delete starts")
	}
	dashboard.HandleKey(tea.KeyPressMsg{Code: tea.KeyEscape})
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatal("navigation is available while stopping")
	}
	reopened := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	if reopened == generation {
		t.Fatal("the reopened dialog has its own generation")
	}
	dashboard.FinishWorkspaceManagement(reopened, []WorkspaceManagementEntry{workspacesEntry("a", "Example")}, nil, false)
	if manager := workspacesManagerOf(t, dashboard); manager.busy == nil || *manager.busy != WorkspaceMutationClose {
		t.Fatal("the reopened dialog shows the delete in flight")
	}
	lines := strings.Join(workspacesDrawManager(dashboard), "\n")
	if !strings.Contains(lines, "Cancel deletion") || !strings.Contains(lines, "Continue working") {
		t.Fatal(lines)
	}
	if owner, ok := dashboard.WorkspaceCloseFinished("a"); !ok || owner != reopened {
		t.Fatalf("owner %d, %v", owner, ok)
	}
	dashboard.FinishWorkspaceManagement(reopened, nil, fmt.Errorf("stop failed"), false)
	if manager := workspacesManagerOf(t, dashboard); manager.busy != nil || manager.error == nil || *manager.error != "stop failed" {
		t.Fatalf("busy %v, error %v", manager.busy, manager.error)
	}
	if _, ok := dashboard.workspaceManagerMutation(WorkspaceMutationClose).(DashboardActionCloseWorkspace); !ok {
		t.Fatal("failure remains retryable")
	}
	if owner, ok := dashboard.WorkspaceCloseFinished("a"); !ok || owner != reopened {
		t.Fatalf("owner %d, %v", owner, ok)
	}
	dashboard.FinishWorkspaceManagement(reopened, []WorkspaceManagementEntry{}, nil, false)
	// The dialog came from the Close shortcut, so a finished close returns
	// to the dashboard rather than to the manager's list (B-11).
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// Launch finding A-5: the Rename and Close shortcuts open their dialog
// straight from the dashboard, so Esc there goes back to the dashboard
// rather than to a Workspaces manager the user never opened.
//
// Hard-won: 3f535f25: shortcut dialogs returned Esc to a manager the user never opened
func TestEscInAWorkspaceDialogOpenedByShortcutReturnsToTheDashboard(t *testing.T) {
	for _, command := range []CommandId{CommandIdRenameWorkspace, CommandIdCloseWorkspace} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
		generation := workspacesLoadGeneration(t, chord(dashboard, command))
		dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("a", "Example")}, nil, false)
		if _, list := workspacesManagerOf(t, dashboard).view.(WorkspaceManagerViewList); list {
			t.Fatalf("%v: the shortcut opens its own view", command)
		}

		dashboard.HandleKey(tea.KeyPressMsg{Code: tea.KeyEscape})

		if _, ok := dashboard.mode.(ModeDashboard); !ok {
			t.Fatalf("%v: mode %T", command, dashboard.mode)
		}
	}
}

// Launch campaign finding D-13: after a restart the tabs read `second
// mjolnir` although they read `mjolnir  second` before, because a fresh
// dashboard ordered tabs by random workspace id. Tabs are ordered by name,
// so neither a restart nor a later runtime update changes the order.
//
// Hard-won: e923d3bf: tab order changed because workspace ids were random
func TestWorkspaceTabsKeepNameOrderAcrossUpdates(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	names := map[string]string{
		"f069": "mjolnir",
		"2901": "second",
	}
	dashboard.SetWorkspaceNames(names)
	if ids := dashboard.workspaceIDs(); !slices.Equal(ids, []string{"f069", "2901"}) {
		t.Fatalf("%v", ids)
	}
	dashboard.SetWorkspaceNames(names)
	if ids := dashboard.workspaceIDs(); !slices.Equal(ids, []string{"f069", "2901"}) {
		t.Fatalf("%v", ids)
	}
}

// Launch finding B-11, Enter half: Enter in the Rename dialog opened by its
// shortcut renamed the workspace but left the Workspaces manager open, a
// screen the user never opened, and the dialog said "Esc closes manager".
// Once the rename lands the dialog returns to the dashboard, as Esc does
// since A-5.
//
// Hard-won: 11f841e4: shortcut Rename completion returned to an unopened manager
func TestARenameOpenedByShortcutReturnsToTheDashboardWhenItLands(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdRenameWorkspace))
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("a", "Example")}, nil, false)
	rendered := strings.Join(workspacesDrawManager(dashboard), "\n")
	if strings.Contains(rendered, "Esc closes manager") || !strings.Contains(rendered, "Enter saves the new name · Esc cancels") {
		t.Fatal(rendered)
	}
	workspacesManagerOf(t, dashboard).name = chat.TextInputFromValue("Renamed")
	if _, ok := dashboard.HandleKey(workspacesEnter()).(DashboardActionRenameWorkspace); !ok {
		t.Fatal("Enter renames")
	}

	if !dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("a", "Renamed")}, nil, false).Foreground {
		t.Fatal("the result belongs to the visible manager")
	}

	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// The Close shortcut likewise returns to the dashboard once the close
// finishes, rather than to the manager's list.
//
// Hard-won: 11f841e4: shortcut Close completion returned to an unopened manager
func TestACloseOpenedByShortcutReturnsToTheDashboardWhenItFinishes(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	workspace := workspacesEntry("a", "Example")
	workspace.Workspace.SessionCount = 1
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspace, workspacesEntry("b", "Other")}, nil, false)
	manager := workspacesManagerOf(t, dashboard)
	manager.syncForm()
	manager.form.Focus(WorkspaceControlConfirmClose)
	if _, ok := dashboard.HandleKey(workspacesEnter()).(DashboardActionCloseWorkspace); !ok {
		t.Fatal("Enter deletes")
	}

	if owner, ok := dashboard.WorkspaceCloseFinished("a"); ok {
		generation = owner
	}
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("b", "Other")}, nil, false)

	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

func workspacesDraft() WorkspaceDraftEntry {
	return WorkspaceDraftEntry{
		ID:        "draft-a",
		SessionID: nil,
		Source:    "composer",
		SavedAt:   "now",
		OwnerPID:  nil,
	}
}

func TestDeletingAnEmptyWorkspaceFromItsShortcutActsWithoutConfirmation(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
	generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
	outcome := dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspacesEntry("a", "Example")}, nil, false)
	if outcome.Action != DashboardAction(DashboardActionCloseWorkspace{Generation: generation, WorkspaceID: "a"}) {
		t.Fatalf("action %s", combinedActionDebug(outcome.Action))
	}
	if manager := workspacesManagerOf(t, dashboard); manager.busy == nil || *manager.busy != WorkspaceMutationClose {
		t.Fatal("the delete is in flight at once")
	}
}

func TestDeletingAWorkspaceWithSessionsOrDraftsFromItsShortcutStillAsks(t *testing.T) {
	for _, check := range []struct {
		sessions uint64
		drafts   bool
	}{{2, false}, {0, true}} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.SetActiveWorkspace(workspacesStringPtr("a"))
		generation := workspacesLoadGeneration(t, chord(dashboard, CommandIdCloseWorkspace))
		workspace := workspacesEntry("a", "Example")
		workspace.Workspace.SessionCount = check.sessions
		if check.drafts {
			workspace.Drafts = append(workspace.Drafts, workspacesDraft())
		}
		outcome := dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{workspace}, nil, false)
		if outcome.Action != DashboardAction(DashboardActionNone{}) {
			t.Fatalf("action %s", combinedActionDebug(outcome.Action))
		}
		manager := workspacesManagerOf(t, dashboard)
		if _, closing := manager.view.(WorkspaceManagerViewClose); manager.busy != nil || !closing {
			t.Fatalf("busy %v, view %#v", manager.busy, manager.view)
		}
	}
}

func TestTheListDeleteButtonActsAtOnceOnlyForAnEmptyWorkspace(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	generation := workspacesLoadGeneration(t, dashboard.BeginWorkspaceManager())
	busy := workspacesEntry("busy", "Busy")
	busy.Workspace.SessionCount = 1
	dashboard.FinishWorkspaceManagement(generation, []WorkspaceManagementEntry{busy, workspacesEntry("empty", "Empty")}, nil, false)
	pressDelete := func(index int) DashboardAction {
		if manager := workspaceManagerRef(dashboard.mode); manager != nil {
			manager.selectIndex(index)
			manager.form.Focus(WorkspaceControlDelete)
		}
		return dashboard.HandleKey(workspacesEnter())
	}
	if action := pressDelete(0); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("action %s", combinedActionDebug(action))
	}
	if _, closing := workspacesManagerOf(t, dashboard).view.(WorkspaceManagerViewClose); !closing {
		t.Fatalf("view %#v", workspacesManagerOf(t, dashboard).view)
	}
	workspacesManagerOf(t, dashboard).resetToList()
	if action := pressDelete(1); action != DashboardAction(DashboardActionCloseWorkspace{Generation: generation, WorkspaceID: "empty"}) {
		t.Fatalf("action %s", combinedActionDebug(action))
	}
}
