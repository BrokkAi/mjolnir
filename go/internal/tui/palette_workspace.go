// Port of mj-tui/src/palette/workspace.rs.
//
// Change Workspace, as a combobox anchored on the palette row that opened
// it.
//
// The palette stays open underneath. The popup is the same ComboBox the
// wizard's Subagents, Model and Effort fields use: the item's own text stays
// where it is, the dropdown glyph replaces its submenu glyph, and the list
// of workspaces opens below it. Enter picks, and Esc closes only the list.
package tui

import (
	"slices"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// WorkspacePopup is the workspace list open on the "Change workspace" row.
type WorkspacePopup struct {
	sessionID string
	// choices holds workspace ids and names, in the workspace manager's
	// order (Rust's `Vec<(String, String)>`).
	choices [][2]string
	// current is the index into choices of the session's current
	// workspace.
	current int
	combo   chat.ComboBoxState[PaletteControl]
}

// Clone copies the popup (Rust's derived Clone).
func (p *WorkspacePopup) Clone() WorkspacePopup {
	clone := *p
	clone.choices = slices.Clone(p.choices)
	return clone
}

// Equal compares two popups field by field (Rust's derived PartialEq).
func (p *WorkspacePopup) Equal(other *WorkspacePopup) bool {
	return p.sessionID == other.sessionID &&
		slices.Equal(p.choices, other.choices) &&
		p.current == other.current &&
		p.combo == other.combo
}

func (p *WorkspacePopup) selected() int {
	return p.combo.Selection(PaletteControlWorkspace, p.current)
}

func (p *WorkspacePopup) kind() chat.ControlKind {
	return chat.ControlKindComboBox{
		Len:      len(p.choices),
		Selected: p.selected(),
		Expanded: p.combo.IsOpen(PaletteControlWorkspace),
	}
}

// options is the list's rows: names only, the current workspace marked.
func (p *WorkspacePopup) options() []chat.Line {
	options := make([]chat.Line, 0, len(p.choices))
	for index, choice := range p.choices {
		if index == p.current {
			options = append(options, chat.RawLine(choice[1]+" (current)"))
		} else {
			options = append(options, chat.RawLine(choice[1]))
		}
	}
	return options
}

// render draws the anchored list on row, the palette row of the command.
// bounds is what the popup may cover.
func (p *WorkspacePopup) render(frame *chat.Frame, bounds uv.Rectangle, row uv.Rectangle, sessionMenu bool, form *chat.Dialog[PaletteControl]) {
	panic("unported: mj-tui/src/palette/workspace.rs WorkspacePopup::render")
}

// beginChangeWorkspace opens Change Workspace from a keybinding or another
// command, which has no palette open: the session menu appears with the
// list already open on its row.
func (d *DashboardState) beginChangeWorkspace() {
	panic("unported: mj-tui/src/palette/workspace.rs DashboardState::begin_change_workspace")
}

// openWorkspacePopup opens the workspace list on the open palette's Change
// workspace row, on the session's current workspace.
func (d *DashboardState) openWorkspacePopup() {
	panic("unported: mj-tui/src/palette/workspace.rs DashboardState::open_workspace_popup")
}

// handleWorkspacePopupEvent gives keys and clicks to the list while it is
// open. Esc closes the list and leaves the menu.
func (d *DashboardState) handleWorkspacePopupEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/palette/workspace.rs DashboardState::handle_workspace_popup_event")
}

// commitWorkspaceChoice moves the session at once (it is reversible);
// picking its own workspace only says so.
func (d *DashboardState) commitWorkspaceChoice(index int) DashboardAction {
	panic("unported: mj-tui/src/palette/workspace.rs DashboardState::commit_workspace_choice")
}

// closePopup closes the list and returns the focus to the menu behind it.
func closePopup(palette *CommandPalette) {
	palette.workspacePopup = nil
	palette.prepare()
	if palette.sessionOnly {
		palette.form.Focus(PaletteControlCommands)
	} else {
		palette.form.Focus(PaletteControlQuery)
	}
}
