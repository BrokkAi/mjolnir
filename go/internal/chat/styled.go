// Mjolnir-local stand-ins for the ratatui text and style types mj-chat draws
// with: Color, Modifier, Style, Span, Line, Text and Alignment.
//
// ratatui is not ported (see "The terminal UI on Charm" in
// .agents/docs/go-port/core-brief.md). These keep ratatui's semantics where
// Mjolnir depends on them: a Style is a patch (unset colors leave the cell's
// color alone, modifiers are added and removed), Color::Reset differs from
// no color, and Debug output matches ratatui's so golden files stay
// unchanged. Cells are drawn into an ultraviolet buffer; Style.ApplyTo
// patches a cell's uv.Style.
//
// Declared by chat-2 alongside theme.rs, the module that hands most of these
// out; this is the one copy for package chat.
package chat

import (
	"fmt"
	"image/color"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"
)

// Color is ratatui's Color: Reset, one of the sixteen named ANSI colors, a
// 256-color index, or RGB. The zero value is no color at all, Rust's `None`
// in a Style field, which leaves whatever color a cell already has.
//
// Encoding: 0 unset, 1 Reset, 2..17 the named colors (ANSI index + 2),
// 0x100|n Indexed(n), 0x1000000|rrggbb Rgb.
type Color uint32

const (
	colorIndexedFlag Color = 0x100
	colorRGBFlag     Color = 0x1000000
)

const (
	ColorReset Color = iota + 1
	ColorBlack
	ColorRed
	ColorGreen
	ColorYellow
	ColorBlue
	ColorMagenta
	ColorCyan
	ColorGray
	ColorDarkGray
	ColorLightRed
	ColorLightGreen
	ColorLightYellow
	ColorLightBlue
	ColorLightMagenta
	ColorLightCyan
	ColorWhite
)

// namedColorDebugNames are the Debug names of ColorReset..ColorWhite.
var namedColorDebugNames = [...]string{
	"Reset", "Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan",
	"Gray", "DarkGray", "LightRed", "LightGreen", "LightYellow", "LightBlue",
	"LightMagenta", "LightCyan", "White",
}

// ColorRGB is Color::Rgb(red, green, blue).
func ColorRGB(red, green, blue uint8) Color {
	return colorRGBFlag | Color(red)<<16 | Color(green)<<8 | Color(blue)
}

// ColorIndexed is Color::Indexed(index), a 256-color index (`38;5;n`).
func ColorIndexed(index uint8) Color { return colorIndexedFlag | Color(index) }

// IsSet reports whether the color is present (Rust `Some`).
func (c Color) IsSet() bool { return c != 0 }

// RGB returns the channels of an Rgb color.
func (c Color) RGB() (red, green, blue uint8, ok bool) {
	if c&colorRGBFlag == 0 {
		return 0, 0, 0, false
	}
	return uint8(c >> 16), uint8(c >> 8), uint8(c), true
}

// Index returns the index of an Indexed color.
func (c Color) Index() (uint8, bool) {
	if c&colorRGBFlag != 0 || c&colorIndexedFlag == 0 {
		return 0, false
	}
	return uint8(c), true
}

// GoString is ratatui's Debug: `Reset`, `Yellow`, `Indexed(6)`,
// `Rgb(15, 18, 20)`.
func (c Color) GoString() string {
	if r, g, b, ok := c.RGB(); ok {
		return fmt.Sprintf("Rgb(%d, %d, %d)", r, g, b)
	}
	if index, ok := c.Index(); ok {
		return fmt.Sprintf("Indexed(%d)", index)
	}
	if c >= ColorReset && c <= ColorWhite {
		return namedColorDebugNames[c-ColorReset]
	}
	return "None"
}

