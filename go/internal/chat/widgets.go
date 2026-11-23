// Mjolnir-local stand-ins for the ratatui frame and the few widgets mj-chat
// draws: Frame, Widget, Clear, Block (borders and titles) and Paragraph
// (with wrap and scroll).
//
// They draw into an ultraviolet buffer and must produce the cells ratatui
// produces for Mjolnir's screens, so the copied Rust golden files stay
// unchanged (see "The terminal UI on Charm" in
// .agents/docs/go-port/core-brief.md). Rectangles are uv.Rectangle and
// positions uv.Position.
//
// Declared by chat-2 alongside theme.rs, whose panel and modal hand out
// Blocks; this is the one copy for package chat.
package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Widget draws itself into area of buf (ratatui's Widget).
type Widget interface {
	Render(area uv.Rectangle, buf *uv.Buffer)
}

// Frame is what one terminal frame draws into: the buffer and where the
// cursor ends up (ratatui's Frame).
type Frame struct {
	buffer *uv.Buffer
	cursor *uv.Position
}

// NewFrame is a frame drawing into buffer, with the cursor hidden.
func NewFrame(buffer *uv.Buffer) *Frame { return &Frame{buffer: buffer} }

// Area is the whole frame.
func (f *Frame) Area() uv.Rectangle { return f.buffer.Bounds() }

// Buffer is the buffer the frame draws into (Frame::buffer_mut).
func (f *Frame) Buffer() *uv.Buffer { return f.buffer }

// RenderWidget draws widget into area.
func (f *Frame) RenderWidget(widget Widget, area uv.Rectangle) { widget.Render(area, f.buffer) }

// SetCursorPosition shows the cursor at position after the frame is drawn.
func (f *Frame) SetCursorPosition(position uv.Position) { f.cursor = &position }

// CursorPosition is where the frame put the cursor, if it showed it.
func (f *Frame) CursorPosition() (uv.Position, bool) {
	if f.cursor == nil {
		return uv.Position{}, false
	}
	return *f.cursor, true
}

// Clear resets every cell of its area to an empty cell (ratatui's Clear).
type Clear struct{}

func (Clear) Render(area uv.Rectangle, buf *uv.Buffer) {
	buf.ClearArea(area.Intersect(buf.Bounds()))
}

// Borders is ratatui's Borders bit set, in ratatui's bit order.
type Borders uint8

const (
	BordersNone   Borders = 0
	BordersTop    Borders = 1 << 0
	BordersRight  Borders = 1 << 1
	BordersBottom Borders = 1 << 2
	BordersLeft   Borders = 1 << 3
	BordersAll            = BordersTop | BordersRight | BordersBottom | BordersLeft
)

// BorderSet is ratatui's symbols::border::Set: the glyphs of a border.
type BorderSet struct {
	TopLeft          string
	TopRight         string
	BottomLeft       string
	BottomRight      string
	VerticalLeft     string
	VerticalRight    string
	HorizontalTop    string
	HorizontalBottom string
}

// BorderType is ratatui's BorderType, the border sets Mjolnir draws with.
type BorderType uint8

const (
	BorderTypePlain BorderType = iota
	BorderTypeRounded
	BorderTypeDouble
	BorderTypeThick
)

// Set is the glyphs of the border type (BorderType::to_border_set).
func (t BorderType) Set() BorderSet {
	switch t {
	case BorderTypeRounded:
		return BorderSet{"╭", "╮", "╰", "╯", "│", "│", "─", "─"}
	case BorderTypeDouble:
		return BorderSet{"╔", "╗", "╚", "╝", "║", "║", "═", "═"}
	case BorderTypeThick:
		return BorderSet{"┏", "┓", "┗", "┛", "┃", "┃", "━", "━"}
	default:
		return BorderSet{"┌", "┐", "└", "┘", "│", "│", "─", "─"}
	}
}

// Padding is ratatui's Padding: blank cells inside a block's borders.
type Padding struct {
	Left   int
	Right  int
	Top    int
	Bottom int
}

// TitlePosition is ratatui's title Position. The zero value is unset: the
// title follows the block's TitlesPosition.
type TitlePosition uint8

const (
	TitlePositionTop TitlePosition = iota + 1
	TitlePositionBottom
)

// BlockTitle is one title of a Block and the border it sits on.
type BlockTitle struct {
	Position TitlePosition
	Line     Line
}

// Block is ratatui's Block: a bordered box with titles on its top and
// bottom borders. Build one with NewBlock or BorderedBlock; the zero value
// has no border glyphs.
type Block struct {
	Titles          []BlockTitle
	TitlesStyle     Style
	TitlesAlignment Alignment
	TitlesPosition  TitlePosition
	Borders         Borders
	BorderStyle     Style
	BorderSet       BorderSet
	Style           Style
	Padding         Padding
}

// NewBlock is Block::new (and Block::default): no borders, plain glyphs,
// titles on top and left aligned.
func NewBlock() Block {
	return Block{
		TitlesAlignment: AlignmentLeft,
		TitlesPosition:  TitlePositionTop,
		BorderSet:       BorderTypePlain.Set(),
	}
}

// BorderedBlock is Block::bordered.
func BorderedBlock() Block { return NewBlock().WithBorders(BordersAll) }

// WithTitle is Block::title: a title on the block's TitlesPosition.
func (b Block) WithTitle(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Line: title})
	return b
}

