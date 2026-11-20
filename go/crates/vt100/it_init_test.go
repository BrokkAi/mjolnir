// Port of vt100 0.16.2 tests/init.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItInit_init(t *testing.T) {
	parser := vt100.ParserDefault()
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))

	cell := parser.Screen().Cell(0, 0)
	assertEq(t, cell.Unwrap().Contents(), "")
	cell = parser.Screen().Cell(23, 79)
	assertEq(t, cell.Unwrap().Contents(), "")
	cell = parser.Screen().Cell(24, 0)
	assertTrue(t, cell.IsNone(), "cell.is_none()")
	cell = parser.Screen().Cell(0, 80)
	assertTrue(t, cell.IsNone(), "cell.is_none()")

	assertEq(t, parser.Screen().Contents(), "")
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J"),
	)

	assertTrue(t, !parser.Screen().ApplicationKeypad(), "!parser.screen().application_keypad()")
	assertTrue(t, !parser.Screen().ApplicationCursor(), "!parser.screen().application_cursor()")
	assertTrue(t, !parser.Screen().HideCursor(), "!parser.screen().hide_cursor()")
	assertTrue(t, !parser.Screen().BracketedPaste(), "!parser.screen().bracketed_paste()")
	assertEq(t,
		parser.Screen().MouseProtocolMode(),
		vt100.MouseProtocolMode_None,
	)
	assertEq(t,
		parser.Screen().MouseProtocolEncoding(),
		vt100.MouseProtocolEncoding_Default,
	)
}
