// Port of alloctests 1.99.1 tests/str.rs (the tests of what this package
// covers; see doc.go for what is left out)
//
// Not ported, because what they test is Go's own spelling or has no form in
// Go: comparison, emptiness, bytes, pointers and owned forms (test_le,
// test_total_ord, test_is_empty, test_into_bytes, test_as_bytes,
// test_as_bytes_fail, test_as_ptr, test_bytesator, test_bytes_revator,
// test_bytesator_nth, test_bytesator_count, test_bytesator_last,
// test_str_default, test_str_container, test_into_string,
// test_box_slice_clone, test_cow_from, const_str_ptr, utf8); concat and join
// (test_concat_*, test_join_*); literal syntax (test_str_multiline,
// test_str_escapes); unsafe and mutable slicing (test_unsafe_slice,
// test_str_slicemut_rangetoinclusive_*, index_mut_fail, test_split_at_mut,
// test_copy_from_str*); const evaluation (test_const_is_utf8,
// const_from_utf8_mostly_ascii, and the const half of from_utf8_error); the
// iterators as objects (test_iterator_advance, test_iterator_clone,
// test_chars_debug, test_utf16_size_hint, rangeinclusive_exhausted); the
// Pattern impls of references and lifetimes (test_pattern_deref_forward,
// different_str_pattern_forwarding_lifetimes); and the unstable
// word_to_titlecase and to_casefold_unnormalized.

package strs