// WithTitleTop is Block::title_top.
func (b Block) WithTitleTop(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Position: TitlePositionTop, Line: title})
	return b
}

// WithTitleBottom is Block::title_bottom.
func (b Block) WithTitleBottom(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Position: TitlePositionBottom, Line: title})
	return b
}

// WithTitleStyle is Block::title_style.
func (b Block) WithTitleStyle(style Style) Block { b.TitlesStyle = style; return b }

// WithTitleAlignment is Block::title_alignment.
func (b Block) WithTitleAlignment(alignment Alignment) Block {
	b.TitlesAlignment = alignment
	return b
}

// WithTitlePosition is Block::title_position.
func (b Block) WithTitlePosition(position TitlePosition) Block {
	b.TitlesPosition = position
	return b
}

// WithBorders is Block::borders.
func (b Block) WithBorders(borders Borders) Block { b.Borders = borders; return b }

// WithBorderStyle is Block::border_style.
func (b Block) WithBorderStyle(style Style) Block { b.BorderStyle = style; return b }

// WithBorderType is Block::border_type.
func (b Block) WithBorderType(borderType BorderType) Block {
	b.BorderSet = borderType.Set()
	return b
}

// WithBorderSet is Block::border_set.
func (b Block) WithBorderSet(set BorderSet) Block { b.BorderSet = set; return b }

// WithStyle is Block::style.
func (b Block) WithStyle(style Style) Block { b.Style = style; return b }

// WithPadding is Block::padding.
func (b Block) WithPadding(padding Padding) Block { b.Padding = padding; return b }

// Inner is the area inside the block's borders, title rows and padding.
func (b Block) Inner(area uv.Rectangle) uv.Rectangle {
	panic("unported: ratatui-widgets block.rs Block::inner")
}

// Render draws the block's style, borders and titles.
func (b Block) Render(area uv.Rectangle, buf *uv.Buffer) {
	panic("unported: ratatui-widgets block.rs Block::render")
}

// Wrap is ratatui's Wrap: Trim drops leading whitespace of wrapped rows.
type Wrap struct {
	Trim bool
}

// Paragraph is ratatui's Paragraph: text, optionally wrapped and scrolled,
// optionally inside a block.
type Paragraph struct {
	Block     *Block
	Style     Style
	Wrap      *Wrap
	Text      Text
	ScrollY   int
	ScrollX   int
	Alignment Alignment
}

// NewParagraph is Paragraph::new.
func NewParagraph(text Text) Paragraph {
	return Paragraph{Text: text, Alignment: AlignmentLeft}
}

// WithBlock is Paragraph::block.
func (p Paragraph) WithBlock(block Block) Paragraph { p.Block = &block; return p }

// WithStyle is Paragraph::style.
func (p Paragraph) WithStyle(style Style) Paragraph { p.Style = style; return p }

// WithWrap is Paragraph::wrap.
func (p Paragraph) WithWrap(wrap Wrap) Paragraph { p.Wrap = &wrap; return p }

// WithScroll is Paragraph::scroll((y, x)).
func (p Paragraph) WithScroll(y, x int) Paragraph { p.ScrollY, p.ScrollX = y, x; return p }

// WithAlignment is Paragraph::alignment.
func (p Paragraph) WithAlignment(alignment Alignment) Paragraph {
	p.Alignment = alignment
	return p
}

// LineCount is how many rows the paragraph takes at width, block included
// (Paragraph::line_count).
func (p Paragraph) LineCount(width int) int {
	panic("unported: ratatui-widgets paragraph.rs Paragraph::line_count")
}

// Render draws the paragraph.
func (p Paragraph) Render(area uv.Rectangle, buf *uv.Buffer) {
	panic("unported: ratatui-widgets paragraph.rs Paragraph::render")
}
