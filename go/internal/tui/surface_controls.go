// Port of mj-tui/src/surface_controls.rs.

// Visible entry points for the same commands the keyboard dispatches.

package tui

import (
	"fmt"
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// SurfaceControl is one clickable control of the dashboard's shared
// surface form. Every variant is comparable, so a SurfaceControl is a form
// key.
type SurfaceControl interface {
	isSurfaceControl()
}

// SurfaceControlCommand runs a registry command.
type SurfaceControlCommand CommandId

// SurfaceControlNotification is a notification card's action.
type SurfaceControlNotification CommandId

// SurfaceControlFooter is a footer hint that runs its command.
type SurfaceControlFooter CommandId

// SurfaceControlSession is a session row's menu, by row index.
type SurfaceControlSession int

// SurfaceControlClosePane is the close chip on one conversation pane's
// title row.
type SurfaceControlClosePane PaneId

// SurfaceControlPaneMenu is the menu chip on one conversation pane's title
// row.
type SurfaceControlPaneMenu PaneId

// SurfaceControlPaneTitleMenu is a support pane's title, which opens the
// menu that hangs from it.
type SurfaceControlPaneTitleMenu SupportPane

// SurfaceControlClearSessionsFilter is the × after the filter label on the
// Sessions title, which drops the filter as Esc on the pane does.
type SurfaceControlClearSessionsFilter struct{}

// SurfaceControlSessionsFilterInput is the Filter: input on the Sessions
// action row; a click starts typing.
type SurfaceControlSessionsFilterInput struct{}

// SurfaceControlMoreWorkspaceMatches is the "N more" button that opens the
// matches in other workspaces.
type SurfaceControlMoreWorkspaceMatches struct{}

// SurfaceControlPanePin is the pin chip on one conversation pane's title
// row.
type SurfaceControlPanePin PaneId

// SurfaceControlPinHere is "Pin selected here" inside an empty pane.
type SurfaceControlPinHere PaneId

// SurfaceControlSessionPin is the pin badge on a session row, by row index.
type SurfaceControlSessionPin int

// SurfaceControlWorkspaceMenu is the pinned workspace manager control.
type SurfaceControlWorkspaceMenu struct{}

// SurfaceControlCapacityDisks is the Disks summary on one Targets row, by
// row index; a click opens its filesystem details dropdown.
type SurfaceControlCapacityDisks int

func (SurfaceControlCommand) isSurfaceControl()              {}
func (SurfaceControlNotification) isSurfaceControl()         {}
func (SurfaceControlFooter) isSurfaceControl()               {}
func (SurfaceControlSession) isSurfaceControl()              {}
func (SurfaceControlClosePane) isSurfaceControl()            {}
func (SurfaceControlPaneMenu) isSurfaceControl()             {}
func (SurfaceControlPaneTitleMenu) isSurfaceControl()        {}
func (SurfaceControlClearSessionsFilter) isSurfaceControl()  {}
func (SurfaceControlSessionsFilterInput) isSurfaceControl()  {}
func (SurfaceControlMoreWorkspaceMatches) isSurfaceControl() {}
func (SurfaceControlPanePin) isSurfaceControl()              {}
func (SurfaceControlPinHere) isSurfaceControl()              {}
func (SurfaceControlSessionPin) isSurfaceControl()           {}
func (SurfaceControlWorkspaceMenu) isSurfaceControl()        {}
func (SurfaceControlCapacityDisks) isSurfaceControl()        {}

// sessionAction is one button of the Sessions action row: its command and
// label.
type sessionAction struct {
	id    CommandId
	label string
}

var sessionActions = [2]sessionAction{
	{CommandIdNewSessionWizard, "Create"},
	{CommandIdResumeDialog, "Open"},
}

// sessionActionsOf is the action row's buttons. (Rust's session_actions,
// renamed because the constant holds that name.)
func sessionActionsOf(dashboard *DashboardState) [2]sessionAction {
	if dashboard.goMode != nil {
		return [2]sessionAction{
			{CommandIdNewSessionWizard, "New"},
			{CommandIdPalette, "Menu"},
		}
	}
	return sessionActions
}

func sessionActionEnabled(dashboard *DashboardState, id CommandId) bool {
	_, ready := spec(id).available(dashboard).(AvailabilityReady)
	return ready
}

func firstEnabledSessionAction(dashboard *DashboardState) (CommandId, bool) {
	for _, action := range sessionActionsOf(dashboard) {
		if sessionActionEnabled(dashboard, action.id) {
			return action.id, true
		}
	}
	return 0, false
}

func adjacentEnabledSessionAction(dashboard *DashboardState, current CommandId, forward bool) (CommandId, bool) {
	actions := sessionActionsOf(dashboard)
	index := -1
	for i, action := range actions {
		if action.id == current {
			index = i
			break
		}
	}
	if index < 0 {
		return 0, false
	}
	count := len(sessionActions)
	for step := 1; step < count; step++ {
		next := (index + step) % count
		if !forward {
			next = (index + count - step) % count
		}
		if id := actions[next].id; sessionActionEnabled(dashboard, id) {
			return id, true
		}
	}
	return 0, false
}

func (d *DashboardState) beginSurfaceFrame() {
	sessions := []string{}
	for _, session := range d.orderedSessions() {
		sessions = append(sessions, session.ID)
	}
	if !slices.Equal(d.sessionMenuIDs, sessions) || d.ModalOpen() {
		d.surfaceForm.CancelPointer()
	}
	d.sessionMenuIDs = sessions
	if focus := d.sessionActionFocus; focus != nil && !sessionActionEnabled(d, *focus) {
		d.sessionActionFocus = nil
		if id, ok := firstEnabledSessionAction(d); ok {
			d.sessionActionFocus = &id
		}
	}
	if d.Focus() == FocusSessions && !d.ModalOpen() && len(d.visibleSessionIndices()) == 0 && d.sessionActionFocus == nil {
		if id, ok := firstEnabledSessionAction(d); ok {
			d.sessionActionFocus = &id
		}
	}
	d.surfaceForm.BeginFrame()
}

func (d *DashboardState) endSurfaceFrame() {
	d.surfaceForm.EndFrame(SurfaceControlCommand(CommandIdPalette))
}

// runAvailableCommand runs a command whose eligibility is checked again at
// release, after any background updates.
func (d *DashboardState) runAvailableCommand(id CommandId) DashboardAction {
	switch availability := spec(id).available(d).(type) {
	case AvailabilityReady:
		return d.DispatchCommand(id)
	case AvailabilityBlocked:
		d.SetNotice(availability.Reason)
		return DashboardActionNone{}
	default:
		return DashboardActionNone{}
	}
}

func (d *DashboardState) handleSurfaceMouse(mouse tea.MouseMsg) (DashboardAction, bool) {
	position := mouse.Mouse()
	_, press := mouse.(tea.MouseClickMsg)
	leftPress := press && position.Button == tea.MouseLeft
	workspaceMenuHit := leftPress && d.workspaceHamburgerArea != nil &&
		rectContains(*d.workspaceHamburgerArea, position.X, position.Y)
	result := d.surfaceForm.Handle(mouse)
	d.lastEventConsumed = result.Consumed
	if leftPress {
		d.notifications.gesture = nil
		for _, gesture := range d.notifications.controls {
			if d.surfaceForm.IsArmed(SurfaceControlNotification(gesture.command)) {
				found := gesture
				d.notifications.gesture = &found
				break
			}
		}
	}
	if workspaceMenuHit && d.surfaceForm.IsFocused(SurfaceControlWorkspaceMenu{}) {
		d.focus = FocusWorkspaces
		d.workspaceControlFocus = WorkspaceControlFocusMenu
		d.setSessionActionFocus(nil)
	}
	if activate, ok := result.Action.(chat.InteractionActivate[SurfaceControl]); ok {
		d.lastRowClick = nil
		return d.activateSurfaceControl(activate.ID), true
	}
	if result.Consumed {
		return DashboardActionNone{}, true
	}
	return nil, false
}

// activateSurfaceControl runs the control a completed click landed on.
func (d *DashboardState) activateSurfaceControl(control SurfaceControl) DashboardAction {
	switch control := control.(type) {
	case SurfaceControlNotification:
		gesture := d.notifications.gesture
		d.notifications.gesture = nil
		if gesture == nil || gesture.command != CommandId(control) {
			return DashboardActionNone{}
		}
		return d.activateNotification(*gesture)
	case SurfaceControlCommand:
		return d.runAvailableCommand(CommandId(control))
	case SurfaceControlFooter:
		return d.runAvailableCommand(CommandId(control))
	case SurfaceControlSession:
		index := int(control)
		if index >= 0 && index < len(d.sessionMenuIDs) {
			if id := d.sessionMenuIDs[index]; d.state.Get().Sessions.ContainsKey(id) {
				d.beginSessionPaletteFor(id)
			}
		}
		return DashboardActionNone{}
	case SurfaceControlClosePane:
		return DashboardActionClosePane{Pane: PaneId(control)}
	case SurfaceControlPaneMenu:
		d.beginPaneMenu(PaneId(control))
		return DashboardActionNone{}
	case SurfaceControlPaneTitleMenu:
		d.beginSupportPaneMenu(SupportPane(control))
		return DashboardActionNone{}
	case SurfaceControlClearSessionsFilter:
		d.clearSessionsFilter()
		return DashboardActionNone{}
	case SurfaceControlMoreWorkspaceMatches:
		if d.sessionsOtherWorkspaceMatches() > 0 {
			filter := *d.sessionsFilter.Get()
			if filter == nil {
				panic("active filter")
			}
			return DashboardActionOpenFilteredResumeDialog{Query: filter.Query.Value(), State: filter.State}
		}
		return DashboardActionNone{}
	case SurfaceControlSessionsFilterInput:
		d.beginSessionsFilter()
		return DashboardActionNone{}
	case SurfaceControlPanePin:
		if id, ok := d.PaneSession(PaneId(control)); ok {
			return d.toggleSessionPin(id)
		}
		d.beginPaneMenu(PaneId(control))
		return DashboardActionNone{}
	case SurfaceControlPinHere:
		if sessionID, ok := d.SelectedSessionID(); ok {
			return DashboardActionPinSession{SessionID: sessionID, Pane: PaneId(control)}
		}
		return DashboardActionNone{}
	case SurfaceControlSessionPin:
		index := int(control)
		if index >= 0 && index < len(d.sessionMenuIDs) {
			return d.toggleSessionPin(d.sessionMenuIDs[index])
		}
		return DashboardActionNone{}
	case SurfaceControlWorkspaceMenu:
		d.focus = FocusWorkspaces
		d.workspaceControlFocus = WorkspaceControlFocusMenu
		d.setSessionActionFocus(nil)
		return d.runAvailableCommand(CommandIdWorkspaces)
	case SurfaceControlCapacityDisks:
		if index := int(control); index < len(d.capacityDetails) {
			d.beginCapacityDisksMenu(index)
		}
		return DashboardActionNone{}
	default:
		return DashboardActionNone{}
	}
}

// paneTitleLabel is the text of a support pane's title: its name, and the
// dropdown mark when the title opens a menu.
func paneTitleLabel(name string, menu bool) string {
	if menu {
		return fmt.Sprintf(" %s %s ", name, chat.ThemeGlyphs().Dropdown)
	}
	return " " + name + " "
}

// registerPaneTitleMenu registers a support pane's title, drawn at area, as
// the button that opens its menu, and says whether the pointer is holding
// it down so the title can be drawn pressed.
func registerPaneTitleMenu(dashboard *DashboardState, pane SupportPane, area uv.Rectangle) bool {
	if area.Dx() == 0 || area.Dy() == 0 {
		return false
	}
	control := SurfaceControlPaneTitleMenu(pane)
	dashboard.surfaceForm.Register(control, chat.ControlKindButton{}, area, true)
	return dashboard.surfaceForm.IsArmed(control)
}

// renderSessionsFilterClear registers the clear chip that ends a filtered
// Sessions title, drawn at area as part of the title, and draws it pressed
// while the pointer holds it down.
func renderSessionsFilterClear(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	if area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	control := SurfaceControlClearSessionsFilter{}
	dashboard.surfaceForm.Register(control, chat.ControlKindButton{}, area, true)
	if dashboard.surfaceForm.IsArmed(control) {
		frame.RenderWidget(chat.NewParagraph(chat.RawText(chat.ThemeGlyphs().Close)).WithStyle(chat.Selection(true)), area)
	}
}

// renderWorkspaceMenu draws the pinned three-cell workspace manager
// control. It is registered in the shared surface form so mouse presses are
// armed and released safely even when the pointer leaves the button between
// events.
func renderWorkspaceMenu(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	if area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	control := SurfaceControlWorkspaceMenu{}
	dashboard.surfaceForm.Register(control, chat.ControlKindButton{}, area, true)
	focused := dashboard.Focus() == FocusWorkspaces && dashboard.workspaceControlFocus == WorkspaceControlFocusMenu
	style := chat.Actionable().Patch(chat.Raised())
	if focused || dashboard.surfaceForm.IsArmed(control) {
		style = chat.FocusControl()
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(chat.ThemeGlyphs().WorkspaceMenu)).WithStyle(style), area)
}

