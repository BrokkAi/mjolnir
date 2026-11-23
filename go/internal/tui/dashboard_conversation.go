// Port of mj-tui/src/dashboard_conversation.rs.

// The conversation band's panes: focus, splits, zoom, pins, resize, and
// the stored arrangement.

package tui

import (
	"maps"
	"slices"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SplitRefusedNotice is the notice a refused split leaves on the bar. A
// later split that succeeds clears it, because it no longer describes the
// last split attempt.
const SplitRefusedNotice = "Not enough room to split this pane."

// resizeStep is how much of a split a single resize step moves.
const resizeStep float32 = 0.05

// unmeasuredConversationArea is the conversation band used before the first
// frame has measured one. It is only a starting point for CanSplit, which
// the next frame recomputes.
var unmeasuredConversationArea = uv.Rect(0, 0, 120, 30)

// FocusedPane is the pane the keyboard belongs to.
func (d *DashboardState) FocusedPane() PaneId {
	return d.navigation.layout().Focused()
}

// PaneSession is the session a pane shows, if it is not empty.
func (d *DashboardState) PaneSession(pane PaneId) (string, bool) {
	session, ok := d.navigation.sessions()[pane]
	return session, ok
}

// PaneAssignment is the identity captured by background work for the
// session assigned to a pane.
func (d *DashboardState) PaneAssignment(pane PaneId) (PaneAssignment, bool) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::pane_assignment")
}

// PaneForSession is the pane showing sessionID. A session is in at most one
// pane.
func (d *DashboardState) PaneForSession(sessionID string) (PaneId, bool) {
	for pane, session := range d.navigation.sessions() {
		if session == sessionID {
			return pane, true
		}
	}
	return 0, false
}

// PaneSessionEntry is one pane that shows a session, with the session.
type PaneSessionEntry struct {
	Pane      PaneId
	SessionID string
}

// PaneSessions is each pane that shows a session, with the session it
// shows, in pane order.
func (d *DashboardState) PaneSessions() []PaneSessionEntry {
	sessions := d.navigation.sessions()
	panes := slices.Sorted(maps.Keys(sessions))
	entries := make([]PaneSessionEntry, 0, len(panes))
	for _, pane := range panes {
		entries = append(entries, PaneSessionEntry{Pane: pane, SessionID: sessions[pane]})
	}
	return entries
}

// PaneSessionIDs is every session the layout currently shows.
func (d *DashboardState) PaneSessionIDs() map[string]struct{} {
	ids := make(map[string]struct{})
	for _, session := range d.navigation.sessions() {
		ids[session] = struct{}{}
	}
	return ids
}

// FocusPane activates a conversation; the Sessions marker derives from this
// pane.
func (d *DashboardState) FocusPane(pane PaneId) {
	if d.navigation.layout().Focused() == pane {
		return
	}
	d.navigation.focusPane(pane)
	if d.navigation.layout().Focused() != pane {
		return
	}
	d.attentionNavigation = nil
	d.markLayoutModified()
	d.clampSelections()
}

// ConversationPanes are the panes to draw and hit-test this frame.
//
// Zoomed, that is the focused pane alone filling the whole band: the others
// keep their place in the arrangement but are not on screen, so nothing may
// be drawn for them or pointed at in them.
func (d *DashboardState) ConversationPanes(area uv.Rectangle) []PaneInfo {
	if d.conversationZoomed && d.navigation.layout().PaneCount() > 1 {
		return []PaneInfo{{ID: d.navigation.layout().Focused(), Rect: area, IsFocused: true}}
	}
	return d.navigation.layout().Panes(area)
}

// ConversationZoomed reports whether the focused pane is filling the band
// on its own.
func (d *DashboardState) ConversationZoomed() bool {
	return d.conversationZoomed && d.navigation.layout().PaneCount() > 1
}

// zoomPaneCommand toggles the zoom on the focused pane. With nothing to hide
// there is nothing to zoom, and the surface says so rather than changing how
// it already looks.
func (d *DashboardState) zoomPaneCommand() DashboardAction {
	if d.navigation.layout().PaneCount() < 2 {
		d.SetNotice("Only one pane; nothing to zoom")
		return DashboardActionNone{}
	}
	d.conversationZoomed = !d.conversationZoomed
	return DashboardActionConversationPanesChanged{FocusMoved: false}
}

