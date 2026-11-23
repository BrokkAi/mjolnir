// Port of mj-tui/src/component_events.rs.
//
// Routes terminal input to the active reusable form before background panes.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// RouteQuestionPress lets a press on a question's answer rows still reach
// the form, which acts on release, while the selection engine tracks it too
// so a drag can select the drawn text instead. It returns the event to hand
// to the form, or nil when the caller routes it as usual.
func (d *DashboardState) RouteQuestionPress(selection *chat.SelectionState, chatOwnsPointer bool, event tea.Msg) tea.Msg {
	panic("unported: mj-tui/src/component_events.rs DashboardState::route_question_press")
}

// SettleQuestionPress releases the form's armed control once the press
// became a text selection, or the gesture ended without reaching the form:
// the form gets no click then.
func (d *DashboardState) SettleQuestionPress(selection *chat.SelectionState, chats map[string]*chat.ActiveChat) {
	if d.questionPress == nil {
		return
	}
	if _, selecting := selection.Range(); selecting || !selection.OwnsPointer() {
		d.questionPress = nil
		d.CancelAllComponentPointers(chats)
	}
}

// CancelAllComponentPointers disarms every chat presentation, including warm
// forms whose session differs from the focused conversation, as global
// commands require.
func (d *DashboardState) CancelAllComponentPointers(chats map[string]*chat.ActiveChat) {
	d.CancelComponentPointer()
	for _, active := range chats {
		active.CancelComponentPointer()
	}
}

// PrepareComponentEvent broadcasts focus loss to every presentation,
// including warm cards outside the focused conversation.
func (d *DashboardState) PrepareComponentEvent(event tea.Msg, chats map[string]*chat.ActiveChat) {
	if _, blurred := event.(tea.BlurMsg); blurred {
		d.CancelAllComponentPointers(chats)
	}
}

func (d *DashboardState) dialogLayerKey() string {
	panic("unported: mj-tui/src/component_events.rs DashboardState::dialog_layer_key")
}

func (d *DashboardState) prepareDialogState() {
	panic("unported: mj-tui/src/component_events.rs DashboardState::prepare_dialog_state")
}

func (d *DashboardState) dialogConfirmationOpen() bool {
	panic("unported: mj-tui/src/component_events.rs DashboardState::dialog_confirmation_open")
}

func (d *DashboardState) renderDialogConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/component_events.rs DashboardState::render_dialog_confirmation")
}

// componentModalOpen reports whether the open mode routes events away from
// the dashboard.
func (d *DashboardState) componentModalOpen() bool {
	_, dashboard := d.mode.(ModeDashboard)
	return d.awayDialog() != nil || !dashboard
}

// ComponentPointerCaptured reports whether an already pressed dashboard
// control owns its drag and release.
func (d *DashboardState) ComponentPointerCaptured() bool {
	return d.surfaceForm.CapturesPointer() || d.workspaceScrollbar.IsDragging()
}

// ComponentHandlesMouse reports whether a form owns this pointer event, ahead
// of selectable body text.
func (d *DashboardState) ComponentHandlesMouse(mouse tea.MouseMsg) bool {
	panic("unported: mj-tui/src/component_events.rs DashboardState::component_handles_mouse")
}

// CancelComponentPointer releases any pressed dashboard control. Global
// shortcuts use this when they take ownership of a pointer gesture; clearing
// an armed control is itself a visible change and therefore requests a
// repaint.
func (d *DashboardState) CancelComponentPointer() bool {
	panic("unported: mj-tui/src/component_events.rs DashboardState::cancel_component_pointer")
}

func (d *DashboardState) resetComponentGeometry() {
	panic("unported: mj-tui/src/component_events.rs DashboardState::reset_component_geometry")
}

func (d *DashboardState) handleComponentEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/component_events.rs DashboardState::handle_component_event")
}
