// Port of mj-tui/src/dashboard_workspaces.rs.

// Workspace tabs, the sub-agent and reviewer child scopes, and the focus ring
// of the combined dashboard surface.

package tui

import (
	"fmt"
	"iter"
	"maps"
	"reflect"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	if active := *d.activeWorkspaceID.Get(); active != nil {
		return *active, true
	}
	return "", false
}

func (d *DashboardState) SubagentParentID() (string, bool) {
	if workspace := *d.childWorkspace.Get(); workspace != nil {
		return workspace.ParentID, true
	}
	return "", false
}

func (d *DashboardState) ReviewersWorkspaceOpen() bool {
	workspace := *d.childWorkspace.Get()
	return workspace != nil && workspace.Kind == core.SubagentKindReviewer
}

// childWorkspaceFor is the child scope that shows child, or nil when it has
// no parent.
func (d *DashboardState) childWorkspaceFor(child string) *ChildWorkspace {
	parentID, ok := d.subagentParentFor(child)
	if !ok {
		return nil
	}
	kind := core.SubagentKindWorker
	if relation := d.state.Get().Subagents.GetShared(child); relation != nil {
		switch relation.Kind {
		// The oracle shows in the Sub-agents view.
		case core.SubagentKindOracle, "":
			kind = core.SubagentKindWorker
		default:
			kind = relation.Kind
		}
	}
	return &ChildWorkspace{ParentID: parentID, Kind: kind}
}

func (d *DashboardState) subagentWorkspaceChain() []ChildWorkspace {
	var chain []ChildWorkspace
	seen := map[string]struct{}{}
	var next *ChildWorkspace
	if current := *d.childWorkspace.Get(); current != nil {
		copied := *current
		next = &copied
	}
	for next != nil {
		workspace := *next
		if _, repeated := seen[workspace.ParentID]; repeated {
			break
		}
		seen[workspace.ParentID] = struct{}{}
		next = d.childWorkspaceFor(workspace.ParentID)
		chain = append(chain, workspace)
	}
	slices.Reverse(chain)
	return chain
}

func (d *DashboardState) OpenSubagentWorkspace(parentID string) {
	d.openChildWorkspace(parentID, core.SubagentKindWorker)
}

func (d *DashboardState) OpenReviewerWorkspace(parentID string) {
	d.openChildWorkspace(parentID, core.SubagentKindReviewer)
}

func (d *DashboardState) openChildWorkspace(parentID string, kind core.SubagentKind) {
	state := d.state.Get()
	if !state.Sessions.ContainsKey(parentID) {
		return
	}
	d.switchChildScope(&ChildWorkspace{ParentID: parentID, Kind: kind})
	// The pane now lists a different set of rows; the parent list's scroll
	// position means nothing here.
	d.sessionsScroll = 0
	d.SetCurrentSession(nil)
	// Compact relationships still need a selection to load their details;
	// only rows with known activity can be excluded by the idle filter.
	child := ""
	found := false
	if ordered, _ := d.deriveOrderedSessions(); len(ordered) > 0 {
		child, found = ordered[0].ID, true
	} else {
		for _, id := range d.managedChildIDsOfKind(parentID, kind) {
			if !d.state.Get().Sessions.ContainsKey(id) {
				child, found = id, true
				break
			}
		}
	}
	if found {
		d.SelectActiveSession(child)
	}
	d.focus = FocusSessions
	d.clampSelections()
}

func (d *DashboardState) CloseSubagentWorkspace() {
	workspace := *d.childWorkspace.Get()
	if workspace == nil {
		return
	}
	parentID := workspace.ParentID
	next := d.childWorkspaceFor(parentID)
	d.switchChildScope(next)
	d.sessionsScroll = 0
	d.SelectActiveSession(parentID)
	d.clampSelections()
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
	previous := *d.childWorkspace.Get()
	if (previous == nil) == (next == nil) && (next == nil || *previous == *next) {
		return
	}
	// The zero ChildWorkspace keys the top-level list (Rust's None).
	var previousKey, nextKey ChildWorkspace
	if previous != nil {
		previousKey = *previous
	}
	var stored *ChildWorkspace
	if next != nil {
		nextKey = *next
		copied := *next
		stored = &copied
	}
	d.childWorkspace.Set(stored)
	if d.sessionsFiltersByScope == nil {
		d.sessionsFiltersByScope = make(map[ChildWorkspace]*SessionsFilter)
	}
	d.sessionsFiltersByScope[previousKey] = *d.sessionsFilter.Get()
	d.sessionsFilter.Set(nil)
	filter, retained := d.sessionsFiltersByScope[nextKey]
	delete(d.sessionsFiltersByScope, nextKey)
	if !retained {
		filter = nil
		if state := dashboardStateDefaultSessionsStateFilterFor(next); state != nil {
			filter = &SessionsFilter{State: state}
		}
	}
	d.sessionsFilter.Set(filter)
}

