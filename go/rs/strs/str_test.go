// Port of alloctests 1.99.1 tests/str.rs (the tests of what this package
// covers; see doc.go for what is left out)

package strs

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func eq[T comparable](t *testing.T, got, want T) {
	t.Helper()
	if got != want {
		t.Errorf("got %#v, want %#v", got, want)
	}
}

func eqSlice[T comparable](t *testing.T, got, want []T) {
	t.Helper()
	if !slices.Equal(got, want) {
		t.Errorf("got %#v, want %#v", got, want)
	}
}

func isTrue(t *testing.T, condition bool) {
	t.Helper()
	if !condition {
		t.Error("assertion failed")
	}
}

// shouldPanic is #[should_panic(expected = ..)]: f must panic with a message
// that contains expected.
func shouldPanic(t *testing.T, expected string, f func()) {
	t.Helper()
	defer func() {
		t.Helper()
		recovered := recover()
		if recovered == nil {
			t.Errorf("did not panic; expected a panic containing %q", expected)
			return
		}
		if message := rs.PanicInfoOf(recovered).Message; !strings.Contains(message, expected) {
			t.Errorf("panicked with %q, expected a message containing %q", message, expected)
		}
	}()
	f()
}

func pair(a, b string) rs.Option[rs.Tuple2[string, string]] { return rs.Some(rs.T2(a, b)) }

const thaiChineseViet = "ประเทศไทย中华Việt Nam"

// charIs is the closure `|c: char| c == want`.
func charIs(want rune) func(rune) bool { return func(c rune) bool { return c == want } }

func TestStr_test_find(t *testing.T) {
	eq(t, FindChar("hello", 'l'), rs.Some(2))
	eq(t, FindFn("hello", charIs('o')), rs.Some(4))
	isTrue(t, FindChar("hello", 'x').IsNone())
	isTrue(t, FindFn("hello", charIs('x')).IsNone())
	eq(t, FindChar(thaiChineseViet, '华'), rs.Some(30))
	eq(t, FindFn(thaiChineseViet, charIs('华')), rs.Some(30))
}

func TestStr_test_rfind(t *testing.T) {
	eq(t, RfindChar("hello", 'l'), rs.Some(3))
	eq(t, RfindFn("hello", charIs('o')), rs.Some(4))
	isTrue(t, RfindChar("hello", 'x').IsNone())
	isTrue(t, RfindFn("hello", charIs('x')).IsNone())
	eq(t, RfindChar(thaiChineseViet, '华'), rs.Some(30))
	eq(t, RfindFn(thaiChineseViet, charIs('华')), rs.Some(30))
}

func TestStr_test_find_str(t *testing.T) {
	// byte positions
	eq(t, Find("", ""), rs.Some(0))
	isTrue(t, Find("banana", "apple pie").IsNone())

	data := "abcabc"
	eq(t, Find(data[0:6], "ab"), rs.Some(0))
	eq(t, Find(data[2:6], "ab"), rs.Some(3-2))
	isTrue(t, Find(data[2:4], "ab").IsNone())

	data = thaiChineseViet + thaiChineseViet
	isTrue(t, Find(data, "ไท华").IsNone())
	eq(t, Find(data[0:43], ""), rs.Some(0))
	eq(t, Find(data[6:43], ""), rs.Some(6-6))

	eq(t, Find(data[0:43], "ประ"), rs.Some(0))
	eq(t, Find(data[0:43], "ทศไ"), rs.Some(12))
	eq(t, Find(data[0:43], "ย中"), rs.Some(24))
	eq(t, Find(data[0:43], "iệt"), rs.Some(34))
	eq(t, Find(data[0:43], "Nam"), rs.Some(40))

	eq(t, Find(data[43:86], "ประ"), rs.Some(43-43))
	eq(t, Find(data[43:86], "ทศไ"), rs.Some(55-43))
	eq(t, Find(data[43:86], "ย中"), rs.Some(67-43))
	eq(t, Find(data[43:86], "iệt"), rs.Some(77-43))
	eq(t, Find(data[43:86], "Nam"), rs.Some(83-43))

	// find every substring -- assert that it finds it, or an earlier occurrence.
	s := "Việt Namacbaabcaabaaba"
	for _, start := range CharIndices(s) {
		i := start.F0
		ip := i + CharLenUtf8(start.F1)
		ends := []int{}
		for _, end := range CharIndices(s[ip:]) {
			ends = append(ends, end.F0)
		}
		ends = append(ends, len(s)-ip)
		for _, j := range ends {
			pat := s[i : ip+j]
			if x, ok := Find(s, pat).Get(); !ok || x > i {
				t.Errorf("Find(%q, %q) = %v, %v", s, pat, x, ok)
			}
			if x, ok := Rfind(s, pat).Get(); !ok || x < i {
				t.Errorf("Rfind(%q, %q) = %v, %v", s, pat, x, ok)
			}
		}
	}
}

func TestStr_test_starts_with(t *testing.T) {
	isTrue(t, StartsWith("", ""))
	isTrue(t, StartsWith("abc", ""))
	isTrue(t, StartsWith("abc", "a"))
	isTrue(t, !StartsWith("a", "abc"))
	isTrue(t, !StartsWith("", "abc"))
	isTrue(t, !StartsWith("ödd", "-"))
	isTrue(t, StartsWith("ödd", "öd"))
}

func TestStr_test_ends_with(t *testing.T) {
	isTrue(t, EndsWith("", ""))
	isTrue(t, EndsWith("abc", ""))
	isTrue(t, EndsWith("abc", "c"))
	isTrue(t, !EndsWith("a", "abc"))
	isTrue(t, !EndsWith("", "abc"))
	isTrue(t, !EndsWith("ddö", "-"))
	isTrue(t, EndsWith("ddö", "dö"))
}

func TestStr_test_replacen(t *testing.T) {
	eq(t, ReplacenChar("", 'a', "b", 5), "")
	eq(t, Replacen("acaaa", "a", "b", 3), "bcbba")
	eq(t, Replacen("aaaa", "a", "b", 0), "aaaa")

	test := "test"
	eq(t, Replacen(" test test ", test, "toast", 3), " toast toast ")
	eq(t, Replacen(" test test ", test, "toast", 0), " test test ")
	eq(t, Replacen(" test test ", test, "", 5), "   ")

	eq(t, ReplacenFn("qwer123zxc789", IsNumeric, "", 3), "qwerzxc789")
}

func TestStr_test_replace(t *testing.T) {
	a := "a"
	eq(t, Replace("", a, "b"), "")
	eq(t, Replace("a", a, "b"), "b")
	eq(t, Replace("ab", a, "b"), "bb")
	test := "test"
	eq(t, Replace(" test test ", test, "toast"), " toast toast ")
	eq(t, Replace(" test test ", test, ""), "   ")
}

