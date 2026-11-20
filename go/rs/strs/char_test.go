// Port of coretests 1.99.1 tests/char.rs (the tests of what this package
// covers; see doc.go for what is left out)
//
// Not ported: test_convert_const (Go's own conversions); test_char_case,
// test_to_casefold_unnormalized and test_as_ascii_invalid, which test the
// unstable char::case, to_casefold_unnormalized and as_ascii;
// test_decode_utf16_size_hint (an iterator's size hint); and
// test_encode_utf8_raw_buffer_too_small and
// test_encode_utf16_raw_buffer_too_small, which test the unstable
// encode_utf8_raw and encode_utf16_raw (CharEncodeUtf8 and CharEncodeUtf16
// return a new value rather than fill a buffer).

package strs

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestChar_test_convert(t *testing.T) {
	eq(t, CharFromU32(0), rs.Some(rune(0)))
	eq(t, CharFromU32(0x61), rs.Some('a'))
	eq(t, CharFromU32(0xD7FF), rs.Some(rune(0xD7FF)))
	isTrue(t, CharFromU32(0xD800).IsNone())
	isTrue(t, CharFromU32(0xDFFF).IsNone())
	eq(t, CharFromU32(0xE000), rs.Some(rune(0xE000)))
	eq(t, CharFromU32(0x10FFFF), rs.Some(rune(0x10FFFF)))
	isTrue(t, CharFromU32(0x110000).IsNone())
	isTrue(t, CharFromU32(0xFFFF_FFFF).IsNone())
}

func TestChar_test_from_str(t *testing.T) {
	eq(t, rs.Unwrap(ParseChar("a")), 'a')
	eq(t, rs.Unwrap(ParseChar("\x00")), rune(0))
	eq(t, rs.Unwrap(ParseChar("\ud7ff")), rune(0xd7FF))
	_, err := ParseChar("")
	isTrue(t, err != nil)
	_, err = ParseChar("abc")
	isTrue(t, err != nil)
}

func TestChar_test_is_cased(t *testing.T) {
	isTrue(t, charIsCased('a'))
	isTrue(t, charIsCased('ö'))
	isTrue(t, charIsCased('ß'))
	isTrue(t, charIsCased('Ü'))
	isTrue(t, charIsCased('P'))
	isTrue(t, charIsCased('ª'))
	isTrue(t, !charIsCased('攂'))
}

func TestChar_test_is_lowercase(t *testing.T) {
	isTrue(t, IsLowercase('a'))
	isTrue(t, IsLowercase('ö'))
	isTrue(t, IsLowercase('ß'))
	isTrue(t, !IsLowercase('Ü'))
	isTrue(t, !IsLowercase('P'))
}

func TestChar_test_is_titlecase(t *testing.T) {
	isTrue(t, charIsTitlecase('ǅ'))
	isTrue(t, charIsTitlecase('ᾨ'))
	isTrue(t, !charIsTitlecase('h'))
	isTrue(t, !charIsTitlecase('ä'))
	isTrue(t, !charIsTitlecase('ß'))
	isTrue(t, !charIsTitlecase('Ö'))
	isTrue(t, !charIsTitlecase('T'))
}

func TestChar_test_is_uppercase(t *testing.T) {
	isTrue(t, !IsUppercase('h'))
	isTrue(t, !IsUppercase('ä'))
	isTrue(t, !IsUppercase('ß'))
	isTrue(t, IsUppercase('Ö'))
	isTrue(t, IsUppercase('T'))
}

// charIsTitlecase is char::is_titlecase, which is unstable in 1.99.1 and so
// not part of the package; the two tests below need it.
func charIsTitlecase(c rune) bool {
	return c > 0x01C4 && unicodeLt(c)
}

func TestChar_titlecase_fast_path(t *testing.T) {
	for c := rune(0); c <= 0x01C4; c++ {
		isTrue(t, !(charIsCased(c) && !IsLowercase(c) && !IsUppercase(c)))
	}
}

func TestChar_at_most_one_case(t *testing.T) {
	for c := rune(0); c <= 0x10FFFF; c++ {
		if CharFromU32(uint32(c)).IsNone() {
			continue
		}
		count := 0
		for _, is := range []bool{!charIsCased(c), IsLowercase(c), IsUppercase(c), charIsTitlecase(c)} {
			if is {
				count++
			}
		}
		if count != 1 {
			t.Fatalf("U+%04X is in %d of the four case classes", c, count)
		}
	}
}

