// Port of unicode-segmentation 1.13.3 src/lib.rs

// src/lib.rs has no `#[test]` functions. These are its documentation
// examples, which Rust runs as tests, apart from those of the three sentence
// methods, which are not ported (see doc.go). The helpers at the end are used
// by the tests of the other files too.

package unicodesegmentation

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The example of the crate documentation.
func TestLib_doc_crate(t *testing.T) {
	s := "a\u0310e\u0301o\u0308\u0332\r\n"
	g := testCollect(Graphemes(s, true).Next)
	b := []string{"a\u0310", "e\u0301", "o\u0308\u0332", "\r\n"}
	testAssertEq(t, g, b)

	s = "The quick (\"brown\") fox can't jump 32.3 feet, right?"
	w := testCollect(UnicodeWords(s).Next)
	b = []string{"The", "quick", "brown", "fox", "can't", "jump", "32.3", "feet", "right"}
	testAssertEq(t, w, b)

	s = "The quick (\"brown\")  fox"
	w = testCollect(SplitWordBounds(s).Next)
	b = []string{"The", " ", "quick", " ", "(", "\"", "brown", "\"", ")", "  ", "fox"}
	testAssertEq(t, w, b)
}

// UnicodeSegmentation::graphemes
func TestLib_doc_graphemes(t *testing.T) {
	gr1 := testCollect(UnicodeSegmentationGraphemes("a\u0310e\u0301o\u0308\u0332", true).Next)
	b := []string{"a\u0310", "e\u0301", "o\u0308\u0332"}

	testAssertEq(t, gr1, b)

	gr2 := testCollect(UnicodeSegmentationGraphemes("a\r\nb\U0001F1F7\U0001F1FA\U0001F1F8\U0001F1F9", true).Next)
	b = []string{"a", "\r\n", "b", "\U0001F1F7\U0001F1FA", "\U0001F1F8\U0001F1F9"}

	testAssertEq(t, gr2, b)
}

// UnicodeSegmentation::grapheme_indices
func TestLib_doc_grapheme_indices(t *testing.T) {
	grInds := testCollect(UnicodeSegmentationGraphemeIndices("a\u0310e\u0301o\u0308\u0332\r\n", true).Next)
	b := []rs.Tuple2[int, string]{
		rs.T2(0, "a\u0310"), rs.T2(3, "e\u0301"), rs.T2(6, "o\u0308\u0332"), rs.T2(11, "\r\n"),
	}

	testAssertEq(t, grInds, b)
}

// UnicodeSegmentation::unicode_words
func TestLib_doc_unicode_words(t *testing.T) {
	uws := "The quick (\"brown\") fox can't jump 32.3 feet, right?"
	uw1 := testCollect(UnicodeWords(uws).Next)
	b := []string{"The", "quick", "brown", "fox", "can't", "jump", "32.3", "feet", "right"}

	testAssertEq(t, uw1, b)
}

// UnicodeSegmentation::unicode_word_indices
func TestLib_doc_unicode_word_indices(t *testing.T) {
	uwis := "The quick (\"brown\") fox can't jump 32.3 feet, right?"
	uwi1 := testCollect(UnicodeWordIndices(uwis).Next)
	b := []rs.Tuple2[int, string]{
		rs.T2(0, "The"), rs.T2(4, "quick"), rs.T2(12, "brown"), rs.T2(20, "fox"), rs.T2(24, "can't"),
		rs.T2(30, "jump"), rs.T2(35, "32.3"), rs.T2(40, "feet"), rs.T2(46, "right"),
	}

	testAssertEq(t, uwi1, b)
}

// UnicodeSegmentation::split_word_bounds
func TestLib_doc_split_word_bounds(t *testing.T) {
	swu1 := testCollect(SplitWordBounds("The quick (\"brown\")  fox").Next)
	b := []string{"The", " ", "quick", " ", "(", "\"", "brown", "\"", ")", "  ", "fox"}

	testAssertEq(t, swu1, b)
}

// UnicodeSegmentation::split_word_bound_indices
func TestLib_doc_split_word_bound_indices(t *testing.T) {
	swi1 := testCollect(SplitWordBoundIndices("Brr, it's 29.3\u00B0F!").Next)
	b := []rs.Tuple2[int, string]{
		rs.T2(0, "Brr"), rs.T2(3, ","), rs.T2(4, " "), rs.T2(5, "it's"), rs.T2(9, " "), rs.T2(10, "29.3"),
		rs.T2(14, "\u00B0"), rs.T2(16, "F"), rs.T2(17, "!"),
	}

	testAssertEq(t, swi1, b)
}

// testCollect is `iter.collect::<Vec<_>>()` for an iterator of this package,
// given its Next (or, for `iter.rev().collect()`, its NextBack).
func testCollect[T any](next func() rs.Option[T]) []T {
	items := []T{}
	for {
		item, ok := next().Get()
		if !ok {
			return items
		}
		items = append(items, item)
	}
}

// testReversed is a reversed copy of items: `items.iter().rev()`.
func testReversed[T any](items []T) []T {
	reversed := slices.Clone(items)
	slices.Reverse(reversed)
	return reversed
}

// testAssertEq is `assert_eq!(left, right)` on two vectors, with the message
// of the Rust assertion, when it has one, after them: a format and its
// arguments.
func testAssertEq[T comparable](t *testing.T, left, right []T, message ...any) {
	t.Helper()
	if !slices.Equal(left, right) {
		text := ""
		if len(message) > 0 {
			text = ": " + fmt.Sprintf(message[0].(string), message[1:]...)
		}
		t.Fatalf("assertion `left == right` failed%s\n  left: %s\n right: %s", text, testDebug(left), testDebug(right))
	}
}

// testDebug writes a vector of pieces, or of pieces with their offsets, for
// a failure message, with every character that is not ASCII escaped.
func testDebug[T any](items []T) string {
	parts := make([]string, len(items))
	for index, item := range items {
		switch v := any(item).(type) {
		case string:
			parts[index] = fmt.Sprintf("%+q", v)
		case rs.Tuple2[int, string]:
			parts[index] = fmt.Sprintf("(%d, %+q)", v.F0, v.F1)
		default:
			parts[index] = fmt.Sprintf("%#v", v)
		}
	}
	return "[" + strings.Join(parts, ", ") + "]"
}
