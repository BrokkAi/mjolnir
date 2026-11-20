// Port of unicode-segmentation 1.13.3 src/word.rs

// The test module of src/word.rs, then the documentation examples of the
// same file, which Rust runs as tests.

package unicodesegmentation

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestWord_test_syriac_abbr_mark(t *testing.T) {
	_, _, cat := tablesWordWordCategory('\u070F')
	if cat != tablesWordWordCat_WcALetter {
		t.Fatalf("assertion `left == right` failed\n  left: %d\n right: %d (WC_ALetter)", cat, tablesWordWordCat_WcALetter)
	}
}

func TestWord_test_end_of_ayah_cat(t *testing.T) {
	_, _, cat := tablesWordWordCategory('\u06DD')
	if cat != tablesWordWordCat_WcNumeric {
		t.Fatalf("assertion `left == right` failed\n  left: %d\n right: %d (WC_Numeric)", cat, tablesWordWordCat_WcNumeric)
	}
}

func TestWord_test_ascii_word_bound_indices_various_cases(t *testing.T) {
	s := "Hello, world!"
	iter := wordNewAsciiWordBoundIndices(s)
	words := testCollect(iter.Next)
	expected := []rs.Tuple2[int, string]{
		rs.T2(0, "Hello"), // simple letters
		rs.T2(5, ","),
		rs.T2(6, " "),     // space after comma
		rs.T2(7, "world"), // skip comma+space, stop at '!'
		rs.T2(12, "!"),    // punctuation at the end
	}
	testAssertEq(t, words, expected)
}

func TestWord_test_ascii_word_indices_various_cases(t *testing.T) {
	s := "Hello, world! can't e.g. var1 123,456 foo_bar example.com 127.0.0.1:9090"
	iter := wordNewUnicodeWordsAscii(s)
	words := testCollect(iter.Next)
	expected := []string{
		"Hello", // simple letters
		"world", // skip comma+space, stop at '!'
		"can't", // apostrophe joins letters
		"e.g",
		"var1",
		"123,456", // digits+comma+digits
		"foo_bar",
		"example.com",
		"127.0.0.1",
		"9090", // port number
	}
	testAssertEq(t, words, expected)
}

// The two proptests run 10000 cases each (`ProptestConfig::with_cases(10000)`),
// drawn here from a fixed seed so that every run checks the same strings.

// wordAsciiString is the strategy of both proptests: a string of 0 to 99
// characters (`proptest::collection::vec(ascii_char(), 0..100)`), each one of
// NUL (0) to DEL (127) (`ascii_char()`).
func wordAsciiString(r *corpusRand) string {
	chars := make([]byte, r.intn(100))
	for index := range chars {
		chars[index] = byte(r.intn(128))
	}
	return string(chars)
}

// Fast path must equal general path for any ASCII input.
func TestWord_proptest_ascii_matches_unicode_word_indices(t *testing.T) {
	r := &corpusRand{state: 0x70726F7074657374}
	for range 10000 {
		s := wordAsciiString(r)
		fast := wordNewAsciiWordBoundIndices(s)
		uni := wordNewWordBoundIndices(s)

		testAssertEq(t, testCollect(fast.Next), testCollect(uni.Next), "for %+q", s)
	}
}

// Fast path must equal general path for any ASCII input, forwards and backwards.
func TestWord_proptest_ascii_matches_unicode_word_indices_rev(t *testing.T) {
	r := &corpusRand{state: 0x70726F7074657374}
	for range 10000 {
		s := wordAsciiString(r)
		fast := wordNewAsciiWordBoundIndices(s)
		uni := wordNewWordBoundIndices(s)
		testAssertEq(t, testCollect(fast.NextBack), testCollect(uni.NextBack), "for %+q", s)
	}
}

// The documentation examples.

// UWordBoundIndices::as_str
func TestWord_doc_uword_bound_indices_as_str(t *testing.T) {
	iter := SplitWordBoundIndices("Hello world")
	testAssertEq(t, []string{iter.AsStr()}, []string{"Hello world"})
	iter.Next()
	testAssertEq(t, []string{iter.AsStr()}, []string{" world"})
	iter.Next()
	testAssertEq(t, []string{iter.AsStr()}, []string{"world"})
}

// UWordBounds::as_str
func TestWord_doc_uword_bounds_as_str(t *testing.T) {
	iter := SplitWordBounds("Hello world")
	testAssertEq(t, []string{iter.AsStr()}, []string{"Hello world"})
	iter.Next()
	testAssertEq(t, []string{iter.AsStr()}, []string{" world"})
	iter.Next()
	testAssertEq(t, []string{iter.AsStr()}, []string{"world"})
}
