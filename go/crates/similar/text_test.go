// Port of similar 2.7.0 src/text/mod.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestText_test_captured_ops(t *testing.T) {
	diff := TextDiffFromLines(
		"Hello World\nsome stuff here\nsome more stuff here\n",
		"Hello World\nsome amazing stuff here\nsome more stuff here\n",
	)
	snapshotAssert(t, "similar__text__captured_ops", debugOps(diff.Ops()))
}

func TestText_test_captured_word_ops(t *testing.T) {
	diff := TextDiffFromWords(
		"Hello World\nsome stuff here\nsome more stuff here\n",
		"Hello World\nsome amazing stuff here\nsome more stuff here\n",
	)
	changes := []Change[string]{}
	for _, op := range diff.Ops() {
		changes = append(changes, diff.IterChanges(op).Collect()...)
	}
	snapshotAssert(t, "similar__text__captured_word_ops", debugChanges(changes))
}

func TestText_test_unified_diff(t *testing.T) {
	diff := TextDiffFromLines(
		"Hello World\nsome stuff here\nsome more stuff here\n",
		"Hello World\nsome amazing stuff here\nsome more stuff here\n",
	)
	rs.Assert(diff.NewlineTerminated())
	snapshotAssert(t, "similar__text__unified_diff", diff.
		UnifiedDiff().
		ContextRadius(3).
		Header("old", "new").
		String())
}

func TestText_test_line_ops(t *testing.T) {
	a := "Hello World\nsome stuff here\nsome more stuff here\n"
	b := "Hello World\nsome amazing stuff here\nsome more stuff here\n"
	diff := TextDiffFromLines(a, b)
	rs.Assert(diff.NewlineTerminated())
	changes := []Change[string]{}
	for _, op := range diff.Ops() {
		changes = append(changes, diff.IterChanges(op).Collect()...)
	}
	snapshotAssert(t, "similar__text__line_ops", debugChanges(changes))

	// The rest of the Rust test is `#[cfg(feature = "bytes")]`.
}

func TestText_test_virtual_newlines(t *testing.T) {
	diff := TextDiffFromLines("a\nb", "a\nc\n")
	rs.Assert(diff.NewlineTerminated())
	changes := []Change[string]{}
	for _, op := range diff.Ops() {
		changes = append(changes, diff.IterChanges(op).Collect()...)
	}
	snapshotAssert(t, "similar__text__virtual_newlines", debugChanges(changes))
}

func TestText_test_char_diff(t *testing.T) {
	diff := TextDiffFromChars("Hello World", "Hallo Welt")
	snapshotAssert(t, "similar__text__char_diff", debugOps(diff.Ops()))

	// The rest of the Rust test is `#[cfg(feature = "bytes")]`.
}

func TestText_test_ratio(t *testing.T) {
	diff := TextDiffFromChars("abcd", "bcde")
	assertEq(t, diff.Ratio(), 0.75)
	diff = TextDiffFromChars("", "")
	assertEq(t, diff.Ratio(), 1.0)
}

func TestText_test_lifetimes_on_iter(t *testing.T) {
	diffLines := func(old string, new_ string) []Change[string] {
		return TextDiffFromLines(old, new_).IterAllChanges().Collect()
	}

	a := "1\n2\n3\n"
	b := "1\n99\n3\n"
	changes := diffLines(a, b)
	snapshotAssert(t, "similar__text__lifetimes_on_iter", debugChanges(changes))
}

func TestText_test_regression_issue_37(t *testing.T) {
	config := TextDiffConfig{}
	diff := config.DiffLines("\u0018\n\n", "\n\n\r")
	output := diff.UnifiedDiff()
	assertEq(t,
		output.ContextRadius(0).String(),
		"@@ -1 +1,0 @@\n-\u0018\n@@ -2,0 +2,2 @@\n+\n+\r",
	)
}

// The documentation examples of src/text/mod.rs, which Rust runs as tests.

func textDocChanges(diff *TextDiff) []rs.Tuple2[ChangeTag, string] {
	changes := []rs.Tuple2[ChangeTag, string]{}
	for x := range diff.IterAllChanges().All() {
		changes = append(changes, rs.T2(x.Tag(), x.Value()))
	}
	return changes
}

func TestText_doc_diff_lines(t *testing.T) {
	diff := TextDiffConfigure().DiffLines("a\nb\nc", "a\nb\nC")
	assertSliceEq(t, textDocChanges(diff), []rs.Tuple2[ChangeTag, string]{
		rs.T2(ChangeTag_Equal, "a\n"),
		rs.T2(ChangeTag_Equal, "b\n"),
		rs.T2(ChangeTag_Delete, "c"),
		rs.T2(ChangeTag_Insert, "C"),
	})
}

func TestText_doc_diff_words(t *testing.T) {
	diff := TextDiffConfigure().DiffWords("foo bar baz", "foo BAR baz")
	assertSliceEq(t, textDocChanges(diff), []rs.Tuple2[ChangeTag, string]{
		rs.T2(ChangeTag_Equal, "foo"),
		rs.T2(ChangeTag_Equal, " "),
		rs.T2(ChangeTag_Delete, "bar"),
		rs.T2(ChangeTag_Insert, "BAR"),
		rs.T2(ChangeTag_Equal, " "),
		rs.T2(ChangeTag_Equal, "baz"),
	})
}

func TestText_doc_diff_chars(t *testing.T) {
	diff := TextDiffConfigure().DiffChars("abcdef", "abcDDf")
	assertSliceEq(t, textDocChanges(diff), []rs.Tuple2[ChangeTag, string]{
		rs.T2(ChangeTag_Equal, "a"),
		rs.T2(ChangeTag_Equal, "b"),
		rs.T2(ChangeTag_Equal, "c"),
		rs.T2(ChangeTag_Delete, "d"),
		rs.T2(ChangeTag_Delete, "e"),
		rs.T2(ChangeTag_Insert, "D"),
		rs.T2(ChangeTag_Insert, "D"),
		rs.T2(ChangeTag_Equal, "f"),
	})
}

func TestText_doc_diff_slices(t *testing.T) {
	old := []string{"foo", "bar", "baz"}
	new_ := []string{"foo", "BAR", "baz"}
	diff := TextDiffConfigure().DiffSlices(old, new_)
	assertSliceEq(t, textDocChanges(diff), []rs.Tuple2[ChangeTag, string]{
		rs.T2(ChangeTag_Equal, "foo"),
		rs.T2(ChangeTag_Delete, "bar"),
		rs.T2(ChangeTag_Insert, "BAR"),
		rs.T2(ChangeTag_Equal, "baz"),
	})
}

// The example at the top of src/lib.rs.
func TestText_doc_lib(t *testing.T) {
	diff := TextDiffFromLines(
		"Hello World\nThis is the second line.\nThis is the third.",
		"Hallo Welt\nThis is the second line.\nThis is life.\nMoar and more",
	)

	printed := ""
	for change := range diff.IterAllChanges().All() {
		var sign string
		switch change.Tag() {
		case ChangeTag_Delete:
			sign = "-"
		case ChangeTag_Insert:
			sign = "+"
		case ChangeTag_Equal:
			sign = " "
		}
		printed += sign + change.String()
	}
	assertEq(t, printed, "-Hello World\n+Hallo Welt\n This is the second line.\n-This is the third.\n+This is life.\n+Moar and more\n")
}
