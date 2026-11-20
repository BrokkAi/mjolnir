// Port of vt100 0.16.2 src/lib.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

// src/lib.rs has no `#[test]` functions. This is the example of its
// documentation, which Rust runs as a test.

// # Synopsis
func TestLib_doc_synopsis(t *testing.T) {
	parser := vt100.ParserNew(24, 80, 0)

	screen := parser.Screen().Clone()
	_ = screen
	parser.Process([]byte("this text is \x1b[31mRED\x1b[m"))
	assertEq(t,
		parser.Screen().Cell(0, 13).Unwrap().Fgcolor(),
		vt100.Color(&vt100.Color_Idx{F0: 1}),
	)

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1b[3D\x1b[32mGREEN"))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jthis text is \x1b[32mGREEN"),
	)
	assertEq(t,
		parser.Screen().ContentsDiff(&screen),
		[]byte("\x1b[1;14H\x1b[32mGREEN"),
	)
}
