// Port of vt100 0.16.2 src/row.rs

package vt100

import (
	"bytes"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type rowRow struct {
	cells   []Cell `rs:"cells"`
	wrapped bool   `rs:"wrapped"`
}

var _ = rs.RegisterStruct[rowRow]("Row")

func rowRowNew(cols uint16) rowRow {
	return rowRow{
		cells:   make([]Cell, int(cols)),
		wrapped: false,
	}
}

// clone is `row.clone()`.
func (self *rowRow) clone() rowRow {
	return rowRow{
		cells:   append(make([]Cell, 0, len(self.cells)), self.cells...),
		wrapped: self.wrapped,
	}
}

func (self *rowRow) cols() uint16 {
	// we limit the number of cols to a u16 (see Size)
	return uint16(len(self.cells))
}

func (self *rowRow) clear(attrs attrsAttrs) {
	for i := range self.cells {
		cell := &self.cells[i]
		cell.clear(attrs)
	}
	self.wrapped = false
}

func (self *rowRow) get(col uint16) rs.Option[*Cell] {
	return vecGet(self.cells, uint(col))
}

func (self *rowRow) getMut(col uint16) rs.Option[*Cell] {
	return vecGet(self.cells, uint(col))
}

func (self *rowRow) insert(i uint16, cell Cell) {
	vecInsert(&self.cells, uint(i), cell)
	self.wrapped = false
}

func (self *rowRow) remove(i uint16) {
	self.clearWide(i)
	vecRemove(&self.cells, uint(i))
	self.wrapped = false
}

func (self *rowRow) erase(i uint16, attrs attrsAttrs) {
	wide := vecIndex(self.cells, uint(i)).IsWide()
	self.clearWide(i)
	vecIndex(self.cells, uint(i)).clear(attrs)
	var offset uint16 = 1
	if wide {
		offset = 2
	}
	if i == self.cols()-offset {
		self.wrapped = false
	}
}

func (self *rowRow) truncate(len_ uint16) {
	if int(len_) < len(self.cells) {
		self.cells = self.cells[:int(len_)]
	}
	self.wrapped = false
	lastCell := vecIndex(self.cells, uint(len_)-1)
	if lastCell.IsWide() {
		lastCell.clear(*lastCell.attrsMethod())
	}
}

func (self *rowRow) resize(len_ uint16, cell Cell) {
	if int(len_) <= len(self.cells) {
		self.cells = self.cells[:int(len_)]
	} else {
		for len(self.cells) < int(len_) {
			self.cells = append(self.cells, cell)
		}
	}
	self.wrapped = false
}

func (self *rowRow) wrap(wrap bool) {
	self.wrapped = wrap
}

func (self *rowRow) wrappedMethod() bool {
	return self.wrapped
}

func (self *rowRow) clearWide(col uint16) {
	cell := vecIndex(self.cells, uint(col))
	var other *Cell
	if cell.IsWide() {
		other = vecIndex(self.cells, uint(col+1))
	} else if cell.IsWideContinuation() {
		other = vecIndex(self.cells, uint(col-1))
	} else {
		return
	}
	other.clear(*other.attrsMethod())
}

func (self *rowRow) writeContents(
	contents *[]uint8,
	start uint16,
	width uint16,
	wrapping bool,
) {
	prevWasWide := false

	prevCol := start
	for index := int(start); index < len(self.cells) && index-int(start) < int(width); index++ {
		cell := &self.cells[index]
		if prevWasWide {
			prevWasWide = false
			continue
		}
		prevWasWide = cell.IsWide()

		// we limit the number of cols to a u16 (see Size)
		col := uint16(index)
		if cell.HasContents() {
			for i := uint16(0); i < col-prevCol; i++ {
				*contents = append(*contents, ' ')
			}
			prevCol += col - prevCol

			*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
			if cell.IsWide() {
				prevCol += 2
			} else {
				prevCol += 1
			}
		}
	}
	if prevCol == start && wrapping {
		*contents = append(*contents, '\n')
	}
}

func (self *rowRow) writeContentsFormatted(
	contents *[]uint8,
	start uint16,
	width uint16,
	row uint16,
	wrapping bool,
	prevPos_ rs.Option[gridPos],
	prevAttrs_ rs.Option[attrsAttrs],
) (gridPos, attrsAttrs) {
	prevWasWide := false
	defaultCell := cellNew()

	prevPos := prevPos_.UnwrapOrElse(func() gridPos {
		if wrapping {
			return gridPos{
				row: row - 1,
				col: self.cols(),
			}
		} else {
			return gridPos{row: row, col: start}
		}
	})
	prevAttrs := prevAttrs_.UnwrapOrDefault()

	firstCell := vecIndex(self.cells, uint(start))
	if wrapping && firstCell.Equal(&defaultCell) {
		defaultAttrs := defaultCell.attrsMethod()
		if prevAttrs != *defaultAttrs {
			defaultAttrs.writeEscapeCodeDiff(contents, &prevAttrs)
			prevAttrs = *defaultAttrs
		}
		*contents = append(*contents, ' ')
		termBackspace{}.writeBuf(contents)
		termEraseCharNew(1).writeBuf(contents)
		prevPos = gridPos{row: row, col: 0}
	}

	erase := rs.None[rs.Tuple2[uint16, attrsAttrs]]()
	for index := int(start); index < len(self.cells) && index-int(start) < int(width); index++ {
		cell := &self.cells[index]
		if prevWasWide {
			prevWasWide = false
			continue
		}
		prevWasWide = cell.IsWide()

		// we limit the number of cols to a u16 (see Size)
		col := uint16(index)
		pos := gridPos{row: row, col: col}

		if pending, ok := erase.Get(); ok {
			prevCol, attrs := pending.F0, pending.F1
			if cell.HasContents() || *cell.attrsMethod() != attrs {
				newPos := gridPos{row: row, col: prevCol}
				if wrapping &&
					prevPos.row+1 == newPos.row &&
					prevPos.col >= self.cols() {
					if newPos.col > 0 {
						*contents = append(*contents, bytes.Repeat([]uint8{' '}, int(newPos.col))...)
					} else {
						*contents = append(*contents, ' ')
						termBackspace{}.writeBuf(contents)
					}
				} else {
					termMoveFromToNew(prevPos, newPos).
						writeBuf(contents)
				}
				prevPos = newPos
				if prevAttrs != attrs {
					attrs.writeEscapeCodeDiff(contents, &prevAttrs)
					prevAttrs = attrs
				}
				termEraseCharNew(pos.col - prevCol).
					writeBuf(contents)
				erase = rs.None[rs.Tuple2[uint16, attrsAttrs]]()
			}
		}

		if !cell.Equal(&defaultCell) {
			attrs := cell.attrsMethod()
			if cell.HasContents() {
				if pos != prevPos {
					if !wrapping ||
						prevPos.row+1 != pos.row ||
						prevPos.col <
							self.cols()-boolU16(cell.IsWide()) ||
						pos.col != 0 {
						termMoveFromToNew(prevPos, pos).
							writeBuf(contents)
					}
					prevPos = pos
				}

				if prevAttrs != *attrs {
					attrs.writeEscapeCodeDiff(contents, &prevAttrs)
					prevAttrs = *attrs
				}

				if cell.IsWide() {
					prevPos.col += 2
				} else {
					prevPos.col += 1
				}
				*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
			} else if erase.IsNone() {
				erase = rs.Some(rs.T2(pos.col, *attrs))
			}
		}
	}
	if pending, ok := erase.Get(); ok {
		prevCol, attrs := pending.F0, pending.F1
		newPos := gridPos{row: row, col: prevCol}
		if wrapping &&
			prevPos.row+1 == newPos.row &&
			prevPos.col >= self.cols() {
			if newPos.col > 0 {
				*contents = append(*contents, bytes.Repeat([]uint8{' '}, int(newPos.col))...)
			} else {
				*contents = append(*contents, ' ')
				termBackspace{}.writeBuf(contents)
			}
		} else {
			termMoveFromToNew(prevPos, newPos).
				writeBuf(contents)
		}
		prevPos = newPos
		if prevAttrs != attrs {
			attrs.writeEscapeCodeDiff(contents, &prevAttrs)
			prevAttrs = attrs
		}
		termClearRowForward{}.writeBuf(contents)
	}

	return prevPos, prevAttrs
}

// while it's true that most of the logic in this is identical to
// write_contents_formatted, i can't figure out how to break out the
// common parts without making things noticeably slower.
func (self *rowRow) writeContentsDiff(
	contents *[]uint8,
	prev *rowRow,
	start uint16,
	width uint16,
	row uint16,
	wrapping bool,
	prevWrapping bool,
	prevPos gridPos,
	prevAttrs attrsAttrs,
) (gridPos, attrsAttrs) {
	prevWasWide := false

	firstCell := vecIndex(self.cells, uint(start))
	prevFirstCell := vecIndex(prev.cells, uint(start))
	if wrapping &&
		!prevWrapping &&
		firstCell.Equal(prevFirstCell) &&
		prevPos.row+1 == row &&
		prevPos.col >=
			self.cols()-boolU16(prevFirstCell.IsWide()) {
		firstCellAttrs := firstCell.attrsMethod()
		if prevAttrs != *firstCellAttrs {
			firstCellAttrs.
				writeEscapeCodeDiff(contents, &prevAttrs)
			prevAttrs = *firstCellAttrs
		}
		cellContents := prevFirstCell.contents[:prevFirstCell.lenMethod()]
		var needErase bool
		if len(cellContents) == 0 {
			cellContents = []uint8(" ")
			needErase = true
		} else {
			needErase = false
		}
		*contents = append(*contents, cellContents...)
		termBackspace{}.writeBuf(contents)
		if prevFirstCell.IsWide() {
			termBackspace{}.writeBuf(contents)
		}
		if needErase {
			termEraseCharNew(1).writeBuf(contents)
		}
		prevPos = gridPos{row: row, col: 0}
	}

	erase := rs.None[rs.Tuple2[uint16, attrsAttrs]]()
	// `self.cells().zip(prev.cells())` stops at the shorter row.
	for index := int(start); index < len(self.cells) && index < len(prev.cells) && index-int(start) < int(width); index++ {
		cell, prevCell := &self.cells[index], &prev.cells[index]
		if prevWasWide {
			prevWasWide = false
			continue
		}
		prevWasWide = cell.IsWide()

		// we limit the number of cols to a u16 (see Size)
		col := uint16(index)
		pos := gridPos{row: row, col: col}

		if pending, ok := erase.Get(); ok {
			prevCol, attrs := pending.F0, pending.F1
			if cell.HasContents() || *cell.attrsMethod() != attrs {
				newPos := gridPos{row: row, col: prevCol}
				if wrapping &&
					prevPos.row+1 == newPos.row &&
					prevPos.col >= self.cols() {
					if newPos.col > 0 {
						*contents = append(*contents, bytes.Repeat([]uint8{' '}, int(newPos.col))...)
					} else {
						*contents = append(*contents, ' ')
						termBackspace{}.writeBuf(contents)
					}
				} else {
					termMoveFromToNew(prevPos, newPos).
						writeBuf(contents)
				}
				prevPos = newPos
				if prevAttrs != attrs {
					attrs.writeEscapeCodeDiff(contents, &prevAttrs)
					prevAttrs = attrs
				}
				termEraseCharNew(pos.col - prevCol).
					writeBuf(contents)
				erase = rs.None[rs.Tuple2[uint16, attrsAttrs]]()
			}
		}

		if !cell.Equal(prevCell) {
			attrs := cell.attrsMethod()
			if cell.HasContents() {
				if pos != prevPos {
					if !wrapping ||
						prevPos.row+1 != pos.row ||
						prevPos.col <
							self.cols()-boolU16(cell.IsWide()) ||
						pos.col != 0 {
						termMoveFromToNew(prevPos, pos).
							writeBuf(contents)
					}
					prevPos = pos
				}

				if prevAttrs != *attrs {
					attrs.writeEscapeCodeDiff(contents, &prevAttrs)
					prevAttrs = *attrs
				}

				if cell.IsWide() {
					prevPos.col += 2
				} else {
					prevPos.col += 1
				}
				*contents = append(*contents, cell.contents[:cell.lenMethod()]...)
			} else if erase.IsNone() {
				erase = rs.Some(rs.T2(pos.col, *attrs))
			}
		}
	}
	if pending, ok := erase.Get(); ok {
		prevCol, attrs := pending.F0, pending.F1
		newPos := gridPos{row: row, col: prevCol}
		if wrapping &&
			prevPos.row+1 == newPos.row &&
			prevPos.col >= self.cols() {
			if newPos.col > 0 {
				*contents = append(*contents, bytes.Repeat([]uint8{' '}, int(newPos.col))...)
			} else {
				*contents = append(*contents, ' ')
				termBackspace{}.writeBuf(contents)
			}
		} else {
			termMoveFromToNew(prevPos, newPos).
				writeBuf(contents)
		}
		prevPos = newPos
		if prevAttrs != attrs {
			attrs.writeEscapeCodeDiff(contents, &prevAttrs)
			prevAttrs = attrs
		}
		termClearRowForward{}.writeBuf(contents)
	}

	// if this row is going from wrapped to not wrapped, we need to erase
	// and redraw the last character to break wrapping. if this row is
	// wrapped, we need to redraw the last character without erasing it to
	// position the cursor after the end of the line correctly so that
	// drawing the next line can just start writing and be wrapped.
	if (!self.wrapped && prev.wrapped) || (!prev.wrapped && self.wrapped) {
		var endPos gridPos
		if vecIndex(self.cells, uint(self.cols()-1)).
			IsWideContinuation() {
			endPos = gridPos{
				row: row,
				col: self.cols() - 2,
			}
		} else {
			endPos = gridPos{
				row: row,
				col: self.cols() - 1,
			}
		}
		termMoveFromToNew(prevPos, endPos).
			writeBuf(contents)
		prevPos = endPos
		if !self.wrapped {
			termEraseCharNew(1).writeBuf(contents)
		}
		endCell := vecIndex(self.cells, uint(endPos.col))
		if endCell.HasContents() {
			attrs := endCell.attrsMethod()
			if prevAttrs != *attrs {
				attrs.writeEscapeCodeDiff(contents, &prevAttrs)
				prevAttrs = *attrs
			}
			*contents = append(*contents, endCell.contents[:endCell.lenMethod()]...)
			if endCell.IsWide() {
				prevPos.col += 2
			} else {
				prevPos.col += 1
			}
		}
	}

	return prevPos, prevAttrs
}
