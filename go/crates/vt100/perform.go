// Port of vt100 0.16.2 src/perform.rs

package vt100

import (
	"bytes"

	"github.com/BrokkAi/mjolnir/go/crates/vte"
	"github.com/BrokkAi/mjolnir/go/rs"
)

var performBase64 = []uint8("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=")
var performClipboardSelector = []uint8("cpqs01234567")

type performWrappedScreen[CB Callbacks] struct {
	screen    Screen
	callbacks CB
}

func performWrappedScreenNew(rows uint16, cols uint16, scrollbackLen int) performWrappedScreen[CallbacksUnit] {
	return performWrappedScreenNewWithCallbacks(rows, cols, scrollbackLen, CallbacksUnit{})
}

func performWrappedScreenNewWithCallbacks[CB Callbacks](
	rows uint16,
	cols uint16,
	scrollbackLen int,
	callbacks CB,
) performWrappedScreen[CB] {
	return performWrappedScreen[CB]{
		screen: screenNew(
			gridSize{rows: rows, cols: cols},
			scrollbackLen,
		),
		callbacks: callbacks,
	}
}

func (self *performWrappedScreen[CB]) Print(c rune) {
	if c == '�' || (c >= '\u0080' && c < ' ') {
		self.callbacks.UnhandledChar(&self.screen, c)
	} else {
		self.screen.text(c)
	}
}

func (self *performWrappedScreen[CB]) Execute(b uint8) {
	switch b {
	case 7:
		self.callbacks.AudibleBell(&self.screen)
	case 8:
		self.screen.bs()
	case 9:
		self.screen.tab()
	case 10:
		self.screen.lf()
	case 11:
		self.screen.vt()
	case 12:
		self.screen.ff()
	case 13:
		self.screen.cr()
	// we don't implement shift in/out alternate character sets, but
	// it shouldn't count as an "error"
	case 14, 15:
	default:
		self.callbacks.UnhandledControl(&self.screen, b)
	}
}

func (self *performWrappedScreen[CB]) EscDispatch(intermediates []uint8, _ignore bool, b uint8) {
	if len(intermediates) > 0 {
		i := intermediates[0]
		self.callbacks.UnhandledEscape(
			&self.screen,
			rs.Some(i),
			sliceGetCopied(intermediates, 1),
			b,
		)
	} else {
		switch b {
		case '7':
			self.screen.decsc()
		case '8':
			self.screen.decrc()
		case '=':
			self.screen.deckpam()
		case '>':
			self.screen.deckpnm()
		case 'M':
			self.screen.ri()
		case 'c':
			self.screen.ris()
		case 'g':
			self.callbacks.VisualBell(&self.screen)
		default:
			self.callbacks.UnhandledEscape(
				&self.screen,
				rs.None[uint8](),
				rs.None[uint8](),
				b,
			)
		}
	}
}

