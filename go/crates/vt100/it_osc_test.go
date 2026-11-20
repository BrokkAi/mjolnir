// Port of vt100 0.16.2 tests/osc.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// oscTitleIconNameWindow is the `Window` of the test `title_icon_name`.
type oscTitleIconNameWindow struct {
	vt100.CallbacksUnit
	title    string
	iconName string
}

func (self *oscTitleIconNameWindow) SetWindowIconName(
	_ *vt100.Screen,
	iconName []uint8,
) {
	self.iconName =
		rs.Unwrap(strs.FromUtf8(iconName))
}

func (self *oscTitleIconNameWindow) SetWindowTitle(_ *vt100.Screen, title []uint8) {
	self.title = rs.Unwrap(strs.FromUtf8(title))
}

func TestItOsc_title_icon_name(t *testing.T) {
	parser :=
		vt100.ParserNewWithCallbacks(24, 80, 0, &oscTitleIconNameWindow{})
	assertEq(t, parser.Callbacks().iconName, "")
	assertEq(t, parser.Callbacks().title, "")
	parser.Process([]byte("\x1b]1;icon_name\x07"))
	assertEq(t, parser.Callbacks().iconName, "icon_name")
	assertEq(t, parser.Callbacks().title, "")
	parser.Process([]byte("\x1b]2;title\x07"))
	assertEq(t, parser.Callbacks().iconName, "icon_name")
	assertEq(t, parser.Callbacks().title, "title")
	parser.Process([]byte("\x1b]0;both\x07"))
	assertEq(t, parser.Callbacks().iconName, "both")
	assertEq(t, parser.Callbacks().title, "both")
}

// oscClipboard is the `Clipboard` of the test `clipboard`. The keys of the
// map are `Vec<u8>` in Rust; a Go string holds the same bytes.
type oscClipboard struct {
	vt100.CallbacksUnit
	clipboard collections.HashMap[string, []uint8]
	pasted    [][]uint8
}

func (self *oscClipboard) CopyToClipboard(
	_ *vt100.Screen,
	ty []uint8,
	data []uint8,
) {
	self.clipboard.Insert(string(ty), append([]uint8{}, data...))
}

func (self *oscClipboard) PasteFromClipboard(_ *vt100.Screen, ty []uint8) {
	self.pasted = append(self.pasted, append([]uint8{}, ty...))
}

func (self *oscClipboard) UnhandledOsc(_ *vt100.Screen, params [][]uint8) {
	panic(rs.Panicf("unhandled osc: {:?}", params))
}

func TestItOsc_clipboard(t *testing.T) {
	parser :=
		vt100.ParserNewWithCallbacks(24, 80, 0, &oscClipboard{})
	assertTrue(t, parser.Callbacks().clipboard.IsEmpty(), "parser.callbacks().clipboard.is_empty()")
	assertTrue(t, len(parser.Callbacks().pasted) == 0, "parser.callbacks().pasted.is_empty()")
	parser.Process([]byte("\x1b]52;c;?\x07"))
	assertTrue(t, parser.Callbacks().clipboard.IsEmpty(), "parser.callbacks().clipboard.is_empty()")
	assertEq(t, parser.Callbacks().pasted, [][]uint8{[]uint8("c")})
	parser.Process([]byte("\x1b]52;c;abcdef==\x07"))
	assertEq(t, parser.Callbacks().clipboard.Len(), 1)
	copied, ok := parser.Callbacks().clipboard.Get("c")
	assertTrue(t, ok, "parser.callbacks().clipboard.get(&b\"c\"[..]).is_some()")
	assertEq(t,
		*copied,
		[]uint8("abcdef=="),
	)
	assertEq(t, parser.Callbacks().pasted, [][]uint8{[]uint8("c")})
}

func TestItOsc_unknown_osc(t *testing.T) {
	helpersFixture(t, "unknown_osc")
}
