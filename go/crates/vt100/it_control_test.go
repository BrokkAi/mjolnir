// Port of vt100 0.16.2 tests/control.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

// controlBelState is the `State` of the test `bel`. The callbacks it does
// not implement keep their defaults, which do nothing, through the embedded
// vt100.CallbacksUnit.
type controlBelState struct {
	vt100.CallbacksUnit
	bel int
}

func (self *controlBelState) AudibleBell(_ *vt100.Screen) {
	self.bel += 1
}

func TestItControl_bel(t *testing.T) {
	parser :=
		vt100.ParserNewWithCallbacks(24, 80, 0, &controlBelState{bel: 0})
	assertEq(t, parser.Callbacks().bel, 0)

	screen := parser.Screen().Clone()
	parser.Process([]byte("\x07"))
	assertEq(t, parser.Callbacks().bel, 1)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x07"))
	assertEq(t, parser.Callbacks().bel, 2)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x07\x07\x07"))
	assertEq(t, parser.Callbacks().bel, 5)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(""))

	screen = parser.Screen().Clone()
	parser.Process([]byte("foo"))
	assertEq(t, parser.Callbacks().bel, 5)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("foo"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("ba\x07r"))
	assertEq(t, parser.Callbacks().bel, 6)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("bar"))
}

func TestItControl_bs(t *testing.T) {
	helpersFixture(t, "bs")
}

func TestItControl_tab(t *testing.T) {
	helpersFixture(t, "tab")
}

func TestItControl_lf(t *testing.T) {
	helpersFixture(t, "lf")
}

func TestItControl_vt(t *testing.T) {
	helpersFixture(t, "vt")
}

func TestItControl_ff(t *testing.T) {
	helpersFixture(t, "ff")
}

func TestItControl_cr(t *testing.T) {
	helpersFixture(t, "cr")
}
