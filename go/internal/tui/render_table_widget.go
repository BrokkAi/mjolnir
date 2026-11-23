// Mjolnir-local stand-in for ratatui's Table widget (ratatui-widgets 0.3.0,
// the version the Rust build pins): Table, its rows and cells, TableState and
// HighlightSpacing.
//
// Only the Sessions and Targets panes (render/sessions.rs and
// render/capacity.rs) draw tables, so the widget lives beside them, as the
// brief's "The terminal UI on Charm" asks. It must produce the cells ratatui
// produces so the copied golden files stay unchanged. The names carry a Table
// prefix so they cannot collide with other Rust types ported into package tui
// (dialogs/container.rs has its own Row, and std::cell::Cell is common).
package tui

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// TableHighlightSpacing is ratatui's HighlightSpacing: when a table reserves
// the column its highlight symbol draws in. The zero value is ratatui's
// default, WhenSelected.
type TableHighlightSpacing uint8

const (
	// TableHighlightSpacingWhenSelected reserves the column only while a row
	// is selected.
	TableHighlightSpacingWhenSelected TableHighlightSpacing = iota
	// TableHighlightSpacingAlways always reserves the column, so the content
	// does not shift when a row is selected.
	TableHighlightSpacingAlways
	// TableHighlightSpacingNever never reserves it, and never draws the
	// symbol.
	TableHighlightSpacingNever
)

// shouldAdd is HighlightSpacing::should_add.
func (s TableHighlightSpacing) shouldAdd(hasSelection bool) bool {
	switch s {
	case TableHighlightSpacingAlways:
		return true
	case TableHighlightSpacingWhenSelected:
		return hasSelection
	default:
		return false
	}
}

// TableCell is ratatui's table Cell: text and a style patched under it.
type TableCell struct {
	Content chat.Text
	Style   chat.Style
}

// NewTableCell is Cell::from(Text).
func NewTableCell(content chat.Text) TableCell { return TableCell{Content: content} }

// TableCellFromLine is Cell::from(Line).
func TableCellFromLine(line chat.Line) TableCell { return NewTableCell(chat.TextFrom(line)) }

// TableCellFromString is Cell::from(&str) and Cell::from(String): the text
// split into lines as Text::raw does.
func TableCellFromString(content string) TableCell { return NewTableCell(chat.RawText(content)) }

// WithStyle is Cell::style.
func (c TableCell) WithStyle(style chat.Style) TableCell { c.Style = style; return c }

// render is Cell::render: the cell's style over area, then its text.
func (c TableCell) render(area uv.Rectangle, buf *uv.Buffer) {
	chat.BufferSetStyle(buf, area, c.Style)
	chat.RenderText(c.Content, area, buf)
}

// TableRow is ratatui's table Row: cells, a height in lines, margins above
// and below, and a style patched under the whole row.
type TableRow struct {
	Cells        []TableCell
	Height       int
	TopMargin    int
	BottomMargin int
	Style        chat.Style
}

// NewTableRow is Row::new: the cells at a height of one line.
func NewTableRow(cells ...TableCell) TableRow { return TableRow{Cells: cells, Height: 1} }

// NewTableRowFromStrings is Row::new over strings, each cell Text::raw.
func NewTableRowFromStrings(cells ...string) TableRow {
	row := TableRow{Height: 1}
	for _, cell := range cells {
		row.Cells = append(row.Cells, TableCellFromString(cell))
	}
	return row
}

// WithHeight is Row::height.
func (r TableRow) WithHeight(height int) TableRow { r.Height = height; return r }

// WithTopMargin is Row::top_margin.
func (r TableRow) WithTopMargin(margin int) TableRow { r.TopMargin = margin; return r }

// WithBottomMargin is Row::bottom_margin.
func (r TableRow) WithBottomMargin(margin int) TableRow { r.BottomMargin = margin; return r }

// WithStyle is Row::style.
func (r TableRow) WithStyle(style chat.Style) TableRow { r.Style = style; return r }