func TestStr_test_replace_2a(t *testing.T) {
	data := "ประเทศไทย中华"
	repl := "دولة الكويت"

	a := "ประเ"
	a2 := "دولة الكويتทศไทย中华"
	eq(t, Replace(data, a, repl), a2)
}

func TestStr_test_replace_2b(t *testing.T) {
	data := "ประเทศไทย中华"
	repl := "دولة الكويت"

	b := "ะเ"
	b2 := "ปรدولة الكويتทศไทย中华"
	eq(t, Replace(data, b, repl), b2)
}

func TestStr_test_replace_2c(t *testing.T) {
	data := "ประเทศไทย中华"
	repl := "دولة الكويت"

	c := "中华"
	c2 := "ประเทศไทยدولة الكويت"
	eq(t, Replace(data, c, repl), c2)
}

func TestStr_test_replace_2d(t *testing.T) {
	data := "ประเทศไทย中华"
	repl := "دولة الكويت"

	d := "ไท华"
	eq(t, Replace(data, d, repl), data)
}

func TestStr_test_replace_pattern(t *testing.T) {
	data := "abcdαβγδabcdαβγδ"
	eq(t, Replace(data, "dαβ", "😺😺😺"), "abc😺😺😺γδabc😺😺😺γδ")
	eq(t, ReplaceChar(data, 'γ', "😺😺😺"), "abcdαβ😺😺😺δabcdαβ😺😺😺δ")
	eq(t, ReplaceChars(data, []rune{'a', 'γ'}, "😺😺😺"), "😺😺😺bcdαβ😺😺😺δ😺😺😺bcdαβ😺😺😺δ")
	eq(t, ReplaceFn(data, charIs('γ'), "😺😺😺"), "abcdαβ😺😺😺δabcdαβ😺😺😺δ")
}

// The tests of `mod slice_index`: each range form, where it is good and where
// it panics, with the panic's message.

func TestStr_slice_index_simple_ascii(t *testing.T) {
	eq(t, Slice("abc", 0, 2), "ab")
	eq(t, SliceInclusive("abc", 0, 1), "ab")
	eq(t, SliceTo("abc", 2), "ab")
	eq(t, SliceToInclusive("abc", 1), "ab")
	eq(t, Slice("abc", 1, 3), "bc")
	eq(t, SliceInclusive("abc", 1, 2), "bc")
	eq(t, Slice("abc", 1, 1), "")
	eq(t, SliceInclusive("abc", 1, 0), "")
}

func TestStr_slice_index_simple_unicode(t *testing.T) {
	// 日本
	eq(t, Slice("\u65e5\u672c", 0, 3), "\u65e5")
	eq(t, SliceInclusive("\u65e5\u672c", 0, 2), "\u65e5")
	eq(t, SliceTo("\u65e5\u672c", 3), "\u65e5")
	eq(t, SliceToInclusive("\u65e5\u672c", 2), "\u65e5")
	eq(t, Slice("\u65e5\u672c", 3, 6), "\u672c")
	eq(t, SliceInclusive("\u65e5\u672c", 3, 5), "\u672c")
	eq(t, SliceFrom("\u65e5\u672c", 3), "\u672c")

	data := "ประเทศไทย中华"
	eq(t, Slice(data, 0, 3), "ป")
	eq(t, Slice(data, 3, 6), "ร")
	eq(t, Slice(data, 3, 3), "")
	eq(t, Slice(data, 30, 33), "华")

	ss := "中华Việt Nam"
	eq(t, Slice(ss, 3, 6), "华")
	eq(t, Slice(ss, 6, 16), "Việt Nam")
	eq(t, SliceInclusive(ss, 6, 15), "Việt Nam")
	eq(t, SliceFrom(ss, 6), "Việt Nam")

	eq(t, Slice(ss, 0, 3), "中")
	eq(t, Slice(ss, 3, 7), "华V")
	eq(t, SliceInclusive(ss, 3, 6), "华V")
	eq(t, Slice(ss, 3, 3), "")
	eq(t, SliceInclusive(ss, 3, 2), "")
}

func TestStr_slice_index_test_slice_fail(t *testing.T) {
	shouldPanic(t, "", func() { Slice("中华Việt Nam", 0, 2) })
}

func TestStr_slice_index_panic_cases(t *testing.T) {
	const data = "abcdef"
	type slicing struct {
		name    string
		good    func() string
		want    string
		bad     func() string
		get     func() rs.Option[string]
		message string
	}
	for _, c := range []slicing{
		{"rangefrom_len", func() string { return SliceFrom(data, 6) }, "",
			func() string { return SliceFrom(data, 7) }, func() rs.Option[string] { return GetFrom(data, 7) }, "out of bounds"},
		{"rangeto_len", func() string { return SliceTo(data, 6) }, "abcdef",
			func() string { return SliceTo(data, 7) }, func() rs.Option[string] { return GetTo(data, 7) }, "out of bounds"},
		{"rangetoinclusive_len", func() string { return SliceToInclusive(data, 5) }, "abcdef",
			func() string { return SliceToInclusive(data, 6) }, func() rs.Option[string] { return GetToInclusive(data, 6) }, "out of bounds"},
		{"rangeinclusive_len", func() string { return SliceInclusive(data, 0, 5) }, "abcdef",
			func() string { return SliceInclusive(data, 0, 6) }, func() rs.Option[string] { return GetInclusive(data, 0, 6) }, "out of bounds"},
		{"range_len_len", func() string { return Slice(data, 6, 6) }, "",
			func() string { return Slice(data, 7, 7) }, func() rs.Option[string] { return Get(data, 7, 7) }, "out of bounds"},
		{"rangeinclusive_len_len", func() string { return SliceInclusive(data, 6, 5) }, "",
			func() string { return SliceInclusive(data, 7, 6) }, func() rs.Option[string] { return GetInclusive(data, 7, 6) }, "out of bounds"},
		{"range_neg_width", func() string { return Slice(data, 4, 4) }, "",
			func() string { return Slice(data, 4, 3) }, func() rs.Option[string] { return Get(data, 4, 3) }, "byte range starts at 4 but ends at 3"},
		{"rangeinclusive_neg_width", func() string { return SliceInclusive(data, 4, 3) }, "",
			func() string { return SliceInclusive(data, 4, 2) }, func() rs.Option[string] { return GetInclusive(data, 4, 2) }, "byte range starts at 4 but ends at 3"},
	} {
		eq(t, c.good(), c.want)
		eq(t, c.get(), rs.None[string]())
		shouldPanic(t, c.message, func() { c.bad() })
	}
}

func TestStr_slice_index_overflow(t *testing.T) {
	// `usize::MAX` is the Go int -1.
	eq(t, GetInclusive("hello", 0, -1), rs.None[string]())
	shouldPanic(t, "out of bounds", func() { SliceInclusive("hello", 0, -1) })
	eq(t, GetToInclusive("hello", -1), rs.None[string]())
	shouldPanic(t, "out of bounds", func() { SliceToInclusive("hello", -1) })
}

