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
	"fmt"
	"strings"
	"unicode"

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
	doubledHorizontal := margin.Horizontal * 2
	doubledVertical := margin.Vertical * 2
	if r.Dx() < doubledHorizontal || r.Dy() < doubledVertical {
		return uv.Rectangle{}
	}
	return uv.Rect(
		r.Min.X+margin.Horizontal,
		r.Min.Y+margin.Vertical,
		r.Dx()-doubledHorizontal,
		r.Dy()-doubledVertical,
	)
}

// RectIntersection is ratatui's `Rect::intersection`: unlike
// image.Rectangle.Intersect, an empty overlap keeps its corner.
func RectIntersection(r uv.Rectangle, other uv.Rectangle) uv.Rectangle {
	x1 := max(r.Min.X, other.Min.X)
	y1 := max(r.Min.Y, other.Min.Y)
	x2 := min(r.Max.X, other.Max.X)
	y2 := min(r.Max.Y, other.Max.Y)
	return uv.Rect(x1, y1, max(x2-x1, 0), max(y2-y1, 0))
}

// RectClamp moves and shrinks r so it lies inside other (ratatui's
// `Rect::clamp`).
func RectClamp(r uv.Rectangle, other uv.Rectangle) uv.Rectangle {
	width := min(r.Dx(), other.Dx())
	height := min(r.Dy(), other.Dy())
	x := max(min(r.Min.X, max(other.Max.X-width, 0)), other.Min.X)
	y := max(min(r.Min.Y, max(other.Max.Y-height, 0)), other.Min.Y)
	return uv.Rect(x, y, width, height)
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
// is visible (ratatui's StatefulWidget for List, with its defaults: top to
// bottom, no scroll padding, the highlight symbol's column reserved only
// while an item is selected).
func (l List) RenderStateful(area uv.Rectangle, buf *uv.Buffer, state *ListState) {
	BufferSetStyle(buf, area, l.Style)
	listArea := area
	if l.Block != nil {
		l.Block.Render(area, buf)
		listArea = l.Block.Inner(area)
	}
	if listArea.Dx() == 0 || listArea.Dy() == 0 {
		return
	}
	if len(l.Items) == 0 {
		state.selected = nil
		return
	}
	// If the selected index is out of bounds, set it to the last item.
	if state.selected != nil && *state.selected >= len(l.Items) {
		last := len(l.Items) - 1
		state.selected = &last
	}
	first, last := l.itemsBounds(state.selected, state.offset, listArea.Dy())
	// This changes the state's offset to the beginning of the now viewable
	// items.
	state.offset = first

	var highlightSymbol Line
	if l.HighlightSymbol != "" {
		highlightSymbol = RawLine(l.HighlightSymbol)
	}
	symbolWidth := highlightSymbol.Width()
	emptySymbol := RawLine(strings.Repeat(" ", symbolWidth))
	selectionSpacing := state.selected != nil

	currentHeight := 0
	for i := first; i < last; i++ {
		item := l.Items[i]
		x, y := listArea.Min.X, listArea.Min.Y+currentHeight
		currentHeight += item.Height()
		rowArea := uv.Rect(x, y, listArea.Dx(), item.Height())
		BufferSetStyle(buf, rowArea, l.Style.Patch(item.Style))
		isSelected := state.selected != nil && *state.selected == i
		itemArea := rowArea
		if selectionSpacing {
			itemArea = uv.Rect(rowArea.Min.X+symbolWidth, rowArea.Min.Y, max(rowArea.Dx()-symbolWidth, 0), rowArea.Dy())
		}
		RenderText(item.Content, itemArea, buf)
		if isSelected {
			BufferSetStyle(buf, rowArea, l.HighlightStyle)
		}
		if selectionSpacing {
			for j := range item.Content.Height() {
				line := emptySymbol
				if isSelected && j == 0 {
					line = highlightSymbol
				}
				line.Render(uv.Rect(x, y+j, symbolWidth, 1), buf)
			}
		}
	}
}

// itemsBounds is ratatui's `List::get_items_bounds` without scroll padding:
// the first item shown and one past the last, keeping the selected item (or
// else the offset) in view.
func (l List) itemsBounds(selected *int, offset int, maxHeight int) (int, int) {
	offset = min(offset, max(len(l.Items)-1, 0))
	first, last := offset, offset
	heightFromOffset := 0
	for _, item := range l.Items[offset:] {
		if heightFromOffset+item.Height() > maxHeight {
			break
		}
		heightFromOffset += item.Height()
		last++
	}
	indexToDisplay := offset
	if selected != nil {
		indexToDisplay = min(*selected, len(l.Items)-1)
	}
	for indexToDisplay >= last {
		heightFromOffset += l.Items[last].Height()
		last++
		for heightFromOffset > maxHeight {
			heightFromOffset = max(heightFromOffset-l.Items[first].Height(), 0)
			first++
		}
	}
	for indexToDisplay < first {
		first--
		heightFromOffset += l.Items[first].Height()
		for heightFromOffset > maxHeight {
			last--
			heightFromOffset = max(heightFromOffset-l.Items[last].Height(), 0)
		}
	}
	return first, last
}

// RenderText draws text into area as ratatui's Widget for Text does: the
// text's style over the area, then one line per row, each aligned by its own
// alignment or else the text's.
func RenderText(text Text, area uv.Rectangle, buf *uv.Buffer) {
	area = RectIntersection(area, buf.Bounds())
	BufferSetStyle(buf, area, text.Style)
	for row, line := range text.Lines {
		if row >= area.Dy() {
			break
		}
		if line.Alignment == 0 {
			line.Alignment = text.Alignment
		}
		line.Render(uv.Rect(area.Min.X, area.Min.Y+row, area.Dx(), 1), buf)
	}
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
// constraints as ratatui's layout solver does (Layout::split; see
// widgets_layout_solver.go). Like ratatui, it panics if the solver fails,
// which well-formed constraints never make it do.
func (l Layout) Split(area uv.Rectangle) []uv.Rectangle {
	rects, err := solveLayout(l, area)
	if err != nil {
		panic(fmt.Sprintf("failed to split: %v", err))
	}
	return rects
}

// BufferSetStyle patches style onto every cell of area (Buffer::set_style).
func BufferSetStyle(buf *uv.Buffer, area uv.Rectangle, style Style) {
	area = RectIntersection(area, buf.Bounds())
	for y := area.Min.Y; y < area.Max.Y; y++ {
		for x := area.Min.X; x < area.Max.X; x++ {
			if cell := buf.CellAt(x, y); cell != nil {
				style.ApplyTo(&cell.Style)
			}
		}
	}
}

// BufferSetCell sets the symbol of the cell at (x, y) and patches style onto
// it (`buffer[(x, y)].set_symbol(..).set_style(..)`). A position outside the
// buffer is ignored, where ratatui would panic.
func BufferSetCell(buf *uv.Buffer, x, y int, symbol string, style Style) {
	cell := buf.CellAt(x, y)
	if cell == nil {
		return
	}
	next := *cell
	next.Content = symbol
	next.Width = max(displayWidth(symbol), 1)
	style.ApplyTo(&next.Style)
	buf.SetCell(x, y, &next)
}

// BufferSetString writes text from (x, y) in style, at most maxWidth cells
// and clipped to the buffer, and returns the position after it
// (Buffer::set_stringn).
//
// Graphemes holding a control character and zero-width graphemes are
// skipped. A wide grapheme is one uv cell of its width followed by uv's
// zero-width placeholders, which stand for the cells ratatui resets behind
// it (so BufferLines reads each as ratatui's blank symbol, a space).
func BufferSetString(buf *uv.Buffer, x, y int, text string, maxWidth int, style Style) (int, int) {
	remaining := max(min(buf.Bounds().Max.X-x, maxWidth), 0)
	for _, symbol := range componentGraphemes(text) {
		if strings.ContainsFunc(symbol, unicode.IsControl) {
			continue
		}
		width := displayWidth(symbol)
		if width == 0 {
			continue
		}
		if width > remaining {
			break
		}
		remaining -= width
		next := uv.Cell{Content: symbol, Width: width}
		if cell := buf.CellAt(x, y); cell != nil {
			next.Style = cell.Style
			next.Link = cell.Link
		}
		style.ApplyTo(&next.Style)
		buf.SetCell(x, y, &next)
		x += width
	}
	return x, y
}

// BufferSetLine writes line from (x, y), at most maxWidth cells, and returns
// the position after it (Buffer::set_line).
func BufferSetLine(buf *uv.Buffer, x, y int, line Line, maxWidth int) (int, int) {
	remaining := maxWidth
	for _, span := range line.Spans {
		if remaining <= 0 {
			break
		}
		nextX, _ := BufferSetString(buf, x, y, span.Content, remaining, line.Style.Patch(span.Style))
		remaining = max(remaining-max(nextX-x, 0), 0)
		x = nextX
	}
	return x, y
}
