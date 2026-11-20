// Port of vt100 0.16.2 src/term.rs

package vt100

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// TODO: read all of this from terminfo

type termBufWrite interface {
	writeBuf(buf *[]uint8)
}

type termClearScreen struct{}

func (self termClearScreen) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x1b[H\x1b[J"...)
}

type termClearRowForward struct{}

func (self termClearRowForward) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x1b[K"...)
}

type termCrlf struct{}

func (self termCrlf) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\r\n"...)
}

type termBackspace struct{}

func (self termBackspace) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x08"...)
}

type termSaveCursor struct{}

func (self termSaveCursor) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x1b7"...)
}

type termRestoreCursor struct{}

func (self termRestoreCursor) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x1b8"...)
}

type termMoveTo struct {
	row uint16
	col uint16
}

func termMoveToNew(pos gridPos) termMoveTo {
	return termMoveTo{
		row: pos.row,
		col: pos.col,
	}
}

func (self termMoveTo) writeBuf(buf *[]uint8) {
	if self.row == 0 && self.col == 0 {
		*buf = append(*buf, "\x1b[H"...)
	} else {
		*buf = append(*buf, "\x1b["...)
		termExtendItoa(buf, uint64(self.row+1))
		*buf = append(*buf, ';')
		termExtendItoa(buf, uint64(self.col+1))
		*buf = append(*buf, 'H')
	}
}

type termClearAttrs struct{}

func (self termClearAttrs) writeBuf(buf *[]uint8) {
	*buf = append(*buf, "\x1b[m"...)
}

type termIntensity uint8

const (
	termIntensity_Normal termIntensity = iota
	termIntensity_Bold
	termIntensity_Dim
)

type termAttrs struct {
	fgcolor   rs.Option[color]
	bgcolor   rs.Option[color]
	intensity rs.Option[termIntensity]
	italic    rs.Option[bool]
	underline rs.Option[bool]
	inverse   rs.Option[bool]
}

func (self termAttrs) fgcolorMethod(fgcolor color) termAttrs {
	self.fgcolor = rs.Some(fgcolor)
	return self
}

func (self termAttrs) bgcolorMethod(bgcolor color) termAttrs {
	self.bgcolor = rs.Some(bgcolor)
	return self
}

func (self termAttrs) intensityMethod(intensity termIntensity) termAttrs {
	self.intensity = rs.Some(intensity)
	return self
}

func (self termAttrs) italicMethod(italic bool) termAttrs {
	self.italic = rs.Some(italic)
	return self
}

func (self termAttrs) underlineMethod(underline bool) termAttrs {
	self.underline = rs.Some(underline)
	return self
}

func (self termAttrs) inverseMethod(inverse bool) termAttrs {
	self.inverse = rs.Some(inverse)
	return self
}

