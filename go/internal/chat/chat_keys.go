// Port of mj-chat/src/chat/keys.rs.
//
// The composer's keyboard handling: one key press against the chat's
// dialogs, prompt controls, history search, and prompt editor.
//
// Key events are Bubble Tea's: crossterm's KeyEvent becomes tea.KeyMsg
// (a tea.KeyPressMsg, repeats included through Key().IsRepeat, or a
// tea.KeyReleaseMsg), and keypad keys arrive as their own codes
// (tea.KeyKpHome) where crossterm reported KeyEventState::KEYPAD.
package chat

import (
	tea "charm.land/bubbletea/v2"
)

// HandleKey applies one key event to the chat and reports what the host
// must do about it.
func (s *ChatState) HandleKey(key tea.KeyMsg) ChatAction {
	panic("unported: mj-chat/src/chat/keys.rs ChatState::handle_key")
}
