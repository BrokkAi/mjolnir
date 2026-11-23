// Port of mj-tui/src/dashboard_input.rs.

// Key, mouse, and paste input for the combined surface.
//
// crossterm's events are Bubble Tea messages here: a key event is a
// tea.KeyMsg (a tea.KeyPressMsg, repeats included through Key().IsRepeat,
// or a tea.KeyReleaseMsg), a mouse event a tea.MouseMsg, a paste a
// tea.PasteMsg, a resize a tea.WindowSizeMsg, and focus changes
// tea.FocusMsg and tea.BlurMsg.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ChatRegionContains is the conversation pane the pointer is over, if any.
// A click there belongs to that pane's chat, whatever has focus.
func (d *DashboardState) ChatRegionContains(column, row int) (PaneId, bool) {
	for _, area := range d.conversationPaneAreas {
		if rectContains(area.transcript, column, row) || rectContains(area.prompt, column, row) {
			return area.pane, true
		}
	}
	return 0, false
}

// OpenWebDialog opens the web-access dialog and asks the controller to load
// it.
func (d *DashboardState) OpenWebDialog() DashboardAction {
	dialog := webDialogLoading()
	d.mode = ModeWeb{&dialog}
	return DashboardActionLoadWebAccess{}
}

// HandleEventResult handles one terminal event and reports whether the
// dashboard consumed it. The legacy key and mouse wrappers below remain
// available to callers that only need the action. A nil Action is Rust's
// None.
func (d *DashboardState) HandleEventResult(event tea.Msg) chat.EventResult[DashboardAction] {
	d.lastEventConsumed = false
	var action DashboardAction = DashboardActionNone{}
	switch event := event.(type) {
	case tea.KeyMsg:
		action = d.HandleKeyAt(event, time.Now())
	case tea.MouseMsg:
		action = d.HandleMouse(event)
	case tea.PasteMsg:
		d.HandlePaste(event.Content)
	// The controller owns terminal-size comparison; focus events do not
	// mutate dashboard state by themselves.
	case tea.WindowSizeMsg, tea.BlurMsg:
		d.CancelComponentPointer()
	}
	if _, none := action.(DashboardActionNone); none {
		action = nil
	}
	return chat.EventResult[DashboardAction]{
		Consumed: d.lastEventConsumed || action != nil,
		Action:   action,
	}
}

func (d *DashboardState) HandleKey(key tea.KeyMsg) DashboardAction {
	return d.HandleKeyAt(key, time.Now())
}

// HandleKeyAt handles one key with an explicit reading of the clock. now
// decides whether the notice on screen has been readable long enough for
// this key press to dismiss it.
func (d *DashboardState) HandleKeyAt(key tea.KeyMsg, now time.Time) DashboardAction {
	press, pressed := key.(tea.KeyPressMsg)
	if pressed {
		d.pasteRevision++
	}
	if d.awayDialog() != nil {
		return d.handleAwayEvent(key)
	}
	if !pressed {
		return DashboardActionNone{}
	}
	if !press.IsRepeat {
		d.modalClickTransition = nil
		d.suppressModalRelease = false
	}
	if d.paneMenu != nil {
		return d.handlePaneMenuEvent(press)
	}
	if isPasteShortcut(press) {
		if !d.componentModalOpen() {
			if action := d.handleStandbyPromptKey(press); action != nil {
				return action
			}
		}
		d.recordEventHandled()
		return DashboardActionPasteFromClipboard{}
	}
	textFocused := d.textInputFocused()
	cancelShortcut := chat.KeyPressCode(press) == 'c' &&
		(press.Mod.Contains(tea.ModCtrl) || dashboardAccelerator(press.Mod))
	if textFocused && cancelShortcut && d.componentModalOpen() {
		return d.handleComponentEvent(tea.KeyPressMsg{Code: tea.KeyEscape})
	}
	// Ctrl-C belongs to the prompt or a text field. Everywhere else it is
	// intentionally inert, including modal controls that happen to use the
	// letter `c` for another purpose.
	if cancelShortcut && (d.focus != FocusNotifications || d.componentModalOpen()) {
		d.recordEventHandled()
		return DashboardActionNone{}
	}

	// Retire the notice this key press is stepping past, but only once it
	// has been on screen long enough to read: for a background failure this
	// bar is the only report there is.
	d.notices.Dismiss(now)
	if d.componentModalOpen() {
		return d.handleComponentEvent(press)
	}
	return d.handleDashboardKey(press)
}

