// Port of vt100 0.16.2 tests/escape.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItEscape_deckpam(t *testing.T) {
	helpersFixture(t, "deckpam")
}

func TestItEscape_ri(t *testing.T) {
	helpersFixture(t, "ri")
}

func TestItEscape_ris(t *testing.T) {
	helpersFixture(t, "ris")
}

// escapeVbState is the `State` of the test `vb`.
type escapeVbState struct {
	vt100.CallbacksUnit
	vb int
}

func (self *escapeVbState) VisualBell(_ *vt100.Screen) {
	self.vb += 1
}

func TestItEscape_vb(t *testing.T) {
	parser :=
		vt100.ParserNewWithCallbacks(24, 80, 0, &escapeVbState{vb: 0})
	assertEq(t, parser.Callbacks().vb, 0)

	screen := parser.Screen().Clone()
	parser.Process([]byte("\x1bg"))
	assertEq(t, parser.Callbacks().vb, 1)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1bg"))
	assertEq(t, parser.Callbacks().vb, 2)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1bg\x1bg\x1bg"))
	assertEq(t, parser.Callbacks().vb, 5)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("foo"))
	assertEq(t, parser.Callbacks().vb, 5)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("foo"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("ba\x1bgr"))
	assertEq(t, parser.Callbacks().vb, 6)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("bar"))
}

func TestItEscape_decsc(t *testing.T) {
	helpersFixture(t, "decsc")
}

func TestItEscape_decsc_resize(t *testing.T) {
	parser := vt100.ParserNew(24, 80, 0)
	parser.Process([]byte("foo\x1b[20;70Hbar\x1b7"))
	assertEq(t, parser.Screen().Contents(), "foo\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n                                                                     bar")
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(19, 72))
	parser.Process([]byte("\x1b[H"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))
	parser.ScreenMut().SetSize(15, 60)
	assertEq(t, parser.Screen().Contents(), "foo")
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))
	parser.Process([]byte("y\x1b8z"))
	assertEq(t, parser.Screen().Contents(), "yoo\n\n\n\n\n\n\n\n\n\n\n\n\n\n                                                           z")
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(14, 60))
}