// heightWithMargin is Row::height_with_margin.
func (r TableRow) heightWithMargin() int { return r.Height + r.TopMargin + r.BottomMargin }

// TableState is ratatui's TableState: the first row drawn, and the selected
// row and column. The zero value is TableState::default().
type TableState struct {
	offset         int
	selected       *int
	selectedColumn *int
}

// WithOffset is TableState::with_offset.
func (s TableState) WithOffset(offset int) TableState { s.offset = offset; return s }

// WithSelected is TableState::with_selected; nil selects nothing.
func (s TableState) WithSelected(selected *int) TableState { s.selected = selected; return s }

// Offset is the first row the table drew.
func (s TableState) Offset() int { return s.offset }

// Selected is the selected row, if any.
func (s TableState) Selected() (int, bool) {
	if s.selected == nil {
		return 0, false
	}
	return *s.selected, true
}

// Select is TableState::select; nil clears the selection and resets the
// offset, as ratatui does.
func (s *TableState) Select(index *int) {
	s.selected = index
	if index == nil {
		s.offset = 0
	}
}

// SelectColumn is TableState::select_column.
func (s *TableState) SelectColumn(index *int) { s.selectedColumn = index }

// Table is ratatui's Table widget.
type Table struct {
	Rows                 []TableRow
	Header               *TableRow
	Footer               *TableRow
	Widths               []chat.Constraint
	ColumnSpacing        int
	Block                *chat.Block
	Style                chat.Style
	RowHighlightStyle    chat.Style
	ColumnHighlightStyle chat.Style
	CellHighlightStyle   chat.Style
	HighlightSymbol      chat.Text
	HighlightSpacing     TableHighlightSpacing
	Flex                 chat.Flex
}

// NewTable is Table::new: rows with column width constraints, a column
// spacing of one cell and no highlight symbol.
func NewTable(rows []TableRow, widths []chat.Constraint) Table {
	return Table{Rows: rows, Widths: widths, ColumnSpacing: 1, Flex: chat.FlexStart}
}

// WithHeader is Table::header.
func (t Table) WithHeader(header TableRow) Table { t.Header = &header; return t }

// WithFooter is Table::footer.
func (t Table) WithFooter(footer TableRow) Table { t.Footer = &footer; return t }

// WithColumnSpacing is Table::column_spacing.
func (t Table) WithColumnSpacing(spacing int) Table { t.ColumnSpacing = spacing; return t }

// WithBlock is Table::block.
func (t Table) WithBlock(block chat.Block) Table { t.Block = &block; return t }

// WithStyle is Table::style.
func (t Table) WithStyle(style chat.Style) Table { t.Style = style; return t }

// WithRowHighlightStyle is Table::row_highlight_style.
func (t Table) WithRowHighlightStyle(style chat.Style) Table { t.RowHighlightStyle = style; return t }

// WithHighlightSymbol is Table::highlight_symbol(&str): the symbol as
// Text::raw.
func (t Table) WithHighlightSymbol(symbol string) Table {
	t.HighlightSymbol = chat.RawText(symbol)
	return t
}

// WithHighlightSpacing is Table::highlight_spacing.
func (t Table) WithHighlightSpacing(spacing TableHighlightSpacing) Table {
	t.HighlightSpacing = spacing
	return t
}

// WithFlex is Table::flex.
func (t Table) WithFlex(flex chat.Flex) Table { t.Flex = flex; return t }

// Render draws the table with a fresh state (ratatui's Widget for Table).
func (t Table) Render(area uv.Rectangle, buf *uv.Buffer) {
	var state TableState
	t.RenderStateful(area, buf, &state)
}