func TestStr_slice_index_boundary(t *testing.T) {
	const (
		data = "abcαβγ"

		badStart      = 4
		goodStart     = 3
		badEnd        = 6
		goodEnd       = 7
		badEndIncl    = badEnd - 1
		goodEndIncl   = goodEnd - 1
		startMessage  = "start byte index 4 is not a char boundary; it is inside 'α' (bytes 3..5 of string)"
		endMessage    = "end byte index 6 is not a char boundary; it is inside 'β' (bytes 5..7 of string)"
		wholeGoodPart = "αβ"
	)
	// it is especially important to test all of the different range types here
	// because some of the logic may be duplicated as part of micro-optimizations
	// to dodge unicode boundary checks on half-ranges.
	eq(t, Slice(data, goodStart, goodEnd), wholeGoodPart)
	eq(t, Get(data, badStart, goodEnd), rs.None[string]())
	shouldPanic(t, startMessage, func() { Slice(data, badStart, goodEnd) })
	eq(t, Get(data, goodStart, badEnd), rs.None[string]())
	shouldPanic(t, endMessage, func() { Slice(data, goodStart, badEnd) })
	eq(t, GetFrom(data, badStart), rs.None[string]())
	shouldPanic(t, startMessage, func() { SliceFrom(data, badStart) })
	eq(t, GetTo(data, badEnd), rs.None[string]())
	shouldPanic(t, endMessage, func() { SliceTo(data, badEnd) })
	eq(t, GetInclusive(data, badStart, goodEndIncl), rs.None[string]())
	shouldPanic(t, startMessage, func() { SliceInclusive(data, badStart, goodEndIncl) })
	eq(t, GetInclusive(data, goodStart, badEndIncl), rs.None[string]())
	shouldPanic(t, endMessage, func() { SliceInclusive(data, goodStart, badEndIncl) })
	eq(t, GetToInclusive(data, badEndIncl), rs.None[string]())
	shouldPanic(t, endMessage, func() { SliceToInclusive(data, badEndIncl) })
}

func TestStr_slice_index_test_slice_fail_truncated_1(t *testing.T) {
	// LOREM_PARAGRAPH is 416 bytes of ASCII; only its length matters.
	lorem := strings.Repeat("Lorem ipsum dolor sit amet, ", 15)[:416]
	shouldPanic(t, "end byte index 1024 is out of bounds for string of length 416", func() { SliceTo(lorem, 1024) })
}

func TestStr_test_str_slice_rangetoinclusive_ok(t *testing.T) {
	s := "abcαβγ"
	eq(t, SliceToInclusive(s, 2), "abc")
	eq(t, SliceToInclusive(s, 4), "abcα")
}

func TestStr_test_str_slice_rangetoinclusive_notok(t *testing.T) {
	s := "abcαβγ"
	shouldPanic(t, "", func() { SliceToInclusive(s, 3) })
}

func TestStr_test_is_char_boundary(t *testing.T) {
	s := "ศไทย中华Việt Nam β-release 🐱123"
	isTrue(t, IsCharBoundary(s, 0))
	isTrue(t, IsCharBoundary(s, len(s)))
	isTrue(t, !IsCharBoundary(s, len(s)+1))
	for _, ic := range CharIndices(s) {
		i, ch := ic.F0, ic.F1
		// ensure character locations are boundaries and continuation bytes are not
		if !IsCharBoundary(s, i) {
			t.Errorf("%d is a char boundary in %q", i, s)
		}
		for j := 1; j < CharLenUtf8(ch); j++ {
			if IsCharBoundary(s, i+j) {
				t.Errorf("%d should not be a char boundary in %q", i+j, s)
			}
		}
	}
}

func TestStr_test_trim_start_matches(t *testing.T) {
	v := []rune{}
	eq(t, TrimStartMatchesChars(" *** foo *** ", v), " *** foo *** ")
	chars := []rune{'*', ' '}
	eq(t, TrimStartMatchesChars(" *** foo *** ", chars), "foo *** ")
	eq(t, TrimStartMatchesChars(" ***  *** ", chars), "")
	eq(t, TrimStartMatchesChars("foo *** ", chars), "foo *** ")

	eq(t, TrimStartMatchesChar("11foo1bar11", '1'), "foo1bar11")
	chars = []rune{'1', '2'}
	eq(t, TrimStartMatchesChars("12foo1bar12", chars), "foo1bar12")
	eq(t, TrimStartMatchesFn("123foo1bar123", IsNumeric), "foo1bar123")
}

func TestStr_test_trim_end_matches(t *testing.T) {
	v := []rune{}
	eq(t, TrimEndMatchesChars(" *** foo *** ", v), " *** foo *** ")
	chars := []rune{'*', ' '}
	eq(t, TrimEndMatchesChars(" *** foo *** ", chars), " *** foo")
	eq(t, TrimEndMatchesChars(" ***  *** ", chars), "")
	eq(t, TrimEndMatchesChars(" *** foo", chars), " *** foo")

	eq(t, TrimEndMatchesChar("11foo1bar11", '1'), "11foo1bar")
	chars = []rune{'1', '2'}
	eq(t, TrimEndMatchesChars("12foo1bar12", chars), "12foo1bar")
	eq(t, TrimEndMatchesFn("123foo1bar123", IsNumeric), "123foo1bar")
}

func TestStr_test_trim_matches(t *testing.T) {
	v := []rune{}
	eq(t, TrimMatchesChars(" *** foo *** ", v), " *** foo *** ")
	chars := []rune{'*', ' '}
	eq(t, TrimMatchesChars(" *** foo *** ", chars), "foo")
	eq(t, TrimMatchesChars(" ***  *** ", chars), "")
	eq(t, TrimMatchesChars("foo", chars), "foo")

	eq(t, TrimMatchesChar("11foo1bar11", '1'), "foo1bar")
	chars = []rune{'1', '2'}
	eq(t, TrimMatchesChars("12foo1bar12", chars), "foo1bar")
	eq(t, TrimMatchesFn("123foo1bar123", IsNumeric), "foo1bar")
}

func TestStr_test_trim_start(t *testing.T) {
	eq(t, TrimStart(""), "")
	eq(t, TrimStart("a"), "a")
	eq(t, TrimStart("    "), "")
	eq(t, TrimStart("     blah"), "blah")
	eq(t, TrimStart("   \u3000  wut"), "wut")
	eq(t, TrimStart("hey "), "hey ")
}

func TestStr_test_trim_end(t *testing.T) {
	eq(t, TrimEnd(""), "")
	eq(t, TrimEnd("a"), "a")
	eq(t, TrimEnd("    "), "")
	eq(t, TrimEnd("blah     "), "blah")
	eq(t, TrimEnd("wut   \u3000  "), "wut")
	eq(t, TrimEnd(" hey"), " hey")
}

