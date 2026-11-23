// Port of mj-tui/src/pane_controls.rs.

// Pin placement and pane menus share the dashboard's component event model.

package tui

import (
	"fmt"
	"maps"
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	form := chat.NewDialog[int]()
	form.Register(0, chat.ControlKindChoiceList{Len: len(entries), Selected: 0}, uv.Rectangle{}, true)
	form.SetMenu(true)
	form.SetListActivation(0, chat.ListActivationSingleClick)
	form.EndFrame(0)
	d.CancelComponentPointer()
	d.paneMenu = &PaneMenu{
		title:        title,
		entries:      entries,
		form:         form,
		destinations: destinations,
	}
}

func (d *DashboardState) beginWorkspaceDropdown() {
	if d.workspaceDropdownArea == nil {
		return
	}
	anchor := *d.workspaceDropdownArea
	ids := d.workspaceIDs()
	active, hasActive := d.ActiveWorkspaceID()
	selected := 0
	if hasActive {
		selected = max(slices.Index(ids, active), 0)
	}
	entries := make([]paneMenuEntry, 0, len(ids))
	for _, id := range ids {
		label := d.workspaceDisplayName(id)
		if hasActive && id == active {
			label = label + " (current)"
		}
		entries = append(entries, paneMenuEntry{label: label, operation: PaneOperationWorkspace(id)})
	}
	if len(entries) == 0 {
		return
	}
	d.showPaneMenu("Workspaces", entries, false)
	if menu := d.paneMenu; menu != nil {
		menu.anchor = &anchor
		menu.form.SetSelected(0, selected)
	}
	d.focus = FocusWorkspaces
	d.workspaceControlFocus = WorkspaceControlFocusTabs
	d.setSessionActionFocus(nil)
}

// beginCapacityDisksMenu opens the selected capacity row's filesystem
// details beneath its Disks cell, using the rendered button geometry as the
// anchor.
func (d *DashboardState) beginCapacityDisksMenu(index int) {
	if index < 0 || index >= len(d.capacityDisksAreas) || d.capacityDisksAreas[index] == nil {
		return
	}
	anchor := *d.capacityDisksAreas[index]
	ids := slices.Sorted(maps.Keys(d.capacityDetails))
	if index >= len(ids) {
		return
	}
	detail := d.capacityDetails[ids[index]]
	targetID := detail.Reading.ID
	rows := capacityDisksMenuLines(&detail)
	if len(rows) == 0 {
		return
	}
	entries := make([]paneMenuEntry, 0, len(rows))
	for _, line := range rows {
		entries = append(entries, paneMenuEntry{label: line.String(), operation: PaneOperationInformation{}})
	}
	d.showPaneMenu("Disks", entries, false)
	if menu := d.paneMenu; menu != nil {
		menu.anchor = &anchor
		menu.displayRows = rows
		menu.capacityTarget = &targetID
	}
	d.focus = FocusTargets
	d.setSessionActionFocus(nil)
	d.capacityIndex = index
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
	type setting struct {
		label   string
		command CommandId
	}
	var index int
	var title string
	var settings []setting
	switch pane {
	case SupportPaneTargets:
		index, title = 1, "Targets"
		settings = []setting{
			{"Runtimes…", CommandIdManageTargets},
			{"Machines…", CommandIdManageMachines},
			{"CPU by session…", CommandIdSessionCpuReport},
		}
	case SupportPaneQuota:
		index, title = 2, "Profiles"
		settings = []setting{
			{"Settings…", CommandIdManageProfiles},
			{"Usage by session…", CommandIdSessionUsageReport},
		}
	default:
		return
	}
	entries := []paneMenuEntry{{label: "Refresh", operation: PaneOperationCommand(CommandIdRefresh)}}
	for _, entry := range settings {
		entries = append(entries, paneMenuEntry{label: entry.label, operation: PaneOperationCommand(entry.command)})
	}
	d.showPaneMenu(title, entries, false)
	// The title's first cell: one in from the pane's left edge, after the
	// border's corner or the minimized row's rule.
	var anchor *uv.Rectangle
	if d.paneAreas != nil {
		if area := d.paneAreas[index]; area.Dx() > 1 && area.Dy() > 0 {
			cell := uv.Rect(area.Min.X+1, area.Min.Y, 1, 1)
			anchor = &cell
		}
	}
	if menu := d.paneMenu; menu != nil {
		menu.anchor = anchor
	}
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
	if d.IsNativeAgent(sessionID) || !d.state.Get().Sessions.ContainsKey(sessionID) {
		return
	}
	label, command := "Dismiss notification", CommandIdDismissNotification
	if _, waiting := d.waitingChildOf(sessionID); waiting {
		label, command = "Open sub-agent question", CommandIdOpenWaitingQuestion
	}
	entries := []paneMenuEntry{
		{label, PaneOperationSessionCommand{SessionID: sessionID, ID: command}},
		{"Copy session ID", PaneOperationSessionCommand{SessionID: sessionID, ID: CommandIdCopySessionId}},
		{"New session with these settings", PaneOperationNewSessionLike(sessionID)},
	}
	firstEnabled := max(slices.IndexFunc(entries, func(entry paneMenuEntry) bool {
		return d.paneOperationEnabled(entry.operation)
	}), 0)
	d.showPaneMenu("Session", entries, false)
	if menu := d.paneMenu; menu != nil {
		anchor := uv.Rect(at.X, at.Y, 1, 1)
		menu.anchor = &anchor
		menu.form.SetSelected(0, firstEnabled)
	}
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
	entries := []paneMenuEntry{
		{"Split right", PaneOperationSplit{Pane: pane, Direction: chat.DirectionHorizontal}},
		{"Split down", PaneOperationSplit{Pane: pane, Direction: chat.DirectionVertical}},
	}
	if focused := d.FocusedPane(); focused != pane {
		entries = append(entries, paneMenuEntry{"Swap with focused pane", PaneOperationSwap{Source: focused, Target: pane}})
	}
	if pane != d.BrowsePane() {
		if session, ok := d.PaneSession(pane); ok {
			entries = append(entries, paneMenuEntry{"Unpin", PaneOperationUnpin(session)})
		} else if session, ok := d.SelectedSessionID(); ok {
			entries = append(entries, paneMenuEntry{"Pin selected here", PaneOperationPinHere{SessionID: session, Pane: pane}})
		}
		entries = append(entries, paneMenuEntry{"Close pane", PaneOperationClose(pane)})
	}
	entries = append(entries, paneMenuEntry{"Zoom / unzoom", PaneOperationZoom(pane)})
	d.showPaneMenu("Pane", entries, false)
}

