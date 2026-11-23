// Mjolnir-local stand-ins for the ratatui list widget, layout solver and
// buffer helpers mj-chat draws with: Margin and the Rect helpers, List,
// ListItem, ListState, StatefulWidget, Layout with its Constraints, and the
// Buffer cell setters.
//
// ratatui is not ported (see "The terminal UI on Charm" in
// .agents/docs/go-port/core-brief.md). This file ports no Rust module; it
// completes chat-2's styled.go (Style, Span, Line, Text, Alignment) and
// widgets.go (Frame, Widget, Clear, Block, Paragraph) with the pieces those
// leave out, following the same conventions: exported fields, `With*`
// builders, uv.Rectangle areas, and cells that match ratatui 0.30
// (ratatui-core 0.1, ratatui-widgets 0.3) so the copied golden files stay
// unchanged.
//
// Declared by chat-3 alongside components/, whose controls, dialogs and
// form layouts draw lists and split areas.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// Margin is ratatui's Margin: cells kept clear on the left and right
// (Horizontal) and on the top and bottom (Vertical).
type Margin struct {
	Horizontal int
	Vertical   int
}

// RectInner shrinks r by margin on each side, or returns the empty
// rectangle at r's origin when the margins do not fit (ratatui's
// `Rect::inner`).
func RectInner(r uv.Rectangle, margin Margin) uv.Rectangle {
	panic("unported: ratatui-core layout/rect.rs Rect::inner")
}

// RectClamp moves and shrinks r so it lies inside other (ratatui's
// `Rect::clamp`).
func RectClamp(r uv.Rectangle, other uv.Rectangle) uv.Rectangle {
	panic("unported: ratatui-core layout/rect.rs Rect::clamp")
}

// StatefulWidget is ratatui's StatefulWidget: a widget that reads and
// updates state (a list's scroll offset) while it draws.
type StatefulWidget[S any] interface {
	RenderStateful(area uv.Rectangle, buf *uv.Buffer, state *S)
}

// RenderStatefulWidget draws a stateful widget into area (ratatui's
// `Frame::render_stateful_widget`).
func RenderStatefulWidget[S any](frame *Frame, widget StatefulWidget[S], area uv.Rectangle, state *S) {
	widget.RenderStateful(area, frame.Buffer(), state)
}

// ListItem is one item of a [List] (ratatui's ListItem).
type ListItem struct {
	Content Text
	Style   Style
}

// NewListItem is ListItem::new.
func NewListItem(content Text) ListItem {
	return ListItem{Content: content}
}

// WithStyle is ListItem::style.
func (i ListItem) WithStyle(style Style) ListItem {
	i.Style = style
	return i
}

// Height is the number of rows the item takes.
func (i ListItem) Height() int {
	return i.Content.Height()
}

// List is ratatui's List widget, drawn top to bottom.
type List struct {
	Items           []ListItem
	Block           *Block
	Style           Style
	HighlightStyle  Style
	HighlightSymbol string
}

// NewList is List::new.
func NewList(items []ListItem) List {
	return List{Items: items}
}

// WithBlock is List::block.
func (l List) WithBlock(block Block) List {
	l.Block = &block
	return l
}

// WithStyle is List::style.
func (l List) WithStyle(style Style) List {
	l.Style = style
	return l
}

// WithHighlightStyle is List::highlight_style.
func (l List) WithHighlightStyle(style Style) List {
	l.HighlightStyle = style
	return l
}

// WithHighlightSymbol is List::highlight_symbol.
func (l List) WithHighlightSymbol(symbol string) List {
	l.HighlightSymbol = symbol
	return l
}

// Render draws the list without selection (ratatui's Widget for List).
func (l List) Render(area uv.Rectangle, buf *uv.Buffer) {
	state := ListState{}
	l.RenderStateful(area, buf, &state)
}

// RenderStateful draws the list, moving state's offset so the selected item
// is visible (ratatui's StatefulWidget for List).
func (l List) RenderStateful(area uv.Rectangle, buf *uv.Buffer, state *ListState) {
	panic("unported: ratatui-widgets list/rendering.rs List::render")
}

// ListState is ratatui's ListState: the first visible item and the selected
// one.
type ListState struct {
	offset   int
	selected *int
}

// WithOffset is ListState::with_offset.
func (s ListState) WithOffset(offset int) ListState {
	s.offset = offset
	return s
}

// WithSelected is ListState::with_selected; nil selects nothing.
func (s ListState) WithSelected(selected *int) ListState {
	s.selected = selected
	return s
}