func TestStr_test_trim(t *testing.T) {
	eq(t, Trim(""), "")
	eq(t, Trim("a"), "a")
	eq(t, Trim("    "), "")
	eq(t, Trim("    blah     "), "blah")
	eq(t, Trim("\nwut   \u3000  "), "wut")
	eq(t, Trim(" hey dude "), "hey dude")
}

func TestStr_test_is_whitespace(t *testing.T) {
	all := func(s string) bool {
		for _, c := range Chars(s) {
			if !IsWhitespace(c) {
				return false
			}
		}
		return true
	}
	isTrue(t, all(""))
	isTrue(t, all(" "))
	isTrue(t, all("\u2009")) // Thin space
	isTrue(t, all("  \n\t   "))
	isTrue(t, !all("   _   "))
}

func TestStr_test_is_utf8(t *testing.T) {
	bad := [][]byte{
		// deny overlong encodings
		{0xc0, 0x80}, {0xc0, 0xae}, {0xe0, 0x80, 0x80}, {0xe0, 0x80, 0xaf}, {0xe0, 0x81, 0x81},
		{0xf0, 0x82, 0x82, 0xac}, {0xf4, 0x90, 0x80, 0x80},
		// deny surrogates
		{0xED, 0xA0, 0x80}, {0xED, 0xBF, 0xBF},
	}
	for _, b := range bad {
		if _, err := FromUtf8(b); err == nil {
			t.Errorf("FromUtf8(% x) is ok", b)
		}
	}
	good := [][]byte{
		{0xC2, 0x80}, {0xDF, 0xBF}, {0xE0, 0xA0, 0x80}, {0xED, 0x9F, 0xBF}, {0xEE, 0x80, 0x80}, {0xEF, 0xBF, 0xBF},
		{0xF0, 0x90, 0x80, 0x80}, {0xF4, 0x8F, 0xBF, 0xBF},
	}
	for _, b := range good {
		if _, err := FromUtf8(b); err != nil {
			t.Errorf("FromUtf8(% x): %v", b, err)
		}
	}
}

func TestStr_from_utf8_mostly_ascii(t *testing.T) {
	// deny invalid bytes embedded in long stretches of ascii
	for i := 32; i < 64; i++ {
		data := make([]byte, 128)
		data[i] = 0xC0
		_, err := FromUtf8(data)
		isTrue(t, err != nil)
		data[i] = 0xC2
		_, err = FromUtf8(data)
		isTrue(t, err != nil)
	}
}

func TestStr_from_utf8_error(t *testing.T) {
	test := func(input string, expectedValidUpTo int, expectedErrorLen rs.Option[int]) {
		t.Helper()
		_, err := FromUtf8([]byte(input))
		utf8Error, ok := err.(Utf8Error)
		if !ok {
			t.Errorf("FromUtf8(%q) = %v", input, err)
			return
		}
		eq(t, utf8Error.ValidUpTo(), expectedValidUpTo)
		eq(t, utf8Error.ErrorLen(), expectedErrorLen)
	}
	none := rs.None[int]()
	test("A\xC3\xA9 \xFF ", 4, rs.Some(1))
	test("A\xC3\xA9 \x80 ", 4, rs.Some(1))
	test("A\xC3\xA9 \xC1 ", 4, rs.Some(1))
	test("A\xC3\xA9 \xC1", 4, rs.Some(1))
	test("A\xC3\xA9 \xC2", 4, none)
	test("A\xC3\xA9 \xC2 ", 4, rs.Some(1))
	test("A\xC3\xA9 \xC2\xC0", 4, rs.Some(1))
	test("A\xC3\xA9 \xE0", 4, none)
	test("A\xC3\xA9 \xE0\x9F", 4, rs.Some(1))
	test("A\xC3\xA9 \xE0\xA0", 4, none)
	test("A\xC3\xA9 \xE0\xA0\xC0", 4, rs.Some(2))
	test("A\xC3\xA9 \xE0\xA0 ", 4, rs.Some(2))
	test("A\xC3\xA9 \xED\xA0\x80 ", 4, rs.Some(1))
	test("A\xC3\xA9 \xF1", 4, none)
	test("A\xC3\xA9 \xF1\x80", 4, none)
	test("A\xC3\xA9 \xF1\x80\x80", 4, none)
	test("A\xC3\xA9 \xF1 ", 4, rs.Some(1))
	test("A\xC3\xA9 \xF1\x80 ", 4, rs.Some(2))
	test("A\xC3\xA9 \xF1\x80\x80 ", 4, rs.Some(3))
}

func TestStr_test_contains(t *testing.T) {
	isTrue(t, Contains("abcde", "bcd"))
	isTrue(t, Contains("abcde", "abcd"))
	isTrue(t, Contains("abcde", "bcde"))
	isTrue(t, Contains("abcde", ""))
	isTrue(t, Contains("", ""))
	isTrue(t, !Contains("abcde", "def"))
	isTrue(t, !Contains("", "a"))

	data := thaiChineseViet
	isTrue(t, Contains(data, "ประเ"))
	isTrue(t, Contains(data, "ะเ"))
	isTrue(t, Contains(data, "中华"))
	isTrue(t, !Contains(data, "ไท华"))
}

func TestStr_test_contains_char(t *testing.T) {
	isTrue(t, ContainsChar("abc", 'b'))
	isTrue(t, ContainsChar("a", 'a'))
	isTrue(t, !ContainsChar("abc", 'd'))
	isTrue(t, !ContainsChar("", 'a'))
}

func TestStr_test_split_at(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	for _, ic := range CharIndices(s) {
		a, b := SplitAt(s, ic.F0)
		eq(t, s[:len(a)], a)
		eq(t, s[len(a):], b)
	}
	a, b := SplitAt(s, len(s))
	eq(t, a, s)
	eq(t, b, "")
}

func TestStr_test_split_at_boundscheck(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	shouldPanic(t, "", func() { SplitAt(s, 1) })
}

func TestStr_test_escape_unicode(t *testing.T) {
	eq(t, EscapeUnicode("abc"), `\u{61}\u{62}\u{63}`)
	eq(t, EscapeUnicode("a c"), `\u{61}\u{20}\u{63}`)
	eq(t, EscapeUnicode("\r\n\t"), `\u{d}\u{a}\u{9}`)
	eq(t, EscapeUnicode("'\"\\"), `\u{27}\u{22}\u{5c}`)
	eq(t, EscapeUnicode("\x00\x01\u00fe\u00ff"), `\u{0}\u{1}\u{fe}\u{ff}`)
	eq(t, EscapeUnicode("\u0100\uffff"), `\u{100}\u{ffff}`)
	eq(t, EscapeUnicode("\U00010000\U0010ffff"), `\u{10000}\u{10ffff}`)
	eq(t, EscapeUnicode("ab\ufb00"), `\u{61}\u{62}\u{fb00}`)
	eq(t, EscapeUnicode("\U0001d4ea\r"), `\u{1d4ea}\u{d}`)
}