func (self *performWrappedScreen[CB]) CsiDispatch(
	params *vte.Params,
	intermediates []uint8,
	_ignore bool,
	c rune,
) {
	unhandled := func(screen *Screen) {
		self.callbacks.UnhandledCsi(
			screen,
			sliceGetCopied(intermediates, 0),
			sliceGetCopied(intermediates, 1),
			paramsCollect(params),
			c,
		)
	}
	switch {
	case len(intermediates) == 0:
		switch c {
		case '@':
			self.screen.ich(performCanonicalizeParams1(params, 1))
		case 'A':
			self.screen.cuu(performCanonicalizeParams1(params, 1))
		case 'B':
			self.screen.cud(performCanonicalizeParams1(params, 1))
		case 'C':
			self.screen.cuf(performCanonicalizeParams1(params, 1))
		case 'D':
			self.screen.cub(performCanonicalizeParams1(params, 1))
		case 'E':
			self.screen.cnl(performCanonicalizeParams1(params, 1))
		case 'F':
			self.screen.cpl(performCanonicalizeParams1(params, 1))
		case 'G':
			self.screen.cha(performCanonicalizeParams1(params, 1))
		case 'H':
			self.screen.cup(performCanonicalizeParams2(params, 1, 1))
		case 'J':
			self.
				screen.
				ed(performCanonicalizeParams1(params, 0), unhandled)
		case 'K':
			self.
				screen.
				el(performCanonicalizeParams1(params, 0), unhandled)
		case 'L':
			self.screen.il(performCanonicalizeParams1(params, 1))
		case 'M':
			self.screen.dl(performCanonicalizeParams1(params, 1))
		case 'P':
			self.screen.dch(performCanonicalizeParams1(params, 1))
		case 'S':
			self.screen.su(performCanonicalizeParams1(params, 1))
		case 'T':
			self.screen.sd(performCanonicalizeParams1(params, 1))
		case 'X':
			self.screen.ech(performCanonicalizeParams1(params, 1))
		case 'd':
			self.screen.vpa(performCanonicalizeParams1(params, 1))
		case 'm':
			self.screen.sgr(params, unhandled)
		case 'r':
			self.screen.decstbm(performCanonicalizeParamsDecstbm(
				params,
				self.screen.gridMethod().sizeMethod(),
			))
		case 't':
			paramsIter := params.Iter()
			op := rs.None[uint16]()
			if x, ok := paramsIter.Next().Get(); ok {
				op = sliceGetCopied(x, 0)
			}
			if op == rs.Some[uint16](8) {
				screenRows, screenCols := self.screen.Size()
				rows := screenRows
				if x, ok := paramsIter.Next().Get(); ok {
					rows = sliceGetCopied(x, 0).UnwrapOr(screenRows)
				}
				cols := screenCols
				if x, ok := paramsIter.Next().Get(); ok {
					cols = sliceGetCopied(x, 0).UnwrapOr(screenCols)
				}
				self.callbacks.Resize(&self.screen, rs.T2(rows, cols))
			} else {
				self.callbacks.UnhandledCsi(
					&self.screen,
					rs.None[uint8](),
					rs.None[uint8](),
					paramsCollect(params),
					c,
				)
			}
		default:
			self.callbacks.UnhandledCsi(
				&self.screen,
				rs.None[uint8](),
				rs.None[uint8](),
				paramsCollect(params),
				c,
			)
		}
	case intermediates[0] == '?':
		switch c {
		case 'J':
			self.
				screen.
				decsed(performCanonicalizeParams1(params, 0), unhandled)
		case 'K':
			self.
				screen.
				decsel(performCanonicalizeParams1(params, 0), unhandled)
		case 'h':
			self.screen.decset(params, unhandled)
		case 'l':
			self.screen.decrst(params, unhandled)
		default:
			self.callbacks.UnhandledCsi(
				&self.screen,
				rs.Some[uint8]('?'),
				sliceGetCopied(intermediates, 1),
				paramsCollect(params),
				c,
			)
		}
	default:
		i := intermediates[0]
		self.callbacks.UnhandledCsi(
			&self.screen,
			rs.Some(i),
			sliceGetCopied(intermediates, 1),
			paramsCollect(params),
			c,
		)
	}
}

