// Port of vt100 0.16.2 tests/window_contents.rs

package vt100_test

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItWindowContents_formatted(t *testing.T) {
	parser := vt100.ParserDefault()
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J"),
	)

	parser.Process([]byte("foobar"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertTrue(t, !parser.Screen().Cell(0, 2).Unwrap().Bold(), "!parser.screen().cell(0, 2).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 3).Unwrap().Bold(), "!parser.screen().cell(0, 3).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 4).Unwrap().Bold(), "!parser.screen().cell(0, 4).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 5).Unwrap().Bold(), "!parser.screen().cell(0, 5).unwrap().bold()")
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoobar"),
	)

	parser.Process([]byte("\x1b[1;4H\x1b[1;7m\x1b[33mb"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertTrue(t, !parser.Screen().Cell(0, 2).Unwrap().Bold(), "!parser.screen().cell(0, 2).unwrap().bold()")
	assertTrue(t, parser.Screen().Cell(0, 3).Unwrap().Bold(), "parser.screen().cell(0, 3).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 4).Unwrap().Bold(), "!parser.screen().cell(0, 4).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 5).Unwrap().Bold(), "!parser.screen().cell(0, 5).unwrap().bold()")
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoo\x1b[33;1;7mb\x1b[mar\x1b[1;5H\x1b[33;1;7m"),
	)

	parser.Process([]byte("\x1b[1;5H\x1b[22;42ma"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertTrue(t, !parser.Screen().Cell(0, 2).Unwrap().Bold(), "!parser.screen().cell(0, 2).unwrap().bold()")
	assertTrue(t, parser.Screen().Cell(0, 3).Unwrap().Bold(), "parser.screen().cell(0, 3).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 4).Unwrap().Bold(), "!parser.screen().cell(0, 4).unwrap().bold()")
	assertTrue(t, !parser.Screen().Cell(0, 5).Unwrap().Bold(), "!parser.screen().cell(0, 5).unwrap().bold()")
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoo\x1b[33;1;7mb\x1b[42;22ma\x1b[mr\x1b[1;6H\x1b[33;42;7m"),
	)

	parser.Process([]byte("\x1b[1;6H\x1b[35mr\r\nquux"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoo\x1b[33;1;7mb\x1b[42;22ma\x1b[35mr\r\nquux"),
	)

	parser.Process([]byte("\x1b[2;1H\x1b[45mquux"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoo\x1b[33;1;7mb\x1b[42;22ma\x1b[35mr\r\n\x1b[45mquux"),
	)

	parser.
		Process([]byte("\x1b[2;2H\x1b[38;2;123;213;231mu\x1b[38;5;254mu\x1b[39mx"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertEq(t, parser.Screen().ContentsFormatted(), []byte("\x1b[?25h\x1b[m\x1b[H\x1b[Jfoo\x1b[33;1;7mb\x1b[42;22ma\x1b[35mr\r\n\x1b[45mq\x1b[38;2;123;213;231mu\x1b[38;5;254mu\x1b[39mx"))
}

func TestItWindowContents_empty_cells(t *testing.T) {
	parser := vt100.ParserDefault()
	parser.Process([]byte("\x1b[5C\x1b[32m bar\x1b[H\x1b[31mfoo"))
	helpersContentsFormattedReproducesScreen(t, parser.Screen())
	assertEq(t, parser.Screen().Contents(), "foo   bar")
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[31mfoo\x1b[2C\x1b[32m bar\x1b[1;4H\x1b[31m"),
	)
}

func TestItWindowContents_cursor_positioning(t *testing.T) {
	parser := vt100.ParserDefault()

	screen := parser.Screen().Clone()
	parser.Process([]byte(":\x1b[K"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 1))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J:"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte(":"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("a"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 2))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J:a"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("a"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1b[1;2H\x1b[K"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(0, 1))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J:"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("\x1b[1;2H\x1b[K"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1b[H\x1b[J\x1b[4;80H"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(3, 79))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[4;80H"),
	)
	assertEq(t,
		parser.Screen().ContentsDiff(&screen),
		[]byte("\x1b[H\x1b[K\x1b[4;80H"),
	)

	screen = parser.Screen().Clone()
	parser.Process([]byte("a"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(3, 80))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[4;80Ha"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("a"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\n"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(4, 80))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[4;80Ha\n"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("\n"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("b"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(5, 1))
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[4;80Ha\x1b[6;1Hb"),
	)
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("\r\nb"))
}

// windowContentsRows is a list of 24 rows that are all empty but for the
// ones given, as the test `rows` writes them out.
func windowContentsRows[T any](rows map[int]T) []T {
	out := make([]T, 24)
	for i, row := range rows {
		out[i] = row
	}
	return out
}

func TestItWindowContents_rows(t *testing.T) {
	parser := vt100.ParserDefault()
	screen1 := parser.Screen().Clone()
	assertEq(t,
		slices.Collect(screen1.Rows(0, 80)),
		windowContentsRows(map[int]string{}),
	)
	assertEq(t, slices.Collect(screen1.RowsFormatted(0, 80)), windowContentsRows(map[int][]uint8{}))
	assertEq(t,
		slices.Collect(screen1.Rows(5, 15)),
		windowContentsRows(map[int]string{}),
	)
	assertEq(t, slices.Collect(screen1.RowsFormatted(5, 15)), windowContentsRows(map[int][]uint8{}))

	parser.
		Process([]byte("\x1b[31mfoo\x1b[10;10H\x1b[32mbar\x1b[20;20H\x1b[33mbaz"))
	screen2 := parser.Screen().Clone()
	assertEq(t,
		slices.Collect(screen2.Rows(0, 80)),
		windowContentsRows(map[int]string{
			0:  "foo",
			9:  "         bar",
			19: "                   baz",
		}),
	)
	assertEq(t,
		slices.Collect(screen2.RowsFormatted(0, 80)),
		windowContentsRows(map[int][]uint8{
			0:  []uint8("\x1b[31mfoo"),
			9:  []uint8("\x1b[9C\x1b[32mbar"),
			19: []uint8("\x1b[19C\x1b[33mbaz"),
		}),
	)
	assertEq(t,
		slices.Collect(screen2.Rows(5, 15)),
		windowContentsRows(map[int]string{
			9:  "    bar",
			19: "              b",
		}),
	)
	assertEq(t,
		slices.Collect(screen2.RowsFormatted(5, 15)),
		windowContentsRows(map[int][]uint8{
			9:  []uint8("\x1b[4C\x1b[32mbar"),
			19: []uint8("\x1b[14C\x1b[33mb"),
		}),
	)

	assertEq(t,
		slices.Collect(screen2.RowsDiff(&screen1, 0, 80)),
		windowContentsRows(map[int][]uint8{
			0:  []uint8("\x1b[31mfoo"),
			9:  []uint8("\x1b[9C\x1b[32mbar"),
			19: []uint8("\x1b[19C\x1b[33mbaz"),
		}),
	)

	parser.Process([]byte("\x1b[10;11Ho"))
	screen3 := parser.Screen().Clone()
	assertEq(t,
		slices.Collect(screen3.RowsDiff(&screen2, 0, 80)),
		windowContentsRows(map[int][]uint8{
			9: []uint8("\x1b[10C\x1b[33mo"),
		}),
	)
}

func TestItWindowContents_contents_between(t *testing.T) {
	parser := vt100.ParserDefault()
	assertEq(t, parser.Screen().ContentsBetween(0, 0, 0, 0), "")
	assertEq(t, parser.Screen().ContentsBetween(0, 0, 5, 0), "\n\n\n\n\n")
	assertEq(t, parser.Screen().ContentsBetween(5, 0, 0, 0), "")

	parser.Process(
		[]byte("Lorem ipsum dolor sit amet, consectetur adipiscing elit, " +
			"sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n\n" +
			"Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris " +
			"nisi ut aliquip ex ea commodo consequat.\n\n" +
			"Duis aute irure dolor in reprehenderit in voluptate velit esse cillum " +
			"dolore eu fugiat nulla pariatur.\n\n" +
			"Excepteur sint occaecat cupidatat non proident, sunt in culpa qui " +
			"officia deserunt mollit anim id est laborum."),
	)
	assertEq(t, parser.Screen().ContentsBetween(0, 0, 0, 0), "")
	assertEq(t,
		parser.Screen().ContentsBetween(0, 0, 0, 26),
		"Lorem ipsum dolor sit amet",
	)
	assertEq(t, parser.Screen().ContentsBetween(0, 26, 0, 0), "")
	assertEq(t,
		parser.Screen().ContentsBetween(0, 57, 1, 43),
		"sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.",
	)
	assertEq(t,
		parser.Screen().ContentsBetween(0, 57, 2, 0),
		"sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n",
	)
	assertEq(t, parser.Screen().ContentsBetween(2, 0, 0, 57), "")
}

func TestItWindowContents_diff_basic(t *testing.T) {
	parser := vt100.ParserDefault()
	screen1 := parser.Screen().Clone()
	parser.Process([]byte("\x1b[5C\x1b[32m bar"))
	screen2 := parser.Screen().Clone()
	assertEq(t, screen2.ContentsDiff(&screen1), []byte("\x1b[5C\x1b[32m bar"))
	helpersAssertContentsDiffReproducesStateFromScreens(t,
		&screen1, &screen2,
	)

	parser.Process([]byte("\x1b[H\x1b[31mfoo"))
	screen3 := parser.Screen().Clone()
	assertEq(t, screen3.ContentsDiff(&screen2), []byte("\x1b[H\x1b[31mfoo"))
	helpersAssertContentsDiffReproducesStateFromScreens(t,
		&screen2, &screen3,
	)

	parser.Process([]byte("\x1b[1;7H\x1b[32mbaz"))
	screen4 := parser.Screen().Clone()
	assertEq(t, screen4.ContentsDiff(&screen3), []byte("\x1b[5C\x1b[32mz"))
	helpersAssertContentsDiffReproducesStateFromScreens(t,
		&screen3, &screen4,
	)

	parser.Process([]byte("\x1b[1;8H\x1b[X"))
	screen5 := parser.Screen().Clone()
	assertEq(t, screen5.ContentsDiff(&screen4), []byte("\x1b[1;8H\x1b[X"))
	helpersAssertContentsDiffReproducesStateFromScreens(t,
		&screen4, &screen5,
	)
}

func TestItWindowContents_diff_erase(t *testing.T) {
	parser := vt100.ParserDefault()

	screen := parser.Screen().Clone()
	parser.Process([]byte("foo\x1b[5;5Hbar"))
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("foo\x1b[5;5Hbar"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1b[3D\x1b[2X"))
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("\x1b[5;5H\x1b[2X"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1bcfoo\x1b[5;5Hbar"))
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("ba\x1b[C"))

	screen = parser.Screen().Clone()
	parser.Process([]byte("\x1b[3D\x1b[3X"))
	assertEq(t, parser.Screen().ContentsDiff(&screen), []byte("\x1b[5;5H\x1b[K"))
}

func TestItWindowContents_diff_crawl_short(t *testing.T) {
	windowContentsDiffCrawl(t, 500)
}

func TestItWindowContents_diff_crawl_full(t *testing.T) {
	ignored(t)
	windowContentsDiffCrawl(t, 7625)
}

// windowContentsCrawlFrame is the contents of tests/data/crawl/crawl<i>.
func windowContentsCrawlFrame(t testing.TB, i int) []uint8 {
	t.Helper()
	frames, err := vt100.TestdataCrawlFrames()
	if err != nil {
		t.Fatalf("read the crawl frames: %v", err)
	}
	if i < 1 || i > len(frames) {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: no frame tests/data/crawl/crawl%d", i)
	}
	return frames[i-1]
}

func windowContentsDiffCrawl(t testing.TB, i int) {
	t.Helper()
	parser := vt100.ParserDefault()
	screens := []vt100.Screen{}
	for frameIndex := 1; frameIndex <= i; frameIndex++ {
		frame := windowContentsCrawlFrame(t, frameIndex)
		parser.Process(frame)
		screens = append(screens, parser.Screen().Clone())
	}

	for index := 0; index+1 < len(screens); index++ {
		prevScreen, screen := &screens[index], &screens[index+1]
		helpersAssertContentsDiffReproducesStateFromScreens(t,
			prevScreen,
			screen,
		)
	}
}