func TestStr_test_escape_debug(t *testing.T) {
	// Note that there are subtleties with the number of backslashes
	// on the left- and right-hand sides. In particular, Unicode code points
	// are usually escaped with two backslashes on the right-hand side, as
	// they are escaped. However, when the character is unescaped (e.g., for
	// printable characters), only a single backslash appears (as the character
	// itself appears in the debug string).
	eq(t, EscapeDebug("abc"), "abc")
	eq(t, EscapeDebug("a c"), "a c")
	eq(t, EscapeDebug("éèê"), "éèê")
	eq(t, EscapeDebug("\x00\r\n\t"), `\0\r\n\t`)
	eq(t, EscapeDebug("'\"\\"), `\'\"\\`)
	eq(t, EscapeDebug("\u007f\u00ff"), "\\u{7f}\u00ff")
	eq(t, EscapeDebug("\u0100\uffff"), "\u0100\\u{ffff}")
	eq(t, EscapeDebug("\U00010000\U0010ffff"), "\U00010000\\u{10ffff}")
	eq(t, EscapeDebug("ab\u200b"), `ab\u{200b}`)
	eq(t, EscapeDebug("\U0010d4ea\r"), `\u{10d4ea}\r`)
	eq(t, EscapeDebug("\u0301a\u0301bé\ue000"), "\\u{301}a\u0301bé\\u{e000}")
}

func TestStr_test_escape_default(t *testing.T) {
	eq(t, EscapeDefault("abc"), "abc")
	eq(t, EscapeDefault("a c"), "a c")
	eq(t, EscapeDefault("éèê"), `\u{e9}\u{e8}\u{ea}`)
	eq(t, EscapeDefault("\r\n\t"), `\r\n\t`)
	eq(t, EscapeDefault("'\"\\"), `\'\"\\`)
	eq(t, EscapeDefault("\u007f\u00ff"), `\u{7f}\u{ff}`)
	eq(t, EscapeDefault("\u0100\uffff"), `\u{100}\u{ffff}`)
	eq(t, EscapeDefault("\U00010000\U0010ffff"), `\u{10000}\u{10ffff}`)
	eq(t, EscapeDefault("ab\u200b"), `ab\u{200b}`)
	eq(t, EscapeDefault("\U0010d4ea\r"), `\u{10d4ea}\r`)
}

func TestStr_test_iterator(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	v := []rune{'ศ', 'ไ', 'ท', 'ย', '中', '华', 'V', 'i', 'ệ', 't', ' ', 'N', 'a', 'm'}
	eqSlice(t, Chars(s), v)
	eq(t, CharsCount(s), len(v))
}

func TestStr_test_rev_iterator(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	v := []rune{'m', 'a', 'N', ' ', 't', 'ệ', 'i', 'V', '华', '中', 'ย', 'ท', 'ไ', 'ศ'}
	eqSlice(t, CharsRev(s), v)
}

func TestStr_test_iterator_last(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	eq(t, CharsNextBack(s), rs.Some('m'))
}

func TestStr_test_char_indicesator(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	p := []int{0, 3, 6, 9, 12, 15, 18, 19, 20, 23, 24, 25, 26, 27}
	v := []rune{'ศ', 'ไ', 'ท', 'ย', '中', '华', 'V', 'i', 'ệ', 't', ' ', 'N', 'a', 'm'}
	var want []rs.Tuple2[int, rune]
	for i := range p {
		want = append(want, rs.T2(p[i], v[i]))
	}
	eqSlice(t, CharIndices(s), want)
}

func TestStr_test_char_indices_revator(t *testing.T) {
	s := "ศไทย中华Việt Nam"
	p := []int{27, 26, 25, 24, 23, 20, 19, 18, 15, 12, 9, 6, 3, 0}
	v := []rune{'m', 'a', 'N', ' ', 't', 'ệ', 'i', 'V', '华', '中', 'ย', 'ท', 'ไ', 'ศ'}
	var want []rs.Tuple2[int, rune]
	for i := range p {
		want = append(want, rs.T2(p[i], v[i]))
	}
	eqSlice(t, CharIndicesRev(s), want)
}

const maryLamb = "\nMäry häd ä little lämb\nLittle lämb\n"

func reversed(parts []string) []string {
	out := slices.Clone(parts)
	slices.Reverse(out)
	return out
}

func TestStr_test_splitn_char_iterator(t *testing.T) {
	data := maryLamb

	eqSlice(t, SplitnChar(data, 4, ' '), []string{"\nMäry", "häd", "ä", "little lämb\nLittle lämb\n"})
	eqSlice(t, SplitnFn(data, 4, charIs(' ')), []string{"\nMäry", "häd", "ä", "little lämb\nLittle lämb\n"})

	// Unicode
	eqSlice(t, SplitnChar(data, 4, 'ä'), []string{"\nM", "ry h", "d ", " little lämb\nLittle lämb\n"})
	eqSlice(t, SplitnFn(data, 4, charIs('ä')), []string{"\nM", "ry h", "d ", " little lämb\nLittle lämb\n"})
}

func TestStr_test_split_char_iterator_no_trailing(t *testing.T) {
	data := maryLamb

	eqSlice(t, SplitChar(data, '\n'), []string{"", "Märy häd ä little lämb", "Little lämb", ""})
	eqSlice(t, SplitTerminatorChar(data, '\n'), []string{"", "Märy häd ä little lämb", "Little lämb"})
}

func TestStr_test_split_char_iterator_inclusive(t *testing.T) {
	data := maryLamb

	eqSlice(t, SplitInclusiveChar(data, '\n'), []string{"\n", "Märy häd ä little lämb\n", "Little lämb\n"})

	uppercaseSeparated := "SheePSharKTurtlECaT"
	firstChar := true
	split := SplitInclusiveFn(uppercaseSeparated, func(c rune) bool {
		split := !firstChar && IsUppercase(c)
		firstChar = split
		return split
	})
	eqSlice(t, split, []string{"SheeP", "SharK", "TurtlE", "CaT"})
}

func TestStr_test_rsplit(t *testing.T) {
	data := maryLamb

	eqSlice(t, RsplitChar(data, ' '), []string{"lämb\n", "lämb\nLittle", "little", "ä", "häd", "\nMäry"})
	eqSlice(t, Rsplit(data, "lämb"), []string{"\n", "\nLittle ", "\nMäry häd ä little "})
	eqSlice(t, RsplitFn(data, charIs('ä')), []string{"mb\n", "mb\nLittle l", " little l", "d ", "ry h", "\nM"})
}

func TestStr_test_rsplitn(t *testing.T) {
	data := maryLamb

	eqSlice(t, RsplitnChar(data, 2, ' '), []string{"lämb\n", "\nMäry häd ä little lämb\nLittle"})
	eqSlice(t, Rsplitn(data, 2, "lämb"), []string{"\n", "\nMäry häd ä little lämb\nLittle "})
	eqSlice(t, RsplitnFn(data, 2, charIs('ä')), []string{"mb\n", "\nMäry häd ä little lämb\nLittle l"})
}