func (self *performWrappedScreen[CB]) OscDispatch(params [][]uint8, _belTerminated bool) {
	switch {
	case len(params) == 2 && bytes.Equal(params[0], []uint8("0")):
		s := params[1]
		self.callbacks.SetWindowIconName(&self.screen, s)
		self.callbacks.SetWindowTitle(&self.screen, s)
	case len(params) == 2 && bytes.Equal(params[0], []uint8("1")):
		s := params[1]
		self.callbacks.SetWindowIconName(&self.screen, s)
	case len(params) == 2 && bytes.Equal(params[0], []uint8("2")):
		s := params[1]
		self.callbacks.SetWindowTitle(&self.screen, s)
	case len(params) == 3 && bytes.Equal(params[0], []uint8("52")):
		ty, data := params[1], params[2]
		selector := true
		for _, c := range ty {
			if !bytes.Contains(performClipboardSelector, []uint8{c}) {
				selector = false
				break
			}
		}
		switch {
		case selector && bytes.Equal(data, []uint8("?")):
			self.callbacks.
				PasteFromClipboard(&self.screen, ty)
		case selector && bytesAll(data, func(c uint8) bool { return bytes.Contains(performBase64, []uint8{c}) }):
			self.callbacks.CopyToClipboard(
				&self.screen,
				ty,
				data,
			)
		default:
			self.callbacks.
				UnhandledOsc(&self.screen, params)
		}
	default:
		self.callbacks.UnhandledOsc(&self.screen, params)
	}
}

// Hook, Put, Unhook and Terminated are the methods of vte::Perform this type
// leaves at their defaults.

func (self *performWrappedScreen[CB]) Hook(params *vte.Params, intermediates []uint8, ignore bool, action rune) {
	vte.PerformDefaultHook(self, params, intermediates, ignore, action)
}

func (self *performWrappedScreen[CB]) Put(byte_ uint8) {
	vte.PerformDefaultPut(self, byte_)
}

func (self *performWrappedScreen[CB]) Unhook() {
	vte.PerformDefaultUnhook(self)
}

func (self *performWrappedScreen[CB]) Terminated() bool {
	return vte.PerformDefaultTerminated(self)
}

func performCanonicalizeParams1(params *vte.Params, default_ uint16) uint16 {
	var first uint16 = 0
	if x, ok := params.Iter().Next().Get(); ok {
		first = sliceGetCopied(x, 0).UnwrapOr(0)
	}
	if first == 0 {
		return default_
	} else {
		return first
	}
}

func performCanonicalizeParams2(
	params *vte.Params,
	default1 uint16,
	default2 uint16,
) (uint16, uint16) {
	iter := params.Iter()
	var first uint16 = 0
	if x, ok := iter.Next().Get(); ok {
		first = sliceGetCopied(x, 0).UnwrapOr(0)
	}
	if first == 0 {
		first = default1
	}

	var second uint16 = 0
	if x, ok := iter.Next().Get(); ok {
		second = sliceGetCopied(x, 0).UnwrapOr(0)
	}
	if second == 0 {
		second = default2
	}

	return first, second
}

func performCanonicalizeParamsDecstbm(
	params *vte.Params,
	size gridSize,
) (uint16, uint16) {
	iter := params.Iter()
	var top uint16 = 0
	if x, ok := iter.Next().Get(); ok {
		top = sliceGetCopied(x, 0).UnwrapOr(0)
	}
	if top == 0 {
		top = 1
	}

	var bottom uint16 = 0
	if x, ok := iter.Next().Get(); ok {
		bottom = sliceGetCopied(x, 0).UnwrapOr(0)
	}
	if bottom == 0 {
		bottom = size.rows
	}

	return top, bottom
}

// sliceGetCopied is `slice.get(index).copied()`, and with index 0
// `slice.first().copied()`.
func sliceGetCopied[T any](slice []T, index int) rs.Option[T] {
	if index < len(slice) {
		return rs.Some(slice[index])
	}
	return rs.None[T]()
}

// paramsCollect is `&params.iter().collect::<Vec<_>>()`: every parameter
// with its subparameters.
func paramsCollect(params *vte.Params) [][]uint16 {
	collected := make([][]uint16, 0, params.Len())
	for iter := params.Iter(); ; {
		param, ok := iter.Next().Get()
		if !ok {
			return collected
		}
		collected = append(collected, param)
	}
}

// bytesAll is `bytes.iter().all(f)`.
func bytesAll(bytes_ []uint8, f func(uint8) bool) bool {
	for _, b := range bytes_ {
		if !f(b) {
			return false
		}
	}
	return true
}
