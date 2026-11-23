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
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// ChatRegionContains is the conversation pane the pointer is over, if any.
// A click there belongs to that pane's chat, whatever has focus.
func (d *DashboardState) ChatRegionContains(column, row int) (PaneId, bool) {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::chat_region_contains")
}

// OpenWebDialog opens the web-access dialog and asks the controller to load
// it.
func (d *DashboardState) OpenWebDialog() DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::open_web_dialog")
}

// HandleEventResult handles one terminal event and reports whether the
// dashboard consumed it. The legacy key and mouse wrappers below remain
// available to callers that only need the action. A nil Action is Rust's
// None.
func (d *DashboardState) HandleEventResult(event tea.Msg) chat.EventResult[DashboardAction] {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_event_result")
}

func (d *DashboardState) HandleKey(key tea.KeyMsg) DashboardAction {
	return d.HandleKeyAt(key, time.Now())
}

// HandleKeyAt handles one key with an explicit reading of the clock. now
// decides whether the notice on screen has been readable long enough for
// this key press to dismiss it.
func (d *DashboardState) HandleKeyAt(key tea.KeyMsg, now time.Time) DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_key_at")
}

func (d *DashboardState) textInputFocused() bool {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::text_input_focused")
}

func (d *DashboardState) PasteTarget() DashboardPasteTarget {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::paste_target")
}

// ApplyClipboardText applies a modal clipboard read; it cannot follow focus
// into another field or session.
func (d *DashboardState) ApplyClipboardText(target *DashboardPasteTarget, text string) bool {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::apply_clipboard_text")
}

func (d *DashboardState) HandlePaste(pasted string) {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_paste")
}

// FrameSurfaces are the surfaces the last frame registered, for the
// selection engine.
func (d *DashboardState) FrameSurfaces() *chat.FrameSurfaces {
	return &d.frameSurfaces
}

func (d *DashboardState) HandleMouse(mouse tea.MouseMsg) DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_mouse")
}

// beginPointerPress abandons keyboard-only navigation modes on every
// surface.
func (d *DashboardState) beginPointerPress() {
	d.notices.Dismiss(time.Now())
	d.prefixPending = false
	d.resizeMode = false
}

func (d *DashboardState) handleMouseInner(mouse tea.MouseMsg) DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_mouse_inner")
}

// handleRowClick selects the clicked row and, if it's the second click on
// the same row within doubleClickInterval, performs the same action Enter
// would.
func (d *DashboardState) handleRowClick(focus Focus, index int) DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_row_click")
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
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_dashboard_key")
}

// handleSessionActionKey handles the small action row at the top of
// Sessions. The row is a second selection target within the pane: Up from
// its first session enters it, Down returns to the first session, and
// Left/Right skip any disabled action. nil means the regular dashboard key
// handling still owns the key; an action (DashboardActionNone included)
// means the key was consumed, including a no-op.
func (d *DashboardState) handleSessionActionKey(key tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::handle_session_action_key")
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
	panic("unported: mj-tui/src/dashboard_input.rs DashboardState::open_selected_session")
}
