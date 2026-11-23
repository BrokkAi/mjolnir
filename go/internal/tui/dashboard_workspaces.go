// Port of mj-tui/src/dashboard_workspaces.rs.

// Workspace tabs, the sub-agent and reviewer child scopes, and the focus ring
// of the combined dashboard surface.

package tui

import (
	"iter"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// StoppedBySuspend is what the dashboard did about sub-agents whose parent's
// suspend stopped them, for the host to finish: their conversations go
// without saving a draft, since their records are gone, and the parent's
// conversation opens where the view went back to.
type StoppedBySuspend struct {
	Sessions []string
	Reopen   *string
}

// SubagentStoppedBySuspend is a sub-agent whose record left because its
// parent's suspend stopped it.
type SubagentStoppedBySuspend struct {
	sessionID string
	parentID  string
	title     string
	// shown: its conversation, or its own sub-agents, were on screen.
	shown bool
}

// ChildWorkspace is the Sub-agents or Reviewers view of one parent session.
// It is comparable, so it can key a map; compare is Rust's derived Ord.
type ChildWorkspace struct {
	ParentID string
	Kind     core.SubagentKind
}

// compare is Rust's derived Ord: by parent id, then by kind in declaration
// order (worker, reviewer, oracle).
func (w ChildWorkspace) compare(other ChildWorkspace) int {
	if order := strings.Compare(w.ParentID, other.ParentID); order != 0 {
		return order
	}
	ordinal := func(kind core.SubagentKind) int {
		switch kind {
		case core.SubagentKindWorker:
			return 0
		case core.SubagentKindReviewer:
			return 1
		default:
			return 2
		}
	}
	return ordinal(w.Kind) - ordinal(other.Kind)
}

// ActiveWorkspaceID is the workspace currently used as the Sessions-pane
// filter.
func (d *DashboardState) ActiveWorkspaceID() (string, bool) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::active_workspace_id")
}

func (d *DashboardState) SubagentParentID() (string, bool) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::subagent_parent_id")
}

func (d *DashboardState) ReviewersWorkspaceOpen() bool {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::reviewers_workspace_open")
}

// childWorkspaceFor is the child scope that shows child, or nil when it has
// no parent.
func (d *DashboardState) childWorkspaceFor(child string) *ChildWorkspace {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::child_workspace_for")
}

func (d *DashboardState) subagentWorkspaceChain() []ChildWorkspace {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::subagent_workspace_chain")
}

func (d *DashboardState) OpenSubagentWorkspace(parentID string) {
	d.openChildWorkspace(parentID, core.SubagentKindWorker)
}

func (d *DashboardState) OpenReviewerWorkspace(parentID string) {
	d.openChildWorkspace(parentID, core.SubagentKindReviewer)
}

func (d *DashboardState) openChildWorkspace(parentID string, kind core.SubagentKind) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::open_child_workspace")
}

func (d *DashboardState) CloseSubagentWorkspace() {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::close_subagent_workspace")
}

// dashboardStateDefaultSessionsStateFilterFor is Rust's associated function
// `DashboardState::default_sessions_state_filter_for`: a Sub-agents view
// starts narrowed to the sub-agents that are not idle.
func dashboardStateDefaultSessionsStateFilterFor(scope *ChildWorkspace) *SessionStateFilter {
	if scope == nil || scope.Kind != core.SubagentKindWorker {
		return nil
	}
	filter := SessionStateFilterNotIdle
	return &filter
}

func (d *DashboardState) switchChildScope(next *ChildWorkspace) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::switch_child_scope")
}

// subagentsStoppedBySuspend is the active sub-agents that next no longer has
// because their parent's suspend, or its discard, stopped them: the parent
// is still there, and it is being suspended or already is.
func (d *DashboardState) subagentsStoppedBySuspend(next *core.StateState) []SubagentStoppedBySuspend {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::subagents_stopped_by_suspend")
}

// leaveSubagentsStoppedBySuspend takes the sub-agents stopped by their
// parent's suspend off the view (R15-1). Their records are gone, so a
// Sub-agents view that showed them would fall to an empty list: it goes back
// to the parent's scope with the parent selected, and says what happened
// instead.
func (d *DashboardState) leaveSubagentsStoppedBySuspend(stopped []SubagentStoppedBySuspend) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::leave_subagents_stopped_by_suspend")
}