func renderSessionButtons(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	form := &dashboard.surfaceForm
	if area.Dx() == 0 || area.Dy() == 0 {
		return
	}
	count := dashboard.sessionsOtherWorkspaceMatches()
	commands := sessionActionsOf(dashboard)
	regularWidth := len(fmt.Sprintf(" %d more ", count))
	for _, command := range commands {
		regularWidth += len(command.label) + 3
	}
	if !dashboard.SessionsMinimized() {
		regularWidth += len(filterLabel) + filterFieldMin + 2
	}
	compact := count > 0 && area.Dx() < regularWidth
	padding := " "
	if compact {
		padding = ""
	}
	moreLabel := fmt.Sprintf("%s%d more%s", padding, count, padding)
	moreWidth := chat.RawLine(moreLabel).Width()
	var moreArea *uv.Rectangle
	if count > 0 && area.Dx() >= moreWidth {
		rect := uv.Rect(area.Max.X-moreWidth, area.Min.Y, moreWidth, 1)
		moreArea = &rect
	}
	buttonsRight := area.Max.X
	if moreArea != nil {
		buttonsRight = max(moreArea.Min.X-1, 0)
	}
	x := area.Min.X
	for _, command := range commands {
		label := command.label
		if compact && command.id == CommandIdNewSessionWizard {
			label = "New"
		}
		label = padding + label + padding
		width := chat.RawLine(label).Width()
		if x+width > buttonsRight {
			break
		}
		rect := uv.Rect(x, area.Min.Y, width, 1)
		control := SurfaceControlCommand(command.id)
		enabled := sessionActionEnabled(dashboard, command.id)
		form.Register(control, chat.ControlKindButton{}, rect, enabled)
		focused := dashboard.Focus() == FocusSessions && dashboard.sessionActionFocus != nil &&
			*dashboard.sessionActionFocus == command.id && !dashboard.ModalOpen()
		var style chat.Style
		switch {
		case enabled && (focused || form.IsArmed(control)):
			style = chat.FocusControl()
		case enabled && command.id == CommandIdNewSessionWizard && !chat.ReverseVideo():
			// Without painted surfaces, bold reverse video is reserved for
			// actual keyboard focus.
			style = chat.ActiveControl()
		case enabled:
			style = chat.Actionable().Patch(chat.Raised())
		default:
			style = chat.Muted().Patch(chat.Raised())
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(label)).WithStyle(style), rect)
		x += width + 1
	}
	inputRow := area
	if moreArea != nil {
		control := SurfaceControlMoreWorkspaceMatches{}
		form.Register(control, chat.ControlKindButton{}, *moreArea, true)
		style := chat.Actionable().Patch(chat.Raised())
		if form.IsArmed(control) {
			style = chat.FocusControl()
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(moreLabel)).WithStyle(style), *moreArea)
		inputRow = uv.Rect(area.Min.X, area.Min.Y, max(moreArea.Min.X-(area.Min.X+1), 0), area.Dy())
	}
	renderSessionsFilterInput(frame, inputRow, x, dashboard)
}

