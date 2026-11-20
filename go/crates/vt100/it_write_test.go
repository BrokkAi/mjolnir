// Port of vt100 0.16.2 tests/write.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItWrite_write_text(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	bytes_ := rs.Unwrap(parser.Write(input))
	assertEq(t, bytes_, len(input))
	assertEq(t, parser.Screen().Contents(), "foobar")
}

func TestItWrite_cell_contents(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	bytes_ := rs.Unwrap(parser.Write(input))
	assertEq(t, bytes_, len(input))
	assertEq(t, parser.Screen().Cell(0, 0).Unwrap().Contents(), "f")
	assertEq(t, parser.Screen().Cell(0, 1).Unwrap().Contents(), "o")
	assertEq(t, parser.Screen().Cell(0, 2).Unwrap().Contents(), "o")
	assertEq(t, parser.Screen().Cell(0, 3).Unwrap().Contents(), "b")
	assertEq(t, parser.Screen().Cell(0, 4).Unwrap().Contents(), "a")
	assertEq(t, parser.Screen().Cell(0, 5).Unwrap().Contents(), "r")
	assertEq(t, parser.Screen().Cell(0, 6).Unwrap().Contents(), "")
}

func TestItWrite_cell_colors(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	bytes_ := rs.Unwrap(parser.Write(input))
	assertEq(t, bytes_, len(input))

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

func TestItWrite_cell_attrs(t *testing.T) {
	parser := vt100.ParserDefault()
	input := []byte("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")
	bytes_ := rs.Unwrap(parser.Write(input))
	assertEq(t, bytes_, len(input))

	assertTrue(t, parser.Screen().Cell(0, 4).Unwrap().Italic(), "parser.screen().cell(0, 4).unwrap().italic()")
}
