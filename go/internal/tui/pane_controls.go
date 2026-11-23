// Port of mj-tui/src/pane_controls.rs.

// Pin placement and pane menus share the dashboard's component event model.

package tui

import (
	"fmt"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// PaneOperation is what one pane-menu row does. Every variant is
// comparable.
type PaneOperation interface {
	isPaneOperation()
}

type PaneOperationSplit struct {
	Pane      PaneId
	Direction chat.Direction
}

type PaneOperationPinSplit struct {
	SessionID string
	Direction chat.Direction
}

type PaneOperationPinHere struct {
	SessionID string
	Pane      PaneId
}

type PaneOperationChooseEmpty string

type PaneOperationUnpin string

type PaneOperationSwap struct {
	Source PaneId
	Target PaneId
}

type PaneOperationZoom PaneId

type PaneOperationClose PaneId

// PaneOperationCommand runs one registry command, exactly as its key or
// palette row does.
type PaneOperationCommand CommandId

// PaneOperationInformation is an informational row in the read-only
// capacity dropdown.
type PaneOperationInformation struct{}

type PaneOperationWorkspace string

// PaneOperationPreferProfile moves a profile to the front of
// [quota] prefer.
type PaneOperationPreferProfile string

// PaneOperationClearLoadBalancing empties [quota] prefer.
type PaneOperationClearLoadBalancing struct{}

// PaneOperationRefreshProfile probes this profile's quota now.
type PaneOperationRefreshProfile string

// PaneOperationProfileUsage opens Usage by session filtered to this
// profile.
type PaneOperationProfileUsage string

// PaneOperationSessionCommand runs a session command for the session whose
// row was right-clicked, through the same entry point its menu row uses.
type PaneOperationSessionCommand struct {
	SessionID string
	ID        CommandId
}

// PaneOperationNewSessionLike creates a top-level session with this
// session's settings.
type PaneOperationNewSessionLike string

type PaneOperationClearWorkspaceNotifications string

func (PaneOperationSplit) isPaneOperation()                       {}
func (PaneOperationPinSplit) isPaneOperation()                    {}
func (PaneOperationPinHere) isPaneOperation()                     {}
func (PaneOperationChooseEmpty) isPaneOperation()                 {}
func (PaneOperationUnpin) isPaneOperation()                       {}
func (PaneOperationSwap) isPaneOperation()                        {}
func (PaneOperationZoom) isPaneOperation()                        {}
func (PaneOperationClose) isPaneOperation()                       {}
func (PaneOperationCommand) isPaneOperation()                     {}
func (PaneOperationInformation) isPaneOperation()                 {}
func (PaneOperationWorkspace) isPaneOperation()                   {}
func (PaneOperationPreferProfile) isPaneOperation()               {}
func (PaneOperationClearLoadBalancing) isPaneOperation()          {}
func (PaneOperationRefreshProfile) isPaneOperation()              {}
func (PaneOperationProfileUsage) isPaneOperation()                {}
func (PaneOperationSessionCommand) isPaneOperation()              {}
func (PaneOperationNewSessionLike) isPaneOperation()              {}
func (PaneOperationClearWorkspaceNotifications) isPaneOperation() {}

// paneMenuEntry is one row of a pane menu: its label and what it does.
type paneMenuEntry struct {
	label     string
	operation PaneOperation
}

// PaneMenu is the open pane, title, row or dropdown menu.
type PaneMenu struct {
	title              string
	entries            []paneMenuEntry
	form               chat.Dialog[int]
	destinations       bool
	pressedDestination *int
	// popup is where the menu was last drawn. The empty-pane chooser lies
	// inside a destination pane, so a click on the menu must not count as a
	// click on the pane beneath it.
	popup uv.Rectangle
	// anchor is the title a dropdown hangs from. Nil centres the menu.
	anchor *uv.Rectangle
	// displayRows are styled, non-command rows used by the capacity details
	// dropdown; nil when the menu shows its entries.
	displayRows []chat.Line
	// capacityTarget is the target whose Disks cell opened this menu, if
	// any.
	capacityTarget *string
}

func (d *DashboardState) showPaneMenu(title string, entries []paneMenuEntry, destinations bool) {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::show_pane_menu")
}

func (d *DashboardState) beginWorkspaceDropdown() {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::begin_workspace_dropdown")
}

// beginCapacityDisksMenu opens the selected capacity row's filesystem
// details beneath its Disks cell, using the rendered button geometry as the
// anchor.
func (d *DashboardState) beginCapacityDisksMenu(index int) {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::begin_capacity_disks_menu")
}

func (d *DashboardState) capacityDisksMenuOpen(targetID string) bool {
	return d.paneMenu != nil && d.paneMenu.capacityTarget != nil && *d.paneMenu.capacityTarget == targetID
}

// beginSupportPaneMenu opens the small menu that hangs from a support
// pane's title: Refresh, then the Setup pages for what the pane lists.
// Targets has Runtimes… and Machines…; Profiles has Settings…, the Agent
// Profiles page. Clicking the title opens it, and so does . while the pane
// has the keyboard. Sessions has its own buttons, so it has no title menu.
func (d *DashboardState) beginSupportPaneMenu(pane SupportPane) {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::begin_support_pane_menu")
}

// beginQuotaRowMenu is the right-click menu on a Quota profile row: refresh
// and usage for that profile, then the load-balancing preference.
func (d *DashboardState) beginQuotaRowMenu(profileID string, at uv.Position) {
	entries := []paneMenuEntry{
		{"Refresh", PaneOperationRefreshProfile(profileID)},
		{"Usage…", PaneOperationProfileUsage(profileID)},
		{fmt.Sprintf("Prefer %s in load balancing", profileID), PaneOperationPreferProfile(profileID)},
		{"Clear load balancing preferences", PaneOperationClearLoadBalancing{}},
	}
	d.showPaneMenu(profileID, entries, false)
	if d.paneMenu != nil {
		anchor := uv.Rect(at.X, at.Y, 1, 1)
		d.paneMenu.anchor = &anchor
	}
}

// beginSessionRowMenu is the right-click menu on a session row. Its entries
// act on that row's session, never on the selected one. A harness-native
// child has no row and no record of its own, so it gets no menu.
func (d *DashboardState) beginSessionRowMenu(sessionID string, at uv.Position) {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::begin_session_row_menu")
}

// dismissibleSessionsIn is the sessions in workspaceID whose notification
// "Dismiss notification" could dismiss now. The workspace menu draws its row
// and runs it from this one answer.
func (d *DashboardState) dismissibleSessionsIn(workspaceID string) []string {
	ids := []string{}
	for _, session := range d.state.Get().Sessions.All() {
		if session.WorkspaceID != workspaceID {
			continue
		}
		if _, ready := dismissNotificationAvailableFor(d, session.ID).(AvailabilityReady); ready {
			ids = append(ids, session.ID)
		}
	}
	return ids
}

func (d *DashboardState) beginWorkspaceRowMenu(workspaceID string, at uv.Position) {
	entries := []paneMenuEntry{{"Clear notifications", PaneOperationClearWorkspaceNotifications(workspaceID)}}
	d.showPaneMenu("Workspace", entries, false)
	if d.paneMenu != nil {
		anchor := uv.Rect(at.X, at.Y, 1, 1)
		d.paneMenu.anchor = &anchor
	}
}

// paneOperationEnabled says whether a pane-menu row can run now. Both the
// drawing and the activation ask here, so a row that looks disabled cannot
// be run from the keyboard, a digit or the mouse.
func (d *DashboardState) paneOperationEnabled(operation PaneOperation) bool {
	switch operation := operation.(type) {
	case PaneOperationClearLoadBalancing:
		// Clearing an empty preference list does nothing, so the row says so
		// by being disabled.
		return len(d.config.Get().Quota.Prefer) > 0
	case PaneOperationSessionCommand:
		switch operation.ID {
		case CommandIdDismissNotification:
			_, ready := dismissNotificationAvailableFor(d, operation.SessionID).(AvailabilityReady)
			return ready
		case CommandIdOpenWaitingQuestion:
			_, waiting := d.waitingChildOf(operation.SessionID)
			return waiting
		default:
			return d.state.Get().Sessions.ContainsKey(operation.SessionID)
		}
	case PaneOperationNewSessionLike:
		return d.state.Get().Sessions.ContainsKey(string(operation))
	case PaneOperationClearWorkspaceNotifications:
		return len(d.dismissibleSessionsIn(string(operation))) > 0
	default:
		return true
	}
}

// runSessionCommand runs id for sessionID as its own menu row would: the
// command acts on the named session for exactly this call.
func (d *DashboardState) runSessionCommand(sessionID string, id CommandId) DashboardAction {
	if !d.state.Get().Sessions.ContainsKey(sessionID) {
		d.SetNotice("This session is no longer available.")
		return DashboardActionNone{}
	}
	saved := d.commandSessionOverride
	d.commandSessionOverride = &sessionID
	action := d.runAvailableCommand(id)
	d.commandSessionOverride = saved
	return action
}

func (d *DashboardState) beginPinMenu(session string) {
	if !d.state.Get().Sessions.ContainsKey(session) {
		return
	}
	entries := []paneMenuEntry{
		{"Pin and browse right", PaneOperationPinSplit{SessionID: session, Direction: chat.DirectionHorizontal}},
		{"Pin and browse down", PaneOperationPinSplit{SessionID: session, Direction: chat.DirectionVertical}},
	}
	if len(d.emptyPinPanes()) > 0 {
		entries = append(entries, paneMenuEntry{"Pin in empty pane…", PaneOperationChooseEmpty(session)})
	}
	d.showPaneMenu("Pin session", entries, false)
}

// paneShowsPinHint says whether the pane chrome puts "Pin selected here" on
// the pane's first inside row: an empty pane other than Browse. Anything
// drawn in the pane keeps clear of that row.
func (d *DashboardState) paneShowsPinHint(pane PaneId) bool {
	_, occupied := d.PaneSession(pane)
	return !occupied && pane != d.BrowsePane()
}

func (d *DashboardState) emptyPinPanes() []PaneId {
	panes := []PaneId{}
	for _, pane := range d.navigation.layout().PaneIDs() {
		if _, occupied := d.PaneSession(pane); pane != d.BrowsePane() && !occupied {
			panes = append(panes, pane)
		}
	}
	return panes
}

func (d *DashboardState) toggleSessionPin(session string) DashboardAction {
	if _, pinned := d.PinID(session); pinned {
		return DashboardActionUnpinSession{SessionID: session}
	}
	d.beginPinMenu(session)
	return DashboardActionNone{}
}

func (d *DashboardState) beginPaneMenu(pane PaneId) {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::begin_pane_menu")
}

func (d *DashboardState) handlePaneMenuEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/pane_controls.rs DashboardState::handle_pane_menu_event")
}

