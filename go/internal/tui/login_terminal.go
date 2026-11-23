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
	"fmt"
	"image/color"
	"io"
	"runtime"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"
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
	parameter := uint16(0)
	if len(params) > 0 && len(params[0]) > 0 {
		parameter = params[0][0]
	}
	if prefix != nil {
		return
	}
	switch {
	case command == 'n' && parameter == 6:
		position := screen.CursorPosition()
		r.bytes = fmt.Appendf(r.bytes, "\x1b[%d;%dR", position.Y+1, position.X+1)
	case command == 'n' && parameter == 5:
		r.bytes = append(r.bytes, "\x1b[0n"...)
	case command == 'c' && parameter == 0:
		r.bytes = append(r.bytes, "\x1b[?1;2c"...)
	}
}

// loginCSIParams groups the parser's parameters into vt100's form: each
// parameter with its colon-separated subparameters.
func loginCSIParams(params ansi.Params) [][]uint16 {
	var grouped [][]uint16
	var current []uint16
	for _, param := range params {
		current = append(current, uint16(max(param.Param(0), 0)))
		if !param.HasMore() {
			grouped = append(grouped, current)
			current = nil
		}
	}
	if current != nil {
		grouped = append(grouped, current)
	}
	return grouped
}

// LoginPane is a terminal modal: it owns emulation; the caller owns its
// disposable connection.
type LoginPane struct {
	parser  *vt.Emulator
	replies *Replies
	modes   *loginTerminalModes
	profile string
	status  string
}

// loginTerminalModes are the emulated terminal's DECCKM and bracketed paste
// modes (vt100's application_cursor and bracketed_paste), which the vt
// emulator reports only through its mode callbacks. They live apart from the
// pane so the emulator's callbacks do not keep the pane reachable.
type loginTerminalModes struct {
	applicationCursor bool
	bracketedPaste    bool
}

func (m *loginTerminalModes) set(mode ansi.Mode, on bool) {
	switch mode {
	case ansi.ModeCursorKeys:
		m.applicationCursor = on
	case ansi.ModeBracketedPaste:
		m.bracketedPaste = on
	}
}

func NewLoginPane(profile string, cols int, rows int) *LoginPane {
	parser := vt.NewEmulator(max(cols, 1), max(rows, 1))
	// vt100 is created without scrollback.
	parser.SetScrollbackSize(0)
	replies := &Replies{}
	modes := &loginTerminalModes{}
	pane := &LoginPane{
		parser:  parser,
		replies: replies,
		modes:   modes,
		profile: profile,
		status:  "Connecting to the daemon…",
	}
	// vt100 hands the queries it does not implement to the Replies callback,
	// which answers only these; register them ahead of the emulator's own
	// answers so the harness sees what the Rust build sends.
	for _, command := range []rune{'n', 'c'} {
		parser.RegisterCsiHandler(int(command), func(params ansi.Params) bool {
			replies.unhandledCSI(parser, nil, loginCSIParams(params), command)
			return true
		})
	}
	parser.SetCallbacks(vt.Callbacks{
		EnableMode:  func(mode ansi.Mode) { modes.set(mode, true) },
		DisableMode: func(mode ansi.Mode) { modes.set(mode, false) },
	})
	// The emulator answers the queries vt100 ignores (secondary device
	// attributes, mode reports, colour queries, in-band resize) by writing
	// into an unbuffered pipe, which would block the dashboard until read.
	// vt100 sends nothing for them, so they are read and dropped here. The
	// pipe closes once the pane is unreachable, which ends this reader.
	go func() {
		_, _ = io.Copy(io.Discard, parser)
	}()
	if writer, ok := parser.InputPipe().(*io.PipeWriter); ok {
		runtime.AddCleanup(pane, func(writer *io.PipeWriter) { _ = writer.CloseWithError(io.EOF) }, writer)
	}
	return pane
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
	_, _ = p.parser.Write(bytes)
	replies := p.replies.bytes
	p.replies.bytes = nil
	return replies
}

// Paste is text as the harness should receive a paste: bracketed when the
// harness asked for bracketed paste.
func (p *LoginPane) Paste(text string) []byte {
	if p.modes.bracketedPaste {
		return []byte("\x1b[200~" + text + "\x1b[201~")
	}
	return []byte(text)
}

