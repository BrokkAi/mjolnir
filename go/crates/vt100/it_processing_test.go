// Port of vt100 0.16.2 tests/processing.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

func TestItProcessing_split_escape_sequences(t *testing.T) {
	helpersFixture(t, "split_escape_sequences")
}

func TestItProcessing_split_utf8(t *testing.T) {
	helpersFixture(t, "split_utf8")
}

// processingSplitOscWindow is the `Window` of the test `split_osc`.
type processingSplitOscWindow struct {
	vt100.CallbacksUnit
	title    string
	iconName string
}

func (self *processingSplitOscWindow) SetWindowIconName(
	_ *vt100.Screen,
	iconName []uint8,
) {
	self.iconName =
		rs.Unwrap(strs.FromUtf8(iconName))
}

func (self *processingSplitOscWindow) SetWindowTitle(_ *vt100.Screen, title []uint8) {
	self.title = rs.Unwrap(strs.FromUtf8(title))
}

func TestItProcessing_split_osc(t *testing.T) {
	parser :=
		vt100.ParserNewWithCallbacks(24, 80, 0, &processingSplitOscWindow{})
	for _, c := range []uint8("\x1b]0;a '[]_\x07") {
		assertEq(t, parser.Callbacks().iconName, "")
		assertEq(t, parser.Callbacks().title, "")
		parser.Process([]uint8{c})
	}
	assertEq(t, parser.Callbacks().iconName, "a '[]_")
	assertEq(t, parser.Callbacks().title, "a '[]_")
}