// filterLabel is the label before the Sessions filter's text field.
const filterLabel = "Filter: "

// filterFieldMin is the least room the text field is worth drawing in;
// narrower rows leave the input out rather than show a field nobody can
// type a word into.
const filterFieldMin = 4

// filterFieldMax is the widest the text field grows, so a wide pane keeps
// the buttons and the input together instead of spreading them across the
// row.
const filterFieldMax = 24

// filterInputArea is where the Filter: input sits in the Sessions action
// row: the label and field start at start, the first cell after the
// buttons. False when the pane is in its most compact view, or the row has
// no room left.
func filterInputArea(dashboard *DashboardState, row uv.Rectangle, start int) (uv.Rectangle, bool) {
	if dashboard.SessionsMinimized() {
		return uv.Rectangle{}, false
	}
	left := start + 1
	width := max(row.Max.X-left, 0)
	label := len(filterLabel)
	if width < label+filterFieldMin {
		return uv.Rectangle{}, false
	}
	return uv.Rect(left, row.Min.Y, min(width, label+filterFieldMax), 1), true
}

// renderSessionsFilterInput draws the Filter: ____ input to the right of
// the buttons. It shows and edits the same text the / key does, with the
// readline caret drawn as a reversed cell while it is being edited. The
// clear chip lives on the pane title, not here.
func renderSessionsFilterInput(frame *chat.Frame, row uv.Rectangle, start int, dashboard *DashboardState) {
	area, ok := filterInputArea(dashboard, row, start)
	if !ok {
		return
	}
	form := &dashboard.surfaceForm
	filter := *dashboard.sessionsFilter.Get()
	editing := filter != nil && filter.Editing
	room := max(area.Dx()-len(filterLabel), 0)
	form.Register(SurfaceControlSessionsFilterInput{}, chat.ControlKindButton{}, area, true)
	// The label stays put; the field scrolls to keep the cursor in view and
	// pads with underscores so the box shows its extent. Outside editing the
	// caret is not drawn and the end of the text shows, as before.
	value, cursor := "", 0
	if filter != nil {
		value = filter.Query.Value()
		cursor = len(value)
		if editing {
			cursor = filter.Query.Cursor()
		}
	}
	window := room
	if !editing {
		window++
	}
	before, caret, after := chat.TextFieldCaretWindow(value, cursor, window)
	if !editing {
		caret, after = "", ""
	}
	used := chat.RawLine(before + caret + after).Width()
	spans := []chat.Span{
		chat.StyledSpan(filterLabel, chat.Muted()),
		chat.StyledSpan(before, chat.Actionable()),
	}
	if editing {
		spans = append(spans,
			chat.StyledSpan(caret, chat.FocusControl()),
			chat.StyledSpan(after, chat.Actionable()),
		)
	}
	spans = append(spans, chat.StyledSpan(strings.Repeat("_", max(room-used, 0)), chat.Muted()))
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(chat.LineFrom(spans...))), area)
}