func TestChar_test_is_whitespace(t *testing.T) {
	isTrue(t, IsWhitespace(' '))
	isTrue(t, IsWhitespace('\u2007'))
	isTrue(t, IsWhitespace('\t'))
	isTrue(t, IsWhitespace('\n'))
	isTrue(t, !IsWhitespace('a'))
	isTrue(t, !IsWhitespace('_'))
	isTrue(t, !IsWhitespace(0))
}

func TestChar_test_to_digit(t *testing.T) {
	none := rs.None[uint32]()
	eq(t, CharToDigit('0', 10), rs.Some(uint32(0)))
	eq(t, CharToDigit('1', 2), rs.Some(uint32(1)))
	eq(t, CharToDigit('2', 3), rs.Some(uint32(2)))
	eq(t, CharToDigit('9', 10), rs.Some(uint32(9)))
	eq(t, CharToDigit('a', 16), rs.Some(uint32(10)))
	eq(t, CharToDigit('A', 16), rs.Some(uint32(10)))
	eq(t, CharToDigit('b', 16), rs.Some(uint32(11)))
	eq(t, CharToDigit('B', 16), rs.Some(uint32(11)))
	eq(t, CharToDigit('A', 36), rs.Some(uint32(10)))
	eq(t, CharToDigit('z', 36), rs.Some(uint32(35)))
	eq(t, CharToDigit('Z', 36), rs.Some(uint32(35)))
	eq(t, CharToDigit('[', 36), none)
	eq(t, CharToDigit('`', 36), none)
	eq(t, CharToDigit('{', 36), none)
	eq(t, CharToDigit('$', 36), none)
	eq(t, CharToDigit('@', 16), none)
	eq(t, CharToDigit('G', 16), none)
	eq(t, CharToDigit('g', 16), none)
	eq(t, CharToDigit(' ', 10), none)
	eq(t, CharToDigit('/', 10), none)
	eq(t, CharToDigit(':', 10), none)
	eq(t, CharToDigit(':', 11), none)
}

func TestChar_test_to_lowercase(t *testing.T) {
	lower := func(c rune) string { return string(CharToLowercase(c)) }
	eq(t, lower('A'), "a")
	eq(t, lower('Ö'), "ö")
	eq(t, lower('ß'), "ß")
	eq(t, lower('Ü'), "ü")
	eq(t, lower('💩'), "💩")
	eq(t, lower('Σ'), "σ")
	eq(t, lower('Τ'), "τ")
	eq(t, lower('Ι'), "ι")
	eq(t, lower('Γ'), "γ")
	eq(t, lower('Μ'), "μ")
	eq(t, lower('Α'), "α")
	eq(t, lower('Σ'), "σ")
	eq(t, lower('ǅ'), "ǆ")
	eq(t, lower('ﬁ'), "ﬁ")
	eq(t, lower('İ'), "i\u0307")
}

func TestChar_test_to_uppercase(t *testing.T) {
	upper := func(c rune) string { return string(CharToUppercase(c)) }
	eq(t, upper('a'), "A")
	eq(t, upper('ö'), "Ö")
	eq(t, upper('ß'), "SS") // not ẞ: Latin capital letter sharp s
	eq(t, upper('ü'), "Ü")
	eq(t, upper('💩'), "💩")

	eq(t, upper('σ'), "Σ")
	eq(t, upper('τ'), "Τ")
	eq(t, upper('ι'), "Ι")
	eq(t, upper('γ'), "Γ")
	eq(t, upper('μ'), "Μ")
	eq(t, upper('α'), "Α")
	eq(t, upper('ς'), "Σ")
	eq(t, upper('ǅ'), "Ǆ")
	eq(t, upper('ﬁ'), "FI")
	eq(t, upper('ᾀ'), "ἈΙ")
}

func TestChar_test_is_control(t *testing.T) {
	isTrue(t, IsControl(0x0))
	isTrue(t, IsControl(0x3))
	isTrue(t, IsControl(0x6))
	isTrue(t, IsControl(0x9))
	isTrue(t, IsControl(0x7f))
	isTrue(t, IsControl(0x92))
	isTrue(t, !IsControl(0x20))
	isTrue(t, !IsControl(0x55))
	isTrue(t, !IsControl(0x68))
}