func TestStr_test_split_once(t *testing.T) {
	none := rs.None[rs.Tuple2[string, string]]()
	eq(t, SplitOnce("", "->"), none)
	eq(t, SplitOnce("-", "->"), none)
	eq(t, SplitOnce("->", "->"), pair("", ""))
	eq(t, SplitOnce("a->", "->"), pair("a", ""))
	eq(t, SplitOnce("->b", "->"), pair("", "b"))
	eq(t, SplitOnce("a->b", "->"), pair("a", "b"))
	eq(t, SplitOnce("a->b->c", "->"), pair("a", "b->c"))
	eq(t, SplitOnce("---", "--"), pair("", "-"))
}

func TestStr_test_rsplit_once(t *testing.T) {
	none := rs.None[rs.Tuple2[string, string]]()
	eq(t, RsplitOnce("", "->"), none)
	eq(t, RsplitOnce("-", "->"), none)
	eq(t, RsplitOnce("->", "->"), pair("", ""))
	eq(t, RsplitOnce("a->", "->"), pair("a", ""))
	eq(t, RsplitOnce("->b", "->"), pair("", "b"))
	eq(t, RsplitOnce("a->b", "->"), pair("a", "b"))
	eq(t, RsplitOnce("a->b->c", "->"), pair("a->b", "c"))
	eq(t, RsplitOnce("---", "--"), pair("-", ""))
}

func TestStr_test_split_whitespace(t *testing.T) {
	data := "\n \tMäry   häd\tä  little lämb\nLittle lämb\n"
	eqSlice(t, SplitWhitespace(data), []string{"Märy", "häd", "ä", "little", "lämb", "Little", "lämb"})
}

func TestStr_test_lines(t *testing.T) {
	check := func(data string, expected []string) {
		t.Helper()
		eqSlice(t, Lines(data), expected)
	}
	check("", nil)
	check("\n", []string{""})
	check("\n2nd", []string{"", "2nd"})
	check("\r\n", []string{""})
	check("bare\r", []string{"bare\r"})
	check("bare\rcr", []string{"bare\rcr"})
	check("Text\n\r", []string{"Text", "\r"})
	check("\nMäry häd ä little lämb\n\r\nLittle lämb\n", []string{"", "Märy häd ä little lämb", "", "Little lämb"})
	check("\r\nMäry häd ä little lämb\n\nLittle lämb", []string{"", "Märy häd ä little lämb", "", "Little lämb"})
}

func TestStr_test_splitator(t *testing.T) {
	check := func(s, sep string, u []string) {
		t.Helper()
		eqSlice(t, Split(s, sep), u)
	}
	check("--1233345--", "12345", []string{"--1233345--"})
	check("abc::hello::there", "::", []string{"abc", "hello", "there"})
	check("::hello::there", "::", []string{"", "hello", "there"})
	check("hello::there::", "::", []string{"hello", "there", ""})
	check("::hello::there::", "::", []string{"", "hello", "there", ""})
	check("ประเทศไทย中华Việt Nam", "中华", []string{"ประเทศไทย", "Việt Nam"})
	check("zzXXXzzYYYzz", "zz", []string{"", "XXX", "YYY", ""})
	check("zzXXXzYYYz", "XXX", []string{"zz", "zYYYz"})
	check(".XXX.YYY.", ".", []string{"", "XXX", "YYY", ""})
	check("", ".", []string{""})
	check("zz", "zz", []string{"", ""})
	check("ok", "z", []string{"ok"})
	check("zzz", "zz", []string{"", "z"})
	check("zzzzz", "zz", []string{"", "", "z"})
}

func TestStr_test_empty_match_indices(t *testing.T) {
	data := "aä中!"
	eqSlice(t, MatchIndices(data, ""), []rs.Tuple2[int, string]{rs.T2(0, ""), rs.T2(1, ""), rs.T2(3, ""), rs.T2(6, ""), rs.T2(7, "")})
}

func TestStr_test_bool_from_str(t *testing.T) {
	eq(t, rs.OkOption(ParseBool("true")), rs.Some(true))
	eq(t, rs.OkOption(ParseBool("false")), rs.Some(false))
	eq(t, rs.OkOption(ParseBool("not even a boolean")), rs.None[bool]())
}

func checkContainsAllSubstrings(t *testing.T, haystack string) {
	t.Helper()
	for i := 0; i < len(haystack); i++ {
		// check different haystack lengths since we special-case short haystacks.
		haystack := haystack[0:i]
		isTrue(t, Contains(haystack, ""))
		for j := 0; j < len(haystack); j++ {
			for k := j + 1; k <= len(haystack); k++ {
				needle := haystack[j:k]
				isTrue(t, Contains(haystack, needle))
				modifiedNeedle := needle
				ReplaceRange(&modifiedNeedle, 0, 1, "\x00")
				isTrue(t, !Contains(haystack, modifiedNeedle))

				modifiedNeedle = needle
				ReplaceRange(&modifiedNeedle, len(needle)-1, len(needle), "\x00")
				isTrue(t, !Contains(haystack, modifiedNeedle))
			}
		}
	}
}

func TestStr_strslice_issue_16589(t *testing.T) {
	isTrue(t, Contains("bananas", "nana"))

	// prior to the fix for #16589, x.contains("abcdabcd") returned false
	// test all substrings for good measure
	checkContainsAllSubstrings(t, "012345678901234567890123456789bcdabcdabcd")
}

func TestStr_strslice_issue_16878(t *testing.T) {
	isTrue(t, !Contains("1234567ah012345678901ah", "hah"))
	isTrue(t, !Contains("00abc01234567890123456789abc", "bcabc"))
}

func TestStr_strslice_issue_104726(t *testing.T) {
	// Edge-case in the simd_contains impl.
	// The first and last byte are the same so it backtracks by one byte
	// which aligns with the end of the string. Previously incorrect offset calculations
	// lead to out-of-bounds slicing.
	needle := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaba"
	haystack := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"
	isTrue(t, !Contains(haystack, needle))
}

func TestStr_test_strslice_contains(t *testing.T) {
	x := "There are moments, Jeeves, when one asks oneself, 'Do trousers matter?'"
	checkContainsAllSubstrings(t, x)
}

func TestStr_test_rsplitn_char_iterator(t *testing.T) {
	data := maryLamb

	eqSlice(t, reversed(RsplitnChar(data, 4, ' ')), []string{"\nMäry häd ä", "little", "lämb\nLittle", "lämb\n"})
	eqSlice(t, reversed(RsplitnFn(data, 4, charIs(' '))), []string{"\nMäry häd ä", "little", "lämb\nLittle", "lämb\n"})

	// Unicode
	eqSlice(t, reversed(RsplitnChar(data, 4, 'ä')), []string{"\nMäry häd ", " little l", "mb\nLittle l", "mb\n"})
	eqSlice(t, reversed(RsplitnFn(data, 4, charIs('ä'))), []string{"\nMäry häd ", " little l", "mb\nLittle l", "mb\n"})
}