func (self termAttrs) writeBuf(buf *[]uint8) {
	if self.fgcolor.IsNone() &&
		self.bgcolor.IsNone() &&
		self.intensity.IsNone() &&
		self.italic.IsNone() &&
		self.underline.IsNone() &&
		self.inverse.IsNone() {
		return
	}

	*buf = append(*buf, "\x1b["...)
	first := true

	// The parameters are u8 in Rust, and `i + 30` and its like are u8
	// additions; none of them can overflow for the values each branch allows.
	writeParam := func(i uint8) {
		if first {
			first = false
		} else {
			*buf = append(*buf, ';')
		}
		termExtendItoa(buf, uint64(i))
	}

	if fgcolor, ok := self.fgcolor.Get(); ok {
		switch fgcolor.tag {
		case colorDefault:
			writeParam(39)
		case colorIdx:
			i := fgcolor.r
			if i < 8 {
				writeParam(i + 30)
			} else if i < 16 {
				writeParam(i + 82)
			} else {
				writeParam(38)
				writeParam(5)
				writeParam(i)
			}
		case colorRgb:
			writeParam(38)
			writeParam(2)
			writeParam(fgcolor.r)
			writeParam(fgcolor.g)
			writeParam(fgcolor.b)
		default:
			panic(rs.Unreachable())
		}
	}

	if bgcolor, ok := self.bgcolor.Get(); ok {
		switch bgcolor.tag {
		case colorDefault:
			writeParam(49)
		case colorIdx:
			i := bgcolor.r
			if i < 8 {
				writeParam(i + 40)
			} else if i < 16 {
				writeParam(i + 92)
			} else {
				writeParam(48)
				writeParam(5)
				writeParam(i)
			}
		case colorRgb:
			writeParam(48)
			writeParam(2)
			writeParam(bgcolor.r)
			writeParam(bgcolor.g)
			writeParam(bgcolor.b)
		default:
			panic(rs.Unreachable())
		}
	}

	if intensity, ok := self.intensity.Get(); ok {
		switch intensity {
		case termIntensity_Normal:
			writeParam(22)
		case termIntensity_Bold:
			writeParam(1)
		case termIntensity_Dim:
			writeParam(2)
		default:
			panic(rs.Unreachable())
		}
	}

	if italic, ok := self.italic.Get(); ok {
		if italic {
			writeParam(3)
		} else {
			writeParam(23)
		}
	}

	if underline, ok := self.underline.Get(); ok {
		if underline {
			writeParam(4)
		} else {
			writeParam(24)
		}
	}

	if inverse, ok := self.inverse.Get(); ok {
		if inverse {
			writeParam(7)
		} else {
			writeParam(27)
		}
	}

	*buf = append(*buf, 'm')
}

type termMoveRight struct {
	count uint16
}

func termMoveRightNew(count uint16) termMoveRight {
	return termMoveRight{count: count}
}

func termMoveRightDefault() termMoveRight {
	return termMoveRight{count: 1}
}

func (self termMoveRight) writeBuf(buf *[]uint8) {
	switch n := self.count; n {
	case 0:
	case 1:
		*buf = append(*buf, "\x1b[C"...)
	default:
		*buf = append(*buf, "\x1b["...)
		termExtendItoa(buf, uint64(n))
		*buf = append(*buf, 'C')
	}
}

type termEraseChar struct {
	count uint16
}

func termEraseCharNew(count uint16) termEraseChar {
	return termEraseChar{count: count}
}

func termEraseCharDefault() termEraseChar {
	return termEraseChar{count: 1}
}

func (self termEraseChar) writeBuf(buf *[]uint8) {
	switch n := self.count; n {
	case 0:
	case 1:
		*buf = append(*buf, "\x1b[X"...)
	default:
		*buf = append(*buf, "\x1b["...)
		termExtendItoa(buf, uint64(n))
		*buf = append(*buf, 'X')
	}
}

type termHideCursor struct {
	state bool
}

func termHideCursorNew(state bool) termHideCursor {
	return termHideCursor{state: state}
}

func (self termHideCursor) writeBuf(buf *[]uint8) {
	if self.state {
		*buf = append(*buf, "\x1b[?25l"...)
	} else {
		*buf = append(*buf, "\x1b[?25h"...)
	}
}

type termMoveFromTo struct {
	from gridPos
	to   gridPos
}

func termMoveFromToNew(from gridPos, to gridPos) termMoveFromTo {
	return termMoveFromTo{from: from, to: to}
}

func (self termMoveFromTo) writeBuf(buf *[]uint8) {
	if self.to.row == self.from.row+1 && self.to.col == 0 {
		termCrlf{}.writeBuf(buf)
	} else if self.from.row == self.to.row && self.from.col < self.to.col {
		termMoveRightNew(self.to.col - self.from.col).
			writeBuf(buf)
	} else if self.to != self.from {
		termMoveToNew(self.to).writeBuf(buf)
	}
}

type termApplicationKeypad struct {
	state bool
}

func termApplicationKeypadNew(state bool) termApplicationKeypad {
	return termApplicationKeypad{state: state}
}