import (
	"fmt"
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

// thaiChineseVietShort is the string most of the iterator tests walk.
const thaiChineseVietShort = "ศไทย中华Việt Nam"

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

func TestStr_test_collect(t *testing.T) {
	empty := ""
	s := string(Chars(empty))
	eq(t, empty, s)
	data := "\u0e1b\u0e23\u0e30\u0e40\u0e17\u0e28\u0e44\u0e17\u0e22\u4e2d"
	s = string(Chars(data))
	eq(t, data, s)
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

// The tests of `mod slice_index`. The current implementation of SliceIndex
// fails to handle methods orthogonally from range types; therefore, it is
// worth testing all of the indexing operations on each input.

// strRange is a range expression a Rust string is indexed with: `..`,
// `a..b`, `a..=b`, `..b`, `..=b` or `a..`.
type strRange struct {
	form       string
	start, end int
}

func rangeFull() strRange                    { return strRange{"..", 0, 0} }
func rangeAB(start, end int) strRange        { return strRange{"a..b", start, end} }
func rangeInclusive(start, end int) strRange { return strRange{"a..=b", start, end} }
func rangeTo(end int) strRange               { return strRange{"..b", 0, end} }
func rangeToInclusive(end int) strRange      { return strRange{"..=b", 0, end} }
func rangeFrom(start int) strRange           { return strRange{"a..", start, 0} }

// index is `&s[r]`.
func (r strRange) index(s string) string {
	switch r.form {
	case "..":
		return s
	case "a..b":
		return Slice(s, r.start, r.end)
	case "a..=b":
		return SliceInclusive(s, r.start, r.end)
	case "..b":
		return SliceTo(s, r.end)
	case "..=b":
		return SliceToInclusive(s, r.end)
	default:
		return SliceFrom(s, r.start)
	}
}

// get is `s.get(r)`.
func (r strRange) get(s string) rs.Option[string] {
	switch r.form {
	case "..":
		return rs.Some(s)
	case "a..b":
		return Get(s, r.start, r.end)
	case "a..=b":
		return GetInclusive(s, r.start, r.end)
	case "..b":
		return GetTo(s, r.end)
	case "..=b":
		return GetToInclusive(s, r.end)
	default:
		return GetFrom(s, r.start)
	}
}

// assertRangeEq is the assert_range_eq! macro: it tests a slicing operation
// **that should succeed,** on all of the indexing methods (`&s[r]` and
// `s.get(r)`; the unchecked and `mut` forms have no Go counterpart). It is
// not suitable for testing failure on invalid inputs.
func assertRangeEq(t testing.TB, s string, r strRange, expected string) {
	t.Helper()
	if got := r.index(s); got != expected {
		t.Errorf("assertion `left == right` failed: (in assertion for: index)\n  left: %q\n right: %q", got, expected)
	}
	if got := r.get(s); got != rs.Some(expected) {
		t.Errorf("assertion `left == right` failed: (in assertion for: get)\n  left: %v\n right: Some(%q)", got, expected)
	}
}

// failureRecorder is a testing.TB that keeps the first failure reported to
// it instead of failing the test, so that a test can watch assertRangeEq
// fail.
type failureRecorder struct {
	testing.TB
	failure string
}

func (r *failureRecorder) Helper() {}

func (r *failureRecorder) Errorf(format string, args ...any) {
	if r.failure == "" {
		r.failure = fmt.Sprintf(format, args...)
	}
}

// Make sure the macro can actually detect bugs,
// because if it can't, then what are we even doing here?
//
// (Be aware this only demonstrates the ability to detect bugs
// in the FIRST method that panics, as the macro is not designed
// to be used in `should_panic`)
func TestStr_slice_index_assert_range_eq_can_fail_by_panic(t *testing.T) {
	shouldPanic(t, "out of bounds", func() { assertRangeEq(t, "abc", rangeAB(0, 5), "abc") })
}

// (Be aware this only demonstrates the ability to detect bugs
// in the FIRST method it calls, as the macro is not designed
// to be used in `should_panic`)
func TestStr_slice_index_assert_range_eq_can_fail_by_inequality(t *testing.T) {
	recorder := &failureRecorder{TB: t}
	assertRangeEq(recorder, "abc", rangeAB(0, 2), "abc")
	if !strings.Contains(recorder.failure, "==") {
		t.Errorf("assertRangeEq did not fail with \"==\": %q", recorder.failure)
	}
}

// panicCase is one case of the panic_cases! macro: indexing data with bad
// panics with a message containing message (index_fail; index_mut_fail is
// the same in Go), while get gives None and each good range gives its output
// (pass).
type panicCase struct {
	name    string
	data    string
	good    []goodRange
	bad     strRange
	message string
}

// goodRange is a similar input for which `data[r]` succeeds, and the
// corresponding output str. This helps validate "critical points" where an
// input range straddles the boundary between valid and invalid (such as the
// input `len..len`, which is just barely valid).
type goodRange struct {
	r      strRange
	output string
}

func runPanicCases(t *testing.T, cases []panicCase) {
	t.Helper()
	for _, c := range cases {
		t.Run(c.name+"/pass", func(t *testing.T) {
			for _, good := range c.good {
				assertRangeEq(t, c.data, good.r, good.output)
			}
			eq(t, c.bad.get(c.data), rs.None[string]())
		})
		t.Run(c.name+"/index_fail", func(t *testing.T) {
			shouldPanic(t, c.message, func() { c.bad.index(c.data) })
		})
	}
}

func TestStr_slice_index_simple_ascii(t *testing.T) {
	assertRangeEq(t, "abc", rangeFull(), "abc")

	assertRangeEq(t, "abc", rangeAB(0, 2), "ab")
	assertRangeEq(t, "abc", rangeInclusive(0, 1), "ab")
	assertRangeEq(t, "abc", rangeTo(2), "ab")
	assertRangeEq(t, "abc", rangeToInclusive(1), "ab")

	assertRangeEq(t, "abc", rangeAB(1, 3), "bc")
	assertRangeEq(t, "abc", rangeInclusive(1, 2), "bc")
	assertRangeEq(t, "abc", rangeAB(1, 1), "")
	assertRangeEq(t, "abc", rangeInclusive(1, 0), "")
}

func TestStr_slice_index_simple_unicode(t *testing.T) {
	// 日本
	assertRangeEq(t, "\u65e5\u672c", rangeFull(), "\u65e5\u672c")

	assertRangeEq(t, "\u65e5\u672c", rangeAB(0, 3), "\u65e5")
	assertRangeEq(t, "\u65e5\u672c", rangeInclusive(0, 2), "\u65e5")
	assertRangeEq(t, "\u65e5\u672c", rangeTo(3), "\u65e5")
	assertRangeEq(t, "\u65e5\u672c", rangeToInclusive(2), "\u65e5")

	assertRangeEq(t, "\u65e5\u672c", rangeAB(3, 6), "\u672c")
	assertRangeEq(t, "\u65e5\u672c", rangeInclusive(3, 5), "\u672c")
	assertRangeEq(t, "\u65e5\u672c", rangeFrom(3), "\u672c")

	data := "ประเทศไทย中华"
	assertRangeEq(t, data, rangeAB(0, 3), "ป")
	assertRangeEq(t, data, rangeAB(3, 6), "ร")
	assertRangeEq(t, data, rangeAB(3, 3), "")
	assertRangeEq(t, data, rangeAB(30, 33), "华")

	/*0: 中
	 3: 华
	 6: V
	 7: i
	 8: ệ
	11: t
	12:
	13: N
	14: a
	15: m */
	ss := "中华Việt Nam"
	assertRangeEq(t, ss, rangeAB(3, 6), "华")
	assertRangeEq(t, ss, rangeAB(6, 16), "Việt Nam")
	assertRangeEq(t, ss, rangeInclusive(6, 15), "Việt Nam")
	assertRangeEq(t, ss, rangeFrom(6), "Việt Nam")

	assertRangeEq(t, ss, rangeAB(0, 3), "中")
	assertRangeEq(t, ss, rangeAB(3, 7), "华V")
	assertRangeEq(t, ss, rangeInclusive(3, 6), "华V")
	assertRangeEq(t, ss, rangeAB(3, 3), "")
	assertRangeEq(t, ss, rangeInclusive(3, 2), "")
}

func TestStr_slice_index_simple_big(t *testing.T) {
	aMillionLetterX := func() string {
		i := 0
		var rs_ strings.Builder
		for i < 100000 {
			rs_.WriteString("\u534e\u534e\u534e\u534e\u534e\u534e\u534e\u534e\u534e\u534e")
			i++
		}
		return rs_.String()
	}
	halfAMillionLetterX := func() string {
		i := 0
		var rs_ strings.Builder
		for i < 100000 {
			rs_.WriteString("\u534e\u534e\u534e\u534e\u534e")
			i++
		}
		return rs_.String()
	}
	letters := aMillionLetterX()
	assertRangeEq(t, letters, rangeAB(0, 3*500000), halfAMillionLetterX())
}

func TestStr_slice_index_test_slice_fail(t *testing.T) {
	shouldPanic(t, "", func() { Slice("中华Việt Nam", 0, 2) })
}

func TestStr_slice_index_panic_cases(t *testing.T) {
	runPanicCases(t, []panicCase{
		{"rangefrom_len", "abcdef", []goodRange{{rangeFrom(6), ""}}, rangeFrom(7), "out of bounds"},
		{"rangeto_len", "abcdef", []goodRange{{rangeTo(6), "abcdef"}}, rangeTo(7), "out of bounds"},
		{"rangetoinclusive_len", "abcdef", []goodRange{{rangeToInclusive(5), "abcdef"}}, rangeToInclusive(6), "out of bounds"},
		{"rangeinclusive_len", "abcdef", []goodRange{{rangeInclusive(0, 5), "abcdef"}}, rangeInclusive(0, 6), "out of bounds"},
		{"range_len_len", "abcdef", []goodRange{{rangeAB(6, 6), ""}}, rangeAB(7, 7), "out of bounds"},
		{"rangeinclusive_len_len", "abcdef", []goodRange{{rangeInclusive(6, 5), ""}}, rangeInclusive(7, 6), "out of bounds"},
	})

	// rangeinclusive_exhausted indexes with a `RangeInclusive` whose
	// iterator has been run to its end, which has no Go form.

	runPanicCases(t, []panicCase{
		{"range_neg_width", "abcdef", []goodRange{{rangeAB(4, 4), ""}}, rangeAB(4, 3), "byte range starts at 4 but ends at 3"},
		{"rangeinclusive_neg_width", "abcdef", []goodRange{{rangeInclusive(4, 3), ""}}, rangeInclusive(4, 2), "byte range starts at 4 but ends at 3"},
	})
}

func TestStr_slice_index_overflow(t *testing.T) {
	// `usize::MAX` is rs.UsizeMax. The usize that, as in Rust, wraps to 0
	// when one is added is the Go int -1, which this package also reads as
	// a usize; both are tested.
	runPanicCases(t, []panicCase{
		// note: using 0 specifically ensures that the result of overflowing is 0..0,
		//       so that `get` doesn't simply return None for the wrong reason.
		{"rangeinclusive", "hello", nil, rangeInclusive(0, rs.UsizeMax), "out of bounds"},
		{"rangetoinclusive", "hello", nil, rangeToInclusive(rs.UsizeMax), "out of bounds"},
		{"rangeinclusive_wrapping", "hello", nil, rangeInclusive(0, -1), "out of bounds"},
		{"rangetoinclusive_wrapping", "hello", nil, rangeToInclusive(-1), "out of bounds"},
	})
}

func TestStr_slice_index_boundary(t *testing.T) {
	const (
		data = "abc\u03b1\u03b2\u03b3"

		badStart    = 4
		goodStart   = 3
		badEnd      = 6
		goodEnd     = 7
		badEndIncl  = badEnd - 1
		goodEndIncl = goodEnd - 1

		startMessage = "start byte index 4 is not a char boundary; it is inside 'α' (bytes 3..5 of string)"
		endMessage   = "end byte index 6 is not a char boundary; it is inside 'β' (bytes 5..7 of string)"
	)

	// it is especially important to test all of the different range types here
	// because some of the logic may be duplicated as part of micro-optimizations
	// to dodge unicode boundary checks on half-ranges.
	runPanicCases(t, []panicCase{
		{"range_1", data, nil, rangeAB(badStart, goodEnd), startMessage},
		{"range_2", data, nil, rangeAB(goodStart, badEnd), endMessage},
		{"rangefrom", data, nil, rangeFrom(badStart), startMessage},
		{"rangeto", data, nil, rangeTo(badEnd), endMessage},
		{"rangeinclusive_1", data, nil, rangeInclusive(badStart, goodEndIncl), startMessage},
		{"rangeinclusive_2", data, nil, rangeInclusive(goodStart, badEndIncl), endMessage},
		{"rangetoinclusive", data, nil, rangeToInclusive(badEndIncl), endMessage},
	})
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

func TestStr_vec_str_conversions(t *testing.T) {
	s1 := "All mimsy were the borogoves"

	v := []byte(s1)
	s2 := rs.Unwrap(FromUtf8(v))
	i := 0
	n1 := len(s1)
	n2 := len(v)
	eq(t, n1, n2)
	for i < n1 {
		a := s1[i]
		b := s2[i]
		eq(t, a, b)
		i++
	}
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

// flatMapRev is `s.chars().flat_map(f).rev()`.
func flatMapRev(s string, f func(rune) []rune) []rune {
	var it []rune
	for _, c := range Chars(s) {
		it = append(it, f(c)...)
	}
	slices.Reverse(it)
	return it
}

func TestStr_test_to_lowercase_rev_iterator(t *testing.T) {
	s := "A\u00d6\u00df\u00dc\U0001f4a9\u03a3\u03a4\u0399\u0393\u039c\u0391\u03a3\u01c5\ufb01\u0130"
	v := []rune{'\u0307', 'i', '\ufb01', '\u01c6', '\u03c3', '\u03b1', '\u03bc', '\u03b3', '\u03b9', '\u03c4', '\u03c3', '\U0001f4a9', '\u00fc', '\u00df', '\u00f6', 'a'}

	pos := 0
	it := flatMapRev(s, CharToLowercase)

	for _, c := range it {
		eq(t, c, v[pos])
		pos++
	}
	eq(t, pos, len(v))
}

func TestStr_test_to_uppercase_rev_iterator(t *testing.T) {
	s := "a\u00f6\u00df\u00fc\U0001f4a9\u03c3\u03c4\u03b9\u03b3\u03bc\u03b1\u03c2\u01c5\ufb01\u1f80"
	v := []rune{'\u0399', '\u1f08', 'I', 'F', '\u01c4', '\u03a3', '\u0391', '\u039c', '\u0393', '\u0399', '\u03a4', '\u03a3', '\U0001f4a9', '\u00dc', 'S', 'S', '\u00d6', 'A'}

	pos := 0
	it := flatMapRev(s, CharToUppercase)

	for _, c := range it {
		eq(t, c, v[pos])
		pos++
	}
	eq(t, pos, len(v))
}

func TestStr_test_chars_decoding(t *testing.T) {
	for code := uint32(0); code < 0x110000; code++ {
		c, ok := CharFromU32(code).Get()
		if !ok {
			continue
		}
		s := CharEncodeUtf8(c)
		if CharsNext(s) != rs.Some(c) {
			t.Fatalf("character %x=%c does not decode correctly", c, c)
		}
	}
}

func TestStr_test_chars_rev_decoding(t *testing.T) {
	for code := uint32(0); code < 0x110000; code++ {
		c, ok := CharFromU32(code).Get()
		if !ok {
			continue
		}
		s := CharEncodeUtf8(c)
		if CharsNextBack(s) != rs.Some(c) {
			t.Fatalf("character %x=%c does not decode correctly", c, c)
		}
	}
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

func TestStr_test_char_indices_last(t *testing.T) {
	s := thaiChineseVietShort
	it := CharIndices(s)
	it = it[1:] // it.next()
	eq(t, it[len(it)-1], rs.T2(27, 'm'))
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

func TestStr_test_split_char_iterator_inclusive_rev(t *testing.T) {
	data := maryLamb

	split := reversed(SplitInclusiveChar(data, '\n'))
	eqSlice(t, split, []string{"Little l\u00e4mb\n", "M\u00e4ry h\u00e4d \u00e4 little l\u00e4mb\n", "\n"})

	// The rest of the Rust test runs a predicate that keeps state, from the
	// back of the string (`split_inclusive(..).rev()`). The parts are a slice
	// here, which SplitInclusiveFn builds from the front, calling the
	// predicate front to back; there is no Go form for the other order.
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

func TestStr_test_str_from_utf8(t *testing.T) {
	xs := []byte("hello")
	eq(t, rs.Unwrap(FromUtf8(xs)), "hello")

	xs = []byte(thaiChineseVietShort)
	eq(t, rs.Unwrap(FromUtf8(xs)), thaiChineseVietShort)

	xs = []byte("hello\xFF")
	_, err := FromUtf8(xs)
	isTrue(t, err != nil)
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

func TestStr_test_utf16_count(t *testing.T) {
	eq(t, len(EncodeUtf16("")), 0)
	eq(t, len(EncodeUtf16("a")), 1)
	eq(t, len(EncodeUtf16("\u00e9")), 1)
	eq(t, len(EncodeUtf16("\u5b57")), 1)
	eq(t, len(EncodeUtf16("\U0001F4A9")), 2)
	iter := EncodeUtf16("\U0001F4A9\u5b57\u00e9a")
	eq(t, len(iter), 5)
	iter = iter[1:]
	eq(t, len(iter), 4) // counting half of the surrogate pair
	iter = iter[1:]
	eq(t, len(iter), 3)
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

func TestStr_utf8_char_counts(t *testing.T) {
	strs := []struct {
		tmplStr       string
		tmplCharCount int
	}{{"e", 1}, {"\u00e9", 1}, {"\u20ac", 1}, {"\U00010000", 1}, {"e\u00e9\u20ac\U00010000", 4}}
	spread := 8
	var reps []int
	for _, n := range []int{8, 64, 256, 512, 1024, 1 << 16} {
		for rep := n - spread; rep <= n+spread; rep++ {
			reps = append(reps, rep)
		}
	}
	var padding []string
	for length := 0; length < 8; length++ {
		padding = append(padding, Repeat(" ", length))
	}

	for _, repeat := range reps {
		for _, tmpl := range strs {
			// Rust repeats the template once for each padding; the string
			// is the same each time.
			repeated := Repeat(tmpl.tmplStr, repeat)
			for _, padStart := range padding {
				for _, padEnd := range padding {
					// Create a string with padding...
					withPadding := padStart + repeated + padEnd
					// ...and then skip past that padding. This should ensure
					// that we test several different alignments for both head
					// and tail.
					si := len(padStart)
					ei := len(withPadding) - len(padEnd)
					target := withPadding[si:ei]

					isTrue(t, !StartsWith(target, " ") && !EndsWith(target, " "))
					expectedCount := tmpl.tmplCharCount * repeat
					if got := CharsCount(target); got != expectedCount {
						t.Fatalf("wrong count for `%q.repeat(%d)` (padding: `(%d, %d)`): %d, want %d",
							tmpl.tmplStr, repeat, len(padStart), len(padEnd), got, expectedCount)
					}
				}
			}
		}
	}
}
