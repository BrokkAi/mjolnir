// Port of alloctests 1.99.1 tests/string.rs (the tests of what this package
// covers; see doc.go for what is left out)
//
// Not ported: what Go spells with its own operators and conversions
// (test_from_str, test_from_cow_str, test_unsized_to_string, test_push_bytes,
// test_push_str, test_add_assign, test_push, test_str_clear, test_str_add,
// test_extend_ref, test_into_boxed_str, test_from_char, test_str_concat);
// formatting, which go/rs does (test_simple_types, test_vectors); capacity,
// which a Go string does not have (test_reserve_exact, test_try_with_capacity,
// test_try_reserve, test_try_reserve_exact); ranges with an excluded start
// bound, which have no Go form here (test_drain_start_overflow,
// test_replace_range_start_overflow); RangeBounds impls that answer
// differently each time they are asked, where the Go functions take two ints
// (test_replace_range_evil_start_bound, test_replace_range_evil_end_bound);
// and the unstable remove_matches, replace_first and replace_last.

package strs

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestString_test_from_utf8(t *testing.T) {
	xs := []byte("hello")
	eq(t, rs.Unwrap(StringFromUtf8(xs)), "hello")

	xs = []byte("ศไทย中华Việt Nam")
	eq(t, rs.Unwrap(StringFromUtf8(xs)), "ศไทย中华Việt Nam")

	xs = []byte("hello\xFF")
	_, err := StringFromUtf8(xs)
	fromUtf8Error := err.(*FromUtf8Error)
	eq(t, string(fromUtf8Error.AsBytes()), "hello\xff")
	eq(t, string(fromUtf8Error.IntoBytes()), "hello\xff")
	eq(t, fromUtf8Error.Utf8Error().ValidUpTo(), 5)
}

// lossyCases are shared by test_from_utf8_lossy and
// test_fromutf8error_into_lossy.
var lossyCases = [][2]string{
	{"hello", "hello"},
	{"ศไทย中华Việt Nam", "ศไทย中华Việt Nam"},
	{"Hello\xC2 There\xFF Goodbye", "Hello� There� Goodbye"},
	{"Hello\xC0\x80 There\xE6\x83 Goodbye", "Hello�� There� Goodbye"},
	{"\xF5foo\xF5\x80bar", "�foo��bar"},
	{"\xF1foo\xF1\x80bar\xF1\x80\x80baz", "�foo�bar�baz"},
	{"\xF4foo\xF4\x80bar\xF4\xBFbaz", "�foo�bar��baz"},
	{"\xF0\x80\x80\x80foo\xF0\x90\x80\x80bar", "����foo\U00010000bar"},
	// surrogates
	{"\xED\xA0\x80foo\xED\xBF\xBFbar", "���foo���bar"},
}

func TestString_test_from_utf8_lossy(t *testing.T) {
	for _, c := range lossyCases {
		eq(t, FromUtf8Lossy([]byte(c[0])), c[1])
	}
}

func TestString_test_fromutf8error_into_lossy(t *testing.T) {
	convert := func(input []byte) string {
		s, err := StringFromUtf8(input)
		if err != nil {
			return err.(*FromUtf8Error).IntoUtf8Lossy()
		}
		return s
	}
	for _, c := range lossyCases {
		eq(t, convert([]byte(c[0])), c[1])
	}
}

