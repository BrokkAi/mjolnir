// Port of vt100 0.16.2 tests/split-escapes.rs

package vt100_test

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// splitEscapesGetFileContents is `get_file_contents`. The one file the tests
// read, tests/data/weechat.typescript, is in testdata/vt100-rust/ (see ORIGIN
// there).
func splitEscapesGetFileContents(t testing.TB, name string) []uint8 {
	t.Helper()
	if name != "tests/data/weechat.typescript" {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: no test data %s", name)
	}
	buf, err := vt100.TestdataWeechat()
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %v", err)
	}
	return slices.Clone(buf)
}

func splitEscapesWriteToParser(chunks [][]uint8) (string, []uint8) {
	parser := vt100.ParserNew(37, 193, 0)
	for _, chunk := range chunks {
		parser.Process(chunk)
	}
	return parser.Screen().Contents(),
		parser.Screen().ContentsFormatted()
}

func splitEscapesTestSplits(t testing.TB, filename string, limit rs.Option[int]) {
	t.Helper()
	bytes_ := splitEscapesGetFileContents(t, filename)
	len_ := len(bytes_)
	expectedContents, expectedFormatted := splitEscapesWriteToParser([][]uint8{slices.Clone(bytes_)})
	for i := 0; i < len_-1; i++ {
		if limit, ok := limit.Get(); ok {
			if i > limit {
				break
			}
		}
		bytesCopy := slices.Clone(bytes_)
		start, end := bytesCopy[:i], bytesCopy[i:]
		chunks := [][]uint8{slices.Clone(start), slices.Clone(end)}
		gotContents, gotFormatted := splitEscapesWriteToParser(chunks)
		if !(gotContents == expectedContents && slices.Equal(gotFormatted, expectedFormatted)) {
			t.Fatalf("failed to render %s when split at byte %d", filename, i)
		}
	}
}

func TestItSplitEscapes_split_escapes_weechat(t *testing.T) {
	splitEscapesTestSplits(t, "tests/data/weechat.typescript", rs.Some(500))
}

func TestItSplitEscapes_split_escapes_weechat_full(t *testing.T) {
	ignored(t)
	splitEscapesTestSplits(t, "tests/data/weechat.typescript", rs.None[int]())
}
