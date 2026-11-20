// Port of similar 2.7.0 src/text/abstraction.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestTextAbstraction_test_split_lines(t *testing.T) {
	assertSliceEq(t,
		DiffableStrTokenizeLines("first\nsecond\rthird\r\nfourth\nlast"),
		[]string{"first\n", "second\r", "third\r\n", "fourth\n", "last"},
	)
	assertSliceEq(t, DiffableStrTokenizeLines("\n\n"), []string{"\n", "\n"})
	assertSliceEq(t, DiffableStrTokenizeLines("\n"), []string{"\n"})
	rs.Assert(len(DiffableStrTokenizeLines("")) == 0)
}

func TestTextAbstraction_test_split_words(t *testing.T) {
	assertSliceEq(t,
		DiffableStrTokenizeWords("foo    bar baz\n\n  aha"),
		[]string{"foo", "    ", "bar", " ", "baz", "\n\n  ", "aha"},
	)
}

func TestTextAbstraction_test_split_chars(t *testing.T) {
	assertSliceEq(t,
		DiffableStrTokenizeChars("abcfö❄\ufe0f"),
		[]string{"a", "b", "c", "f", "ö", "❄", "\ufe0f"},
	)
}
