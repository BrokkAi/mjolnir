// Port of vt100 0.16.2 tests/basic.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItBasic_object_creation(t *testing.T) {
	parser := vt100.ParserDefault()
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
}

func TestItBasic_process_text(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	parser.Process(input)
	assertEq(t, parser.Screen().Contents(), "foobar")
}

func TestItBasic_set_size(t *testing.T) {
	parser := vt100.ParserDefault()
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))

	parser.ScreenMut().SetSize(34, 8)
	assertEq(t, pair(parser.Screen().Size()), pair(34, 8))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))

	parser.Process([]byte("\x1b[30;5H"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(29, 4))

	parser.ScreenMut().SetSize(24, 80)
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(23, 4))

	parser.ScreenMut().SetSize(34, 8)
	assertEq(t, pair(parser.Screen().Size()), pair(34, 8))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(23, 4))

	parser.Process([]byte("\x1b[?1049h"))
	assertEq(t, pair(parser.Screen().Size()), pair(34, 8))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))

	parser.ScreenMut().SetSize(24, 80)
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 0))

	parser.Process([]byte("\x1b[?1049l"))
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(23, 4))

	parser.ScreenMut().SetSize(34, 8)
	parser.Process([]byte("\x1bc01234567890123456789"))
	assertEq(t, parser.Screen().Contents(), "01234567890123456789")

	parser.ScreenMut().SetSize(24, 80)
	assertEq(t, parser.Screen().Contents(), "01234567\n89012345\n6789")

	parser.ScreenMut().SetSize(34, 8)
	assertEq(t, parser.Screen().Contents(), "01234567\n89012345\n6789")

	parser = vt100.ParserDefault()
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	parser.ScreenMut().SetSize(30, 100)
	assertEq(t, pair(parser.Screen().Size()), pair(30, 100))
	parser.Process([]byte("\x1b[75Cfoobar"))
	assertEq(t,
		parser.Screen().Contents(),
		"                                                                           foobar",
	)

	parser = vt100.ParserDefault()
	assertEq(t, pair(parser.Screen().Size()), pair(24, 80))
	parser.ScreenMut().SetSize(30, 100)
	assertEq(t, pair(parser.Screen().Size()), pair(30, 100))
	parser.Process([]byte("1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12\r\n13\r\n14\r\n15\r\n16\r\n17\r\n18\r\n19\r\n20\r\n21\r\n22\r\n23\r\n24\x1b[24;99Hfoobar"))
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24                                                                                                foobar",
	)
}

func TestItBasic_cell_contents(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	parser.Process(input)
	assertEq(t, parser.Screen().Cell(0, 0).Unwrap().Contents(), "f")
	assertEq(t, parser.Screen().Cell(0, 1).Unwrap().Contents(), "o")
	assertEq(t, parser.Screen().Cell(0, 2).Unwrap().Contents(), "o")
	assertEq(t, parser.Screen().Cell(0, 3).Unwrap().Contents(), "b")
	assertEq(t, parser.Screen().Cell(0, 4).Unwrap().Contents(), "a")
	assertEq(t, parser.Screen().Cell(0, 5).Unwrap().Contents(), "r")
	assertEq(t, parser.Screen().Cell(0, 6).Unwrap().Contents(), "")
}

func TestItBasic_cell_colors(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	parser.Process(input)

	assertEq(t,
		parser.Screen().Cell(0, 0).Unwrap().Fgcolor(),
		vt100.ColorDefault(),
	)
	assertEq(t,
		parser.Screen().Cell(0, 3).Unwrap().Fgcolor(),
		vt100.Color(&vt100.Color_Idx{F0: 2}),
	)
	assertEq(t,
		parser.Screen().Cell(0, 4).Unwrap().Fgcolor(),
		vt100.Color(&vt100.Color_Idx{F0: 2}),
	)
	assertEq(t,
		parser.Screen().Cell(0, 4).Unwrap().Bgcolor(),
		vt100.Color(&vt100.Color_Idx{F0: 2}),
	)
}

func TestItBasic_cell_attrs(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	parser.Process(input)

	assertTrue(t, parser.Screen().Cell(0, 4).Unwrap().Italic(), "parser.screen().cell(0, 4).unwrap().italic()")
}