func (d *DashboardState) textInputFocused() bool {
	modal := d.activeModal()
	return modal != nil && modal.textInputFocused()
}

func (d *DashboardState) PasteTarget() DashboardPasteTarget {
	target := DashboardPasteTarget{
		revision: d.pasteRevision,
		mode:     modeDiscriminant(d.mode),
		pane:     d.FocusedPane(),
		focus:    d.focus,
	}
	if d.focus == FocusNotifications {
		if session, ok := d.DisplayedNotificationSessionID(); ok {
			target.session = &session
		}
	} else if session, ok := d.CurrentSessionID(); ok {
		target.session = &session
	}
	return target
}

// ApplyClipboardText applies a modal clipboard read; it cannot follow focus
// into another field or session.
func (d *DashboardState) ApplyClipboardText(target *DashboardPasteTarget, text string) bool {
	current := d.PasteTarget()
	if !inputPasteTargetsEqual(&current, target) {
		return false
	}
	d.HandlePaste(text)
	return true
}

// inputPasteTargetsEqual is DashboardPasteTarget's derived PartialEq.
func inputPasteTargetsEqual(left, right *DashboardPasteTarget) bool {
	if left.revision != right.revision || left.mode != right.mode ||
		left.pane != right.pane || left.focus != right.focus ||
		(left.session == nil) != (right.session == nil) {
		return false
	}
	return left.session == nil || *left.session == *right.session
}

func (d *DashboardState) HandlePaste(pasted string) {
	d.pasteRevision++
	if d.awayDialog() != nil {
		d.handleAwayEvent(tea.PasteMsg{Content: pasted})
		return
	}
	if d.componentModalOpen() {
		d.handleComponentEvent(tea.PasteMsg{Content: pasted})
		return
	}
	if d.focus == FocusNotifications {
		d.pasteNotificationReply(pasted)
		return
	}
	if d.focus != FocusPrompt {
		return
	}
	normalized := strings.ReplaceAll(strings.ReplaceAll(pasted, "\r\n", "\n"), "\r", "\n")
	if sessionID, ok := d.standbyPromptSession(); ok {
		d.standbyPromptMut(sessionID).HandleComposerPaste(normalized)
	}
}

// FrameSurfaces are the surfaces the last frame registered, for the
// selection engine.
func (d *DashboardState) FrameSurfaces() *chat.FrameSurfaces {
	return &d.frameSurfaces
}

func (d *DashboardState) HandleMouse(mouse tea.MouseMsg) DashboardAction {
	d.pasteRevision++
	if d.awayDialog() != nil {
		return d.handleAwayEvent(mouse)
	}
	now := time.Now()
	at := mouse.Mouse()
	if inputMouseLeftDown(mouse) {
		transition := d.modalClickTransition
		d.modalClickTransition = nil
		d.suppressModalRelease = transition != nil &&
			transition.position.X == at.X && transition.position.Y == at.Y &&
			max(now.Sub(transition.at), 0) <= chat.DoubleClickInterval
		if d.suppressModalRelease {
			return DashboardActionNone{}
		}
	}
	if inputMouseLeftUp(mouse) && d.suppressModalRelease {
		d.suppressModalRelease = false
		return DashboardActionNone{}
	}
	before := d.dialogLayerKey()
	action := d.handleMouseInner(mouse)
	if inputMouseLeftUp(mouse) && before != d.dialogLayerKey() {
		d.modalClickTransition = &modalClickTransition{position: uv.Pos(at.X, at.Y), at: now}
		d.CancelComponentPointer()
	}
	return action
}

// inputMouseLeftDown reports crossterm's Down(Left).
func inputMouseLeftDown(mouse tea.MouseMsg) bool {
	click, ok := mouse.(tea.MouseClickMsg)
	return ok && click.Button == tea.MouseLeft
}

// inputMouseRightDown reports crossterm's Down(Right).
func inputMouseRightDown(mouse tea.MouseMsg) bool {
	click, ok := mouse.(tea.MouseClickMsg)
	return ok && click.Button == tea.MouseRight
}

