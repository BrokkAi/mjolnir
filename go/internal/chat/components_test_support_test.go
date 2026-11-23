// Port of mj-chat/src/components/test_support.rs, with the stand-ins the
// component tests use for ratatui's TestBackend terminal and crossterm's
// event constructors and Debug output.
//
// Rust's `key(code)` fixture is chat_test_support_test.go's key, which makes
// the same unmodified press.

package chat

import (
	"fmt"
	"image/color"
	"strconv"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"
)

// componentTerminal stands in for ratatui's Terminal over a TestBackend:
// every draw renders a fresh frame, and the backend then holds exactly
// what that frame drew.
type componentTerminal struct {
	width, height int
	buffer        *uv.Buffer
	cursor        *uv.Position
}

func newComponentTerminal(width, height int) *componentTerminal {
	return &componentTerminal{width: width, height: height, buffer: uv.NewBuffer(width, height)}
}

func (t *componentTerminal) draw(render func(frame *Frame)) {
	buffer := uv.NewBuffer(t.width, t.height)
	frame := NewFrame(buffer)
	render(frame)
	t.buffer = buffer
	t.cursor = nil
	if position, ok := frame.CursorPosition(); ok {
		t.cursor = &position
	}
}

// symbol is ratatui's `buffer[(x, y)].symbol()`: a wide glyph's covered
// cells read as the blank symbol.
func (t *componentTerminal) symbol(x, y int) string {
	return cellSymbol(t.buffer.CellAt(x, y))
}

func cellSymbol(cell *uv.Cell) string {
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

// content is every cell's symbol in row order, ratatui's
// `buffer.content().iter().map(|cell| cell.symbol()).collect()`.
func (t *componentTerminal) content() string {
	var text strings.Builder
	for y := range t.height {
		for x := range t.width {
			text.WriteString(t.symbol(x, y))
		}
	}
	return text.String()
}

// rows is each row's symbols, untrimmed.
func (t *componentTerminal) rows() []string {
	rows := make([]string, 0, t.height)
	for y := range t.height {
		var row strings.Builder
		for x := range t.width {
			row.WriteString(t.symbol(x, y))
		}
		rows = append(rows, row.String())
	}
	return rows
}

// Mouse events, as crossterm's MouseEventKind variants with no modifiers.

func formMouseDown(x, y int) tea.MouseMsg {
	return tea.MouseClickMsg{X: x, Y: y, Button: tea.MouseLeft}
}

func formMouseUp(x, y int) tea.MouseMsg {
	return tea.MouseReleaseMsg{X: x, Y: y, Button: tea.MouseLeft}
}

func formMouseDrag(x, y int) tea.MouseMsg {
	return tea.MouseMotionMsg{X: x, Y: y, Button: tea.MouseLeft}
}

func formMouseMoved(x, y int) tea.MouseMsg {
	return tea.MouseMotionMsg{X: x, Y: y, Button: tea.MouseNone}
}

func formScrollDown(x, y int) tea.MouseMsg {
	return tea.MouseWheelMsg{X: x, Y: y, Button: tea.MouseWheelDown}
}

// chord is a key press carrying modifiers, which the shared `key` fixture
// cannot make.
func chord(code rune, modifiers tea.KeyMod) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: code, Mod: modifiers}
}

// intPtr is `Some(value)` for a row map entry.
func intPtr(value int) *int {
	return &value
}

// debugOption is Rust's Debug of an `Option<T>` given T's Debug.
func debugOption(present bool, inner string) string {
	if !present {
		return "None"
	}
	return "Some(" + inner + ")"
}

// debugInteraction is Rust's Debug of `Option<Interaction<K>>`.
func debugInteraction[K comparable](interaction Interaction[K]) string {
	if interaction == nil {
		return "None"
	}
	var inner string
	switch action := interaction.(type) {
	case InteractionActivate[K]:
		inner = fmt.Sprintf("Activate(%v)", action.ID)
	case InteractionToggle[K]:
		inner = fmt.Sprintf("Toggle(%v)", action.ID)
	case InteractionSelect[K]:
		inner = fmt.Sprintf("Select(%v, %d)", action.ID, action.Index)
	case InteractionComboBoxCommit[K]:
		inner = fmt.Sprintf("ComboBoxCommit(%v, %d)", action.ID, action.Index)
	case InteractionComboBoxDismiss[K]:
		inner = fmt.Sprintf("ComboBoxDismiss(%v)", action.ID)
	case InteractionComplete[K]:
		inner = fmt.Sprintf("Complete(%v)", action.ID)
	case InteractionPathCommit[K]:
		inner = fmt.Sprintf("PathCommit(%v, %d)", action.ID, action.Index)
	case InteractionPathDismiss[K]:
		inner = fmt.Sprintf("PathDismiss(%v)", action.ID)
	case InteractionEdit[K]:
		inner = fmt.Sprintf("Edit(%v, %s)", action.ID, debugFieldEdit(action.Edit))
	case InteractionCancel[K]:
		inner = "Cancel"
	}
	return "Some(" + inner + ")"
}

