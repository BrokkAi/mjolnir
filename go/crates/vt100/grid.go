// Port of vt100 0.16.2 src/grid.rs

package vt100

import (
	"bytes"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type gridGrid struct {
	size            gridSize `rs:"size"`
	pos             gridPos  `rs:"pos"`
	savedPos        gridPos  `rs:"saved_pos"`
	rows            []rowRow `rs:"rows"`
	scrollTop       uint16   `rs:"scroll_top"`
	scrollBottom    uint16   `rs:"scroll_bottom"`
	originMode      bool     `rs:"origin_mode"`
	savedOriginMode bool     `rs:"saved_origin_mode"`
	// The scrollback is a VecDeque in Rust: rows are pushed at the back and
	// popped from the front.
	scrollback       []rowRow `rs:"scrollback"`
	scrollbackLen    int      `rs:"scrollback_len"`
	scrollbackOffset int      `rs:"scrollback_offset"`
}

var _ = rs.RegisterStruct[gridGrid]("Grid")

func gridGridNew(size gridSize, scrollbackLen int) gridGrid {
	return gridGrid{
		size:             size,
		pos:              gridPos{},
		savedPos:         gridPos{},
		rows:             []rowRow{},
		scrollTop:        0,
		scrollBottom:     size.rows - 1,
		originMode:       false,
		savedOriginMode:  false,
		scrollback:       []rowRow{},
		scrollbackLen:    scrollbackLen,
		scrollbackOffset: 0,
	}
}

// clone is `grid.clone()`: the rows and the scrollback are copied.
func (self *gridGrid) clone() gridGrid {
	cloned := *self
	cloned.rows = make([]rowRow, len(self.rows))
	for i := range self.rows {
		cloned.rows[i] = self.rows[i].clone()
	}
	cloned.scrollback = make([]rowRow, len(self.scrollback))
	for i := range self.scrollback {
		cloned.scrollback[i] = self.scrollback[i].clone()
	}
	return cloned
}

func (self *gridGrid) allocateRows() {
	if len(self.rows) == 0 {
		for i := 0; i < int(self.size.rows); i++ {
			self.rows = append(self.rows, rowRowNew(self.size.cols))
		}
	}
}

func (self *gridGrid) newRow() rowRow {
	return rowRowNew(self.size.cols)
}

func (self *gridGrid) clear() {
	self.pos = gridPos{}
	self.savedPos = gridPos{}
	for i := range self.rows {
		row := &self.rows[i]
		row.clear(attrsAttrs{})
	}
	self.scrollTop = 0
	self.scrollBottom = self.size.rows - 1
	self.originMode = false
	self.savedOriginMode = false
}

func (self *gridGrid) sizeMethod() gridSize {
	return self.size
}

func (self *gridGrid) setSize(size gridSize) {
	if size.cols != self.size.cols {
		for i := range self.rows {
			row := &self.rows[i]
			row.wrap(false)
		}
	}

	if self.scrollBottom == self.size.rows-1 {
		self.scrollBottom = size.rows - 1
	}

	self.size = size
	for i := range self.rows {
		row := &self.rows[i]
		row.resize(size.cols, cellNew())
	}
	// `self.rows.resize(usize::from(size.rows), self.new_row())`: the new row
	// is made once and cloned for every row that is added.
	if int(size.rows) <= len(self.rows) {
		for i := int(size.rows); i < len(self.rows); i++ {
			self.rows[i] = rowRow{}
		}
		self.rows = self.rows[:int(size.rows)]
	} else {
		for len(self.rows) < int(size.rows) {
			self.rows = append(self.rows, self.newRow())
		}
	}

	if self.scrollBottom >= size.rows {
		self.scrollBottom = size.rows - 1
	}
	if self.scrollBottom < self.scrollTop {
		self.scrollTop = 0
	}

	self.rowClampTop(false)
	self.rowClampBottom(false)
	self.colClamp()

	if self.savedPos.row > self.size.rows-1 {
		self.savedPos.row = self.size.rows - 1
	}
	if self.savedPos.col > self.size.cols-1 {
		self.savedPos.col = self.size.cols - 1
	}
}

func (self *gridGrid) posMethod() gridPos {
	return self.pos
}

func (self *gridGrid) setPos(pos gridPos) {
	if self.originMode {
		pos.row = saturatingAddU16(pos.row, self.scrollTop)
	}
	self.pos = pos
	self.rowClampTop(self.originMode)
	self.rowClampBottom(self.originMode)
	self.colClamp()
}

func (self *gridGrid) saveCursor() {
	self.savedPos = self.pos
	self.savedOriginMode = self.originMode
}

func (self *gridGrid) restoreCursor() {
	self.pos = self.savedPos
	self.originMode = self.savedOriginMode
}

// visibleRowsLen is the number of rows `visible_rows()` yields: first the
// part of the scrollback that is scrolled into view, then the rows of the
// screen that are left.
func (self *gridGrid) visibleRowsLen() (fromScrollback int, fromRows int) {
	rowsLen := len(self.rows)
	// when scrollback_offset > rows_len (e.g. rows = 3,
	// scrollback_len = 10, offset = 9) the skip(10 - 9)
	// will take 9 rows instead of 3. we need to set
	// the upper bound to rows_len (e.g. 3)
	fromScrollback = min(self.scrollbackOffset, rowsLen)
	// same for rows_len - scrollback_offset (e.g. 3 - 9).
	// it'll panic with overflow. we have to saturate the subtraction.
	fromRows = rowsLen - self.scrollbackOffset
	if fromRows < 0 {
		fromRows = 0
	}
	return fromScrollback, fromRows
}

// visibleRows is `visible_rows()`: it calls yield with the index of each
// visible row and the row, until yield returns false.
func (self *gridGrid) visibleRows(yield func(i int, row *rowRow) bool) {
	scrollbackLen := len(self.scrollback)
	fromScrollback, fromRows := self.visibleRowsLen()
	skip := scrollbackLen - self.scrollbackOffset
	for i := 0; i < fromScrollback; i++ {
		if !yield(i, &self.scrollback[skip+i]) {
			return
		}
	}
	for i := 0; i < fromRows; i++ {
		if !yield(fromScrollback+i, &self.rows[i]) {
			return
		}
	}
}

// visibleRowsSlice is `visible_rows()` collected, for the callers that walk
// two grids side by side.
func (self *gridGrid) visibleRowsSlice() []*rowRow {
	fromScrollback, fromRows := self.visibleRowsLen()
	rows := make([]*rowRow, 0, fromScrollback+fromRows)
	self.visibleRows(func(_ int, row *rowRow) bool {
		rows = append(rows, row)
		return true
	})
	return rows
}

func (self *gridGrid) visibleRow(row uint16) rs.Option[*rowRow] {
	fromScrollback, fromRows := self.visibleRowsLen()
	index := int(row)
	if index < fromScrollback {
		return rs.Some(&self.scrollback[len(self.scrollback)-self.scrollbackOffset+index])
	}
	index -= fromScrollback
	if index < fromRows {
		return rs.Some(&self.rows[index])
	}
	return rs.None[*rowRow]()
}

func (self *gridGrid) drawingRow(row uint16) rs.Option[*rowRow] {
	return vecGet(self.rows, uint(row))
}

func (self *gridGrid) drawingRowMut(row uint16) rs.Option[*rowRow] {
	return vecGet(self.rows, uint(row))
}

func (self *gridGrid) currentRowMut() *rowRow {
	return self.drawingRowMut(self.pos.row).
		// we assume self.pos.row is always valid
		Unwrap()
}

func (self *gridGrid) visibleCell(pos gridPos) rs.Option[*Cell] {
	if row, ok := self.visibleRow(pos.row).Get(); ok {
		return row.get(pos.col)
	}
	return rs.None[*Cell]()
}

func (self *gridGrid) drawingCell(pos gridPos) rs.Option[*Cell] {
	if row, ok := self.drawingRow(pos.row).Get(); ok {
		return row.get(pos.col)
	}
	return rs.None[*Cell]()
}

func (self *gridGrid) drawingCellMut(pos gridPos) rs.Option[*Cell] {
	if row, ok := self.drawingRowMut(pos.row).Get(); ok {
		return row.getMut(pos.col)
	}
	return rs.None[*Cell]()
}

func (self *gridGrid) scrollbackLenMethod() int {
	return self.scrollbackLen
}

func (self *gridGrid) scrollbackMethod() int {
	return self.scrollbackOffset
}

func (self *gridGrid) setScrollback(rows int) {
	self.scrollbackOffset = min(rows, len(self.scrollback))
}

func (self *gridGrid) writeContents(contents *[]uint8) {
	wrapping := false
	self.visibleRows(func(_ int, row *rowRow) bool {
		row.writeContents(contents, 0, self.size.cols, wrapping)
		if !row.wrappedMethod() {
			*contents = append(*contents, '\n')
		}
		wrapping = row.wrappedMethod()
		return true
	})

	for bytes.HasSuffix(*contents, []uint8{'\n'}) {
		*contents = (*contents)[:len(*contents)-1]
	}
}

func (self *gridGrid) writeContentsFormatted(
	contents *[]uint8,
) attrsAttrs {
	termClearAttrs{}.writeBuf(contents)
	termClearScreen{}.writeBuf(contents)

	prevAttrs := attrsAttrs{}
	prevPos := gridPos{}
	wrapping := false
	self.visibleRows(func(i_ int, row *rowRow) bool {
		// we limit the number of cols to a u16 (see Size), so
		// visible_rows() can never return more rows than will fit
		i := uint16(i_)
		newPos, newAttrs := row.writeContentsFormatted(
			contents,
			0,
			self.size.cols,
			i,
			wrapping,
			rs.Some(prevPos),
			rs.Some(prevAttrs),
		)
		prevPos = newPos
		prevAttrs = newAttrs
		wrapping = row.wrappedMethod()
		return true
	})

	self.writeCursorPositionFormatted(
		contents,
		rs.Some(prevPos),
		rs.Some(prevAttrs),
	)

	return prevAttrs
}

func (self *gridGrid) writeContentsDiff(
	contents *[]uint8,
	prev *gridGrid,
	prevAttrs attrsAttrs,
) attrsAttrs {
	prevPos := prev.pos
	wrapping := false
	prevWrapping := false
	rows, prevRows := self.visibleRowsSlice(), prev.visibleRowsSlice()
	for i_ := 0; i_ < len(rows) && i_ < len(prevRows); i_++ {
		row, prevRow := rows[i_], prevRows[i_]
		// we limit the number of cols to a u16 (see Size), so
		// visible_rows() can never return more rows than will fit
		i := uint16(i_)
		newPos, newAttrs := row.writeContentsDiff(
			contents,
			prevRow,
			0,
			self.size.cols,
			i,
			wrapping,
			prevWrapping,
			prevPos,
			prevAttrs,
		)
		prevPos = newPos
		prevAttrs = newAttrs
		wrapping = row.wrappedMethod()
		prevWrapping = prevRow.wrappedMethod()
	}

	self.writeCursorPositionFormatted(
		contents,
		rs.Some(prevPos),
		rs.Some(prevAttrs),
	)

	return prevAttrs
}

func (self *gridGrid) writeCursorPositionFormatted(
	contents *[]uint8,
	prevPos rs.Option[gridPos],
	prevAttrs_ rs.Option[attrsAttrs],
) {
	prevAttrs := prevAttrs_.UnwrapOrDefault()
	// writing a character to the last column of a row doesn't wrap the
	// cursor immediately - it waits until the next character is actually
	// drawn. it is only possible for the cursor to have this kind of
	// position after drawing a character though, so if we end in this
	// position, we need to redraw the character at the end of the row.
	if prevPos != rs.Some(self.pos) && self.pos.col >= self.size.cols {
		pos := gridPos{
			row: self.pos.row,
			col: self.size.cols - 1,
		}
		if self.
			drawingCell(pos).
			// we assume self.pos.row is always valid, and self.size.cols
			// - 1 is always a valid column
			Unwrap().
			IsWideContinuation() {
			pos.col = self.size.cols - 2
		}
		cell :=
			// we assume self.pos.row is always valid, and self.size.cols
			// - 2 must be a valid column because self.size.cols - 1 is
			// always valid and we just checked that the cell at
			// self.size.cols - 1 is a wide continuation character, which
			// means that the first half of the wide character must be
			// before it
			self.drawingCell(pos).Unwrap()
		if cell.HasContents() {
			if prevPos, ok := prevPos.Get(); ok {
				termMoveFromToNew(prevPos, pos).
					writeBuf(contents)
			} else {
				termMoveToNew(pos).writeBuf(contents)
			}
			cell.attrsMethod().writeEscapeCodeDiff(contents, &prevAttrs)
			*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
			prevAttrs.writeEscapeCodeDiff(contents, cell.attrsMethod())
		} else {
			// if the cell doesn't have contents, we can't have gotten
			// here by drawing a character in the last column. this means
			// that as far as i'm aware, we have to have reached here from
			// a newline when we were already after the end of an earlier
			// row. in the case where we are already after the end of an
			// earlier row, we can just write a few newlines, otherwise we
			// also need to do the same as above to get ourselves to after
			// the end of a row.
			found := false
			for i_ := int(self.pos.row) - 1; i_ >= 0; i_-- {
				i := uint16(i_)
				pos.row = i
				pos.col = self.size.cols - 1
				if self.
					drawingCell(pos).
					// i is always less than self.pos.row, which we assume
					// to be always valid, so it must also be valid.
					// self.size.cols - 1 is always a valid col.
					Unwrap().
					IsWideContinuation() {
					pos.col = self.size.cols - 2
				}
				cell := self.
					drawingCell(pos).
					// i is always less than self.pos.row, which we assume
					// to be always valid, so it must also be valid.
					// self.size.cols - 2 is valid because self.size.cols
					// - 1 is always valid, and col gets set to
					// self.size.cols - 2 when the cell at self.size.cols
					// - 1 is a wide continuation character, meaning that
					// the first half of the wide character must be before
					// it
					Unwrap()
				if cell.HasContents() {
					if prevPos, ok := prevPos.Get(); ok {
						if prevPos.row != i ||
							prevPos.col < self.size.cols {
							termMoveFromToNew(prevPos, pos).
								writeBuf(contents)
							cell.attrsMethod().writeEscapeCodeDiff(
								contents,
								&prevAttrs,
							)
							*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
							prevAttrs.writeEscapeCodeDiff(
								contents,
								cell.attrsMethod(),
							)
						}
					} else {
						termMoveToNew(pos).writeBuf(contents)
						cell.attrsMethod().writeEscapeCodeDiff(
							contents,
							&prevAttrs,
						)
						*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
						prevAttrs.writeEscapeCodeDiff(
							contents,
							cell.attrsMethod(),
						)
					}
					*contents = append(*contents,
						bytes.Repeat([]uint8{'\n'}, int(self.pos.row-i))...,
					)
					found = true
					break
				}
			}

			// this can happen if you get the cursor off the end of a row,
			// and then do something to clear the end of the current row
			// without moving the cursor (IL, DL, ED, EL, etc). we know
			// there can't be something in the last column because we
			// would have caught that above, so it should be safe to
			// overwrite it.
			if !found {
				pos = gridPos{
					row: self.pos.row,
					col: self.size.cols - 1,
				}
				if prevPos, ok := prevPos.Get(); ok {
					termMoveFromToNew(prevPos, pos).
						writeBuf(contents)
				} else {
					termMoveToNew(pos).writeBuf(contents)
				}
				*contents = append(*contents, ' ')
				// we know that the cell has no contents, but it still may
				// have drawing attributes (background color, etc)
				endCell := self.
					drawingCell(pos).
					// we assume self.pos.row is always valid, and
					// self.size.cols - 1 is always a valid column
					Unwrap()
				endCell.
					attrsMethod().
					writeEscapeCodeDiff(contents, &prevAttrs)
				termSaveCursor{}.writeBuf(contents)
				termBackspace{}.writeBuf(contents)
				termEraseCharNew(1).writeBuf(contents)
				termRestoreCursor{}.writeBuf(contents)
				prevAttrs.
					writeEscapeCodeDiff(contents, endCell.attrsMethod())
			}
		}
	} else if prevPos, ok := prevPos.Get(); ok {
		termMoveFromToNew(prevPos, self.pos).
			writeBuf(contents)
	} else {
		termMoveToNew(self.pos).writeBuf(contents)
	}
}

func (self *gridGrid) eraseAll(attrs attrsAttrs) {
	for i := range self.rows {
		row := &self.rows[i]
		row.clear(attrs)
	}
}

func (self *gridGrid) eraseAllForward(attrs attrsAttrs) {
	pos := self.pos
	for i := int(pos.row) + 1; i < len(self.rows); i++ {
		row := &self.rows[i]
		row.clear(attrs)
	}

	self.eraseRowForward(attrs)
}

func (self *gridGrid) eraseAllBackward(attrs attrsAttrs) {
	pos := self.pos
	for i := 0; i < int(pos.row) && i < len(self.rows); i++ {
		row := &self.rows[i]
		row.clear(attrs)
	}

	self.eraseRowBackward(attrs)
}

func (self *gridGrid) eraseRow(attrs attrsAttrs) {
	self.currentRowMut().clear(attrs)
}

func (self *gridGrid) eraseRowForward(attrs attrsAttrs) {
	size := self.size
	pos := self.pos
	row := self.currentRowMut()
	for col := uint32(pos.col); col < uint32(size.cols); col++ {
		row.erase(uint16(col), attrs)
	}
}

func (self *gridGrid) eraseRowBackward(attrs attrsAttrs) {
	size := self.size
	pos := self.pos
	row := self.currentRowMut()
	for col := uint32(0); col <= uint32(min(pos.col, size.cols-1)); col++ {
		row.erase(uint16(col), attrs)
	}
}

func (self *gridGrid) insertCells(count uint16) {
	size := self.size
	pos := self.pos
	wide := pos.col < size.cols &&
		self.
			drawingCell(pos).
			// we assume self.pos.row is always valid, and we know we are
			// not off the end of a row because we just checked pos.col <
			// size.cols
			Unwrap().
			IsWideContinuation()
	row := self.currentRowMut()
	for i := uint32(0); i < uint32(count); i++ {
		if wide {
			row.getMut(pos.col).Unwrap().setWideContinuation(false)
		}
		row.insert(pos.col, cellNew())
		if wide {
			row.getMut(pos.col).Unwrap().setWideContinuation(true)
		}
	}
	row.truncate(size.cols)
}

func (self *gridGrid) deleteCells(count uint16) {
	size := self.size
	pos := self.pos
	row := self.currentRowMut()
	for i := uint32(0); i < uint32(min(count, size.cols-pos.col)); i++ {
		row.remove(pos.col)
	}
	row.resize(size.cols, cellNew())
}

func (self *gridGrid) eraseCells(count uint16, attrs attrsAttrs) {
	size := self.size
	pos := self.pos
	row := self.currentRowMut()
	for col := uint32(pos.col); col < uint32(min(saturatingAddU16(pos.col, count), size.cols)); col++ {
		row.erase(uint16(col), attrs)
	}
}

func (self *gridGrid) insertLines(count uint16) {
	for i := uint32(0); i < uint32(count); i++ {
		vecRemove(&self.rows, uint(self.scrollBottom))
		vecInsert(&self.rows, uint(self.pos.row), self.newRow())
		// self.scroll_bottom is maintained to always be a valid row
		vecIndex(self.rows, uint(self.scrollBottom)).wrap(false)
	}
}

func (self *gridGrid) deleteLines(count uint16) {
	for i := uint32(0); i < uint32(min(count, self.size.rows-self.pos.row)); i++ {
		vecInsert(&self.rows, uint(self.scrollBottom)+1, self.newRow())
		vecRemove(&self.rows, uint(self.pos.row))
	}
}

func (self *gridGrid) scrollUp(count uint16) {
	for i := uint32(0); i < uint32(min(count, self.size.rows-self.scrollTop)); i++ {
		vecInsert(&self.rows, uint(self.scrollBottom)+1, self.newRow())
		removed := vecRemove(&self.rows, uint(self.scrollTop))
		if self.scrollbackLen > 0 && !self.scrollRegionActive() {
			self.scrollback = append(self.scrollback, removed)
			for len(self.scrollback) > self.scrollbackLen {
				// pop_front
				self.scrollback[0] = rowRow{}
				self.scrollback = self.scrollback[1:]
			}
			if self.scrollbackOffset > 0 {
				self.scrollbackOffset =
					min(len(self.scrollback), self.scrollbackOffset+1)
			}
		}
	}
}

func (self *gridGrid) scrollDown(count uint16) {
	for i := uint32(0); i < uint32(count); i++ {
		vecRemove(&self.rows, uint(self.scrollBottom))
		vecInsert(&self.rows, uint(self.scrollTop), self.newRow())
		// self.scroll_bottom is maintained to always be a valid row
		vecIndex(self.rows, uint(self.scrollBottom)).wrap(false)
	}
}

func (self *gridGrid) setScrollRegion(top uint16, bottom uint16) {
	bottom = min(bottom, self.sizeMethod().rows-1)
	if top < bottom {
		self.scrollTop = top
		self.scrollBottom = bottom
	} else {
		self.scrollTop = 0
		self.scrollBottom = self.sizeMethod().rows - 1
	}
	self.pos.row = self.scrollTop
	self.pos.col = 0
}

func (self *gridGrid) inScrollRegion() bool {
	return self.pos.row >= self.scrollTop && self.pos.row <= self.scrollBottom
}

func (self *gridGrid) scrollRegionActive() bool {
	return self.scrollTop != 0 || self.scrollBottom != self.size.rows-1
}

func (self *gridGrid) setOriginMode(mode bool) {
	self.originMode = mode
	self.setPos(gridPos{row: 0, col: 0})
}

func (self *gridGrid) rowIncClamp(count uint16) {
	inScrollRegion := self.inScrollRegion()
	self.pos.row = saturatingAddU16(self.pos.row, count)
	self.rowClampBottom(inScrollRegion)
}

func (self *gridGrid) rowIncScroll(count uint16) uint16 {
	inScrollRegion := self.inScrollRegion()
	self.pos.row = saturatingAddU16(self.pos.row, count)
	lines := self.rowClampBottom(inScrollRegion)
	if inScrollRegion {
		self.scrollUp(lines)
		return lines
	} else {
		return 0
	}
}

func (self *gridGrid) rowDecClamp(count uint16) {
	inScrollRegion := self.inScrollRegion()
	self.pos.row = saturatingSubU16(self.pos.row, count)
	self.rowClampTop(inScrollRegion)
}

func (self *gridGrid) rowDecScroll(count uint16) {
	inScrollRegion := self.inScrollRegion()
	// need to account for clamping by both row_clamp_top and by
	// saturating_sub
	extraLines := saturatingSubU16(count, self.pos.row)
	self.pos.row = saturatingSubU16(self.pos.row, count)
	lines := self.rowClampTop(inScrollRegion)
	self.scrollDown(lines + extraLines)
}

func (self *gridGrid) rowSet(i uint16) {
	self.pos.row = i
	self.rowClamp()
}

func (self *gridGrid) colInc(count uint16) {
	self.pos.col = saturatingAddU16(self.pos.col, count)
}

func (self *gridGrid) colIncClamp(count uint16) {
	self.pos.col = saturatingAddU16(self.pos.col, count)
	self.colClamp()
}

func (self *gridGrid) colDec(count uint16) {
	self.pos.col = saturatingSubU16(self.pos.col, count)
}

func (self *gridGrid) colTab() {
	self.pos.col -= self.pos.col % 8
	self.pos.col += 8
	self.colClamp()
}

func (self *gridGrid) colSet(i uint16) {
	self.pos.col = i
	self.colClamp()
}

func (self *gridGrid) colWrap(width uint16, wrap bool) {
	if self.pos.col > self.size.cols-width {
		prevPos := self.pos
		self.pos.col = 0
		scrolled := self.rowIncScroll(1)
		prevPos.row -= scrolled
		newPos := self.pos
		self.drawingRowMut(prevPos.row).
			// we assume self.pos.row is always valid, and so prev_pos.row
			// must be valid because it is always less than or equal to
			// self.pos.row
			Unwrap().
			wrap(wrap && prevPos.row+1 == newPos.row)
	}
}

func (self *gridGrid) rowClampTop(limitToScrollRegion bool) uint16 {
	if limitToScrollRegion && self.pos.row < self.scrollTop {
		rows := self.scrollTop - self.pos.row
		self.pos.row = self.scrollTop
		return rows
	} else {
		return 0
	}
}

func (self *gridGrid) rowClampBottom(limitToScrollRegion bool) uint16 {
	var bottom uint16
	if limitToScrollRegion {
		bottom = self.scrollBottom
	} else {
		bottom = self.size.rows - 1
	}
	if self.pos.row > bottom {
		rows := self.pos.row - bottom
		self.pos.row = bottom
		return rows
	} else {
		return 0
	}
}

func (self *gridGrid) rowClamp() {
	if self.pos.row > self.size.rows-1 {
		self.pos.row = self.size.rows - 1
	}
}

func (self *gridGrid) colClamp() {
	if self.pos.col > self.size.cols-1 {
		self.pos.col = self.size.cols - 1
	}
}

type gridSize struct {
	rows uint16 `rs:"rows"`
	cols uint16 `rs:"cols"`
}

var _ = rs.RegisterStruct[gridSize]("Size")

type gridPos struct {
	row uint16 `rs:"row"`
	col uint16 `rs:"col"`
}

var _ = rs.RegisterStruct[gridPos]("Pos")