func TestStr_test_split_char_iterator(t *testing.T) {
	data := maryLamb

	// Rust's `split(..).rev()` is the same search run from the back, which
	// for these patterns is rsplit.
	want := []string{"\nMäry", "häd", "ä", "little", "lämb\nLittle", "lämb\n"}
	eqSlice(t, SplitChar(data, ' '), want)
	eqSlice(t, reversed(RsplitChar(data, ' ')), want)
	eqSlice(t, SplitFn(data, charIs(' ')), want)
	eqSlice(t, reversed(RsplitFn(data, charIs(' '))), want)

	// Unicode
	want = []string{"\nM", "ry h", "d ", " little l", "mb\nLittle l", "mb\n"}
	eqSlice(t, SplitChar(data, 'ä'), want)
	eqSlice(t, reversed(RsplitChar(data, 'ä')), want)
	eqSlice(t, SplitFn(data, charIs('ä')), want)
	eqSlice(t, reversed(RsplitFn(data, charIs('ä'))), want)
}

func TestStr_test_rev_split_char_iterator_no_trailing(t *testing.T) {
	data := maryLamb

	eqSlice(t, reversed(RsplitChar(data, '\n')), []string{"", "Märy häd ä little lämb", "Little lämb", ""})
	eqSlice(t, reversed(RsplitTerminatorChar(data, '\n')), []string{"", "Märy häd ä little lämb", "Little lämb"})
}

func TestStr_test_utf16_code_units(t *testing.T) {
	eqSlice(t, EncodeUtf16("é\U0001F4A9"), []uint16{0xE9, 0xD83D, 0xDCA9})
}

func TestStr_starts_with_in_unicode(t *testing.T) {
	isTrue(t, !StartsWith("├── Cargo.toml", "# "))
}

func TestStr_starts_short_long(t *testing.T) {
	isTrue(t, !StartsWith("", "##"))
	isTrue(t, !StartsWith("##", "####"))
	isTrue(t, StartsWith("####", "##"))
	isTrue(t, !StartsWith("##ä", "####"))
	isTrue(t, StartsWith("####ä", "##"))
	isTrue(t, !StartsWith("##", "####ä"))
	isTrue(t, StartsWith("##ä##", "##ä"))

	isTrue(t, StartsWith("", ""))
	isTrue(t, StartsWith("ä", ""))
	isTrue(t, StartsWith("#ä", ""))
	isTrue(t, StartsWith("##ä", ""))
	isTrue(t, StartsWith("ä###", ""))
	isTrue(t, StartsWith("#ä##", ""))
	isTrue(t, StartsWith("##ä#", ""))
}

func TestStr_contains_weird_cases(t *testing.T) {
	isTrue(t, ContainsChar("* \t", ' '))
	isTrue(t, !ContainsChar("* \t", '?'))
	isTrue(t, !ContainsChar("* \t", '\U0001F4A9'))
}

func TestStr_trim_ws(t *testing.T) {
	eq(t, TrimStartMatchesFn(" \t  a \t  ", IsWhitespace), "a \t  ")
	eq(t, TrimEndMatchesFn(" \t  a \t  ", IsWhitespace), " \t  a")
	eq(t, TrimMatchesFn(" \t  a \t  ", IsWhitespace), "a")
	eq(t, TrimStartMatchesFn(" \t   \t  ", IsWhitespace), "")
	eq(t, TrimEndMatchesFn(" \t   \t  ", IsWhitespace), "")
	eq(t, TrimMatchesFn(" \t   \t  ", IsWhitespace), "")
}

func TestStr_to_lowercase(t *testing.T) {
	eq(t, ToLowercase(""), "")
	eq(t, ToLowercase("AÉǅaé "), "aéǆaé ")

	// https://github.com/rust-lang/rust/issues/26035
	eq(t, ToLowercase("ΑΣ"), "ας")
	eq(t, ToLowercase("Α'Σ"), "α'ς")
	eq(t, ToLowercase("Α''Σ"), "α''ς")

	eq(t, ToLowercase("ΑΣ Α"), "ας α")
	eq(t, ToLowercase("Α'Σ Α"), "α'ς α")
	eq(t, ToLowercase("Α''Σ Α"), "α''ς α")

	eq(t, ToLowercase("ΑΣ' Α"), "ας' α")
	eq(t, ToLowercase("ΑΣ'' Α"), "ας'' α")

	eq(t, ToLowercase("Α'Σ' Α"), "α'ς' α")
	eq(t, ToLowercase("Α''Σ'' Α"), "α''ς'' α")

	eq(t, ToLowercase("Α Σ"), "α σ")
	eq(t, ToLowercase("Α 'Σ"), "α 'σ")
	eq(t, ToLowercase("Α ''Σ"), "α ''σ")

	eq(t, ToLowercase("Σ"), "σ")
	eq(t, ToLowercase("'Σ"), "'σ")
	eq(t, ToLowercase("''Σ"), "''σ")

	eq(t, ToLowercase("ΑΣΑ"), "ασα")
	eq(t, ToLowercase("ΑΣ'Α"), "ασ'α")
	eq(t, ToLowercase("ΑΣ''Α"), "ασ''α")

	// https://github.com/rust-lang/rust/issues/124714
	// input lengths around the boundary of the chunk size used by the ascii prefix optimization
	eq(t, ToLowercase("abcdefghijklmnoΣ"), "abcdefghijklmnoς")
	eq(t, ToLowercase("abcdefghijklmnopΣ"), "abcdefghijklmnopς")
	eq(t, ToLowercase("abcdefghijklmnopqΣ"), "abcdefghijklmnopqς")

	// a really long string that has it's lowercase form
	// even longer. this tests that implementations don't assume
	// an incorrect upper bound on allocations
	upper := Repeat("İ", 512)
	lower := Repeat("i\u0307", 512)
	eq(t, ToLowercase(upper), lower)

	// a really long ascii-only string.
	// This test that the ascii hot-path
	// functions correctly
	upper = Repeat("A", 511)
	lower = Repeat("a", 511)
	eq(t, ToLowercase(upper), lower)
}

func TestStr_to_uppercase(t *testing.T) {
	eq(t, ToUppercase(""), "")
	eq(t, ToUppercase("aéǅßẞﬁᾀ"), "AÉǄSSẞFIἈΙ")
}

func TestStr_test_repeat(t *testing.T) {
	eq(t, Repeat("", 3), "")
	eq(t, Repeat("abc", 0), "")
	eq(t, Repeat("α", 3), "ααα")
}

