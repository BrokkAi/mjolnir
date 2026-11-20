// Port of vt100 0.16.2 tests/helpers/mod.rs

package vt100_test

import (
	"slices"
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The helpers every test file of the crate shares (`mod helpers;`). They are
// called with the test's *testing.T, which receives what Rust writes to
// standard error, and a failed assertion stops the test.

var helpersQuiet = false

// helpersIs is the macro `is!`: when got and expected differ it reports
// both under the text of their expressions, and the comparison that called
// it fails.
func helpersIs[T any](t testing.TB, got, expected T, gotText, expectedText string) bool {
	if !helpersEq(got, expected) {
		// Only here: Helper records the caller's frames, and the
		// comparisons run for every cell of every screen.
		t.Helper()
		if !helpersQuiet {
			t.Logf("%s != %s:", gotText, expectedText)
			t.Logf("     got: %s", rs.Format("{:?}", got))
			t.Logf("expected: %s", rs.Format("{:?}", expected))
		}
		return false
	}
	return true
}

// helpersEq is the `==` of `is!`. A cell compares with its own PartialEq,
// which is what rs.Eq calls too, without going through reflection: the
// helpers compare every cell of every screen.
func helpersEq[T any](got, expected T) bool {
	if got, ok := any(got).(rs.Option[*vt100.Cell]); ok {
		expected := any(expected).(rs.Option[*vt100.Cell])
		gotCell, gotSome := got.Get()
		expectedCell, expectedSome := expected.Get()
		if gotSome != expectedSome {
			return false
		}
		return !gotSome || gotCell.Equal(expectedCell)
	}
	return rs.Eq(got, expected)
}

// helpersOk is the macro `ok!`.
func helpersOk(t testing.TB, condition bool, text string) bool {
	if !condition {
		t.Helper()
		if !helpersQuiet {
			t.Logf("!%s", text)
		}
		return false
	}
	return true
}

// helpersBytes is `Bytes`: bytes whose Debug form is a byte string.
type helpersBytes []uint8

func (self helpersBytes) FmtDebug(f *rs.Formatter) error {
	var out strings.Builder
	out.WriteString("b\"")
	for _, c := range self {
		switch {
		case c == 10:
			out.WriteString("\\n")
		case c == 13:
			out.WriteString("\\r")
		case c == 92:
			out.WriteString("\\\\")
		case c >= 32 && c <= 126:
			out.WriteByte(c)
		default:
			out.WriteString("\\x" + hex2(c))
		}
	}
	out.WriteString("\"")
	return f.WriteStr(out.String())
}

func hex2(c uint8) string {
	const digits = "0123456789abcdef"
	return string([]byte{digits[c>>4], digits[c&15]})
}

// zipped is `a.zip(b)` over two of the screen's row iterators: the pairs up
// to the end of the shorter.
func zipped[T any](a, b func(yield func(T) bool)) [][2]T {
	left, right := slices.Collect(a), slices.Collect(b)
	out := make([][2]T, 0, min(len(left), len(right)))
	for i := 0; i < len(left) && i < len(right); i++ {
		out = append(out, [2]T{left[i], right[i]})
	}
	return out
}

func helpersCompareScreens(
	t testing.TB,
	got *vt100.Screen,
	expected *vt100.Screen,
) bool {
	t.Helper()
	rows, cols := got.Size()

	if !helpersIs(t, got.Contents(), expected.Contents(), "got.contents()", "expected.contents()") {
		return false
	}
	if !helpersIs(t,
		helpersBytes(got.ContentsFormatted()),
		helpersBytes(expected.ContentsFormatted()),
		"Bytes(&got.contents_formatted())", "Bytes(&expected.contents_formatted())",
	) {
		return false
	}
	for _, rows := range zipped(got.Rows(0, cols), expected.Rows(0, cols)) {
		gotRow, expectedRow := rows[0], rows[1]
		if !helpersIs(t, gotRow, expectedRow, "got_row", "expected_row") {
			return false
		}
	}
	for _, rows := range zipped(got.RowsFormatted(0, cols), expected.RowsFormatted(0, cols)) {
		gotRow, expectedRow := rows[0], rows[1]
		if !helpersIs(t, helpersBytes(gotRow), helpersBytes(expectedRow), "Bytes(&got_row)", "Bytes(&expected_row)") {
			return false
		}
	}
	for i := uint32(0); i < uint32(rows); i++ {
		if !helpersIs(t, got.RowWrapped(uint16(i)), expected.RowWrapped(uint16(i)), "got.row_wrapped(i)", "expected.row_wrapped(i)") {
			return false
		}
	}
	defaultParser, defaultParser2 := vt100.ParserDefault(), vt100.ParserDefault()
	if !helpersIs(t,
		helpersBytes(got.ContentsDiff(defaultParser.Screen())),
		helpersBytes(expected.ContentsDiff(defaultParser2.Screen())),
		"Bytes(&got.contents_diff(vt100::Parser::default().screen()))",
		"Bytes(&expected.contents_diff(vt100::Parser::default().screen()))",
	) {
		return false
	}

	if !helpersIs(t, helpersBytes(got.ContentsDiff(got)), helpersBytes(""), "Bytes(&got.contents_diff(got))", "Bytes(b\"\")") {
		return false
	}

	for row := uint32(0); row < uint32(rows); row++ {
		for col := uint32(0); col < uint32(cols); col++ {
			expectedCell := expected.Cell(uint16(row), uint16(col))
			gotCell := got.Cell(uint16(row), uint16(col))
			if !helpersIs(t, gotCell, expectedCell, "got_cell", "expected_cell") {
				return false
			}
		}
	}

	gotRow, gotCol := got.CursorPosition()
	expectedRow, expectedCol := expected.CursorPosition()
	if !helpersIs(t, pair(gotRow, gotCol), pair(expectedRow, expectedCol), "got.cursor_position()", "expected.cursor_position()") {
		return false
	}
	if !helpersOk(t, gotRow <= rows, "got.cursor_position().0 <= rows") {
		return false
	}
	if !helpersOk(t, expectedRow <= rows, "expected.cursor_position().0 <= rows") {
		return false
	}
	if !helpersOk(t, gotCol <= cols, "got.cursor_position().1 <= cols") {
		return false
	}
	if !helpersOk(t, expectedCol <= cols, "expected.cursor_position().1 <= cols") {
		return false
	}

	if !helpersIs(t, got.ApplicationKeypad(), expected.ApplicationKeypad(), "got.application_keypad()", "expected.application_keypad()") {
		return false
	}
	if !helpersIs(t, got.ApplicationCursor(), expected.ApplicationCursor(), "got.application_cursor()", "expected.application_cursor()") {
		return false
	}
	if !helpersIs(t, got.HideCursor(), expected.HideCursor(), "got.hide_cursor()", "expected.hide_cursor()") {
		return false
	}
	if !helpersIs(t, got.BracketedPaste(), expected.BracketedPaste(), "got.bracketed_paste()", "expected.bracketed_paste()") {
		return false
	}
	if !helpersIs(t, got.MouseProtocolMode(), expected.MouseProtocolMode(), "got.mouse_protocol_mode()", "expected.mouse_protocol_mode()") {
		return false
	}
	if !helpersIs(t,
		got.MouseProtocolEncoding(),
		expected.MouseProtocolEncoding(),
		"got.mouse_protocol_encoding()", "expected.mouse_protocol_encoding()",
	) {
		return false
	}

	return true
}

func helpersContentsFormattedReproducesState(t testing.TB, input []uint8) bool {
	t.Helper()
	parser := vt100.ParserDefault()
	parser.Process(input)
	return helpersContentsFormattedReproducesScreen(t, parser.Screen())
}

func helpersRowsFormattedReproducesState(t testing.TB, input []uint8) bool {
	t.Helper()
	parser := vt100.ParserDefault()
	parser.Process(input)
	return helpersRowsFormattedReproducesScreen(t, parser.Screen())
}

func helpersContentsFormattedReproducesScreen(t testing.TB, screen *vt100.Screen) bool {
	t.Helper()
	newInput := screen.ContentsFormatted()
	newInput = append(newInput, screen.InputModeFormatted()...)
	assertEq(t, newInput, screen.StateFormatted())
	newParser := vt100.ParserDefault()
	newParser.Process(newInput)
	gotScreen := newParser.Screen().Clone()

	return helpersCompareScreens(t, &gotScreen, screen)
}

func helpersRowsFormattedReproducesScreen(t testing.TB, screen *vt100.Screen) bool {
	t.Helper()
	newInput := []uint8{}
	wrapped := false
	idx := 0
	for row := range screen.RowsFormatted(0, 80) {
		newInput = append(newInput, "\x1b[m"...)
		if !wrapped {
			newInput = append(newInput, "\x1b["+strconv.Itoa(idx+1)+"H"...)
		}
		newInput = append(newInput, row...)
		// `idx.try_into().unwrap()`: there are never more rows than a u16
		// holds.
		wrapped = screen.RowWrapped(uint16(idx))
		idx++
	}
	newInput = append(newInput, "\x1b[m"...)
	newInput = append(newInput, screen.CursorStateFormatted()...)
	newInput = append(newInput, screen.AttributesFormatted()...)
	newInput = append(newInput, screen.InputModeFormatted()...)
	newParser := vt100.ParserDefault()
	newParser.Process(newInput)
	gotScreen := newParser.Screen().Clone()

	return helpersCompareScreens(t, &gotScreen, screen)
}

func helpersAssertContentsFormattedReproducesState(t testing.TB, input []uint8) {
	t.Helper()
	assertTrue(t, helpersContentsFormattedReproducesState(t, input), "contents_formatted_reproduces_state(input)")
}

func helpersAssertRowsFormattedReproducesState(t testing.TB, input []uint8) {
	t.Helper()
	assertTrue(t, helpersRowsFormattedReproducesState(t, input), "rows_formatted_reproduces_state(input)")
}

func helpersContentsDiffReproducesState(t testing.TB, input []uint8) bool {
	t.Helper()
	return helpersContentsDiffReproducesStateFrom(t, input, []uint8{})
}

func helpersContentsDiffReproducesStateFrom(
	t testing.TB,
	input []uint8,
	prevInput []uint8,
) bool {
	t.Helper()
	parser := vt100.ParserDefault()
	parser.Process(prevInput)
	prevScreen := parser.Screen().Clone()
	parser.Process(input)

	return helpersContentsDiffReproducesStateFromScreens(t, &prevScreen, parser.Screen())
}

func helpersContentsDiffReproducesStateFromScreens(
	t testing.TB,
	prevScreen *vt100.Screen,
	screen *vt100.Screen,
) bool {
	t.Helper()
	diffInput := screen.ContentsDiff(prevScreen)
	diffInput = append(diffInput, screen.InputModeDiff(prevScreen)...)
	assertEq(t, diffInput, screen.StateDiff(prevScreen))

	diffPrevInput := prevScreen.ContentsFormatted()
	diffPrevInput = append(diffPrevInput, screen.InputModeFormatted()...)

	newParser := vt100.ParserDefault()
	newParser.Process(diffPrevInput)
	newParser.Process(diffInput)
	gotScreen := newParser.Screen().Clone()

	return helpersCompareScreens(t, &gotScreen, screen)
}

func helpersAssertContentsDiffReproducesStateFromScreens(
	t testing.TB,
	prevScreen *vt100.Screen,
	screen *vt100.Screen,
) {
	t.Helper()
	assertTrue(t,
		helpersContentsDiffReproducesStateFromScreens(t, prevScreen, screen),
		"contents_diff_reproduces_state_from_screens(prev_screen, screen)",
	)
}

func helpersAssertContentsDiffReproducesStateFrom(
	t testing.TB,
	input []uint8,
	prevInput []uint8,
) {
	t.Helper()
	assertTrue(t,
		helpersContentsDiffReproducesStateFrom(t, input, prevInput),
		"contents_diff_reproduces_state_from(input, prev_input)",
	)
}

func helpersAssertReproducesState(t testing.TB, input []uint8) {
	t.Helper()
	helpersAssertReproducesStateFrom(t, input, []uint8{})
}

func helpersAssertReproducesStateFrom(t testing.TB, input []uint8, prevInput []uint8) {
	t.Helper()
	fullInput := slices.Concat(prevInput, input)
	helpersAssertContentsFormattedReproducesState(t, fullInput)
	helpersAssertRowsFormattedReproducesState(t, fullInput)
	helpersAssertContentsDiffReproducesStateFrom(t, input, prevInput)
}

func helpersFormatBytes(bytes_ []uint8) string {
	v := []uint8{}
	for _, b := range bytes_ {
		switch {
		case b == 10:
			v = append(v, "\\n"...)
		case b == 13:
			v = append(v, "\\r"...)
		case b == 27:
			v = append(v, "\\e"...)
		case b < 32 || b == 127:
			v = append(v, "\\x"+hex2(b)...)
		default:
			v = append(v, b)
		}
	}
	return strs.FromUtf8Lossy(v)
}

func helpersHexChar(c uint8) (uint8, error) {
	switch c {
	case '0':
		return 0, nil
	case '1':
		return 1, nil
	case '2':
		return 2, nil
	case '3':
		return 3, nil
	case '4':
		return 4, nil
	case '5':
		return 5, nil
	case '6':
		return 6, nil
	case '7':
		return 7, nil
	case '8':
		return 8, nil
	case '9':
		return 9, nil
	case 'a':
		return 10, nil
	case 'b':
		return 11, nil
	case 'c':
		return 12, nil
	case 'd':
		return 13, nil
	case 'e':
		return 14, nil
	case 'f':
		return 15, nil
	case 'A':
		return 10, nil
	case 'B':
		return 11, nil
	case 'C':
		return 12, nil
	case 'D':
		return 13, nil
	case 'E':
		return 14, nil
	case 'F':
		return 15, nil
	default:
		return 0, helpersError("invalid hex char")
	}
}

// helpersError is the `String` the helpers use as an error.
type helpersError string

func (self helpersError) Error() string { return string(self) }

func helpersHex(upper uint8, lower uint8) (uint8, error) {
	high, err := helpersHexChar(upper)
	if err != nil {
		return 0, err
	}
	low, err := helpersHexChar(lower)
	if err != nil {
		return 0, err
	}
	return high*16 + low, nil
}

func helpersUnhex(s []uint8) []uint8 {
	ret := []uint8{}
	i := 0
	for i < len(s) {
		if s[i] == '\\' {
			switch s[i+1] {
			case '\\':
				ret = append(ret, '\\')
				i += 2
			case 'x':
				upper := s[i+2]
				lower := s[i+3]
				ret = append(ret, rs.Unwrap(helpersHex(upper, lower)))
				i += 4
			case 'u':
				if s[i+2] != '{' {
					panic(rs.Panicf("assertion `left == right` failed\n  left: {}\n right: {}", s[i+2], uint8('{')))
				}
				digits := []uint8{}
				j := i + 3
				for s[j] != '}' {
					digits = append(digits, s[j])
					j++
				}
				digits2 := []uint8{}
				skipping := true
				for _, x := range digits {
					if skipping && x == '0' {
						continue
					}
					skipping = false
					digits2 = append(digits2, x)
				}
				codepoint := rs.Unwrap(strconv.ParseUint(string(digits2), 16, 32))
				if !utf8.ValidRune(rune(codepoint)) {
					panic(rs.Panicf("called `Result::unwrap()` on an `Err` value: CharTryFromError(())"))
				}
				ret = utf8.AppendRune(ret, rune(codepoint))
				i = j + 1
			case 'r':
				ret = append(ret, 0x0d)
				i += 2
			case 'n':
				ret = append(ret, 0x0a)
				i += 2
			case 't':
				ret = append(ret, 0x09)
				i += 2
			default:
				panic(rs.Panicf("invalid escape"))
			}
		} else {
			ret = append(ret, s[i])
			i++
		}
	}
	return ret
}
