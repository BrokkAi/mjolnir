// Port of mj-chat/src/chat/pointer.rs.

// Pointer routing for the conversation. Mouse events are Bubble Tea mouse
// messages: crossterm's Down(Left) is a tea.MouseClickMsg with MouseLeft,
// ScrollUp/ScrollDown a tea.MouseWheelMsg with MouseWheelUp/MouseWheelDown,
// Drag and Moved a tea.MouseMotionMsg, Up a tea.MouseReleaseMsg.

package chat

import (
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (s *ChatState) setActiveUserShells(shells []core.ActiveUserShell) {
	next := make([]string, 0, len(shells))
	for _, shell := range shells {
		next = append(next, shell.CommandID)
	}
	if !slices.Equal(s.activeUserShells, next) {
		s.activeUserShells = next
	}
}

func (s *ChatState) setActiveAgentTerminals(terminals []core.ActiveAgentTerminal, session *core.MaterializedSession) {
	panic("unported: mj-chat/src/chat/pointer.rs set_active_agent_terminals")
}

func (s *ChatState) activeUserShellIDs() []string {
	return slices.Clone(s.activeUserShells)
}

// ToggleRenderMode switches the transcript between rendered Markdown and raw
// source.
//
// The host owns the key that runs this, so it is called from the dashboard's
// command registry as well as from the transcript control.
func (s *ChatState) ToggleRenderMode() {
	s.renderMode = s.renderMode.Toggled()
	if s.renderMode == client.TranscriptRenderModeRich {
		s.SetNotice("Rich transcript rendering enabled")
	} else {
		s.SetNotice("Raw transcript source enabled")
	}
}

// DictationToggleAction is what a dictation toggle should do right now.
//
// A recording remains stoppable even if the helper becomes unavailable while
// it is running, so an active recording answers even when voice is no longer
// available. An unavailable idle microphone is inert.
func (s *ChatState) DictationToggleAction() ChatAction {
	if s.voiceAvailable || s.voiceActive {
		return ChatActionToggleVoice{}
	}
	return ChatActionNone{}
}

// FrameSurfaces are the surfaces the last frame registered, for the
// selection engine.
func (s *ChatState) FrameSurfaces() *FrameSurfaces {
	return &s.frameSurfaces
}

// ComponentHandlesMouse reports whether a shared component should receive
// this pointer event before text selection or the host surface. Captured
// presses remain owned even after the pointer leaves the control's hitbox.
func (s *ChatState) ComponentHandlesMouse(mouse tea.MouseMsg) bool {
	panic("unported: mj-chat/src/chat/pointer.rs component_handles_mouse")
}

// composerReplaced reports whether a dialog of this conversation's own
// currently stands in for the ordinary composer.
func (s *ChatState) composerReplaced() bool {
	return s.earlier != nil ||
		s.elicitation != nil ||
		s.configPicker != nil ||
		s.taskDialogOpen
}

// ConversationPointerCaptured: captured controls and scrollbars keep their
// conversation as owner even when the pointer crosses another session's
// notification form.
func (s *ChatState) ConversationPointerCaptured() bool {
	panic("unported: mj-chat/src/chat/pointer.rs conversation_pointer_captured")
}

// CancelConversationPointer cancels any pointer gesture owned by a chat
// component.
func (s *ChatState) CancelConversationPointer() {
	s.cancelTranscriptScrollbarPointer()
	s.turnControlDialog.CancelPointer()
	s.taskDialogForm.CancelPointer()
	s.voiceForm.CancelPointer()
	s.cancelConfigPickerPointer()
	if s.elicitation != nil {
		s.elicitation.cancelPointerIn(ElicitationSurfaceConversation)
	}
}

func (s *ChatState) CancelComponentPointer() {
	s.CancelConversationPointer()
	if s.elicitation != nil {
		s.elicitation.cancelPointerIn(ElicitationSurfaceNotification)
	}
}

// ResetComponentGeometry clears rendered component geometry before a host
// redraw. Persistent focus and pointer ownership survive a normal resize;
// only hitboxes are invalidated until the next registration pass.
func (s *ChatState) ResetComponentGeometry() {
	s.taskDialogForm.ResetGeometry()
	s.voiceForm.ResetGeometry()
	s.voiceButtonArea = nil
	s.configChipAreas = s.configChipAreas[:0]
	s.taskControlArea = nil
	s.subagentControlArea = nil
	s.taskDialogArea = nil
	s.taskDialogControlIDs = s.taskDialogControlIDs[:0]
	s.resetConfigPickerGeometry()
	if s.elicitation != nil {
		s.elicitation.resetComponentGeometry()
	}
}

// scrollElicitationMessage scrolls the elicitation message pane, for a drag
// held at its edge.
func (s *ChatState) scrollElicitationMessage(rows int) {
	if s.elicitation != nil {
		s.elicitation.scrollMessage(rows)
	}
}

// ElicitationSelectionText is the message text a selection in the
// elicitation pane covers.
func (s *ChatState) ElicitationSelectionText(selection *SelectionRange) (string, bool) {
	panic("unported: mj-chat/src/chat/pointer.rs elicitation_selection_text")
}

// HandleMouse routes a pointer event. Capture is on for every surface, so
// the app owns the wheel: terminal scrollback repaints whole TUI frames and
// is unusably slow on long sessions.
func (s *ChatState) HandleMouse(mouse tea.MouseMsg) ChatAction {
	panic("unported: mj-chat/src/chat/pointer.rs handle_mouse")
}

// isWebURL reports an http or https URL with an authority; agent text
// chooses these destinations, and other schemes can start arbitrary local
// handlers.
func isWebURL(url string) bool {
	scheme, rest, ok := strings.Cut(url, ":")
	if !ok {
		return false
	}
	// ASCII-only folding, as Rust's eq_ignore_ascii_case: Unicode folding
	// would accept "httpſ".
	scheme = strings.Map(func(r rune) rune {
		if 'A' <= r && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, scheme)
	return (scheme == "http" || scheme == "https") && strings.HasPrefix(rest, "//")
}