func renderSessionRowActions(frame *chat.Frame, dashboard *DashboardState) {
	form := &dashboard.surfaceForm
	for _, entry := range dashboard.sessionRowAreas {
		index, row := entry.First, entry.Second
		width := row.Dx()
		if width < 5 || row.Dy() == 0 {
			continue
		}
		if width >= 10 {
			pinArea := uv.Rect(row.Max.X-5, row.Min.Y, 2, 1)
			alignment := chat.AlignmentRight
			if width < 24 {
				pinArea = uv.Rect(row.Max.X-3, row.Min.Y, 3, 1)
				// Beside the menu, the badge sits against the menu's leading
				// space so one space separates the two whatever the badge's
				// width.
				alignment = chat.AlignmentLeft
			}
			control := SurfaceControlSessionPin(index)
			form.Register(control, chat.ControlKindButton{}, pinArea, true)
			var pin *uint32
			if index >= 0 && index < len(dashboard.sessionMenuIDs) {
				if id, ok := dashboard.PinID(dashboard.sessionMenuIDs[index]); ok {
					pin = &id
				}
			}
			text, style := sessionPinBadge(pin)
			frame.RenderWidget(chat.NewParagraph(chat.RawText(text)).WithStyle(style).WithAlignment(alignment), pinArea)
		}
		if width < 24 {
			continue
		}
		area := uv.Rect(max(row.Max.X-3, 0), row.Min.Y, min(3, width), 1)
		id := SurfaceControlSession(index)
		form.Register(id, chat.ControlKindButton{}, area, true)
		style := chat.Actionable()
		if form.IsArmed(id) {
			style = chat.Selection(true)
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(chat.ThemeGlyphs().RowMenu)).WithStyle(style), area)
	}
}