// subagentsStoppedBySuspend is the active sub-agents that next no longer has
// because their parent's suspend, or its discard, stopped them: the parent
// is still there, and it is being suspended or already is.
func (d *DashboardState) subagentsStoppedBySuspend(next *core.StateState) []SubagentStoppedBySuspend {
	current := d.state.Get()
	equal := func(left, right *core.SessionRecord) bool { return reflect.DeepEqual(left, right) }
	candidates := map[string]struct{}{}
	for id := range d.durableRecords.Changes(next.Sessions, equal) {
		candidates[id] = struct{}{}
	}
	for id := range d.presentedRecords.Changes(current.Sessions, equal) {
		candidates[id] = struct{}{}
	}
	d.reconciliationVisits += len(candidates)
	var stopped []SubagentStoppedBySuspend
	for _, id := range slices.Sorted(maps.Keys(candidates)) {
		if next.Sessions.ContainsKey(id) {
			continue
		}
		relation := current.Subagents.GetShared(id)
		if relation == nil {
			continue
		}
		child := current.Sessions.GetShared(relation.ChildSessionID)
		if child == nil || !child.State.IsActive() {
			continue
		}
		parentID := relation.ParentSessionID
		operation, operating := d.SessionOperationKind(parentID)
		parentSuspending := operating && operation == SessionOperationKindSuspending
		var suspending bool
		if parent := next.Sessions.GetShared(parentID); parent != nil {
			suspending = parentSuspending ||
				parent.State == core.SessionStateClosing ||
				parent.State == core.SessionStateDestroying ||
				parent.State == core.SessionStateStopped
		} else {
			// The feed carries only live sessions, so a parent that finished
			// its suspend can leave in the same frame.
			parent := current.Sessions.GetShared(parentID)
			suspending = parentSuspending || (parent != nil &&
				(parent.State == core.SessionStateClosing || parent.State == core.SessionStateDestroying))
		}
		shown := false
		if id, ok := d.SubagentParentID(); ok && id == child.ID {
			shown = true
		}
		if id, ok := d.SelectedSessionID(); ok && id == child.ID {
			shown = true
		}
		for _, id := range d.navigation.sessions() {
			if id == child.ID {
				shown = true
			}
		}
		if suspending {
			stopped = append(stopped, SubagentStoppedBySuspend{
				sessionID: child.ID,
				parentID:  parentID,
				title:     child.ListedTitle(),
				shown:     shown,
			})
		}
	}
	return stopped
}

