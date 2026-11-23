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
	"fmt"
	"slices"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	// The row's own words, so the field reads as the item opened. The menu
	// spells it in title case, the palette in sentence case.
	label := "Change workspace"
	if sessionMenu {
		label = "Change Workspace"
	}
	fieldWidth := min(chat.RawLine(label).Width()+1+chat.RawLine(chat.ComboBoxCurrentGlyph()).Width(), row.Dx())
	field := uv.Rect(row.Min.X, row.Min.Y, fieldWidth, 1)
	frame.RenderWidget(chat.Clear{}, field)
	chat.ComboBoxRender(
		frame,
		bounds,
		field,
		label,
		p.options(),
		p.selected(),
		p.combo.IsOpen(PaletteControlWorkspace),
		true,
		" workspace · ↑/↓ select · Enter choose ",
		chat.PopupSideBelow,
		// The palette stays open underneath; the list takes less of it.
		chat.ComboBoxDefaultMaxVisibleRows-2,
		&form.Form,
		PaletteControlWorkspace,
	)
}

// beginChangeWorkspace opens Change Workspace from a keybinding or another
// command, which has no palette open: the session menu appears with the
// list already open on its row.
func (d *DashboardState) beginChangeWorkspace() {
	if _, open := d.mode.(ModePalette); !open {
		d.beginSessionPalette()
		if palette, ok := d.mode.(ModePalette); ok {
			index := slices.IndexFunc(palette.entries, func(entry PaletteEntry) bool {
				return entry.id == CommandIdChangeWorkspace
			})
			if index >= 0 {
				palette.selected = index
				palette.prepare()
			}
		}
	}
	d.openWorkspacePopup()
}

// openWorkspacePopup opens the workspace list on the open palette's Change
// workspace row, on the session's current workspace.
func (d *DashboardState) openWorkspacePopup() {
	palette, ok := d.mode.(ModePalette)
	if !ok {
		return
	}
	var session *core.SessionRecord
	if palette.sessionID != nil {
		session = d.state.Get().Sessions.GetShared(*palette.sessionID)
	} else if id, ok := d.commandSessionID(); ok {
		session = d.state.Get().Sessions.GetShared(id)
	}
	if session == nil {
		d.CancelModal()
		d.SetNotice("This session is no longer available.")
		return
	}
	sessionID := session.ID
	workspaceID := session.WorkspaceID
	var choices [][2]string
	for _, id := range d.workspaceIDs() {
		choices = append(choices, [2]string{id, d.workspaceDisplayName(id)})
	}
	if len(choices) < 2 {
		d.CancelModal()
		d.SetNotice("There is no other workspace to move this session to.")
		return
	}
	current := max(slices.IndexFunc(choices, func(choice [2]string) bool { return choice[0] == workspaceID }), 0)
	var combo chat.ComboBoxState[PaletteControl]
	combo.Open(PaletteControlWorkspace, current)
	palette.workspacePopup = &WorkspacePopup{
		sessionID: sessionID,
		choices:   choices,
		current:   current,
		combo:     combo,
	}
	palette.prepare()
	palette.form.Focus(PaletteControlWorkspace)
}

// handleWorkspacePopupEvent gives keys and clicks to the list while it is
// open. Esc closes the list and leaves the menu.
func (d *DashboardState) handleWorkspacePopupEvent(event tea.Msg) DashboardAction {
	palette, ok := d.mode.(ModePalette)
	if !ok {
		return DashboardActionNone{}
	}
	popup := palette.workspacePopup
	if popup == nil {
		return DashboardActionNone{}
	}
	result := palette.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	routed := popup.combo.Route(result.Action)
	if commit, ok := routed.(chat.InteractionComboBoxCommit[PaletteControl]); ok {
		return d.commitWorkspaceChoice(commit.Index)
	}
	if _, ok := routed.(chat.InteractionCancel[PaletteControl]); ok {
		d.CancelModal()
		return DashboardActionNone{}
	}
	if popup.combo.IsOpen(PaletteControlWorkspace) {
		palette.prepare()
	} else {
		closePopup(palette.CommandPalette)
	}
	return DashboardActionNone{}
}

// commitWorkspaceChoice moves the session at once (it is reversible);
// picking its own workspace only says so.
func (d *DashboardState) commitWorkspaceChoice(index int) DashboardAction {
	palette, ok := d.mode.(ModePalette)
	if !ok {
		return DashboardActionNone{}
	}
	popup := palette.workspacePopup
	if popup == nil {
		return DashboardActionNone{}
	}
	palette.workspacePopup = nil
	if index < 0 || index >= len(popup.choices) {
		closePopup(palette.CommandPalette)
		return DashboardActionNone{}
	}
	workspaceID, workspaceName := popup.choices[index][0], popup.choices[index][1]
	d.CancelModal()
	session := d.state.Get().Sessions.GetShared(popup.sessionID)
	if session == nil {
		d.SetNotice("This session is no longer available.")
		return DashboardActionNone{}
	}
	name := sessionName(session)
	if index == popup.current {
		d.SetNotice(fmt.Sprintf(
			"Session \"%s\" is already in workspace \"%s\".",
			FitSessionName(name, NoticeNameCells),
			FitSessionName(workspaceName, NoticeNameCells),
		))
		return DashboardActionNone{}
	}
	return DashboardActionChangeWorkspace{
		SessionID:     popup.sessionID,
		WorkspaceID:   workspaceID,
		WorkspaceName: workspaceName,
	}
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
