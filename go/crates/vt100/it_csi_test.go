// Port of vt100 0.16.2 tests/csi.rs

package vt100_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItCsi_absolute_movement(t *testing.T) {
	helpersFixture(t, "absolute_movement")
}

func TestItCsi_row_clamp(t *testing.T) {
	vt := vt100.ParserDefault()
	assertEq(t, pair(vt.Screen().CursorPosition()), pair(0, 0))
	vt.Process([]byte("\x1b[15d"))
	assertEq(t, pair(vt.Screen().CursorPosition()), pair(14, 0))
	vt.Process([]byte("\x1b[150d"))
	assertEq(t, pair(vt.Screen().CursorPosition()), pair(23, 0))
}

func TestItCsi_relative_movement(t *testing.T) {
	helpersFixture(t, "relative_movement")
}

func TestItCsi_ed(t *testing.T) {
	helpersFixture(t, "ed")
}

func TestItCsi_el(t *testing.T) {
	helpersFixture(t, "el")
}

func TestItCsi_ich_dch_ech(t *testing.T) {
	helpersFixture(t, "ich_dch_ech")
}

func TestItCsi_il_dl(t *testing.T) {
	helpersFixture(t, "il_dl")
}

func TestItCsi_scroll(t *testing.T) {
	helpersFixture(t, "scroll")
}

// csiXtwinopsCallbacks is the `Callbacks` of the test `xtwinops`: it applies
// every resize the terminal asks for.
type csiXtwinopsCallbacks struct {
	vt100.CallbacksUnit
}

func (self *csiXtwinopsCallbacks) Resize(
	screen *vt100.Screen,
	request rs.Tuple2[uint16, uint16],
) {
	rows, cols := request.F0, request.F1
	screen.SetSize(rows, cols)
}

func TestItCsi_xtwinops(t *testing.T) {
	vt := vt100.ParserNewWithCallbacks(24, 80, 0, &csiXtwinopsCallbacks{})
	assertEq(t, pair(vt.Screen().Size()), pair(24, 80))
	vt.Process([]byte("\x1b[8;24;80t"))
	assertEq(t, pair(vt.Screen().Size()), pair(24, 80))
	vt.Process([]byte("\x1b[8t"))
	assertEq(t, pair(vt.Screen().Size()), pair(24, 80))
	vt.Process([]byte("\x1b[8;80;24t"))
	assertEq(t, pair(vt.Screen().Size()), pair(80, 24))
	vt.Process([]byte("\x1b[8;24t"))
	assertEq(t, pair(vt.Screen().Size()), pair(24, 24))

	vt = vt100.ParserNewWithCallbacks(24, 80, 0, &csiXtwinopsCallbacks{})
	assertEq(t, pair(vt.Screen().Size()), pair(24, 80))
	vt.Process([]byte(strings.Repeat("a", 90)))
	assertEq(t,
		nth(vt.Screen().Rows(0, 80), 0).Unwrap(),
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
	)
	assertEq(t, nth(vt.Screen().Rows(0, 80), 1).Unwrap(), "aaaaaaaaaa")
	vt.Process(
		[]byte("\x1b[H\x1b[8;24;15tbbbbbbbbbbbbbbbbbbbb\x1b[8;24;80tcccccccccccccccccccc"),
	)
	assertEq(t, nth(vt.Screen().Rows(0, 80), 0).Unwrap(), "bbbbbbbbbbbbbbb")
	assertEq(t,
		nth(vt.Screen().Rows(0, 80), 1).Unwrap(),
		"bbbbbcccccccccccccccccccc",
	)
}