// leaveSubagentsStoppedBySuspend takes the sub-agents stopped by their
// parent's suspend off the view (R15-1). Their records are gone, so a
// Sub-agents view that showed them would fall to an empty list: it goes back
// to the parent's scope with the parent selected, and says what happened
// instead.
func (d *DashboardState) leaveSubagentsStoppedBySuspend(stopped []SubagentStoppedBySuspend) {
	parents := map[string]struct{}{}
	for _, child := range stopped {
		parents[child.parentID] = struct{}{}
	}
	for _, parentID := range slices.Sorted(maps.Keys(parents)) {
		var titles []string
		inView := false
		for _, child := range stopped {
			if child.parentID != parentID {
				continue
			}
			titles = append(titles, child.title)
			inView = inView || child.shown
		}
		if id, ok := d.SubagentParentID(); ok && id == parentID {
			inView = true
		}
		if !inView {
			continue
		}
		next := d.childWorkspaceFor(parentID)
		d.switchChildScope(next)
		d.SelectActiveSession(parentID)
		d.clampSelections()
		named := core.QuotedTitles(titles)
		if len(titles) == 1 {
			d.SetNotice(fmt.Sprintf("Sub-agent %s was stopped by the suspend", named))
		} else {
			d.SetNotice(fmt.Sprintf("Sub-agents %s were stopped by the suspend", named))
		}
		// A parent that left the feed has stopped; there is nothing to
		// reopen.
		if d.state.Get().Sessions.ContainsKey(parentID) {
			reopen := parentID
			d.stoppedBySuspend.Reopen = &reopen
		}
	}
	for _, child := range stopped {
		d.stoppedBySuspend.Sessions = append(d.stoppedBySuspend.Sessions, child.sessionID)
	}
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
	d.resizeMode = false
	if manager, ok := d.mode.(ModeWorkspaceManager); ok {
		manager.activeWorkspaceID = workspaceCopyID(workspaceID)
	}
	current := *d.activeWorkspaceID.Get()
	if (current == nil) == (workspaceID == nil) && (workspaceID == nil || *current == *workspaceID) {
		d.clampSelections()
		return
	}
	// Only the pending attention jump continues the traversal across tabs.
	attentionNavigation := d.attentionNavigation
	d.attentionNavigation = nil
	if navigation := attentionNavigation; navigation != nil {
		session := d.state.Get().Sessions.GetShared(navigation.sessionID)
		continues := d.pendingNavigation != nil &&
			d.pendingNavigation.sessionID == navigation.sessionID &&
			session != nil && workspaceID != nil && session.WorkspaceID == *workspaceID
		if !continues {
			attentionNavigation = nil
		}
	}
	workspaceFocused := d.focus == FocusWorkspaces
	if current != nil {
		if d.workspaceViews == nil {
			d.workspaceViews = make(map[string]WorkspaceViewState)
		}
		d.workspaceViews[*current] = workspaceViewStateFromDashboard(d)
	}

	d.switchGoWorkspace(workspaceID)
	d.sessionsFilter.Set(nil)
	d.activeWorkspaceID.Set(workspaceCopyID(workspaceID))
	d.workspaceName = ""
	if workspaceID != nil {
		d.workspaceName = d.workspaceDisplayName(*workspaceID)
	}
	d.openingSession = nil
	view, restored := WorkspaceViewState{}, false
	if workspaceID != nil {
		view, restored = d.workspaceViews[*workspaceID]
	}
	if restored {
		d.sessionsScroll = view.sessionsScroll
		d.sessionsWheelScrolledFrom = view.sessionsWheelScrolledFrom
		d.targetsScroll = view.targetsScroll
		d.quotaScroll = view.quotaScroll
		d.capacityIndex = view.capacityIndex
		d.quotaSelection = view.quotaSelection
		d.restoreWorkspacePaneSizes(view.paneSizes)
		d.restoreConversationLayout(&view.conversationLayout)
		d.collapsedProjectKeys.Set(view.collapsedProjectKeys)
		d.focus = view.focus
	} else {
		d.sessionsScroll = 0
		d.targetsScroll = 0
		d.quotaScroll = 0
		d.sessionsWheelScrolledFrom = nil
		d.capacityIndex = 0
		d.quotaSelection = nil
		d.restoreWorkspacePaneSizes(core.NewPaneSizes())
		d.resetConversationLayout()
		if workspaceID != nil && d.goMode != nil && d.goMode.LastSessionID != nil {
			d.SelectActiveSession(*d.goMode.LastSessionID)
		}
		d.collapsedProjectKeys.Set(map[string]struct{}{})
		d.focus = FocusSessions
	}
	active, hasActive := d.ActiveWorkspaceID()
	if parent := d.navigationSubagentParent; parent != nil {
		parentID := *parent
		if record := d.state.Get().Sessions.GetShared(parentID); record != nil && hasActive && record.WorkspaceID == active {
			reviewer := false
			if pending := d.pendingNavigation; pending != nil {
				if relation := d.state.Get().Subagents.GetShared(pending.sessionID); relation != nil {
					reviewer = relation.Kind == core.SubagentKindReviewer
				}
			}
			if reviewer {
				d.OpenReviewerWorkspace(parentID)
			} else {
				d.OpenSubagentWorkspace(parentID)
			}
			if pending := d.pendingNavigation; pending != nil {
				d.SelectActiveSession(pending.sessionID)
				d.clearFilterHidingSelection()
			}
		}
		d.navigationSubagentParent = nil
	} else if pending := d.pendingNavigation; pending != nil {
		if record := d.state.Get().Sessions.GetShared(pending.sessionID); record != nil && hasActive && record.WorkspaceID == active {
			key := d.ProjectSource(record).Key
			delete(*d.collapsedProjectKeys.Mut(), key)
			d.SelectActiveSession(pending.sessionID)
			d.clearFilterHidingSelection()
			// A move that only selects the row lands on Sessions, wherever
			// the focus was, so the row's own keys act on it.
			if !pending.open {
				d.focus = FocusSessions
				workspaceFocused = false
			}
		}
	}
	d.clampSelections()
	if workspaceFocused {
		d.focus = FocusWorkspaces
	}
	d.attentionNavigation = attentionNavigation
}