func TestString_test_from_utf16(t *testing.T) {
	pairs := []struct {
		s string
		u []uint16
	}{
		{"𐍅𐌿𐌻𐍆𐌹𐌻𐌰\n", []uint16{
			0xd800, 0xdf45, 0xd800, 0xdf3f, 0xd800, 0xdf3b, 0xd800, 0xdf46, 0xd800, 0xdf39,
			0xd800, 0xdf3b, 0xd800, 0xdf30, 0x000a,
		}},
		{"𐐒𐑉𐐮𐑀𐐲𐑋 𐐏𐐲𐑍\n", []uint16{
			0xd801, 0xdc12, 0xd801, 0xdc49, 0xd801, 0xdc2e, 0xd801, 0xdc40, 0xd801, 0xdc32,
			0xd801, 0xdc4b, 0x0020, 0xd801, 0xdc0f, 0xd801, 0xdc32, 0xd801, 0xdc4d, 0x000a,
		}},
		{"𐌀𐌖𐌋𐌄𐌑𐌉·𐌌𐌄𐌕𐌄𐌋𐌉𐌑\n", []uint16{
			0xd800, 0xdf00, 0xd800, 0xdf16, 0xd800, 0xdf0b, 0xd800, 0xdf04, 0xd800, 0xdf11,
			0xd800, 0xdf09, 0x00b7, 0xd800, 0xdf0c, 0xd800, 0xdf04, 0xd800, 0xdf15, 0xd800,
			0xdf04, 0xd800, 0xdf0b, 0xd800, 0xdf09, 0xd800, 0xdf11, 0x000a,
		}},
		{"𐒋𐒘𐒈𐒑𐒛𐒒 𐒕𐒓 𐒈𐒚𐒍 𐒏𐒜𐒒𐒖𐒆 𐒕𐒆\n", []uint16{
			0xd801, 0xdc8b, 0xd801, 0xdc98, 0xd801, 0xdc88, 0xd801, 0xdc91, 0xd801, 0xdc9b,
			0xd801, 0xdc92, 0x0020, 0xd801, 0xdc95, 0xd801, 0xdc93, 0x0020, 0xd801, 0xdc88,
			0xd801, 0xdc9a, 0xd801, 0xdc8d, 0x0020, 0xd801, 0xdc8f, 0xd801, 0xdc9c, 0xd801,
			0xdc92, 0xd801, 0xdc96, 0xd801, 0xdc86, 0x0020, 0xd801, 0xdc95, 0xd801, 0xdc86,
			0x000a,
		}},
		// Issue #12318, even-numbered non-BMP planes
		{"\U00020000", []uint16{0xD840, 0xDC00}},
	}

	for _, p := range pairs {
		s, u := p.s, p.u
		sAsUtf16 := EncodeUtf16(s)
		uAsString := rs.Unwrap(FromUtf16(u))

		decodeUtf16(u, func(_ rune, ok bool) bool {
			isTrue(t, ok)
			return true
		})
		eqSlice(t, sAsUtf16, u)

		eq(t, uAsString, s)
		eq(t, FromUtf16Lossy(u), s)

		eq(t, rs.Unwrap(FromUtf16(sAsUtf16)), s)
		eqSlice(t, EncodeUtf16(uAsString), u)
	}
}

func TestString_test_utf16_invalid(t *testing.T) {
	for _, u := range [][]uint16{
		// completely positive cases tested above.
		// lead + eof
		{0xD800},
		// lead + lead
		{0xD800, 0xD800},
		// isolated trail
		{0x0061, 0xDC00},
		// general
		{0xD800, 0xd801, 0xdc8b, 0xD800},
	} {
		_, err := FromUtf16(u)
		isTrue(t, err != nil)
		if err != nil {
			eq(t, err.Error(), "invalid utf-16: lone surrogate found")
		}
	}
}

func TestString_test_from_utf16_lossy(t *testing.T) {
	// completely positive cases tested above.
	// lead + eof
	eq(t, FromUtf16Lossy([]uint16{0xD800}), "�")
	// lead + lead
	eq(t, FromUtf16Lossy([]uint16{0xD800, 0xD800}), "��")

	// isolated trail
	eq(t, FromUtf16Lossy([]uint16{0x0061, 0xDC00}), "a�")

	// general
	eq(t, FromUtf16Lossy([]uint16{0xD800, 0xd801, 0xdc8b, 0xD800}), "�𐒋�")
}