// sessionPinBadge is the pin badge at a session row's right edge: the pin
// glyph for an unpinned session, or the session's pin label in its pin
// colour.
func sessionPinBadge(pin *uint32) (string, chat.Style) {
	if pin == nil {
		return chat.ThemeGlyphs().Pin, chat.Actionable()
	}
	id := *pin
	text := chat.PinLabel(id)
	if id < 26 {
		text = chat.ThemeGlyphs().Pinned + text
	}
	return text, chat.Style{}.Foreground(chat.PinColor(id))
}

// sessionTitleWidth is the cells of a session row's first line left of the
// controls that renderSessionRowActions draws over its right edge. A wide
// row keeps one space between its text and the pin badge.
func sessionTitleWidth(rowWidth int, pin *uint32) int {
	if rowWidth < 24 {
		return max(rowWidth-3, 0)
	}
	badge, _ := sessionPinBadge(pin)
	return max(rowWidth-(3+chat.RawLine(badge).Width()+1), 0)
}

// paneCloseControlReserve is the width a pane title has to leave clear for
// renderPaneCloseControl.
const paneCloseControlReserve = 4

// renderPaneCloseControl draws the three-cell close chip on one
// conversation pane's title row, in the same right-edge column the support
// panes put their size controls in.
func renderPaneCloseControl(frame *chat.Frame, dashboard *DashboardState, transcript uv.Rectangle, pane PaneId) {
	if pane == dashboard.BrowsePane() || transcript.Dx() < paneCloseControlReserve+2 || transcript.Dy() == 0 {
		return
	}
	area := uv.Rect(max(transcript.Max.X-paneCloseControlReserve, 0), transcript.Min.Y, 3, 1)
	control := SurfaceControlClosePane(pane)
	form := &dashboard.surfaceForm
	form.Register(control, chat.ControlKindButton{}, area, pane != dashboard.BrowsePane())
	style := chat.Actionable()
	if form.IsArmed(control) {
		style = chat.Selection(true)
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(chat.ThemeGlyphs().Close)).WithStyle(style), area)
}