// Key is the bytes a terminal sends for key, nil for a key it cannot encode.
func (p *LoginPane) Key(key tea.KeyMsg) []byte {
	pressed := key.Key()
	shift := pressed.Mod.Contains(tea.ModShift)
	alt := pressed.Mod.Contains(tea.ModAlt)
	control := pressed.Mod.Contains(tea.ModCtrl)
	modifier := 1
	if shift {
		modifier++
	}
	if alt {
		modifier += 2
	}
	if control {
		modifier += 4
	}
	cursor := func(end rune, ss3 bool) []byte {
		if modifier > 1 {
			return fmt.Appendf(nil, "\x1b[1;%d%c", modifier, end)
		}
		introducer := '['
		if ss3 {
			introducer = 'O'
		}
		return fmt.Appendf(nil, "\x1b%c%c", introducer, end)
	}
	tilde := func(number int) []byte {
		if modifier > 1 {
			return fmt.Appendf(nil, "\x1b[%d;%d~", number, modifier)
		}
		return fmt.Appendf(nil, "\x1b[%d~", number)
	}
	code := chat.KeyPressCode(tea.KeyPressMsg(pressed))
	var bytes []byte
	switch {
	case code == tea.KeyTab && shift:
		// crossterm reports Shift-Tab as BackTab.
		bytes = []byte("\x1b[Z")
	case chat.IsCharKeyCode(code) && control &&
		((code >= 'a' && code <= 'z') || (code >= 'A' && code <= 'Z') ||
			code == '@' || code == '[' || code == '\\' || code == ']' ||
			code == '^' || code == '_' || code == ' ' || code == '?'):
		if code == '?' {
			bytes = []byte{127}
		} else {
			bytes = []byte{byte(code) & 31}
		}
	case chat.IsCharKeyCode(code):
		bytes = []byte(string(code))
	case code == tea.KeyEnter:
		bytes = []byte{'\r'}
	case code == tea.KeyTab:
		bytes = []byte{'\t'}
	case code == tea.KeyBackspace:
		bytes = []byte{127}
	case code == tea.KeyEscape:
		bytes = []byte{27}
	case code == tea.KeyUp:
		return cursor('A', p.modes.applicationCursor)
	case code == tea.KeyDown:
		return cursor('B', p.modes.applicationCursor)
	case code == tea.KeyRight:
		return cursor('C', p.modes.applicationCursor)
	case code == tea.KeyLeft:
		return cursor('D', p.modes.applicationCursor)
	case code == tea.KeyHome:
		return cursor('H', p.modes.applicationCursor)
	case code == tea.KeyEnd:
		return cursor('F', p.modes.applicationCursor)
	case code == tea.KeyInsert:
		return tilde(2)
	case code == tea.KeyDelete:
		return tilde(3)
	case code == tea.KeyPgUp:
		return tilde(5)
	case code == tea.KeyPgDown:
		return tilde(6)
	case code >= tea.KeyF1 && code <= tea.KeyF4:
		return cursor('P'+(code-tea.KeyF1), true)
	case code >= tea.KeyF5 && code <= tea.KeyF12:
		return tilde([...]int{15, 17, 18, 19, 20, 21, 23, 24}[code-tea.KeyF5])
	default:
		return nil
	}
	if alt {
		bytes = append([]byte{27}, bytes...)
	}
	return bytes
}

func (p *LoginPane) Render(frame *chat.Frame, screen uv.Rectangle) {
	popup := LoginPaneModalArea(screen)
	frame.RenderWidget(chat.Clear{}, popup)
	block := chat.Modal().WithTitle(chat.RawLine(fmt.Sprintf(" Sign in · %s ", p.profile)))
	inner := block.Inner(popup)
	frame.RenderWidget(block, popup)
	areas := chat.VerticalLayout(
		chat.ConstraintMin(1),
		chat.ConstraintLength(2),
		chat.ConstraintLength(1),
	).Split(inner)
	frame.RenderWidget(loginPseudoTerminal{screen: p.parser}, areas[0])
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText(p.status)).WithStyle(chat.Muted()).WithWrap(chat.Wrap{Trim: false}),
		areas[1],
	)
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText("Ctrl+] cancels · Ctrl+Q quits")).WithStyle(chat.Muted()),
		areas[2],
	)
}