func TestChar_test_is_numeric(t *testing.T) {
	isTrue(t, IsNumeric('2'))
	isTrue(t, IsNumeric('7'))
	isTrue(t, IsNumeric('¾'))
	isTrue(t, !IsNumeric('c'))
	isTrue(t, !IsNumeric('i'))
	isTrue(t, !IsNumeric('z'))
	isTrue(t, !IsNumeric('Q'))
}

func TestChar_test_escape_debug(t *testing.T) {
	eq(t, CharEscapeDebug('\n'), `\n`)
	eq(t, CharEscapeDebug('\r'), `\r`)
	eq(t, CharEscapeDebug('\''), `\'`)
	eq(t, CharEscapeDebug('"'), `\"`)
	eq(t, CharEscapeDebug(' '), " ")
	eq(t, CharEscapeDebug('a'), "a")
	eq(t, CharEscapeDebug('~'), "~")
	eq(t, CharEscapeDebug('é'), "é")
	eq(t, CharEscapeDebug('文'), "文")
	eq(t, CharEscapeDebug(0x00), `\0`)
	eq(t, CharEscapeDebug(0x1f), `\u{1f}`)
	eq(t, CharEscapeDebug(0x7f), `\u{7f}`)
	eq(t, CharEscapeDebug(0x80), `\u{80}`)
	eq(t, CharEscapeDebug(0xff), "ÿ")
	eq(t, CharEscapeDebug(0x11b), "ě")
	eq(t, CharEscapeDebug(0x1d4b6), "\U0001d4b6")
	eq(t, CharEscapeDebug(0x301), `\u{301}`)       // combining character
	eq(t, CharEscapeDebug(0x200b), `\u{200b}`)     // zero width space
	eq(t, CharEscapeDebug(0xe000), `\u{e000}`)     // private use 1
	eq(t, CharEscapeDebug(0x100000), `\u{100000}`) // private use 2
	eq(t, CharEscapeDebug(0xff9e), "ﾞ")            // halfwidth dakuten
	eq(t, CharEscapeDebug(0xff9f), "ﾟ")            // halfwidth handakuten
}

func TestChar_test_escape_default(t *testing.T) {
	eq(t, CharEscapeDefault('\n'), `\n`)
	eq(t, CharEscapeDefault('\r'), `\r`)
	eq(t, CharEscapeDefault('\t'), `\t`)
	eq(t, CharEscapeDefault('\''), `\'`)
	eq(t, CharEscapeDefault('"'), `\"`)
	eq(t, CharEscapeDefault(' '), " ")
	eq(t, CharEscapeDefault('a'), "a")
	eq(t, CharEscapeDefault('~'), "~")
	eq(t, CharEscapeDefault('é'), `\u{e9}`)
	eq(t, CharEscapeDefault(0x00), `\u{0}`)
	eq(t, CharEscapeDefault(0x1f), `\u{1f}`)
	eq(t, CharEscapeDefault(0x7f), `\u{7f}`)
	eq(t, CharEscapeDefault(0x80), `\u{80}`)
	eq(t, CharEscapeDefault(0xff), `\u{ff}`)
	eq(t, CharEscapeDefault(0x11b), `\u{11b}`)
	eq(t, CharEscapeDefault(0x1d4b6), `\u{1d4b6}`)
	eq(t, CharEscapeDefault(0x200b), `\u{200b}`)     // zero width space
	eq(t, CharEscapeDefault(0xe000), `\u{e000}`)     // private use 1
	eq(t, CharEscapeDefault(0x100000), `\u{100000}`) // private use 2
}

func TestChar_test_escape_unicode(t *testing.T) {
	eq(t, CharEscapeUnicode(0x00), `\u{0}`)
	eq(t, CharEscapeUnicode('\n'), `\u{a}`)
	eq(t, CharEscapeUnicode(' '), `\u{20}`)
	eq(t, CharEscapeUnicode('a'), `\u{61}`)
	eq(t, CharEscapeUnicode(0x11b), `\u{11b}`)
	eq(t, CharEscapeUnicode(0x1d4b6), `\u{1d4b6}`)
}

func TestChar_test_encode_utf8(t *testing.T) {
	check := func(input rune, expect []byte) {
		t.Helper()
		s := CharEncodeUtf8(input)
		_, err := FromUtf8([]byte(s))
		isTrue(t, err == nil)
		eqSlice(t, []byte(s), expect)
		eq(t, CharLenUtf8(input), len(expect))
	}

	check('x', []byte{0x78})
	check(0xe9, []byte{0xc3, 0xa9})
	check(0xa66e, []byte{0xea, 0x99, 0xae})
	check(0x1f4a9, []byte{0xf0, 0x9f, 0x92, 0xa9})
}