func (d *DashboardState) handlePaneMenuEvent(event tea.Msg) DashboardAction {
	d.lastEventConsumed = true
	if menu := d.paneMenu; menu != nil && menu.capacityTarget != nil {
		closes := false
		switch event := event.(type) {
		case tea.KeyPressMsg:
			closes = chat.KeyPressCode(event) == 'd'
		case tea.MouseClickMsg:
			mouse := event.Mouse()
			closes = mouse.Button == tea.MouseLeft && menu.anchor != nil && rectContains(*menu.anchor, mouse.X, mouse.Y)
		}
		if closes {
			d.paneMenu = nil
			return DashboardActionNone{}
		}
	}
	menu := d.paneMenu
	if menu == nil {
		return DashboardActionNone{}
	}
	direct := -1
	if mouseEvent, ok := event.(tea.MouseMsg); ok && menu.destinations {
		mouse := mouseEvent.Mouse()
		if !rectContains(menu.popup, mouse.X, mouse.Y) {
			hit := slices.IndexFunc(menu.entries, func(entry paneMenuEntry) bool {
				pinHere, ok := entry.operation.(PaneOperationPinHere)
				if !ok {
					return false
				}
				return slices.ContainsFunc(d.conversationPaneAreas, func(area conversationPaneArea) bool {
					return area.pane == pinHere.Pane &&
						(rectContains(area.transcript, mouse.X, mouse.Y) || rectContains(area.prompt, mouse.X, mouse.Y))
				})
			})
			_, press := mouseEvent.(tea.MouseClickMsg)
			_, release := mouseEvent.(tea.MouseReleaseMsg)
			if press && mouse.Button == tea.MouseLeft && hit >= 0 {
				menu.pressedDestination = &hit
				return DashboardActionNone{}
			}
			if release && mouse.Button == tea.MouseLeft && menu.pressedDestination != nil {
				pressed := *menu.pressedDestination
				menu.pressedDestination = nil
				if hit != pressed {
					return DashboardActionNone{}
				}
				direct = pressed
			}
		}
	}
	if key, ok := event.(tea.KeyPressMsg); ok {
		if code := chat.KeyPressCode(key); code >= '1' && code <= '9' {
			direct = int(code - '1')
		}
	}
	var interaction chat.Interaction[int]
	if direct < 0 {
		interaction = menu.form.Handle(event).Action
	}
	if _, cancel := interaction.(chat.InteractionCancel[int]); cancel {
		d.paneMenu = nil
		return DashboardActionNone{}
	}
	selected := direct
	if selected < 0 {
		if activate, ok := interaction.(chat.InteractionActivate[int]); ok && activate.ID == 0 {
			selected, _ = menu.form.Selected(0)
		}
	}
	if selected < 0 || selected >= len(menu.entries) {
		return DashboardActionNone{}
	}
	operation := menu.entries[selected].operation
	if !d.paneOperationEnabled(operation) {
		return DashboardActionNone{}
	}
	d.paneMenu = nil
	d.lastRowClick = nil
	switch operation := operation.(type) {
	case PaneOperationSplit:
		return DashboardActionSplitConversation{Pane: operation.Pane, Direction: operation.Direction}
	case PaneOperationPinSplit:
		return DashboardActionOpenSessionInSplit{SessionID: operation.SessionID, Direction: operation.Direction}
	case PaneOperationPinHere:
		return DashboardActionPinSession{SessionID: operation.SessionID, Pane: operation.Pane}
	case PaneOperationUnpin:
		return DashboardActionUnpinSession{SessionID: string(operation)}
	case PaneOperationChooseEmpty:
		session := string(operation)
		entries := []paneMenuEntry{}
		for index, pane := range d.emptyPinPanes() {
			entries = append(entries, paneMenuEntry{
				label:     fmt.Sprintf("%d: Pin in empty pane", index+1),
				operation: PaneOperationPinHere{SessionID: session, Pane: pane},
			})
		}
		d.conversationZoomed = false
		d.showPaneMenu("Choose empty pane (click or number)", entries, true)
		return DashboardActionNone{}
	case PaneOperationSwap:
		d.SwapConversationPanes(operation.Source, operation.Target)
		return DashboardActionConversationPanesChanged{FocusMoved: false}
	case PaneOperationZoom:
		d.FocusPane(PaneId(operation))
		return d.zoomPaneCommand()
	case PaneOperationClose:
		return DashboardActionClosePane{Pane: PaneId(operation)}
	case PaneOperationCommand:
		return d.runAvailableCommand(CommandId(operation))
	case PaneOperationInformation:
		return DashboardActionNone{}
	case PaneOperationPreferProfile:
		return DashboardActionPreferLoadBalancingProfile{ProfileID: string(operation)}
	case PaneOperationClearLoadBalancing:
		if len(d.config.Get().Quota.Prefer) == 0 {
			return DashboardActionNone{}
		}
		return DashboardActionClearLoadBalancingPreferences{}
	case PaneOperationRefreshProfile:
		return DashboardActionProbeQuotaProfile{ProfileID: string(operation)}
	case PaneOperationProfileUsage:
		profile := string(operation)
		d.beginSessionUsageReport(&profile)
		return DashboardActionNone{}
	case PaneOperationSessionCommand:
		return d.runSessionCommand(operation.SessionID, operation.ID)
	case PaneOperationNewSessionLike:
		return d.beginNewSessionLike(string(operation))
	case PaneOperationClearWorkspaceNotifications:
		dismissals := []client.DaemonPair[string, core.NotificationDismissal]{}
		for _, sessionID := range d.dismissibleSessionsIn(string(operation)) {
			if target, ok := d.submitNotificationDismissal(sessionID); ok {
				dismissals = append(dismissals, client.DaemonPair[string, core.NotificationDismissal]{First: sessionID, Second: target})
			}
		}
		if len(dismissals) == 0 {
			return DashboardActionNone{}
		}
		return DashboardActionDismissNotifications{Dismissals: dismissals}
	case PaneOperationWorkspace:
		if slices.Contains(d.workspaceIDs(), string(operation)) {
			return DashboardActionSelectWorkspace{WorkspaceID: string(operation)}
		}
		return DashboardActionNone{}
	default:
		return DashboardActionNone{}
	}
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
	lead := paneChromeWidth(dashboard, pane, width)
	room := max(max(max(width-2, 0)-lead, 0)-paneTitleReserve(dashboard, width, 0), 0)
	var fitted *chat.Line
	for shown := len(status); shown >= 0; shown-- {
		var title strings.Builder
		title.WriteString(" " + name)
		for _, word := range status[:shown] {
			title.WriteString(" · ")
			title.WriteString(word)
		}
		title.WriteString(" ")
		line := chat.RawLine(title.String())
		if line.Width() <= room {
			fitted = &line
			break
		}
	}
	if fitted == nil {
		line := chat.TruncateLineToWidth(chat.RawLine(" "+name+" "), room)
		fitted = &line
	}
	spans := []chat.Span{chat.RawSpan(strings.Repeat(" ", lead))}
	spans = append(spans, fitted.Spans...)
	return chat.LineFrom(spans...)
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
	for _, paneArea := range dashboard.conversationPaneAreas {
		pane, transcript := paneArea.pane, paneArea.transcript
		width := transcript.Dx()
		if width < 12 || transcript.Dy() == 0 {
			continue
		}
		session, hasSession := dashboard.PaneSession(pane)
		var badge *uint32
		if hasSession {
			if id, ok := dashboard.PinID(session); ok {
				badge = &id
			}
		}
		label := paneChromeLabel(dashboard, pane)
		style := chat.Muted()
		if badge != nil {
			style = chat.Style{}.Foreground(chat.PinColor(*badge))
		}
		badgeStyle := style
		if selected, ok := dashboard.SelectedSessionID(); dashboard.Focus() == FocusSessions && hasSession && ok && session == selected {
			badgeStyle = style.Add(chat.ModifierBold | chat.ModifierUnderlined)
		}
		// A label cut short ends in an ellipsis rather than a stray letter.
		header := chat.TruncateLineToWidth(
			chat.LineFrom(
				chat.StyledSpan(" "+label+" ", badgeStyle),
				chat.StyledSpan(paneChromeSuffix, chat.Muted()),
			),
			width-12,
		)
		frame.RenderWidget(
			chat.NewParagraph(chat.TextFrom(header)),
			uv.Rect(transcript.Min.X+1, transcript.Min.Y, width-12, 1),
		)
		form := &dashboard.surfaceForm
		zoomReserve := 0
		if dashboard.ConversationZoomed() {
			zoomReserve = 3
		}
		pinGlyph := chat.ThemeGlyphs().Pin
		if badge != nil {
			pinGlyph = chat.ThemeGlyphs().Pinned
		}
		for _, chip := range []struct {
			control SurfaceControl
			offset  int
			glyph   string
		}{
			{SurfaceControlPaneMenu(pane), 7, chat.ThemeGlyphs().RowMenu},
			{SurfaceControlPanePin(pane), 10, pinGlyph},
		} {
			area := uv.Rect(max(transcript.Max.X-(chip.offset+zoomReserve), 0), transcript.Min.Y, 3, 1)
			form.Register(chip.control, chat.ControlKindButton{}, area, true)
			// Paint all three cells, so nothing underneath shows through.
			frame.RenderWidget(chat.NewParagraph(chat.RawText(" "+chip.glyph+" ")).WithStyle(style), area)
		}
		if dashboard.paneShowsPinHint(pane) && transcript.Dy() > 2 {
			area := uv.Rect(transcript.Min.X+1, transcript.Min.Y+1, min(19, width-2), 1)
			_, selected := dashboard.SelectedSessionID()
			form.Register(SurfaceControlPinHere(pane), chat.ControlKindButton{}, area, selected)
			frame.RenderWidget(chat.NewParagraph(chat.RawText("Pin selected here")).WithStyle(chat.Title(true)), area)
		}
	}
}