// inputMouseLeftUp reports crossterm's Up(Left).
func inputMouseLeftUp(mouse tea.MouseMsg) bool {
	release, ok := mouse.(tea.MouseReleaseMsg)
	return ok && release.Button == tea.MouseLeft
}

// inputMouseWheel reports crossterm's ScrollUp (tea.MouseWheelUp) or ScrollDown
// (tea.MouseWheelDown).
func inputMouseWheel(mouse tea.MouseMsg, direction tea.MouseButton) bool {
	wheel, ok := mouse.(tea.MouseWheelMsg)
	return ok && wheel.Button == direction
}

// beginPointerPress abandons keyboard-only navigation modes on every
// surface.
func (d *DashboardState) beginPointerPress() {
	d.notices.Dismiss(time.Now())
	d.prefixPending = false
	d.resizeMode = false
}

func (d *DashboardState) handleMouseInner(mouse tea.MouseMsg) DashboardAction {
	at := mouse.Mouse()
	column, row := at.X, at.Y
	if inputMouseLeftDown(mouse) {
		d.beginPointerPress()
	}
	if d.paneMenu != nil {
		return d.handlePaneMenuEvent(mouse)
	}
	// An open away dialog owns the pointer, so no background menu opens
	// under it.
	if _, dashboard := d.mode.(ModeDashboard); inputMouseRightDown(mouse) && dashboard && d.awayDialog() == nil {
		// A right-clicked profile row offers its load-balancing menu.
		for _, area := range d.quotaRowAreas {
			if rectContains(area.Second, column, row) {
				d.beginQuotaRowMenu(area.First, uv.Pos(column, row))
				d.recordEventHandled()
				return DashboardActionNone{}
			}
		}
		for _, area := range d.workspaceTabAreas {
			if rectContains(area.Second, column, row) {
				d.beginWorkspaceRowMenu(area.First, uv.Pos(column, row))
				d.recordEventHandled()
				return DashboardActionNone{}
			}
		}
		// A right-clicked session row opens a small menu for that session,
		// anchored at the click.
		for _, area := range d.sessionRowAreas {
			if !rectContains(area.Second, column, row) {
				continue
			}
			if index := area.First; index >= 0 && index < len(d.sessionMenuIDs) {
				if id := d.sessionMenuIDs[index]; d.state.Get().Sessions.ContainsKey(id) {
					d.beginSessionRowMenu(id, uv.Pos(column, row))
					d.recordEventHandled()
					return DashboardActionNone{}
				}
			}
			break
		}
		if pane, ok := d.ChatRegionContains(column, row); ok {
			d.beginPaneMenu(pane)
			d.recordEventHandled()
			return DashboardActionNone{}
		}
	}
	if d.componentModalOpen() {
		return d.handleComponentEvent(mouse)
	}
	if _, dashboard := d.mode.(ModeDashboard); !dashboard {
		return DashboardActionNone{}
	}
	switch pointer := d.workspaceScrollbar.HandleMouse(mouse).(type) {
	case chat.ScrollbarPointerScrollTo:
		d.workspaceScroll = pointer.Offset
		d.recordEventHandled()
		return DashboardActionNone{}
	case chat.ScrollbarPointerConsumed:
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	if action, ok := d.handleSurfaceMouse(mouse); ok {
		return action
	}
	if inputMouseLeftDown(mouse) {
		if action := workspaceTabClick(d, column, row); action != nil {
			return action
		}

		if d.workspacePaneArea != nil && rectContains(*d.workspacePaneArea, column, row) {
			d.focus = FocusWorkspaces
			d.workspaceControlFocus = WorkspaceControlFocusTabs
			d.setSessionActionFocus(nil)
			d.recordEventHandled()
			return DashboardActionNone{}
		}
		for _, control := range d.paneSizeControlAreas {
			if !rectContains(control.area, column, row) {
				continue
			}
			if control.size == core.PaneSizeMaximized && !d.paneMaximizeEnabled(control.pane) {
				// Defend against stale geometry if a resize arrives before
				// the next frame redraws the visible controls.
				return DashboardActionNone{}
			}
			d.SetPaneSize(control.pane, control.size)
			d.recordEventHandled()
			return DashboardActionNone{}
		}
		for _, heading := range d.projectHeadingAreas {
			if rectContains(heading.Second, column, row) {
				projectKey := heading.First
				d.FocusSessions()
				d.toggleProject(projectKey)
				d.recordEventHandled()
				return DashboardActionNone{}
			}
		}
		for _, area := range d.sessionRowAreas {
			if rectContains(area.Second, column, row) {
				d.recordEventHandled()
				return d.handleRowClick(FocusSessions, area.First)
			}
		}
		// The click missed every row; forget any pending double click so a
		// stray click elsewhere can't pair up with the next row click.
		d.lastRowClick = nil
	}
	if d.notificationsArea != nil && rectContains(*d.notificationsArea, column, row) {
		switch {
		case inputMouseLeftDown(mouse):
			d.focus = FocusNotifications
		case inputMouseWheel(mouse, tea.MouseWheelUp):
			d.scrollNotification(-3)
		case inputMouseWheel(mouse, tea.MouseWheelDown):
			d.scrollNotification(3)
		default:
			return DashboardActionNone{}
		}
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	// Expanded workspaces scroll their rows; the collapsed selector keeps the
	// adjacent-workspace wheel navigation.
	wheelUp, wheelDown := inputMouseWheel(mouse, tea.MouseWheelUp), inputMouseWheel(mouse, tea.MouseWheelDown)
	if (wheelUp || wheelDown) && d.workspacePaneArea != nil && rectContains(*d.workspacePaneArea, column, row) {
		delta := 1
		if wheelUp {
			delta = -1
		}
		d.recordEventHandled()
		if d.workspaceDropdownArea != nil {
			return d.selectAdjacentWorkspace(delta)
		}
		d.scrollWorkspaces(delta)
		return DashboardActionNone{}
	}
	if inputMouseLeftDown(mouse) {
		var hit QuotaRow
		for _, area := range d.quotaPoolAreas {
			if rectContains(area.Second, column, row) {
				hit = area.First
				break
			}
		}
		if hit != nil {
			index := slices.IndexFunc(d.quotaRows(), func(current QuotaRow) bool { return current == hit })
			if index < 0 {
				return DashboardActionNone{}
			}
			d.focus = FocusQuota
			d.setSelectionFor(FocusQuota, index)
			if _, pool := d.selectedQuotaRow().(QuotaRowPool); pool {
				d.toggleQuotaPool()
			}
			d.recordEventHandled()
			return DashboardActionNone{}
		}
	}
	if d.paneAreas == nil {
		return DashboardActionNone{}
	}
	hovered, found := FocusSessions, false
	for index, area := range d.paneAreas {
		if rectContains(area, column, row) {
			hovered, found = [dashboardPaneCount]Focus{FocusSessions, FocusTargets, FocusQuota}[index], true
			break
		}
	}
	if !found {
		return DashboardActionNone{}
	}
	// Minimized Targets and Quota show no selected row. Their summary can
	// take focus so the pane-size key can restore it, but hidden rows do not
	// move or activate underneath the user.
	rowsVisible := hovered == FocusSessions
	if pane, ok := hovered.SupportPane(); ok && d.PaneSize(pane) != core.PaneSizeMinimized {
		rowsVisible = true
	}
	// A wheel or a press inside a pane is the dashboard's answer, even when
	// it produces no action: the frame the next pointer event is hit-tested
	// against has to be rebuilt after it.
	switch {
	// The Sessions wheel scrolls the list under a fixed selection; the
	// Targets list moves its selection instead; Quota scrolls by line.
	case wheelUp && rowsVisible:
		switch hovered {
		case FocusSessions:
			d.scrollSessionsView(-1)
		case FocusQuota:
			d.quotaScroll = max(d.quotaScroll-1, 0)
			if d.recenterOnSelection != nil && *d.recenterOnSelection == FocusQuota {
				d.recenterOnSelection = nil
			}
		default:
			d.scrollSelectionFor(hovered, -1)
		}
		d.recordEventHandled()
	case wheelDown && rowsVisible:
		switch hovered {
		case FocusSessions:
			d.scrollSessionsView(1)
		case FocusQuota:
			d.quotaScroll++
			if d.recenterOnSelection != nil && *d.recenterOnSelection == FocusQuota {
				d.recenterOnSelection = nil
			}
		default:
			d.scrollSelectionFor(hovered, 1)
		}
		d.recordEventHandled()
	case inputMouseLeftDown(mouse):
		d.focus = hovered
		if hovered != FocusSessions {
			d.setSessionActionFocus(nil)
		}
		d.clampSelections()
		d.recordEventHandled()
	}
	return DashboardActionNone{}
}

// handleRowClick selects the clicked row and, if it's the second click on
// the same row within doubleClickInterval, performs the same action Enter
// would.
func (d *DashboardState) handleRowClick(focus Focus, index int) DashboardAction {
	// Clicking a row selects it wherever the dial has left the pane.
	d.recenterOnSelection = nil
	d.focus = focus
	d.setSessionActionFocus(nil)
	if focus == FocusSessions {
		if sessions := d.orderedSessions(); index >= 0 && index < len(sessions) {
			d.SelectActiveSession(sessions[index].ID)
		}
	} else {
		d.setSelectionFor(focus, index)
	}
	now := time.Now()
	last := d.lastRowClick
	isDoubleClick := last != nil && last.focus == focus && last.index == index &&
		max(now.Sub(last.at), 0) <= doubleClickInterval
	if isDoubleClick {
		d.lastRowClick = nil
		return d.openSelectedSession()
	}
	d.lastRowClick = &rowClick{focus: focus, index: index, at: now}
	return DashboardActionNone{}
}

// handleDashboardKey handles keys for the combined surface's panes.
//
// The composer is a separate focus and never reaches here, so the pane
// actions are plain letters rather than accelerated ones: no key typed at a
// pane can be mistaken for text.
//
// Everything that runs a named command is looked up in the action registry
// (actions.go) rather than matched here, so the keys, the footer, and the
// command palette all read one table. What stays as hand-written arms is
// the input that is not a command: list navigation, and the two keys whose
// meaning depends on state.
func (d *DashboardState) handleDashboardKey(key tea.KeyMsg) DashboardAction {
	press, ok := key.(tea.KeyPressMsg)
	if !ok {
		return DashboardActionNone{}
	}
	if action := d.nativeAgentKey(press); action != nil {
		return action
	}
	if action := d.stoppedSubagentKey(press); action != nil {
		return action
	}
	if action := d.handleNotificationKey(press); action != nil {
		return action
	}
	command := dashboardAccelerator(press.Mod)
	plain := press.Mod&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) == 0
	if action := d.handleWorkspacePaneKey(press); action != nil {
		return action
	}
	// The Sessions filter takes the keys it is editing with, and its state
	// letters, before anything else can read them as navigation. The action
	// row normally keeps the plain letters, but a filter that hides every row
	// moves the focus there, and the letters are then the only way back to
	// the sessions — including the Esc the empty pane advertises.
	filterOwnsLetters := d.sessionActionFocus == nil || *d.sessionsFilter.Get() != nil
	if d.focus == FocusSessions && d.handleSessionsFilterKey(press, plain && filterOwnsLetters) {
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	code := chat.KeyPressCode(press)
	switch {
	// Shift-Tab is the reverse of the registry's Tab.
	case code == tea.KeyTab && press.Mod.Contains(tea.ModShift):
		d.CycleFocus(true)
		d.recordEventHandled()
		return DashboardActionNone{}
	// Escape belongs to the composer and to modals. On a pane it only clears
	// the notice bar: the combined surface is quit with the detach key, and a
	// stray Escape must never take the whole screen away. In a child
	// workspace, Escape and p return to the parent like the X on the
	// workspace strip.
	case (code == tea.KeyEscape || (code == 'p' && !command)) &&
		plain && d.focus == FocusSessions && *d.childWorkspace.Get() != nil:
		d.recordEventHandled()
		return DashboardActionExitSubagentWorkspace{}
	case code == tea.KeyEscape:
		d.notices.Clear()
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	// The standby composer answers the same keys the focused prompt would —
	// the full readline set — before list navigation can claim the arrows.
	if action := d.handleStandbyPromptKey(press); action != nil {
		return action
	}
	if plain && d.focus == FocusSessions {
		if action := d.handleSessionActionKey(press); action != nil {
			d.recordEventHandled()
			return action
		}
	}
	if plain && d.focus == FocusSessions && code == tea.KeyRight {
		if _, ready := spec(CommandIdOpenSubagents).available(d).(AvailabilityReady); ready {
			d.recordEventHandled()
			return d.DispatchCommand(CommandIdOpenSubagents)
		}
	}
	// List navigation, shared by visible lists. It comes before the registry
	// so `j`, `k`, Ctrl-N, and Ctrl-P keep moving the selection.
	if d.focus != FocusNotifications && d.focusedRowsVisible() {
		switch {
		case ((code == tea.KeyUp || code == 'k') && !command) || (code == 'p' && command):
			d.moveSelection(-1)
			d.recordEventHandled()
			return DashboardActionNone{}
		case code == tea.KeyPgUp && !command && d.focus == FocusQuota:
			d.moveSelection(-1)
			d.recordEventHandled()
			return DashboardActionNone{}
		case ((code == tea.KeyDown || code == 'j') && !command) || (code == 'n' && command):
			d.moveSelection(1)
			d.recordEventHandled()
			return DashboardActionNone{}
		case code == tea.KeyPgDown && !command && d.focus == FocusQuota:
			d.moveSelection(1)
			d.recordEventHandled()
			return DashboardActionNone{}
		// `ctrl+d` and `ctrl+u` page by half a screen, as they do in every
		// other list on the surface.
		case code == 'u' && command:
			d.moveSelection(-8)
			d.recordEventHandled()
			return DashboardActionNone{}
		case code == 'd' && command:
			d.moveSelection(8)
			d.recordEventHandled()
			return DashboardActionNone{}
		case code == tea.KeyHome:
			d.setSelectionFor(d.focus, 0)
			d.recordEventHandled()
			return DashboardActionNone{}
		case code == tea.KeyEnd || (code == 'G' && !command):
			length := d.focusLenFor(d.focus)
			d.setSelectionFor(d.focus, max(length-1, 0))
			d.recordEventHandled()
			return DashboardActionNone{}
		}
	}
	// Setup and the session editor never both apply: setup only opens while
	// the config is empty, and an empty config has no sessions. The registry
	// cannot resolve this on the key alone, because `e` is also the
	// Sessions, Targets, and Quota panes' key, so the ambiguity is settled
	// here and ScopeSetup is left out of paneCommandForKey.
	if plain && code == 'e' && d.configIsEmpty() {
		action := d.DispatchCommand(CommandIdOpenConfig)
		d.recordEventHandled()
		return action
	}
	// A digit picks a project by its number, and a registry command carries
	// no argument, so this one stays a hand-written arm.
	if d.focus == FocusSessions && plain && code >= '1' && code <= '9' {
		d.toggleProjectNumber(int(code - '0'))
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	if id, ok := paneCommandForKey(press, d.focus); ok {
		action := d.DispatchCommand(id)
		d.recordEventHandled()
		return action
	}
	return DashboardActionNone{}
}

// handleSessionActionKey handles the small action row at the top of
// Sessions. The row is a second selection target within the pane: Up from
// its first session enters it, Down returns to the first session, and
// Left/Right skip any disabled action. nil means the regular dashboard key
// handling still owns the key; an action (DashboardActionNone included)
// means the key was consumed, including a no-op.
func (d *DashboardState) handleSessionActionKey(key tea.KeyMsg) DashboardAction {
	press, ok := key.(tea.KeyPressMsg)
	if !ok {
		return nil
	}
	code := chat.KeyPressCode(press)
	sessionCount := len(d.visibleSessionIndices())
	focusedAction := d.sessionActionFocus
	if focusedAction != nil {
		id := *focusedAction
		switch {
		case code == tea.KeyLeft || code == tea.KeyRight:
			if next, ok := adjacentEnabledSessionAction(d, id, code == tea.KeyRight); ok {
				d.sessionActionFocus = &next
			}
			return DashboardActionNone{}
		case code == tea.KeyUp:
			return DashboardActionNone{}
		case code == tea.KeyDown && sessionCount > 0:
			d.setSessionActionFocus(nil)
			d.setSelectionFor(FocusSessions, 0)
			return DashboardActionNone{}
		case code == tea.KeyDown:
			return DashboardActionNone{}
		case code == tea.KeyEnter:
			d.setSessionActionFocus(nil)
			return d.runAvailableCommand(id)
		}
		return nil
	}
	firstEnabled := func() *CommandId {
		if first, ok := firstEnabledSessionAction(d); ok {
			return &first
		}
		return nil
	}
	selected, hasSelected := d.selectedVisibleIndex()
	switch {
	case code == tea.KeyUp && (sessionCount == 0 || (hasSelected && selected == 0)):
		d.sessionActionFocus = firstEnabled()
		return DashboardActionNone{}
	case code == tea.KeyDown && sessionCount == 0:
		d.sessionActionFocus = firstEnabled()
		return DashboardActionNone{}
	case (code == tea.KeyLeft || code == tea.KeyRight) && sessionCount == 0:
		first := firstEnabled()
		if code == tea.KeyRight || first == nil {
			d.sessionActionFocus = first
		} else if previous, ok := adjacentEnabledSessionAction(d, *first, false); ok {
			d.sessionActionFocus = &previous
		} else {
			d.sessionActionFocus = first
		}
		return DashboardActionNone{}
	}
	return nil
}

// openSelectedSession opens the selected session's conversation and hands
// the keyboard to its composer.
//
// The conversation already follows the selection, so Enter's job is to take
// the user to the prompt for the row they are on. A failed session is the
// one exception: it asks first, because reading what it did and putting it
// back on a fresh target are both reasonable answers to the same key. That
// is a prompt rather than the silent diversion into the resume wizard this
// used to do - the row is red, and the dialog says what failed.
func (d *DashboardState) openSelectedSession() DashboardAction {
	session := d.selectedSession()
	if session == nil {
		return DashboardActionNone{}
	}
	// A stopped sub-agent is read, not resumed: its parent owns it.
	if d.IsNativeAgent(session.ID) || d.IsStoppedSubagent(session.ID) {
		return DashboardActionOpen{SessionID: session.ID}
	}
	if issue, ok := session.ConfigurationIssue(d.config.Get()); ok {
		dialog := d.confirmDialog(ConfirmationConfigurationRepair{
			SessionID: session.ID,
			Error:     issue,
			Previous:  d.mode,
		})
		d.mode = ModeConfirm{&dialog}
		return DashboardActionNone{}
	}
	// Explicit Open can enter the selected session's standby composer while
	// its worker is starting, resuming, moving, or attaching.
	if _, ok := d.standbyPromptSession(); ok {
		d.FocusPrompt()
		return DashboardActionNone{}
	}
	if operation, ok := (*d.sessionOperations.Get())[session.ID]; ok {
		label := operation.kind.Label()
		if cancel, ok := d.FirstKeyLabel(CommandIdCancelOperation); ok {
			d.notices.Set(fmt.Sprintf("%s is in progress; press %s to cancel it.", label, cancel))
		} else {
			d.notices.Set(fmt.Sprintf("%s is in progress.", label))
		}
		return DashboardActionNone{}
	}
	recoverFailed := func(recoverable, retryLaunch bool) DashboardAction {
		dialog := newConfirmDialog(ConfirmationRecoverFailed{
			SessionID:   session.ID,
			Error:       session.LastError,
			Recoverable: recoverable,
			RetryLaunch: retryLaunch,
		}).namingSession(sessionName(session))
		d.mode = ModeConfirm{&dialog}
		return DashboardActionNone{}
	}
	if _, ok := d.failedLaunchRetries[session.ID]; ok {
		return recoverFailed(true, true)
	}
	if transition := d.TransitionKind(session.ID); transition != nil {
		d.notices.Set(fmt.Sprintf("%s is in progress; select another session while it completes.", transition.Label()))
		return DashboardActionNone{}
	}
	if d.TransitionFailureKind(session.ID) != nil {
		return recoverFailed(session.Checkpoint != nil || session.Target != nil, false)
	}
	if operation, ok := d.moveOperations.Get(session.ID); ok && operation.OffersRetry() {
		dialog := newConfirmDialog(ConfirmationRecoverMove{Operation: operation})
		d.mode = ModeConfirm{&dialog}
		return DashboardActionNone{}
	}
	// A stopped row is listed only while its failure needs a person; Enter
	// resumes it, unless the dialog would refuse the same record.
	if !session.State.IsActive() {
		if reason, ok := unresumableReason(session.State); ok {
			d.notices.Set(unresumableNotice(reason))
			return DashboardActionNone{}
		}
		return d.BeginResumeFor(session.ID)
	}
	// Failed sessions can be inspected or resumed in their retained
	// environment.
	if session.State == core.SessionStateError {
		return recoverFailed(session.Checkpoint != nil || session.Target != nil, false)
	}
	sessionID := session.ID
	d.FocusPrompt()
	return DashboardActionOpen{SessionID: sessionID}
}
