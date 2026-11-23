// Port of mj-tui/src/login_terminal.rs.
//
// Login terminal rendering and terminal keyboard encoding.
//
// The Rust build emulates the harness's login terminal with the vt100 crate
// and draws it with tui-term; the Go build uses Charm's virtual terminal
// emulator, github.com/charmbracelet/x/vt, which draws into the same
// ultraviolet cell buffer as the rest of the dashboard.

package tui

import (
	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/vt"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

const (
	loginModalWidthPercent  = 90
	loginModalHeightPercent = 80
)

// Replies collects the emulator's answers to the harness's device status and
// device attribute queries, for the caller to send back to the harness.
type Replies struct {
	bytes []byte
}

// unhandledCSI answers cursor position (CSI 6 n), device status (CSI 5 n)
// and primary device attribute (CSI c) queries. prefix is the CSI private
// prefix byte, nil when absent.
func (r *Replies) unhandledCSI(screen *vt.Emulator, prefix *byte, params [][]uint16, command rune) {
	panic("unported: mj-tui/src/login_terminal.rs Replies::unhandled_csi")
}

// LoginPane is a terminal modal: it owns emulation; the caller owns its
// disposable connection.
type LoginPane struct {
	parser  *vt.Emulator
	replies *Replies
	profile string
	status  string
}

func NewLoginPane(profile string, cols int, rows int) *LoginPane {
	panic("unported: mj-tui/src/login_terminal.rs LoginPane::new")
}

// LoginPaneTerminalSize is the emulated terminal's (cols, rows) for a screen
// of width by height: the bordered pane reserves three footer lines for
// status and controls.
func LoginPaneTerminalSize(width int, height int) (cols int, rows int) {
	modal := LoginPaneModalArea(uv.Rect(0, 0, width, height))
	return max(modal.Dx()-2, 1), max(modal.Dy()-5, 1)
}

// LoginPaneModalArea is the opaque dialog that contains the emulated harness
// terminal.
func LoginPaneModalArea(screen uv.Rectangle) uv.Rectangle {
	return chat.CenteredRectPercent(loginModalWidthPercent, loginModalHeightPercent, screen)
}

func (p *LoginPane) Resize(cols int, rows int) {
	p.parser.Resize(max(cols, 1), max(rows, 1))
}

func (p *LoginPane) Status(status string) {
	p.status = status
}

// Output feeds harness output to the emulator and returns the replies the
// harness's queries asked for.
func (p *LoginPane) Output(bytes []byte) []byte {
	panic("unported: mj-tui/src/login_terminal.rs LoginPane::output")
}

// Paste is text as the harness should receive a paste: bracketed when the
// harness asked for bracketed paste.
func (p *LoginPane) Paste(text string) []byte {
	panic("unported: mj-tui/src/login_terminal.rs LoginPane::paste")
}

// Key is the bytes a terminal sends for key, nil for a key it cannot encode.
func (p *LoginPane) Key(key tea.KeyMsg) []byte {
	panic("unported: mj-tui/src/login_terminal.rs LoginPane::key")
}

func (p *LoginPane) Render(frame *chat.Frame, screen uv.Rectangle) {
	panic("unported: mj-tui/src/login_terminal.rs LoginPane::render")
}