// TerminalColor is the color a cell is drawn with: nil for Reset (the
// terminal's default) and for no color.
func (c Color) TerminalColor() color.Color {
	if r, g, b, ok := c.RGB(); ok {
		return color.RGBA{R: r, G: g, B: b, A: 0xff}
	}
	if index, ok := c.Index(); ok {
		return ansi.IndexedColor(index)
	}
	if c >= ColorBlack && c <= ColorWhite {
		return ansi.BasicColor(c - ColorBlack)
	}
	return nil
}

// Modifier is ratatui's Modifier bit set, in ratatui's bit order.
type Modifier uint16

const (
	ModifierBold Modifier = 1 << iota
	ModifierDim
	ModifierItalic
	ModifierUnderlined
	ModifierSlowBlink
	ModifierRapidBlink
	ModifierReversed
	ModifierHidden
	ModifierCrossedOut
)

// Contains reports whether every bit of other is set.
func (m Modifier) Contains(other Modifier) bool { return m&other == other }

// Intersects reports whether any bit of other is set.
func (m Modifier) Intersects(other Modifier) bool { return m&other != 0 }

// Style is ratatui's Style: a patch over whatever a cell already holds.
// Unset colors leave the cell's color; AddModifier is turned on and
// SubModifier turned off. The zero value is Style::default().
type Style struct {
	Fg             Color
	Bg             Color
	UnderlineColor Color
	AddModifier    Modifier
	SubModifier    Modifier
}

// Foreground is Style::fg.
func (s Style) Foreground(c Color) Style { s.Fg = c; return s }

// Background is Style::bg.
func (s Style) Background(c Color) Style { s.Bg = c; return s }

// WithUnderlineColor is Style::underline_color.
func (s Style) WithUnderlineColor(c Color) Style { s.UnderlineColor = c; return s }

// Add is Style::add_modifier.
func (s Style) Add(m Modifier) Style {
	s.SubModifier &^= m
	s.AddModifier |= m
	return s
}

// Remove is Style::remove_modifier.
func (s Style) Remove(m Modifier) Style {
	s.AddModifier &^= m
	s.SubModifier |= m
	return s
}

// Patch is Style::patch: other's set colors and modifiers win.
func (s Style) Patch(other Style) Style {
	if other.Fg.IsSet() {
		s.Fg = other.Fg
	}
	if other.Bg.IsSet() {
		s.Bg = other.Bg
	}
	if other.UnderlineColor.IsSet() {
		s.UnderlineColor = other.UnderlineColor
	}
	s.AddModifier = (s.AddModifier &^ other.SubModifier) | other.AddModifier
	s.SubModifier = (s.SubModifier &^ other.AddModifier) | other.SubModifier
	return s
}

func (s Style) Bold() Style       { return s.Add(ModifierBold) }
func (s Style) Dim() Style        { return s.Add(ModifierDim) }
func (s Style) Italic() Style     { return s.Add(ModifierItalic) }
func (s Style) Underlined() Style { return s.Add(ModifierUnderlined) }
func (s Style) Reversed() Style   { return s.Add(ModifierReversed) }
func (s Style) CrossedOut() Style { return s.Add(ModifierCrossedOut) }

// GoString is ratatui's Debug, which prints only what differs from the
// default: `Style::new().fg(Color::Reset).bold().reversed()`.
func (s Style) GoString() string {
	panic("unported: ratatui-core style.rs Style::fmt_stylize")
}

// ApplyTo patches a drawn cell's style the way ratatui's Cell::set_style
// does: set colors replace the cell's (Reset clears to the terminal
// default), AddModifier attributes are turned on and SubModifier ones off.
func (s Style) ApplyTo(cell *uv.Style) {
	panic("unported: ratatui-core buffer/cell.rs Cell::set_style")
}

// Alignment is ratatui's horizontal Alignment. The zero value is unset, as
// a Line's `Option<Alignment>` is: the line follows its paragraph.
type Alignment uint8

const (
	AlignmentLeft Alignment = iota + 1
	AlignmentCenter
	AlignmentRight
)

// Span is ratatui's Span: one run of text in one style.
type Span struct {
	Content string
	Style   Style
}