// TakeStoppedBySuspend is what the host has to finish for sub-agents a
// suspend stopped since it last asked.
func (d *DashboardState) TakeStoppedBySuspend() StoppedBySuspend {
	taken := d.stoppedBySuspend
	d.stoppedBySuspend = StoppedBySuspend{}
	return taken
}

// recordEventHandled records that a handler took responsibility for the
// current event.
func (d *DashboardState) recordEventHandled() {
	d.lastEventConsumed = true
}

// workspaceIDs is the workspace ids in stable tab order. Runtime snapshots
// may remove an id; the controller decides which replacement tab to select.
func (d *DashboardState) workspaceIDs() []string {
	return slices.Clone(d.workspaceOrder)
}

func (d *DashboardState) workspaceDisplayName(workspaceID string) string {
	if name, ok := d.workspaceNames[workspaceID]; ok {
		return name
	}
	return workspaceID
}

// SetActiveWorkspace selects a tab locally; nil selects no workspace. The
// caller should save any open chat draft before invoking this setter; the
// setter itself performs no external work.
func (d *DashboardState) SetActiveWorkspace(workspaceID *string) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::set_active_workspace")
}

// CacheWorkspacePaneSizes applies a controller-provided pane-size cache
// unless this client has edited that workspace's layout since the cache was
// requested.
func (d *DashboardState) CacheWorkspacePaneSizes(workspaceID string, sizes core.PaneSizes) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::cache_workspace_pane_sizes")
}

// WorkspacePaneSizesModified reports whether the local client has edited
// this workspace's pane layout.
func (d *DashboardState) WorkspacePaneSizesModified(workspaceID string) bool {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::workspace_pane_sizes_modified")
}

func (d *DashboardState) registerWorkspaceTabArea(workspaceID string, area uv.Rectangle) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::register_workspace_tab_area")
}

func (d *DashboardState) clearWorkspaceTabAreas() {
	d.workspaceTabAreas = d.workspaceTabAreas[:0]
	d.workspaceDropdownArea = nil
	d.workspaceScrollGeometry = nil
	d.workspaceHamburgerArea = nil
	d.subagentWorkspaceCloseArea = nil
}

// SelectActiveSession selects a conversation immediately, retaining the
// current keyboard focus. Pinned sessions stay in their pane; other sessions
// replace Browse.
func (d *DashboardState) SelectActiveSession(sessionID string) {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::select_active_session")
}

// StartupSessions: global visibility must not change which workspace opens
// automatically.
func (d *DashboardState) StartupSessions() iter.Seq[*core.SessionRecord] {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::startup_sessions")
}

// Focus is the part of the combined surface that owns the keyboard.
func (d *DashboardState) Focus() Focus {
	return d.focus
}

func (d *DashboardState) PromptHasFocus() bool {
	panic("unported: mj-tui/src/dashboard_workspaces.rs DashboardState::prompt_has_focus")
}

func (d *DashboardState) setSessionActionFocus(action *CommandId) {
	if action == nil {
		d.sessionActionFocus = nil
		return
	}
	value := *action
	d.sessionActionFocus = &value
}

func (d *DashboardState) FocusPrompt() bool {
	if d.focus == FocusPrompt {
		return false
	}
	d.focus = FocusPrompt
	d.workspaceControlFocus = WorkspaceControlFocusTabs
	d.setSessionActionFocus(nil)
	return true
}

// FocusSessions focuses the Sessions pane without changing its explicit
// size.
func (d *DashboardState) FocusSessions() bool {
	before := d.focus
	d.focus = FocusSessions
	d.workspaceControlFocus = WorkspaceControlFocusTabs
	d.setSessionActionFocus(nil)
	d.clampSelections()
	return before != d.focus
}

// CycleFocus moves focus one stop along the Tab ring.
//
// Pane sizes are the user's setting, so Tab never changes them.
func (d *DashboardState) CycleFocus(reverse bool) bool {
	previous := d.focus
	d.focus = cycleControl(d.focus, focusOrder[:], reverse)
	if reverse && previous == FocusSessions && d.focus == FocusWorkspaces {
		d.workspaceControlFocus = WorkspaceControlFocusMenu
	} else {
		d.workspaceControlFocus = WorkspaceControlFocusTabs
	}
	if d.focus != FocusSessions {
		d.setSessionActionFocus(nil)
	}
	d.clampSelections()
	return previous != d.focus
}