// dropdownPopup is where a dropdown sits: under the title it hangs from, as
// wide as its longest line needs. It opens upward when there is no room
// below, as for a minimized pane, whose one row sits just above the footer.
func dropdownPopup(area uv.Rectangle, anchor uv.Rectangle, title string, items []chat.Line) uv.Rectangle {
	// The title sits on the top border between the two corners.
	content := chat.RawLine(title).Width() + 2
	for _, item := range items {
		content = max(content, item.Width())
	}
	width := min(max(content+4, 16), area.Dx())
	wantedHeight := min(len(items)+2, area.Dy())
	below := max(area.Max.Y-anchor.Max.Y, 0)
	above := max(anchor.Min.Y-area.Min.Y, 0)
	openBelow := below >= wantedHeight || below >= above
	room := above
	if openBelow {
		room = below
	}
	height := max(min(wantedHeight, room), 1)
	x := max(min(anchor.Min.X, max(area.Max.X-width, 0)), area.Min.X)
	y := max(anchor.Min.Y-height, 0)
	if openBelow {
		y = anchor.Max.Y
	}
	return uv.Rect(x, y, width, height)
}

func renderPaneMenu(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	menu := dashboard.paneMenu
	if menu == nil {
		return
	}
	var items []chat.Line
	if menu.displayRows != nil {
		items = slices.Clone(menu.displayRows)
	} else {
		items = make([]chat.Line, 0, len(menu.entries))
		for _, entry := range menu.entries {
			if workspace, ok := entry.operation.(PaneOperationWorkspace); ok {
				if badge := attentionBadge(dashboard.workspaceAttentionSummary(string(workspace))); badge != nil {
					// Keep attention ahead of names so clipping cannot hide it.
					items = append(items, chat.LineFrom(*badge, chat.RawSpan(" "+entry.label)))
					continue
				}
			}
			items = append(items, chat.RawLine(entry.label))
		}
	}
	var popup uv.Rectangle
	switch {
	case menu.anchor != nil:
		popup = dropdownPopup(area, *menu.anchor, menu.title, items)
	case menu.destinations:
		// Open inside the first destination pane, under its "[1] Pin here"
		// marker, so the chooser sits beside the panes it names; without a
		// drawn pane, centre it like any other menu.
		width := min(area.Dx(), 44)
		height := min(area.Dy(), len(menu.entries)+2)
		var anchor *uv.Rectangle
		for _, entry := range menu.entries {
			pinHere, ok := entry.operation.(PaneOperationPinHere)
			if !ok {
				continue
			}
			index := slices.IndexFunc(dashboard.conversationPaneAreas, func(area conversationPaneArea) bool {
				return area.pane == pinHere.Pane
			})
			if index >= 0 {
				transcript := dashboard.conversationPaneAreas[index].transcript
				anchor = &transcript
				break
			}
		}
		if anchor != nil {
			x := min(anchor.Min.X+1, max(area.Max.X-width, 0))
			y := min(anchor.Min.Y+3, max(area.Max.Y-height, 0))
			popup = uv.Rect(max(x, area.Min.X), max(y, area.Min.Y), width, height)
		} else {
			popup = uv.Rect(area.Min.X+(area.Dx()-width)/2, area.Min.Y+(area.Dy()-height)/2, width, height)
		}
	default:
		width := min(area.Dx(), 44)
		height := min(area.Dy(), len(menu.entries)+2)
		popup = uv.Rect(area.Min.X+(area.Dx()-width)/2, area.Min.Y+(area.Dy()-height)/2, width, height)
	}
	if menu.destinations {
		for index, entry := range menu.entries {
			pinHere, ok := entry.operation.(PaneOperationPinHere)
			if !ok {
				continue
			}
			at := slices.IndexFunc(dashboard.conversationPaneAreas, func(area conversationPaneArea) bool {
				return area.pane == pinHere.Pane
			})
			if at < 0 {
				continue
			}
			transcript := dashboard.conversationPaneAreas[at].transcript
			frame.RenderWidget(
				chat.NewParagraph(chat.RawText(fmt.Sprintf(" [%d] Pin here ", index+1))).WithStyle(chat.Selection(true)),
				uv.Rect(transcript.Min.X+1, transcript.Min.Y+2, max(transcript.Dx()-2, 0), 1),
			)
		}
	}
	menu.popup = popup
	form := &menu.form
	selected, _ := form.Selected(0)
	form.BeginFrame()
	form.SetBounds(popup)
	block := chat.Modal().WithTitle(chat.RawLine(menu.title))
	inner := block.Inner(popup)
	frame.RenderWidget(chat.Clear{}, popup)
	frame.RenderWidget(block, popup)
	rowMap := make([]*int, len(items))
	for index := range rowMap {
		row := index
		rowMap[index] = &row
	}
	if menu.displayRows != nil {
		selected = min(selected, max(len(items)-1, 0))
		enabled := make([]bool, len(items))
		for index := range enabled {
			enabled[index] = true
		}
		chat.ChoiceListRenderWithRows(frame, inner, items, selected, rowMap, enabled, &form.Form, 0)
		form.SetMenu(true)
		form.SetListActivation(0, chat.ListActivationDoubleClick)
		offset := form.ListOffset(0)
		rows := make([]chat.Line, 0, len(items))
		for index, line := range items {
			if index == selected {
				highlight := chat.Style{}.Background(chat.ThemePalette().Selection).Add(chat.ModifierBold)
				if chat.ReverseVideo() {
					highlight = chat.Style{}.Add(chat.ModifierReversed | chat.ModifierBold)
				}
				line = line.PatchStyle(highlight)
			}
			rows = append(rows, line)
		}
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(rows...)).WithScroll(offset, 0), inner)
	} else {
		enabled := make([]bool, 0, len(menu.entries))
		for _, entry := range menu.entries {
			enabled = append(enabled, dashboard.paneOperationEnabled(entry.operation))
		}
		chat.ChoiceListRenderWithRows(frame, inner, items, selected, rowMap, enabled, &form.Form, 0)
		form.SetMenu(true)
		form.SetListActivation(0, chat.ListActivationSingleClick)
	}
	form.EndFrame(0)
}