func TestChar_test_encode_utf16(t *testing.T) {
	eqSlice(t, CharEncodeUtf16('x'), []uint16{0x0078})
	eqSlice(t, CharEncodeUtf16(0xe9), []uint16{0x00e9})
	eqSlice(t, CharEncodeUtf16(0xa66e), []uint16{0xa66e})
	eqSlice(t, CharEncodeUtf16(0x1f4a9), []uint16{0xd83d, 0xdca9})
}

func TestChar_test_len_utf16(t *testing.T) {
	isTrue(t, CharLenUtf16('x') == 1)
	isTrue(t, CharLenUtf16(0xe9) == 1)
	isTrue(t, CharLenUtf16(0xa66e) == 1)
	isTrue(t, CharLenUtf16(0x1f4a9) == 2)
}

func TestChar_test_decode_utf16(t *testing.T) {
	// An unpaired surrogate is written here as a negative number.
	check := func(s []uint16, expected []int32) {
		t.Helper()
		var got []int32
		decodeUtf16(s, func(c rune, ok bool) bool {
			if ok {
				got = append(got, c)
			} else {
				got = append(got, -c)
			}
			return true
		})
		eqSlice(t, got, expected)
	}
	check([]uint16{0xD800, 0x41, 0x42}, []int32{-0xD800, 'A', 'B'})
	check([]uint16{0xD800, 0}, []int32{-0xD800, 0})
	check([]uint16{0xD800}, []int32{-0xD800})
	check([]uint16{0xD840, 0xDC00}, []int32{0x20000})
	check([]uint16{0xD840, 0xD840, 0xDC00}, []int32{-0xD840, 0x20000})
	check([]uint16{0xDC00, 0xD840}, []int32{-0xDC00, -0xD840})
}

func TestChar_ed_iterator_specializations(t *testing.T) {
	// The escapes are returned whole; their chars are checked as chars.
	chars := func(c rune) []rune { return Chars(CharEscapeDefault(c)) }

	// Check counting
	eq(t, len(chars('\n')), 2)
	eq(t, len(chars('c')), 1)
	eq(t, len(chars(' ')), 1)
	eq(t, len(chars('\\')), 2)
	eq(t, len(chars('\'')), 2)

	// Check first, second and last
	eqSlice(t, chars('\n'), []rune{'\\', 'n'})
	eqSlice(t, chars('c'), []rune{'c'})
	eqSlice(t, chars(' '), []rune{' '})
	eqSlice(t, chars('\\'), []rune{'\\', '\\'})
	eqSlice(t, chars('\''), []rune{'\\', '\''})
}

func TestChar_eu_iterator_specializations(t *testing.T) {
	// The escape is returned whole; its chars are checked as chars, the
	// iterator's methods as the slice's.
	check := func(c rune) {
		t.Helper()
		chars := Chars(CharEscapeUnicode(c))
		length := len(chars)

		// For all possible in-bound offsets
		iter := chars
		for offset := 0; offset < length; offset++ {
			// Check last
			eq(t, iter[len(iter)-1], '}')

			// Check len
			eq(t, len(iter), length-offset)

			// Check nth
			eq(t, chars[offset], iter[0])
			iter = iter[1:]
		}

		// Check post-last
		eq(t, len(iter), 0)
	}

	check(0x0)
	check(0x1)
	check(0x12)
	check(0x123)
	check(0x1234)
	check(0x12340)
	check(0x10FFFF)
}

func TestChar_test_from_digit_radix_too_high(t *testing.T) {
	shouldPanic(t, "from_digit: radix is too high (maximum 36)", func() { CharFromDigit(0, 37) })
}

func TestChar_test_from_digit_invalid_radix(t *testing.T) {
	isTrue(t, CharFromDigit(10, 9).IsNone())
}

func TestChar_test_to_digit_radix_too_low(t *testing.T) {
	shouldPanic(t, "to_digit: invalid radix", func() { CharToDigit('a', 1) })
}

func TestChar_test_to_digit_radix_too_high(t *testing.T) {
	shouldPanic(t, "to_digit: invalid radix", func() { CharToDigit('a', 37) })
}