// RenderStateful draws the table and updates state's offset to the first
// row drawn (ratatui's StatefulWidget for Table).
func (t Table) RenderStateful(area uv.Rectangle, buf *uv.Buffer, state *TableState) {
	chat.BufferSetStyle(buf, area, t.Style)
	tableArea := area
	if t.Block != nil {
		t.Block.Render(area, buf)
		tableArea = t.Block.Inner(area)
	}
	if tableArea.Empty() {
		return
	}
	if selected, ok := state.Selected(); ok && selected >= len(t.Rows) {
		last := max(len(t.Rows)-1, 0)
		state.Select(&last)
	}
	if len(t.Rows) == 0 {
		state.Select(nil)
	}
	columnCount := t.columnCount()
	if state.selectedColumn != nil && *state.selectedColumn >= columnCount {
		last := max(columnCount-1, 0)
		state.SelectColumn(&last)
	}
	if columnCount == 0 {
		state.SelectColumn(nil)
	}
	selectionWidth := t.selectionWidth(state)
	columnWidths := t.columnWidths(tableArea.Dx(), selectionWidth, columnCount)
	headerArea, rowsArea, footerArea := t.layout(tableArea)
	t.renderHeaderOrFooter(t.Header, headerArea, buf, columnWidths)
	t.renderRows(rowsArea, buf, state, selectionWidth, columnWidths)
	t.renderHeaderOrFooter(t.Footer, footerArea, buf, columnWidths)
}

// tableColumn is one column's x offset from the table's left edge and its
// width.
type tableColumn struct {
	x     int
	width int
}

// layout is Table::layout: the header, rows and footer areas.
func (t Table) layout(area uv.Rectangle) (header, rows, footer uv.Rectangle) {
	margins := func(row *TableRow) (top, height, bottom int) {
		if row == nil {
			return 0, 0, 0
		}
		return row.TopMargin, row.Height, row.BottomMargin
	}
	headerTop, headerHeight, headerBottom := margins(t.Header)
	footerTop, footerHeight, footerBottom := margins(t.Footer)
	areas := chat.VerticalLayout(
		chat.ConstraintLength(headerTop),
		chat.ConstraintLength(headerHeight),
		chat.ConstraintLength(headerBottom),
		chat.ConstraintMin(0),
		chat.ConstraintLength(footerTop),
		chat.ConstraintLength(footerHeight),
		chat.ConstraintLength(footerBottom),
	).Split(area)
	return areas[1], areas[3], areas[5]
}

// renderHeaderOrFooter is Table::render_header and Table::render_footer.
func (t Table) renderHeaderOrFooter(row *TableRow, area uv.Rectangle, buf *uv.Buffer, columns []tableColumn) {
	if row == nil {
		return
	}
	chat.BufferSetStyle(buf, area, row.Style)
	for i, column := range columns {
		if i >= len(row.Cells) {
			break
		}
		row.Cells[i].render(uv.Rect(area.Min.X+column.x, area.Min.Y, column.width, area.Dy()), buf)
	}
}