const paneChromeSuffix = "| Conversation "

// paneChromeLabel is the pane's own name in its title row: Browse, its pin
// badge, or Empty.
func paneChromeLabel(dashboard *DashboardState, pane PaneId) string {
	if pane == dashboard.BrowsePane() {
		return "Browse"
	}
	if session, ok := dashboard.PaneSession(pane); ok {
		if badge, ok := dashboard.PinID(session); ok {
			return chat.ThemeGlyphs().Pinned + " " + chat.PinLabel(badge)
		}
	}
	return "Empty"
}

// paneChromeWidth is how many columns at the left of a pane's title row
// renderPaneChrome draws over, so the conversation drawn beneath can start
// its own title after them instead of losing its first words.
func paneChromeWidth(dashboard *DashboardState, pane PaneId, width int) int {
	if width < 12 {
		return 0
	}
	label := paneChromeLabel(dashboard, pane)
	drawn := chat.RawLine(" " + label + " " + paneChromeSuffix).Width()
	return min(drawn, width-12)
}

// childPaneTitle is the title of a pane that shows a child's conversation
// read-only in place of a chat (a native child, a stopped sub-agent): the
// child's name first, then as many of its status words as fit, dropped from
// the end. A name too long for the row alone ends in an ellipsis.
//
// Like the chat title, it starts after the label renderPaneChrome draws at
// the left of the title row and stops short of the chips at the right. The
// native child's title started under the label, so the row read "Browse |
// Conversation d · availability unknown" (R12-3).
func childPaneTitle(dashboard *DashboardState, pane PaneId, width int, name string, status []string) chat.Line {
	panic("unported: mj-tui/src/pane_controls.rs child_pane_title")
}