// SplitFocusedPane splits the focused pane and shows session in the new
// leaf, which takes the focus. False means the conversation area is too
// small to split, in which case the layout is unchanged and
// [SplitRefusedNotice] is shown.
func (d *DashboardState) SplitFocusedPane(direction chat.Direction, session *string) (PaneId, bool) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::split_focused_pane")
}

func (d *DashboardState) HasConversationPane(pane PaneId) bool {
	return slices.Contains(d.navigation.layout().PaneIDs(), pane)
}

func (d *DashboardState) SplitConversationPane(target PaneId, direction chat.Direction) (PaneId, bool) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::split_conversation_pane")
}

func (d *DashboardState) splitCommand(direction chat.Direction) DashboardAction {
	return DashboardActionSplitConversation{Pane: d.FocusedPane(), Direction: direction}
}

// ClosePane removes a pinned or empty pane and reports the session it
// showed. Browse remains available for list navigation.
func (d *DashboardState) ClosePane(pane PaneId) (string, bool) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::close_pane")
}

func (d *DashboardState) BrowsePane() PaneId {
	if d.browsePane != nil {
		return *d.browsePane
	}
	return d.navigation.layout().PaneIDs()[0]
}

func (d *DashboardState) PinID(session string) (uint32, bool) {
	id, ok := d.pinIDs[session]
	return id, ok
}

func (d *DashboardState) reconcilePins() {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::reconcile_pins")
}

// TakeSessionToOpen is the session a finished workspace switch should open,
// if the move that asked for the switch wanted it opened rather than only
// selected.
func (d *DashboardState) TakeSessionToOpen() (string, bool) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::take_session_to_open")
}

func (d *DashboardState) RevealPane(pane PaneId) {
	if pane != d.FocusedPane() {
		d.conversationZoomed = false
	}
}

func (d *DashboardState) SwapConversationPanes(source, target PaneId) {
	if d.navigation.swapPanes(source, target) {
		d.conversationZoomed = false
		d.markLayoutModified()
	}
}

// ResizeModeActive reports whether resize mode currently owns ordinary
// keyboard input.
func (d *DashboardState) ResizeModeActive() bool {
	return d.resizeMode && !d.ModalOpen()
}

func (d *DashboardState) beginResizeMode() DashboardAction {
	if d.navigation.layout().PaneCount() < 2 {
		d.SetNotice("Only one pane; nothing to resize")
		return DashboardActionNone{}
	}
	d.conversationZoomed = false
	d.resizeMode = true
	return DashboardActionConversationPanesChanged{FocusMoved: false}
}

func (d *DashboardState) swapPaneCommand(nav NavDirection) DashboardAction {
	panes := d.navigation.layout().Panes(d.conversationArea())
	focused := focusedPaneInfo(panes)
	if focused == nil {
		return DashboardActionNone{}
	}
	target, ok := FindInDirection(focused, nav, panes)
	if !ok {
		return DashboardActionNone{}
	}
	d.conversationZoomed = false
	d.navigation.swapPanes(focused.ID, target)
	d.markLayoutModified()
	return DashboardActionConversationPanesChanged{FocusMoved: false}
}

// focusedPaneInfo is the focused entry of panes, or nil.
func focusedPaneInfo(panes []PaneInfo) *PaneInfo {
	for index := range panes {
		if panes[index].IsFocused {
			return &panes[index]
		}
	}
	return nil
}

// ResizeFocusedPane grows or shrinks the focused pane toward nav by one
// step.
func (d *DashboardState) ResizeFocusedPane(nav NavDirection) {
	area := d.conversationArea()
	d.navigation.resizeFocused(nav, resizeStep, area)
	d.markLayoutModified()
}

// FocusPaneToward moves the focus to the nearest pane toward nav. It reports
// whether a pane was there to move to.
func (d *DashboardState) FocusPaneToward(nav NavDirection) bool {
	panes := d.navigation.layout().Panes(d.conversationArea())
	focused := focusedPaneInfo(panes)
	if focused == nil {
		return false
	}
	target, ok := FindInDirection(focused, nav, panes)
	if !ok {
		return false
	}
	d.FocusPane(target)
	return true
}