// paneZoomControlReserve is the width a pane title leaves clear for
// renderPaneZoomControl, on top of the close chip's own reserve.
const paneZoomControlReserve = 3

// renderPaneZoomControl draws the zoom chip on the zoomed pane's title row,
// left of the close chip. Clicking it runs the same command the key does,
// which unzooms.
func renderPaneZoomControl(frame *chat.Frame, dashboard *DashboardState, transcript uv.Rectangle) {
	reserve := paneCloseControlReserve + paneZoomControlReserve
	if transcript.Dx() < reserve+2 || transcript.Dy() == 0 {
		return
	}
	area := uv.Rect(max(transcript.Max.X-reserve, 0), transcript.Min.Y, 3, 1)
	control := SurfaceControlCommand(CommandIdZoomPane)
	form := &dashboard.surfaceForm
	form.Register(control, chat.ControlKindButton{}, area, true)
	style := chat.Actionable()
	if form.IsArmed(control) {
		style = chat.Selection(true)
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(" Z ")).WithStyle(style), area)
}

func renderFooterCommand(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, id CommandId, text string) {
	control := SurfaceControlFooter(id)
	dashboard.surfaceForm.Register(control, chat.ControlKindButton{}, area, true)
	line := chat.Hints(text)
	if dashboard.surfaceForm.IsArmed(control) {
		line = chat.StyledLine(text, chat.Selection(true))
	}
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), area)
}
