// Port of mj-chat/src/chat/turn_control.rs.

// Turn-control feedback follows durable execution, not submission receipts.

package chat

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Control is a button of the steering dialog (Rust's turn_control::Control;
// components/scope.rs's Control is scopeControl in Go).
type Control int

const (
	ControlKeep Control = iota
	ControlCancel
	ControlRetry
	ControlRemove
)

// turnControlDialog is the steering dialog's form, with Keep as the dismiss,
// cancel-role and default action (Rust's turn_control::dialog()).
func turnControlDialog() Dialog[Control] {
	form := NewDialog[Control]()
	for _, control := range []Control{ControlKeep, ControlCancel, ControlRetry, ControlRemove} {
		form.Declare(control, ControlKindButton{})
	}
	form.SetDismissActions([]Control{ControlKeep})
	form.SetActionRole(ControlKeep, ActionRoleCancel)
	form.SetDefaultAction(ControlKeep)
	form.EndFrame(ControlKeep)
	return form
}

// escapeCommand is the relay command Esc runs, or nil when none is
// available.
func (s *ChatState) escapeCommand() core.RelayCommand {
	if s.turnControls != nil {
		if s.turnControls.Primary == nil {
			return nil
		}
		return s.turnControls.Primary.Command
	}
	// Older workers publish no availability; expose interruption only.
	if s.activePromptID != nil && s.targetedTurnControlSupported {
		return core.RelayCommandCancelTurnFor{ActivePromptID: *s.activePromptID}
	}
	return core.RelayCommandCancelTurn{}
}

// interruptAction is what Esc and the host's Interrupt turn command do. A
// steer can wait behind a long tool call for its delivery boundary, so
// interrupting must stay available; the worker returns the prompt and it
// runs next. Only uncertain delivery stops for review, because a retry could
// repeat it.
func (s *ChatState) interruptAction() ChatAction {
	panic("unported: mj-chat/src/chat/turn_control.rs interrupt_action")
}

func (s *ChatState) syncTurnControl(state *core.RelayOperationalState) {
	panic("unported: mj-chat/src/chat/turn_control.rs sync_turn_control")
}

func (s *ChatState) handleTurnControlDialog(event tea.Msg) ChatAction {
	panic("unported: mj-chat/src/chat/turn_control.rs handle_turn_control_dialog")
}

// turnControlRender draws the steering dialog over area (Rust's
// turn_control::render).
func turnControlRender(frame *Frame, area uv.Rectangle, chat *ChatState) {
	panic("unported: mj-chat/src/chat/turn_control.rs render")
}