// focusPaneCommand moves the focus toward nav, reporting the work the
// controller owes the change: saving the arrangement and re-reading which
// conversation the keyboard is now in. Nothing to move to changes nothing.
func (d *DashboardState) focusPaneCommand(nav NavDirection) DashboardAction {
	if d.FocusPaneToward(nav) {
		return DashboardActionConversationPanesChanged{FocusMoved: true}
	}
	return DashboardActionNone{}
}

// focusLastPaneCommand moves the keyboard back to the pane it was in before
// this one.
//
// There is nothing to go back to before the first focus move, and closing a
// pane forgets it, so the command says so rather than moving the keyboard
// somewhere the user did not ask for.
func (d *DashboardState) focusLastPaneCommand() DashboardAction {
	layout := d.navigation.layout()
	previous, ok := layout.PreviousFocus()
	if !ok || previous == layout.Focused() || !slices.Contains(layout.PaneIDs(), previous) {
		d.SetNotice("No previous pane")
		return DashboardActionNone{}
	}
	d.FocusPane(previous)
	return DashboardActionConversationPanesChanged{FocusMoved: true}
}

// resizePaneCommand moves the focused pane's border toward nav by one step.
func (d *DashboardState) resizePaneCommand(nav NavDirection) DashboardAction {
	d.ResizeFocusedPane(nav)
	return DashboardActionConversationPanesChanged{FocusMoved: false}
}

// conversationArea is the band the panes are laid out in. Before the first
// frame, and while the surface is too small to draw a conversation, the
// layout still has to answer questions about pane sizes, so a nominal band
// stands in.
//
// DashboardState's measured band must not be named conversationArea in Go:
// the method has the name.
func (d *DashboardState) conversationArea() uv.Rectangle {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::conversation_area")
}

// ExportConversationLayout is the live arrangement in the form the
// workspace store keeps.
func (d *DashboardState) ExportConversationLayout() core.ConversationLayout {
	layout := d.navigation.layout().ToConversationLayout(d.navigation.sessions())
	browse := d.BrowsePane().Raw()
	layout.Browse = &browse
	layout.Pins = maps.Clone(d.pinIDs)
	return layout
}

// ConversationLayoutFor is the arrangement saved for one workspace: the
// live tree when it is the active workspace, and the cached copy otherwise.
func (d *DashboardState) ConversationLayoutFor(workspaceID string) core.ConversationLayout {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::conversation_layout_for")
}

// CacheWorkspaceLayout applies a controller-provided layout unless this
// client has already changed that workspace's arrangement.
func (d *DashboardState) CacheWorkspaceLayout(workspaceID string, layout core.ConversationLayout) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::cache_workspace_layout")
}

// WorkspaceLayoutModified reports whether the local client has changed this
// workspace's arrangement.
func (d *DashboardState) WorkspaceLayoutModified(workspaceID string) bool {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::workspace_layout_modified")
}

// restoreConversationLayout adopts a stored arrangement. A pane whose
// recorded session is no longer one this surface knows about becomes an
// empty pane rather than a failure: sessions are closed and removed outside
// this client.
func (d *DashboardState) restoreConversationLayout(layout *core.ConversationLayout) {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::restore_conversation_layout")
}

// resetConversationLayout starts over with one empty pane, for a workspace
// with nothing stored.
func (d *DashboardState) resetConversationLayout() {
	d.navigation.reset()
	d.browsePane = nil
	clear(d.pinIDs)
	d.paneMenu = nil
	d.conversationZoomed = false
}

func (d *DashboardState) markLayoutModified() {
	panic("unported: mj-tui/src/dashboard_conversation.rs DashboardState::mark_layout_modified")
}

// sessionBelongsInLayout reports whether a conversation pane of the active
// workspace may keep showing this session. The session record owns which
// workspace a session is in; a pane, and the selection that follows it,
// leaves with the record when the record moves to another workspace. Native
// agent views have no record of their own and stay with the pane that
// opened them.
func sessionBelongsInLayout[V any](state *core.StateState, nativeAgents map[string]V, activeWorkspaceID *string, sessionID string) bool {
	record, ok := state.Sessions.Get(sessionID)
	if !ok {
		if child, isChild := state.Subagents.Get(sessionID); isChild {
			record, ok = state.Sessions.Get(child.ParentSessionID)
		}
	}
	if !ok {
		return false
	}
	_, native := nativeAgents[sessionID]
	return native || activeWorkspaceID == nil || record.WorkspaceID == *activeWorkspaceID
}