func (self termApplicationKeypad) writeBuf(buf *[]uint8) {
	if self.state {
		*buf = append(*buf, "\x1b="...)
	} else {
		*buf = append(*buf, "\x1b>"...)
	}
}

type termApplicationCursor struct {
	state bool
}

func termApplicationCursorNew(state bool) termApplicationCursor {
	return termApplicationCursor{state: state}
}

func (self termApplicationCursor) writeBuf(buf *[]uint8) {
	if self.state {
		*buf = append(*buf, "\x1b[?1h"...)
	} else {
		*buf = append(*buf, "\x1b[?1l"...)
	}
}

type termBracketedPaste struct {
	state bool
}

func termBracketedPasteNew(state bool) termBracketedPaste {
	return termBracketedPaste{state: state}
}

func (self termBracketedPaste) writeBuf(buf *[]uint8) {
	if self.state {
		*buf = append(*buf, "\x1b[?2004h"...)
	} else {
		*buf = append(*buf, "\x1b[?2004l"...)
	}
}

type termMouseProtocolMode struct {
	mode MouseProtocolMode
	prev MouseProtocolMode
}

func termMouseProtocolModeNew(
	mode MouseProtocolMode,
	prev MouseProtocolMode,
) termMouseProtocolMode {
	return termMouseProtocolMode{mode: mode, prev: prev}
}

func (self termMouseProtocolMode) writeBuf(buf *[]uint8) {
	if self.mode == self.prev {
		return
	}

	switch self.mode {
	case MouseProtocolMode_None:
		switch self.prev {
		case MouseProtocolMode_None:
		case MouseProtocolMode_Press:
			*buf = append(*buf, "\x1b[?9l"...)
		case MouseProtocolMode_PressRelease:
			*buf = append(*buf, "\x1b[?1000l"...)
		case MouseProtocolMode_ButtonMotion:
			*buf = append(*buf, "\x1b[?1002l"...)
		case MouseProtocolMode_AnyMotion:
			*buf = append(*buf, "\x1b[?1003l"...)
		default:
			panic(rs.Unreachable(self.prev))
		}
	case MouseProtocolMode_Press:
		*buf = append(*buf, "\x1b[?9h"...)
	case MouseProtocolMode_PressRelease:
		*buf = append(*buf, "\x1b[?1000h"...)
	case MouseProtocolMode_ButtonMotion:
		*buf = append(*buf, "\x1b[?1002h"...)
	case MouseProtocolMode_AnyMotion:
		*buf = append(*buf, "\x1b[?1003h"...)
	default:
		panic(rs.Unreachable(self.mode))
	}
}

type termMouseProtocolEncoding struct {
	encoding MouseProtocolEncoding
	prev     MouseProtocolEncoding
}

func termMouseProtocolEncodingNew(
	encoding MouseProtocolEncoding,
	prev MouseProtocolEncoding,
) termMouseProtocolEncoding {
	return termMouseProtocolEncoding{encoding: encoding, prev: prev}
}

func (self termMouseProtocolEncoding) writeBuf(buf *[]uint8) {
	if self.encoding == self.prev {
		return
	}

	switch self.encoding {
	case MouseProtocolEncoding_Default:
		switch self.prev {
		case MouseProtocolEncoding_Default:
		case MouseProtocolEncoding_Utf8:
			*buf = append(*buf, "\x1b[?1005l"...)
		case MouseProtocolEncoding_Sgr:
			*buf = append(*buf, "\x1b[?1006l"...)
		default:
			panic(rs.Unreachable(self.prev))
		}
	case MouseProtocolEncoding_Utf8:
		*buf = append(*buf, "\x1b[?1005h"...)
	case MouseProtocolEncoding_Sgr:
		*buf = append(*buf, "\x1b[?1006h"...)
	default:
		panic(rs.Unreachable(self.encoding))
	}
}

// termExtendItoa is `extend_itoa`, which the crate calls with u8 and u16
// values: the number in decimal.
func termExtendItoa(buf *[]uint8, i uint64) {
	*buf = strconv.AppendUint(*buf, i, 10)
}