func TestStr_floor_char_boundary(t *testing.T) {
	checkMany := func(s string, arg []int, ret int) {
		t.Helper()
		for _, idx := range arg {
			if got := FloorCharBoundary(s, idx); got != ret {
				t.Errorf("FloorCharBoundary(%q, %d) = %d, want %d", s, idx, got, ret)
			}
		}
	}
	span := func(from, to int) []int {
		var out []int
		for i := from; i < to; i++ {
			out = append(out, i)
		}
		return out
	}

	// edge case
	checkMany("", []int{0, 1, rs.UsizeMax, -1}, 0)

	// basic check
	checkMany("x", []int{0}, 0)
	checkMany("x", []int{1, rs.UsizeMax, -1}, 1)

	// 1-byte chars
	checkMany("jp", []int{0}, 0)
	checkMany("jp", []int{1}, 1)
	checkMany("jp", span(2, 4), 2)

	// 2-byte chars
	checkMany("ĵƥ", span(0, 2), 0)
	checkMany("ĵƥ", span(2, 4), 2)
	checkMany("ĵƥ", span(4, 6), 4)

	// 3-byte chars
	checkMany("日本", span(0, 3), 0)
	checkMany("日本", span(3, 6), 3)
	checkMany("日本", span(6, 8), 6)

	// 4-byte chars
	checkMany("🇯🇵", span(0, 4), 0)
	checkMany("🇯🇵", span(4, 8), 4)
	checkMany("🇯🇵", span(8, 10), 8)

	// sample from the issue, with all the offsets
	s := "jpĵƥ日本🇯🇵jpĵƥ日本🇯🇵"
	expected := []int{
		0, 1, 2, 2, 4, 4, 6, 6, 6, 9, 9, 9, 12, 12, 12, 12, 16, 16, 16, 16, 20, 21, 22, 22, 24, 24,
		26, 26, 26, 29, 29, 29, 32, 32, 32, 32, 36, 36, 36, 36, 40, 40, 40, 40,
	}
	for idx, ret := range expected {
		checkMany(s, []int{idx}, ret)
	}
}

func TestStr_ceil_char_boundary(t *testing.T) {
	checkMany := func(s string, arg []int, ret int) {
		t.Helper()
		for _, idx := range arg {
			if got := CeilCharBoundary(s, idx); got != ret {
				t.Errorf("CeilCharBoundary(%q, %d) = %d, want %d", s, idx, got, ret)
			}
		}
	}
	span := func(from, to int) []int {
		var out []int
		for i := from; i <= to; i++ {
			out = append(out, i)
		}
		return out
	}

	// edge case
	checkMany("", []int{0}, 0)

	// basic check
	checkMany("x", []int{0}, 0)
	checkMany("x", []int{1}, 1)

	// 1-byte chars
	checkMany("jp", []int{0}, 0)
	checkMany("jp", []int{1}, 1)
	checkMany("jp", []int{2}, 2)

	// 2-byte chars
	checkMany("ĵƥ", span(0, 0), 0)
	checkMany("ĵƥ", span(1, 2), 2)
	checkMany("ĵƥ", span(3, 4), 4)

	// 3-byte chars
	checkMany("日本", span(0, 0), 0)
	checkMany("日本", span(1, 3), 3)
	checkMany("日本", span(4, 6), 6)

	// 4-byte chars
	checkMany("🇯🇵", span(0, 0), 0)
	checkMany("🇯🇵", span(1, 4), 4)
	checkMany("🇯🇵", span(5, 8), 8)

	// above len
	checkMany("hello", span(5, 10), 5)

	// sample from the issue, with all the offsets
	s := "jpĵƥ日本🇯🇵jpĵƥ日本🇯🇵"
	expected := []int{
		0, 1, 2, 4, 4, 6, 6, 9, 9, 9, 12, 12, 12, 16, 16, 16, 16, 20, 20, 20, 20, 21, 22, 24, 24,
		26, 26, 29, 29, 29, 32, 32, 32, 36, 36, 36, 36, 40, 40, 40, 40, 40, 40, 40,
	}
	for idx, ret := range expected {
		checkMany(s, []int{idx}, ret)
	}
}

// The generate_iterator_test! cases: each iterator forwards, and its reverse
// counterpart backwards.

func TestStr_double_ended_split(t *testing.T) {
	res := []string{"foo", "bar", "baz"}
	eqSlice(t, SplitChar("foo.bar.baz", '.'), res)
	eqSlice(t, reversed(RsplitChar("foo.bar.baz", '.')), res)
	eqSlice(t, Split("foo::bar::baz", "::"), res)
	eqSlice(t, reversed(Rsplit("foo::bar::baz", "::")), res)
}

func TestStr_double_ended_split_terminator(t *testing.T) {
	res := []string{"foo", "bar", "baz"}
	eqSlice(t, SplitTerminatorChar("foo;bar;baz;", ';'), res)
	eqSlice(t, reversed(RsplitTerminatorChar("foo;bar;baz;", ';')), res)
}

func TestStr_double_ended_matches(t *testing.T) {
	res := []string{"1", "2", "3"}
	eqSlice(t, MatchesFn("a1b2c3", IsNumeric), res)
	eqSlice(t, reversed(RmatchesFn("a1b2c3", IsNumeric)), res)
}

func TestStr_double_ended_match_indices(t *testing.T) {
	res := []rs.Tuple2[int, string]{rs.T2(1, "1"), rs.T2(3, "2"), rs.T2(5, "3")}
	eqSlice(t, MatchIndicesFn("a1b2c3", IsNumeric), res)
	backwards := RmatchIndicesFn("a1b2c3", IsNumeric)
	slices.Reverse(backwards)
	eqSlice(t, backwards, res)
}

func TestStr_not_double_ended_splitn(t *testing.T) {
	eqSlice(t, Splitn("foo::bar::baz", 2, "::"), []string{"foo", "bar::baz"})
}

func TestStr_not_double_ended_rsplitn(t *testing.T) {
	eqSlice(t, Rsplitn("foo::bar::baz", 2, "::"), []string{"baz", "foo::bar"})
}

func TestStr_utf8_chars(t *testing.T) {
	// Chars of 1, 2, 3, and 4 bytes
	chs := []rune{'e', 'é', '€', 0x10000}
	s := string(chs)
	schs := Chars(s)
	isTrue(t, len(s) == 10)
	isTrue(t, CharsCount(s) == 4)
	isTrue(t, len(schs) == 4)
	eq(t, string(schs), s)

	_, err := FromUtf8([]byte(s))
	isTrue(t, err == nil)
	// invalid prefix
	for _, b := range [][]byte{
		{0x80},
		// invalid 2 byte prefix
		{0xc0}, {0xc0, 0x10},
		// invalid 3 byte prefix
		{0xe0}, {0xe0, 0x10}, {0xe0, 0xff, 0x10},
		// invalid 4 byte prefix
		{0xf0}, {0xf0, 0x10}, {0xf0, 0xff, 0x10}, {0xf0, 0xff, 0xff, 0x10},
	} {
		_, err := FromUtf8(b)
		isTrue(t, err != nil)
	}
}