// renderRows is Table::render_rows.
func (t Table) renderRows(area uv.Rectangle, buf *uv.Buffer, state *TableState, selectionWidth int, columns []tableColumn) {
	if len(t.Rows) == 0 {
		return
	}
	start, end := t.visibleRows(state, area)
	state.offset = start
	yOffset := 0
	var selectedRowArea *uv.Rectangle
	for i := start; i < end; i++ {
		row := t.Rows[i]
		y := area.Min.Y + yOffset + row.TopMargin
		height := max(min(y+row.Height, area.Max.Y)-y, 0)
		rowArea := uv.Rect(area.Min.X, y, area.Dx(), height)
		chat.BufferSetStyle(buf, rowArea, row.Style)
		isSelected := state.selected != nil && *state.selected == i
		if selectionWidth > 0 && isSelected {
			selectionArea := uv.Rect(rowArea.Min.X, rowArea.Min.Y, selectionWidth, rowArea.Dy())
			chat.BufferSetStyle(buf, selectionArea, row.Style)
			chat.RenderText(t.HighlightSymbol, selectionArea, buf)
		}
		for c, column := range columns {
			if c >= len(row.Cells) {
				break
			}
			row.Cells[c].render(uv.Rect(rowArea.Min.X+column.x, rowArea.Min.Y, column.width, rowArea.Dy()), buf)
		}
		if isSelected {
			selected := rowArea
			selectedRowArea = &selected
		}
		yOffset += row.heightWithMargin()
	}
	var selectedColumnArea *uv.Rectangle
	if state.selectedColumn != nil && *state.selectedColumn < len(columns) {
		column := columns[*state.selectedColumn]
		columnArea := uv.Rect(area.Min.X+column.x, area.Min.Y, column.width, area.Dy())
		selectedColumnArea = &columnArea
	}
	switch {
	case selectedRowArea != nil && selectedColumnArea != nil:
		chat.BufferSetStyle(buf, *selectedRowArea, t.RowHighlightStyle)
		chat.BufferSetStyle(buf, *selectedColumnArea, t.ColumnHighlightStyle)
		chat.BufferSetStyle(buf, chat.RectIntersection(*selectedRowArea, *selectedColumnArea), t.CellHighlightStyle)
	case selectedRowArea != nil:
		chat.BufferSetStyle(buf, *selectedRowArea, t.RowHighlightStyle)
	case selectedColumnArea != nil:
		chat.BufferSetStyle(buf, *selectedColumnArea, t.ColumnHighlightStyle)
	}
}

// visibleRows is Table::visible_rows: the first row to draw and the row
// after the last, keeping the selected row on screen.
func (t Table) visibleRows(state *TableState, area uv.Rectangle) (int, int) {
	lastRow := max(len(t.Rows)-1, 0)
	start := min(state.offset, lastRow)
	if state.selected != nil {
		start = min(start, *state.selected)
	}
	end := start
	height := 0
	for _, item := range t.Rows[start:] {
		if height+item.Height > area.Dy() {
			break
		}
		height += item.heightWithMargin()
		end++
	}
	if state.selected != nil {
		selected := min(*state.selected, lastRow)
		for selected >= end {
			height += t.Rows[end].heightWithMargin()
			end++
			for height > area.Dy() {
				height = max(height-t.Rows[start].heightWithMargin(), 0)
				start++
			}
		}
	}
	if height < area.Dy() && end < len(t.Rows) {
		end++
	}
	return start, end
}

// columnWidths is Table::get_column_widths: each column's x offset within
// the table and its width, after the selection column.
func (t Table) columnWidths(maxWidth, selectionWidth, columnCount int) []tableColumn {
	widths := t.Widths
	if len(widths) == 0 {
		widths = make([]chat.Constraint, columnCount)
		for i := range widths {
			widths[i] = chat.ConstraintLength(maxWidth / max(columnCount, 1))
		}
	}
	areas := chat.HorizontalLayout(chat.ConstraintLength(selectionWidth), chat.ConstraintFill(0)).
		Split(uv.Rect(0, 0, maxWidth, 1))
	rects := chat.HorizontalLayout(widths...).WithFlex(t.Flex).WithSpacing(t.ColumnSpacing).Split(areas[1])
	columns := make([]tableColumn, len(rects))
	for i, rect := range rects {
		columns[i] = tableColumn{x: rect.Min.X, width: rect.Dx()}
	}
	return columns
}

// columnCount is Table::column_count: the most cells any row, the header or
// the footer has.
func (t Table) columnCount() int {
	count := 0
	for _, row := range t.Rows {
		count = max(count, len(row.Cells))
	}
	if t.Header != nil {
		count = max(count, len(t.Header.Cells))
	}
	if t.Footer != nil {
		count = max(count, len(t.Footer.Cells))
	}
	return count
}

// selectionWidth is Table::selection_width: the highlight symbol's width
// when the spacing reserves its column.
func (t Table) selectionWidth(state *TableState) int {
	if !t.HighlightSpacing.shouldAdd(state.selected != nil) {
		return 0
	}
	width := 0
	for _, line := range t.HighlightSymbol.Lines {
		width = max(width, line.Width())
	}
	return width
}