// paneChromeChipsReserve is how many columns from the right of a pane's
// title row the pane chips start at: the pin chip, then the menu chip, then
// room for the close chip.
const paneChromeChipsReserve = 10

// paneTitleReserve is the columns a pane's own title must leave clear at
// the right: the host's reserve for its close and zoom chips, widened to
// clear the pin and menu chips renderPaneChrome draws whenever the pane is
// wide enough for them. A title that ran under a chip showed through its
// unpainted cells.
func paneTitleReserve(dashboard *DashboardState, width int, reserve int) int {
	if width < 12 {
		return reserve
	}
	zoom := 0
	if dashboard.ConversationZoomed() {
		zoom = 3
	}
	return max(reserve, paneChromeChipsReserve+zoom)
}

func renderPaneChrome(frame *chat.Frame, dashboard *DashboardState) {
	panic("unported: mj-tui/src/pane_controls.rs render_pane_chrome")
}

// dropdownPopup is where a dropdown sits: under the title it hangs from, as
// wide as its longest line needs. It opens upward when there is no room
// below, as for a minimized pane, whose one row sits just above the footer.
func dropdownPopup(area uv.Rectangle, anchor uv.Rectangle, title string, items []chat.Line) uv.Rectangle {
	panic("unported: mj-tui/src/pane_controls.rs dropdown_popup")
}

func renderPaneMenu(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	panic("unported: mj-tui/src/pane_controls.rs render_pane_menu")
}