// loginPseudoTerminal draws the emulated screen as tui-term's PseudoTerminal
// draws a vt100 screen: the area is cleared, every emulated cell inside it
// takes the cell's character (when it has one), colours and attributes, and
// a visible cursor is drawn as a gray block on an empty cell or reverses the
// character under it.
type loginPseudoTerminal struct {
	screen *vt.Emulator
}

// loginAllModifiers is every ratatui modifier, which Style::reset removes.
const loginAllModifiers = chat.ModifierBold | chat.ModifierDim | chat.ModifierItalic |
	chat.ModifierUnderlined | chat.ModifierSlowBlink | chat.ModifierRapidBlink |
	chat.ModifierReversed | chat.ModifierHidden | chat.ModifierCrossedOut

// loginCellHasContents is vt100's Cell::has_contents. The vt emulator fills
// cells nothing was written to with a space, so a written space reads as
// empty too.
func loginCellHasContents(cell *uv.Cell) bool {
	return cell.Content != "" && cell.Content != " "
}

// loginColor is tui-term's map_color: the default colour is Reset, and the
// sixteen basic colours are indexed colours, as vt100 reports them.
func loginColor(c color.Color) chat.Color {
	switch c := c.(type) {
	case nil:
		return chat.ColorReset
	case ansi.BasicColor:
		return chat.ColorIndexed(uint8(c))
	case ansi.IndexedColor:
		return chat.ColorIndexed(uint8(c))
	}
	r, g, b, _ := c.RGBA()
	return chat.ColorRGB(uint8(r>>8), uint8(g>>8), uint8(b>>8))
}

func (t loginPseudoTerminal) Render(area uv.Rectangle, buf *uv.Buffer) {
	chat.Clear{}.Render(area, buf)
	area = area.Intersect(buf.Bounds())
	for row := 0; row < area.Dy(); row++ {
		for col := 0; col < area.Dx(); col++ {
			source := t.screen.CellAt(col, row)
			if source == nil {
				continue
			}
			target := buf.CellAt(area.Min.X+col, area.Min.Y+row)
			if target == nil {
				continue
			}
			cell := *target
			if loginCellHasContents(source) {
				cell.Content = source.Content
				cell.Width = max(source.Width, 1)
			}
			var modifier chat.Modifier
			attrs := source.Style.Attrs
			if attrs&uv.AttrBold != 0 {
				modifier |= chat.ModifierBold
			}
			if attrs&uv.AttrItalic != 0 {
				modifier |= chat.ModifierItalic
			}
			if source.Style.Underline != uv.UnderlineNone {
				modifier |= chat.ModifierUnderlined
			}
			if attrs&uv.AttrReverse != 0 {
				modifier |= chat.ModifierReversed
			}
			if attrs&uv.AttrFaint != 0 {
				modifier |= chat.ModifierDim
			}
			chat.Style{
				Fg:             loginColor(source.Style.Fg),
				Bg:             loginColor(source.Style.Bg),
				UnderlineColor: chat.ColorReset,
				AddModifier:    modifier,
				SubModifier:    loginAllModifiers &^ modifier,
			}.ApplyTo(&cell.Style)
			buf.SetCell(area.Min.X+col, area.Min.Y+row, &cell)
		}
	}
	if t.screen.CursorHidden() {
		return
	}
	position := t.screen.CursorPosition()
	if position.Y >= area.Dy() || position.X >= area.Dx() {
		return
	}
	source := t.screen.CellAt(position.X, position.Y)
	target := buf.CellAt(area.Min.X+position.X, area.Min.Y+position.Y)
	if source == nil || target == nil {
		return
	}
	cell := *target
	if loginCellHasContents(source) {
		chat.Style{}.Reversed().ApplyTo(&cell.Style)
	} else {
		cell.Content = "█"
		cell.Width = 1
		chat.Style{}.Foreground(chat.ColorGray).ApplyTo(&cell.Style)
	}
	buf.SetCell(area.Min.X+position.X, area.Min.Y+position.Y, &cell)
}