// RawSpan is Span::raw.
func RawSpan(content string) Span { return Span{Content: content} }

// StyledSpan is Span::styled.
func StyledSpan(content string, style Style) Span {
	return Span{Content: content, Style: style}
}

// Width is the span's display width in terminal cells.
func (s Span) Width() int { return displayWidth(s.Content) }

// PatchStyle is Span::patch_style.
func (s Span) PatchStyle(style Style) Span { s.Style = s.Style.Patch(style); return s }

// Line is ratatui's Line: spans drawn on one row, a style patched under
// every span, and an optional alignment.
type Line struct {
	Spans     []Span
	Style     Style
	Alignment Alignment
}

// LineFrom is Line::from(Vec<Span>) and Line::from(Span).
func LineFrom(spans ...Span) Line { return Line{Spans: spans} }

// RawLine is Line::raw (and Line::from(&str)): one unstyled span per line
// of content, as Rust's str::lines splits it.
func RawLine(content string) Line { return Line{Spans: rawLineSpans(content)} }

// StyledLine is Line::styled.
func StyledLine(content string, style Style) Line {
	return Line{Spans: rawLineSpans(content), Style: style}
}

// rawLineSpans splits content as Rust's str::lines does (on "\n", dropping one
// trailing "\r" per line and the empty piece after a final newline), one
// raw span per piece.
func rawLineSpans(content string) []Span {
	var spans []Span
	for _, line := range rustStrLines(content) {
		spans = append(spans, RawSpan(line))
	}
	return spans
}

// rustStrLines is Rust's str::lines.
func rustStrLines(content string) []string {
	if content == "" {
		return nil
	}
	pieces := strings.Split(strings.TrimSuffix(content, "\n"), "\n")
	for i, piece := range pieces {
		pieces[i] = strings.TrimSuffix(piece, "\r")
	}
	return pieces
}

// Width is the sum of the spans' display widths.
func (l Line) Width() int {
	width := 0
	for _, span := range l.Spans {
		width += span.Width()
	}
	return width
}

// String is the line's text, its spans' contents joined (ratatui's Display).
func (l Line) String() string {
	var text strings.Builder
	for _, span := range l.Spans {
		text.WriteString(span.Content)
	}
	return text.String()
}

// WithStyle is Line::style.
func (l Line) WithStyle(style Style) Line { l.Style = style; return l }

// PatchStyle is Line::patch_style.
func (l Line) PatchStyle(style Style) Line { l.Style = l.Style.Patch(style); return l }

// WithAlignment is Line::alignment.
func (l Line) WithAlignment(alignment Alignment) Line { l.Alignment = alignment; return l }

func (l Line) LeftAligned() Line  { return l.WithAlignment(AlignmentLeft) }
func (l Line) Centered() Line     { return l.WithAlignment(AlignmentCenter) }
func (l Line) RightAligned() Line { return l.WithAlignment(AlignmentRight) }

// Render draws the line on the first row of area, as ratatui's Widget for
// Line does.
func (l Line) Render(area uv.Rectangle, buf *uv.Buffer) {
	panic("unported: ratatui-core text/line.rs Line::render")
}

// Text is ratatui's Text: lines, a style under all of them, and an optional
// alignment.
type Text struct {
	Lines     []Line
	Style     Style
	Alignment Alignment
}

// TextFrom is Text::from(Vec<Line>) and Text::from(Line).
func TextFrom(lines ...Line) Text { return Text{Lines: lines} }

// RawText is Text::raw (and Text::from(&str)): one line per line of content,
// and a single empty line for empty content.
func RawText(content string) Text {
	if content == "" {
		return Text{Lines: []Line{{}}}
	}
	pieces := rustStrLines(content)
	lines := make([]Line, len(pieces))
	for i, piece := range pieces {
		lines[i] = RawLine(piece)
	}
	return Text{Lines: lines}
}

// Height is the number of lines.
func (t Text) Height() int { return len(t.Lines) }