func debugFieldEdit(edit FieldEdit) string {
	switch edit := edit.(type) {
	case FieldEditKey:
		return "Key(" + debugKeyEvent(edit.Key) + ")"
	case FieldEditPaste:
		return "Paste(" + strconv.Quote(edit.Text) + ")"
	case FieldEditCursor:
		return fmt.Sprintf("Cursor(%d)", edit.Offset)
	}
	return "?"
}

// debugKeyEvent is crossterm's Debug of the KeyEvent a press stands for.
func debugKeyEvent(press tea.KeyPressMsg) string {
	key := press.Key()
	var code string
	switch character := componentKeyCode(key); character {
	case tea.KeyEnter:
		code = "Enter"
	case tea.KeyEscape:
		code = "Esc"
	case tea.KeyTab:
		code = "Tab"
	case tea.KeyBackspace:
		code = "Backspace"
	case tea.KeyUp:
		code = "Up"
	case tea.KeyDown:
		code = "Down"
	case tea.KeyLeft:
		code = "Left"
	case tea.KeyRight:
		code = "Right"
	case tea.KeyHome:
		code = "Home"
	case tea.KeyEnd:
		code = "End"
	case 0:
		code = "Null"
	default:
		code = "Char(" + strconv.QuoteRune(character) + ")"
	}
	var flags []string
	mods := componentKeyMods(key.Mod)
	for _, flag := range []struct {
		mod  tea.KeyMod
		name string
	}{{tea.ModShift, "SHIFT"}, {tea.ModCtrl, "CONTROL"}, {tea.ModAlt, "ALT"}, {tea.ModSuper, "SUPER"}} {
		if mods.Contains(flag.mod) {
			flags = append(flags, flag.name)
		}
	}
	modifiers := "KeyModifiers(0x0)"
	if len(flags) > 0 {
		modifiers = "KeyModifiers(" + strings.Join(flags, " | ") + ")"
	}
	kind := "Press"
	if key.IsRepeat {
		kind = "Repeat"
	}
	return fmt.Sprintf("KeyEvent { code: %s, modifiers: %s, kind: %s, state: KeyEventState(0x0) }", code, modifiers, kind)
}

// debugCellColor is ratatui's Debug of a drawn cell's color: a cell with no
// color is Reset.
func debugCellColor(value color.Color) string {
	switch value := value.(type) {
	case nil:
		return "Reset"
	case ansi.IndexedColor:
		return fmt.Sprintf("Indexed(%d)", uint8(value))
	case ansi.BasicColor:
		return (ColorBlack + Color(value)).GoString()
	default:
		r, g, b, _ := value.RGBA()
		return fmt.Sprintf("Rgb(%d, %d, %d)", r>>8, g>>8, b>>8)
	}
}

// debugCellModifier is ratatui's Debug of a drawn cell's modifiers.
func debugCellModifier(style uv.Style) string {
	var names []string
	for _, flag := range []struct {
		set  bool
		name string
	}{
		{style.Attrs&uv.AttrBold != 0, "BOLD"},
		{style.Attrs&uv.AttrFaint != 0, "DIM"},
		{style.Attrs&uv.AttrItalic != 0, "ITALIC"},
		{style.Underline != uv.UnderlineStyleNone, "UNDERLINED"},
		{style.Attrs&uv.AttrBlink != 0, "SLOW_BLINK"},
		{style.Attrs&uv.AttrRapidBlink != 0, "RAPID_BLINK"},
		{style.Attrs&uv.AttrReverse != 0, "REVERSED"},
		{style.Attrs&uv.AttrConceal != 0, "HIDDEN"},
		{style.Attrs&uv.AttrStrikethrough != 0, "CROSSED_OUT"},
	} {
		if flag.set {
			names = append(names, flag.name)
		}
	}
	if len(names) == 0 {
		return "NONE"
	}
	return strings.Join(names, " | ")
}