// Offset is the first visible item.
func (s ListState) Offset() int {
	return s.offset
}

// Selected is the selected item, if any.
func (s ListState) Selected() (int, bool) {
	if s.selected == nil {
		return 0, false
	}
	return *s.selected, true
}

// Direction is ratatui's layout Direction. The zero value is ratatui's
// default, Vertical.
type Direction uint8

const (
	DirectionVertical Direction = iota
	DirectionHorizontal
)

// Constraint is ratatui's layout Constraint.
type Constraint interface {
	isConstraint()
}

// ConstraintMin is Constraint::Min.
type ConstraintMin int

// ConstraintMax is Constraint::Max.
type ConstraintMax int

// ConstraintLength is Constraint::Length.
type ConstraintLength int

// ConstraintPercentage is Constraint::Percentage.
type ConstraintPercentage int

// ConstraintFill is Constraint::Fill.
type ConstraintFill int

// ConstraintRatio is Constraint::Ratio.
type ConstraintRatio struct {
	Num int
	Den int
}

func (ConstraintMin) isConstraint()        {}
func (ConstraintMax) isConstraint()        {}
func (ConstraintLength) isConstraint()     {}
func (ConstraintPercentage) isConstraint() {}
func (ConstraintFill) isConstraint()       {}
func (ConstraintRatio) isConstraint()      {}

// Flex is ratatui's layout Flex: where space left over after the
// constraints goes. The zero value is ratatui's default, Start.
type Flex uint8

const (
	FlexStart Flex = iota
	FlexLegacy
	FlexEnd
	FlexCenter
	FlexSpaceBetween
	FlexSpaceEvenly
	FlexSpaceAround
)

// Layout is ratatui's Layout: it splits an area by constraints.
type Layout struct {
	Direction   Direction
	Constraints []Constraint
	Margin      Margin
	Flex        Flex
	Spacing     int
}

// NewLayout is Layout::new (and Layout::default().direction(..)
// .constraints(..)).
func NewLayout(direction Direction, constraints []Constraint) Layout {
	return Layout{Direction: direction, Constraints: constraints}
}

// VerticalLayout is Layout::vertical.
func VerticalLayout(constraints ...Constraint) Layout {
	return NewLayout(DirectionVertical, constraints)
}

// HorizontalLayout is Layout::horizontal.
func HorizontalLayout(constraints ...Constraint) Layout {
	return NewLayout(DirectionHorizontal, constraints)
}

// WithMargin is Layout::margin: margin cells clear on every side.
func (l Layout) WithMargin(margin int) Layout {
	l.Margin = Margin{Horizontal: margin, Vertical: margin}
	return l
}

// WithFlex is Layout::flex.
func (l Layout) WithFlex(flex Flex) Layout {
	l.Flex = flex
	return l
}

// WithSpacing is Layout::spacing.
func (l Layout) WithSpacing(spacing int) Layout {
	l.Spacing = spacing
	return l
}

// Split divides area into one rectangle per constraint, solving the
// constraints as ratatui's layout solver does (Layout::split).
func (l Layout) Split(area uv.Rectangle) []uv.Rectangle {
	panic("unported: ratatui-core layout/layout.rs Layout::split")
}

// BufferSetStyle patches style onto every cell of area (Buffer::set_style).
func BufferSetStyle(buf *uv.Buffer, area uv.Rectangle, style Style) {
	panic("unported: ratatui-core buffer/buffer.rs Buffer::set_style")
}

// BufferSetCell sets the symbol of the cell at (x, y) and patches style onto
// it (`buffer[(x, y)].set_symbol(..).set_style(..)`).
func BufferSetCell(buf *uv.Buffer, x, y int, symbol string, style Style) {
	panic("unported: ratatui-core buffer/cell.rs Cell::set_symbol")
}

// BufferSetString writes text from (x, y) in style, at most maxWidth cells
// and clipped to the buffer, and returns the position after it
// (Buffer::set_stringn).
func BufferSetString(buf *uv.Buffer, x, y int, text string, maxWidth int, style Style) (int, int) {
	panic("unported: ratatui-core buffer/buffer.rs Buffer::set_stringn")
}

// BufferSetLine writes line from (x, y), at most maxWidth cells, and returns
// the position after it (Buffer::set_line).
func BufferSetLine(buf *uv.Buffer, x, y int, line Line, maxWidth int) (int, int) {
	panic("unported: ratatui-core buffer/buffer.rs Buffer::set_line")
}