// CacheWorkspacePaneSizes applies a controller-provided pane-size cache
// unless this client has edited that workspace's layout since the cache was
// requested.
func (d *DashboardState) CacheWorkspacePaneSizes(workspaceID string, sizes core.PaneSizes) {
	if sizes.Validate() != nil {
		return
	}
	if _, modified := d.workspacePaneSizesModified[workspaceID]; modified {
		return
	}
	if active, ok := d.ActiveWorkspaceID(); ok && active == workspaceID {
		d.restoreWorkspacePaneSizes(sizes)
		d.clampSelections()
	}
	view, ok := d.workspaceViews[workspaceID]
	if !ok {
		view = WorkspaceViewState{
			paneSizes:            sizes,
			conversationLayout:   core.NewConversationLayout(),
			collapsedProjectKeys: map[string]struct{}{},
			focus:                FocusSessions,
		}
	}
	view.paneSizes = sizes
	if d.workspaceViews == nil {
		d.workspaceViews = make(map[string]WorkspaceViewState)
	}
	d.workspaceViews[workspaceID] = view
}

// WorkspacePaneSizesModified reports whether the local client has edited
// this workspace's pane layout.
func (d *DashboardState) WorkspacePaneSizesModified(workspaceID string) bool {
	_, modified := d.workspacePaneSizesModified[workspaceID]
	return modified
}

func (d *DashboardState) registerWorkspaceTabArea(workspaceID string, area uv.Rectangle) {
	d.workspaceTabAreas = append(d.workspaceTabAreas, client.DaemonPair[string, uv.Rectangle]{First: workspaceID, Second: area})
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
	// Navigation owns the selection immediately, even while the daemon is
	// loading the row detail for this family's compact relationship.
	state := d.state.Get()
	unloadedChild := false
	if child := state.Subagents.GetShared(sessionID); child != nil {
		if parent, ok := d.SubagentParentID(); ok && parent == child.ParentSessionID {
			unloadedChild = true
		}
	}
	if !state.Sessions.ContainsKey(sessionID) && !unloadedChild {
		return
	}
	d.attentionNavigation = nil
	pane, ok := d.PaneForSession(sessionID)
	if !ok {
		pane = d.BrowsePane()
	}
	d.RevealPane(pane)
	id := sessionID
	d.SetPaneSession(pane, &id)
	d.FocusPane(pane)
}

// StartupSessions: global visibility must not change which workspace opens
// automatically.
func (d *DashboardState) StartupSessions() iter.Seq[*core.SessionRecord] {
	return func(yield func(*core.SessionRecord) bool) {
		active, ok := d.ActiveWorkspaceID()
		for session := range d.state.Get().Sessions.Values() {
			if !session.State.IsActive() || !ok || session.WorkspaceID != active {
				continue
			}
			if !yield(session) {
				return
			}
		}
	}
}

// Focus is the part of the combined surface that owns the keyboard.
func (d *DashboardState) Focus() Focus {
	return d.focus
}

func (d *DashboardState) PromptHasFocus() bool {
	_, dashboard := d.mode.(ModeDashboard)
	return dashboard && d.focus == FocusPrompt
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

// workspaceCopyID is a copy of an optional workspace id, so the dashboard
// never shares the caller's string pointer.
func workspaceCopyID(workspaceID *string) *string {
	if workspaceID == nil {
		return nil
	}
	copied := *workspaceID
	return &copied
}
