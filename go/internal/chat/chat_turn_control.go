// Port of mj-chat/src/chat/turn_control.rs.

// Turn-control feedback follows durable execution, not submission receipts.

package chat

import (
	"reflect"

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
	steering := s.steering
	if steering != nil && !steering.HoldsQueue() {
		steering = nil
	}
	switch {
	case steering != nil && steering.Status == core.SteeringStatusUnconfirmed:
		s.turnControlDialogOpen = true
		return ChatActionNone{}
	case steering != nil:
		var command core.RelayCommand
		if s.turnControls != nil {
			command = s.turnControls.Interrupt
		} else {
			command = s.escapeCommand()
		}
		if command == nil {
			return ChatActionNone{}
		}
		return ChatActionTurnControl{Command: command}
	}
	var available bool
	if s.turnControls == nil {
		available = s.turnInterruptible()
	} else {
		available = s.turnControls.Primary != nil || len(s.activeUserShells) > 0
	}
	if available {
		return ChatActionCancel{}
	}
	return ChatActionNone{}
}

func (s *ChatState) syncTurnControl(state *core.RelayOperationalState) {
	s.targetedTurnControlSupported = state.SupportsTargetedTurnControl()
	s.turnControls = turnControlClone(state.TurnControls)
	changed := !reflect.DeepEqual(s.steering, state.Steering)
	if id := s.turnControlAwaitingState; id != nil {
		var activePromptID *string
		if state.ActivePrompt != nil {
			activePromptID = &state.ActivePrompt.CommandID
		}
		if (state.Steering != nil && state.Steering.CommandID == *id) ||
			(state.CancellingPromptID != nil && s.turnControlTarget != nil && *state.CancellingPromptID == *s.turnControlTarget) ||
			!turnControlSameID(activePromptID, s.turnControlTarget) {
			s.turnControlAwaitingState = nil
		}
	}
	s.activePromptID = nil
	if state.ActivePrompt != nil {
		id := state.ActivePrompt.CommandID
		s.activePromptID = &id
	}
	s.steering = turnControlClone(state.Steering)
	s.cancellingPromptID = turnControlClone(state.CancellingPromptID)
	feedback := ""
	if s.cancellingPromptID != nil {
		feedback = "Interrupting turn…"
	} else if s.steering != nil {
		switch s.steering.Status {
		case core.SteeringStatusPending:
			feedback = "Steering…"
		case core.SteeringStatusUnconfirmed:
			feedback = "Delivery unconfirmed · Esc reviews queued input"
		}
	}
	if feedback != "" {
		if s.operationFeedback == nil {
			s.operationFeedback = map[string]string{}
		}
		s.operationFeedback["turn-control"] = feedback
	} else if !s.turnControlSubmitting && s.turnControlAwaitingState == nil {
		delete(s.operationFeedback, "turn-control")
	}
	if changed {
		if steering := s.steering; steering != nil {
			s.turnControlError = turnControlClone(steering.Message)
			target := steering.ActivePromptID
			s.turnControlTarget = &target
			s.turnControlDialogOpen = steering.Status == core.SteeringStatusFailed ||
				steering.Status == core.SteeringStatusUnconfirmed
			s.turnControlDialog = turnControlDialog()
		} else {
			s.turnControlDialogOpen = false
		}
	}
}

// turnControlClone is Option::clone for a pointer: a fresh copy of the value it
// points to, or nil.
func turnControlClone[T any](value *T) *T {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// turnControlSameID is Option<&String> equality.
func turnControlSameID(left, right *string) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

// turnControlTargetsActivePrompt reports whether the steering the dialog is
// about targets the prompt that is running now.
func (s *ChatState) turnControlTargetsActivePrompt() bool {
	return s.activePromptID != nil && turnControlSameID(s.turnControlTarget, s.activePromptID)
}

func (s *ChatState) handleTurnControlDialog(event tea.Msg) ChatAction {
	if key, ok := event.(tea.KeyMsg); ok && key.Key().Code == tea.KeyEscape {
		s.turnControlDialogOpen = false
		return ChatActionNone{}
	}
	result := s.turnControlDialog.Handle(event)
	activate, ok := result.Action.(InteractionActivate[Control])
	if !ok {
		return ChatActionNone{}
	}
	var command core.RelayCommand
	switch activate.ID {
	case ControlKeep:
	case ControlCancel:
		if s.turnControlTargetsActivePrompt() {
			if s.turnControls != nil {
				command = s.turnControls.Interrupt
			} else {
				command = core.RelayCommandCancelTurnFor{ActivePromptID: *s.activePromptID}
			}
		}
	case ControlRetry:
		if s.activePromptID != nil {
			return ChatActionNone{}
		}
		if s.steering != nil {
			command = core.RelayCommandResolveSteering{SteeringID: s.steering.CommandID}
		}
	case ControlRemove:
		if s.activePromptID != nil {
			return ChatActionNone{}
		}
		if s.steering != nil {
			command = core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: s.steering.QueuedPromptID}
		}
	default:
		return ChatActionNone{}
	}
	s.turnControlDialogOpen = false
	if command == nil {
		return ChatActionNone{}
	}
	return ChatActionTurnControl{Command: command}
}

// turnControlRender draws the steering dialog over area (Rust's
// turn_control::render).
func turnControlRender(frame *Frame, area uv.Rectangle, chat *ChatState) {
	popup := CenteredModalFixed(frame, &chat.frameSurfaces, 72, 11, area)
	block := NewBlock().WithBorders(BordersAll).WithTitle(RawLine(" Steering "))
	inner := block.Inner(popup)
	frame.RenderWidget(block, popup)
	uncertain := chat.steering != nil && chat.steering.Status == core.SteeringStatusUnconfirmed
	text := "The prompt could not be steered and remains queued."
	if chat.turnControlError != nil {
		text = *chat.turnControlError
	}
	if uncertain {
		text += " Delivery may already have occurred. Retrying can deliver the prompt twice."
	}
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-3, 0))
	frame.RenderWidget(NewParagraph(RawText(text)).WithWrap(Wrap{Trim: true}), body)
	var buttons []ButtonSpec[Control]
	switch {
	case chat.turnControlTargetsActivePrompt():
		cancel := "Cancel turn and apply queued prompt"
		if uncertain {
			cancel = "Cancel turn"
		}
		buttons = []ButtonSpec[Control]{
			{ID: ControlKeep, Label: "Keep queued", Enabled: true},
			{ID: ControlCancel, Label: cancel, Enabled: chat.cancellingPromptID == nil},
		}
	case uncertain:
		buttons = []ButtonSpec[Control]{
			{ID: ControlKeep, Label: "Keep queued", Enabled: true},
			{ID: ControlRetry, Label: "Retry queued prompt", Enabled: true},
			{ID: ControlRemove, Label: "Remove queued prompt", Enabled: true},
		}
	default:
		buttons = []ButtonSpec[Control]{{ID: ControlKeep, Label: "Keep queued", Enabled: true}}
	}
	chat.turnControlDialog.BeginFrame()
	chat.turnControlDialog.RenderActions(
		frame,
		uv.Rect(inner.Min.X, body.Max.Y, inner.Dx(), max(inner.Dy()-body.Dy(), 0)),
		buttons,
	)
	chat.turnControlDialog.EndFrame(ControlKeep)
}