func TestString_test_pop(t *testing.T) {
	data := "ประเทศไทย中华b¢€𤭢"
	eq(t, Pop(&data).Unwrap(), '𤭢') // 4 bytes
	eq(t, Pop(&data).Unwrap(), '€') // 3 bytes
	eq(t, Pop(&data).Unwrap(), '¢') // 2 bytes
	eq(t, Pop(&data).Unwrap(), 'b') // 1 bytes
	eq(t, Pop(&data).Unwrap(), '华')
	eq(t, data, "ประเทศไทย中")
}

func TestString_test_split_off_empty(t *testing.T) {
	orig := "Hello, world!"
	split := orig
	empty := SplitOff(&split, len(orig))
	isTrue(t, empty == "")
}

func TestString_test_split_off_past_end(t *testing.T) {
	orig := "Hello, world!"
	split := orig
	shouldPanic(t, "", func() { SplitOff(&split, len(orig)+1) })
}

func TestString_test_split_off_mid_char(t *testing.T) {
	shan := "山"
	shouldPanic(t, "", func() { SplitOff(&shan, 1) })
}

func TestString_test_split_off_ascii(t *testing.T) {
	ab := "ABCD"
	cd := SplitOff(&ab, 2)
	eq(t, ab, "AB")
	eq(t, cd, "CD")
}

func TestString_test_split_off_unicode(t *testing.T) {
	nihon := "日本語"
	goPart := SplitOff(&nihon, len("日本"))
	eq(t, nihon, "日本")
	eq(t, goPart, "語")
}

func TestString_test_str_truncate(t *testing.T) {
	s := "12345"
	Truncate(&s, 5)
	eq(t, s, "12345")
	Truncate(&s, 3)
	eq(t, s, "123")
	Truncate(&s, 0)
	eq(t, s, "")
}

func TestString_test_str_truncate_invalid_len(t *testing.T) {
	s := "12345"
	Truncate(&s, 6)
	eq(t, s, "12345")
}

func TestString_test_str_truncate_split_codepoint(t *testing.T) {
	s := "ü" // ü
	shouldPanic(t, "", func() { Truncate(&s, 1) })
}

func TestString_remove(t *testing.T) {
	s := "ศไทย中华Việt Nam; foobar"
	eq(t, Remove(&s, 0), 'ศ')
	eq(t, len(s), 33)
	eq(t, s, "ไทย中华Việt Nam; foobar")
	eq(t, Remove(&s, 17), 'ệ')
	eq(t, s, "ไทย中华Vit Nam; foobar")
}

func TestString_remove_bad(t *testing.T) {
	s := "ศ"
	shouldPanic(t, "", func() { Remove(&s, 1) })
}

func TestString_test_retain(t *testing.T) {
	s := "α_β_γ"

	Retain(&s, func(rune) bool { return true })
	eq(t, s, "α_β_γ")

	Retain(&s, func(c rune) bool { return c != '_' })
	eq(t, s, "αβγ")

	Retain(&s, func(c rune) bool { return c != 'β' })
	eq(t, s, "αγ")

	Retain(&s, func(c rune) bool { return c == 'α' })
	eq(t, s, "α")

	Retain(&s, func(rune) bool { return false })
	eq(t, s, "")

	// A predicate that panics leaves a string that is still valid UTF-8.
	s = "0è0"
	func() {
		defer func() { _ = recover() }()
		count := 0
		Retain(&s, func(rune) bool {
			count++
			switch count {
			case 1:
				return false
			case 2:
				return true
			default:
				panic("stop")
			}
		})
	}()
	_, err := FromUtf8([]byte(s))
	isTrue(t, err == nil)
}

func TestString_insert(t *testing.T) {
	s := "foobar"
	Insert(&s, 0, 'ệ')
	eq(t, s, "ệfoobar")
	Insert(&s, 6, 'ย')
	eq(t, s, "ệfooยbar")
}

