// Port of vt100 0.16.2 tests/scroll.rs

package vt100_test

import (
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItScroll_scroll_regions(t *testing.T) {
	helpersFixture(t, "decstbm")
}

func TestItScroll_origin_mode(t *testing.T) {
	helpersFixture(t, "origin_mode")
}

func TestItScroll_scrollback(t *testing.T) {
	parser := vt100.ParserNew(24, 80, 10)

	parser.Process([]byte("1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12\r\n13\r\n14\r\n15\r\n16\r\n17\r\n18\r\n19\r\n20\r\n21\r\n22\r\n23\r\n24"))
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n25\r\n26\r\n27\r\n28\r\n29\r\n30"))
	assertEq(t,
		parser.Screen().Contents(),
		"7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30",
	)

	parser.ScreenMut().SetScrollback(0)
	assertEq(t, parser.Screen().Scrollback(), 0)
	assertEq(t,
		parser.Screen().Contents(),
		"7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30",
	)

	parser.ScreenMut().SetScrollback(1)
	assertEq(t, parser.Screen().Scrollback(), 1)
	assertEq(t,
		parser.Screen().Contents(),
		"6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29",
	)

	parser.ScreenMut().SetScrollback(3)
	assertEq(t, parser.Screen().Scrollback(), 3)
	assertEq(t,
		parser.Screen().Contents(),
		"4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27",
	)

	parser.ScreenMut().SetScrollback(6)
	assertEq(t, parser.Screen().Scrollback(), 6)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.ScreenMut().SetScrollback(7)
	assertEq(t, parser.Screen().Scrollback(), 6)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.ScreenMut().SetScrollback(0)
	assertEq(t, parser.Screen().Scrollback(), 0)
	assertEq(t,
		parser.Screen().Contents(),
		"7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30",
	)

	parser.ScreenMut().SetScrollback(7)
	assertEq(t, parser.Screen().Scrollback(), 6)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n31"))
	assertEq(t, parser.Screen().Scrollback(), 7)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n32"))
	assertEq(t, parser.Screen().Scrollback(), 8)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n33"))
	assertEq(t, parser.Screen().Scrollback(), 9)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n34"))
	assertEq(t, parser.Screen().Scrollback(), 10)
	assertEq(t,
		parser.Screen().Contents(),
		"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24",
	)

	parser.Process([]byte("\r\n35"))
	assertEq(t, parser.Screen().Scrollback(), 10)
	assertEq(t,
		parser.Screen().Contents(),
		"2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25",
	)

	parser.Process([]byte("\r\n36"))
	assertEq(t, parser.Screen().Scrollback(), 10)
	assertEq(t,
		parser.Screen().Contents(),
		"3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26",
	)

	parser.ScreenMut().SetScrollback(12)
	assertEq(t, parser.Screen().Scrollback(), 10)
	assertEq(t,
		parser.Screen().Contents(),
		"3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26",
	)

	parser.ScreenMut().SetScrollback(0)
	assertEq(t, parser.Screen().Scrollback(), 0)
	assertEq(t,
		parser.Screen().Contents(),
		"13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30\n31\n32\n33\n34\n35\n36",
	)

	parser.Process([]byte("\r\n37\r\n38"))
	assertEq(t, parser.Screen().Scrollback(), 0)
	assertEq(t,
		parser.Screen().Contents(),
		"15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30\n31\n32\n33\n34\n35\n36\n37\n38",
	)

	parser.ScreenMut().SetScrollback(5)
	assertEq(t, parser.Screen().Scrollback(), 5)
	assertEq(t,
		parser.Screen().Contents(),
		"10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30\n31\n32\n33",
	)

	parser.Process([]byte("\r\n39\r\n40"))
	assertEq(t, parser.Screen().Scrollback(), 7)
	assertEq(t,
		parser.Screen().Contents(),
		"10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n27\n28\n29\n30\n31\n32\n33",
	)
}

func TestItScroll_edge_of_screen(t *testing.T) {
	parser := vt100.ParserDefault()
	screen := parser.Screen().Clone()

	parser.Process([]byte("\x1b[31m\x1b[24;75Hfooba\x08r\x08\x1b[1@a"))
	assertEq(t, pair(parser.Screen().CursorPosition()), pair(23, 79))
	assertEq(t,
		parser.Screen().Contents(),
		"\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n                                                                          foobar",
	)
	assertEq(t,
		parser.Screen().ContentsFormatted(),
		[]byte("\x1b[?25h\x1b[m\x1b[H\x1b[J\x1b[24;75H\x1b[31mfoobar\x1b[24;80H"),
	)
	assertEq(t,
		parser.Screen().ContentsDiff(&screen),
		[]byte("\x1b[24;75H\x1b[31mfoobar\x1b[24;80H"),
	)
}

func TestItScroll_scrollback_larger_than_rows(t *testing.T) {
	parser := vt100.ParserNew(3, 20, 10)

	parser.Process([]byte(scrollGenNums(1, 10, "\r\n")))

	// 1. Extra rows returned
	parser.ScreenMut().SetScrollback(4)
	assertEq(t, parser.Screen().Contents(), scrollGenNums(4, 6, "\n"))

	// 2. Subtraction overflow
	parser.ScreenMut().SetScrollback(10)
	assertEq(t, parser.Screen().Contents(), scrollGenNums(1, 3, "\n"))
}

// scrollGenNums is `gen_nums(first..=last, join)`.
func scrollGenNums(first, last uint8, join string) string {
	nums := []string{}
	for num := int(first); num <= int(last); num++ {
		nums = append(nums, strconv.Itoa(num))
	}
	return strings.Join(nums, join)
}
