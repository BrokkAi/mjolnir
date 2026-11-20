// Port of vt100 0.16.2 src/cell.rs

package vt100

import (
	"bytes"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// chosen to make the size of the cell struct 32 bytes
const cellContentBytes = 22

const cellIsWide uint8 = 0b1000_0000
const cellIsWideContinuation uint8 = 0b0100_0000
const cellLenBits uint8 = 0b0001_1111

// Represents a single terminal cell.
type Cell struct {
	contents [cellContentBytes]uint8 `rs:"contents"`
	len      uint8                   `rs:"len"`
	attrs    attrsAttrs              `rs:"attrs"`
}

var _ = rs.RegisterStruct[Cell]("Cell")

// Equal is `impl PartialEq for Cell`: the same text, width flags and
// attributes.
func (self *Cell) Equal(other *Cell) bool {
	if self.len != other.len {
		return false
	}
	if self.attrs != other.attrs {
		return false
	}
	len_ := self.lenMethod()
	return bytes.Equal(self.contents[:len_], other.contents[:len_])
}

// Clone is `cell.clone()`.
func (self *Cell) Clone() Cell {
	return *self
}

func cellNew() Cell {
	return Cell{
		contents: [cellContentBytes]uint8{},
		len:      0,
		attrs:    attrsAttrs{},
	}
}

func (self *Cell) lenMethod() int {
	return int(self.len & cellLenBits)
}

func (self *Cell) set(c rune, a attrsAttrs) {
	self.len = 0
	self.appendChar(0, c)
	// strings in this context should always be an arbitrary character
	// followed by zero or more zero-width characters, so we should only
	// have to look at the first character
	self.setWide(unicodeWidthCharWidth(c).UnwrapOr(1) > 1)
	self.attrs = a
}

func (self *Cell) append(c rune) {
	len_ := self.lenMethod()
	if len_ >= cellContentBytes-4 {
		return
	}
	if len_ == 0 {
		self.contents[0] = ' '
		self.len += 1
	}

	// we already checked that we have space for another codepoint
	self.appendChar(self.lenMethod(), c)
}

// Writes bytes representing c at start
// Requires caller to verify start <= CODEPOINTS_IN_CELL * 4
func (self *Cell) appendChar(start int, c rune) {
	self.len += uint8(utf8.EncodeRune(self.contents[start:], c))
}

func (self *Cell) clear(attrs attrsAttrs) {
	self.len = 0
	self.attrs = attrs
}

// Returns the text contents of the cell.
//
// Can include multiple unicode characters if combining characters are
// used, but will contain at most one character with a non-zero character
// width.
func (self *Cell) Contents() string {
	// Since contents has been constructed by appending chars encoded as UTF-8 it will be valid UTF-8
	return string(self.contents[:self.lenMethod()])
}

// Returns whether the cell contains any text data.
func (self *Cell) HasContents() bool {
	return self.lenMethod() > 0
}

// Returns whether the text data in the cell represents a wide character.
func (self *Cell) IsWide() bool {
	return self.len&cellIsWide != 0
}

// Returns whether the cell contains the second half of a wide character
// (in other words, whether the previous cell in the row contains a wide
// character)
func (self *Cell) IsWideContinuation() bool {
	return self.len&cellIsWideContinuation != 0
}

func (self *Cell) setWide(wide bool) {
	if wide {
		self.len |= cellIsWide
	} else {
		self.len &= ^cellIsWide
	}
}

func (self *Cell) setWideContinuation(wide bool) {
	if wide {
		self.len |= cellIsWideContinuation
	} else {
		self.len &= ^cellIsWideContinuation
	}
}

func (self *Cell) attrsMethod() *attrsAttrs {
	return &self.attrs
}

// Returns the foreground color of the cell.
func (self *Cell) Fgcolor() Color {
	return self.attrs.fgcolor.color()
}

// Returns the background color of the cell.
func (self *Cell) Bgcolor() Color {
	return self.attrs.bgcolor.color()
}

// Returns whether the cell should be rendered with the bold text
// attribute.
func (self *Cell) Bold() bool {
	return self.attrs.bold()
}

// Returns whether the cell should be rendered with the dim text
// attribute.
func (self *Cell) Dim() bool {
	return self.attrs.dim()
}

// Returns whether the cell should be rendered with the italic text
// attribute.
func (self *Cell) Italic() bool {
	return self.attrs.italic()
}

// Returns whether the cell should be rendered with the underlined text
// attribute.
func (self *Cell) Underline() bool {
	return self.attrs.underline()
}

// Returns whether the cell should be rendered with the inverse text
// attribute.
func (self *Cell) Inverse() bool {
	return self.attrs.inverse()
}