func TestString_insert_bad1(t *testing.T) {
	s := ""
	shouldPanic(t, "", func() { Insert(&s, 1, 't') })
}

func TestString_insert_bad2(t *testing.T) {
	s := "ệ"
	shouldPanic(t, "", func() { Insert(&s, 1, 't') })
}

func TestString_test_slicing(t *testing.T) {
	s := "foobar"
	eq(t, "foobar", s) // &s[..]
	eq(t, "foo", SliceTo(s, 3))
	eq(t, "bar", SliceFrom(s, 3))
	eq(t, "oob", Slice(s, 1, 4))
}

func TestString_test_from_iterator(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	t_ := "ศไทย中华"
	u := "Việt Nam"

	a := string(Chars(s))
	eq(t, s, a)

	b := t_
	b += string(Chars(u)) // b.extend(u.chars())
	eq(t, s, b)

	c := strings.Join([]string{t_, u}, "") // [t, u].into_iter().collect()
	eq(t, s, c)

	d := t_
	d += strings.Join([]string{u}, "") // d.extend(vec![u])
	eq(t, s, d)
}

func TestString_test_drain(t *testing.T) {
	s := "αβγ"
	eq(t, Drain(&s, 2, 4), "β")
	eq(t, s, "αγ")

	u := "abcd"
	Drain(&u, 0, 0)
	eq(t, u, "abcd")
	Drain(&u, 0, 1)
	eq(t, u, "bcd")
	Drain(&u, 3, len(u))
	eq(t, u, "bcd")
	Drain(&u, 0, len(u))
	eq(t, u, "")
}

func TestString_test_drain_end_overflow(t *testing.T) {
	s := "abc"
	shouldPanic(t, "", func() { DrainInclusive(&s, 0, rs.UsizeMax) })
}

func TestString_test_replace_range(t *testing.T) {
	s := "Hello, world!"
	ReplaceRange(&s, 7, 12, "世界")
	eq(t, s, "Hello, 世界!")
}

func TestString_test_replace_range_start_char_boundary(t *testing.T) {
	s := "Hello, 世界!"
	shouldPanic(t, "start of range should be a character boundary", func() { ReplaceRange(&s, 8, len(s), "") })
}

func TestString_test_replace_range_end_char_boundary(t *testing.T) {
	s := "Hello, 世界!"
	shouldPanic(t, "end of range should be a character boundary", func() { ReplaceRange(&s, 0, 8, "") })
}

func TestString_test_replace_range_inclusive_range(t *testing.T) {
	v := "12345"
	ReplaceRangeInclusive(&v, 2, 3, "789")
	eq(t, v, "127895")
	ReplaceRangeInclusive(&v, 1, 2, "A")
	eq(t, v, "1A895")
}

func TestString_test_replace_range_out_of_bounds(t *testing.T) {
	s := "12345"
	shouldPanic(t, "range end index 6 out of range for slice of length 5", func() { ReplaceRange(&s, 5, 6, "789") })
}

func TestString_test_replace_range_inclusive_out_of_bounds(t *testing.T) {
	s := "12345"
	shouldPanic(t, "range end index 5 out of range for slice of length 5", func() { ReplaceRangeInclusive(&s, 5, 5, "789") })
}

// The overflowed index value is target-dependent,
// so we don't check for its exact value in the panic message
func TestString_test_replace_range_end_overflow(t *testing.T) {
	s := "456"
	shouldPanic(t, "out of range for slice of length 3", func() { ReplaceRangeInclusive(&s, 0, rs.UsizeMax, "") })
}

func TestString_test_replace_range_empty(t *testing.T) {
	s := "12345"
	ReplaceRange(&s, 1, 2, "")
	eq(t, s, "1345")
}

func TestString_test_replace_range_unbounded(t *testing.T) {
	s := "12345"
	ReplaceRange(&s, 0, len(s), "")
	eq(t, s, "")
}
