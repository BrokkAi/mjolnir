// Port of similar 2.7.0 src/udiff.rs

package similar

import (
	"testing"
)

func TestUdiff_test_unified_diff(t *testing.T) {
	diff := TextDiffFromLines(
		"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\nz\nA\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nL\nM\nN\nO\nP\nQ\nR\nS\nT\nU\nV\nW\nX\nY\nZ",
		"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\nS\nt\nu\nv\nw\nx\ny\nz\nA\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nL\nM\nN\no\nP\nQ\nR\nS\nT\nU\nV\nW\nX\nY\nZ",
	)
	snapshotAssert(t, "similar__udiff__unified_diff", diff.UnifiedDiff().Header("a.txt", "b.txt").String())
}

func TestUdiff_test_empty_unified_diff(t *testing.T) {
	diff := TextDiffFromLines("abc", "abc")
	assertEq(t, diff.UnifiedDiff().Header("a.txt", "b.txt").String(), "")
}

func TestUdiff_test_unified_diff_newline_hint(t *testing.T) {
	diff := TextDiffFromLines("a\n", "b")
	snapshotAssert(t, "similar__udiff__unified_diff_newline_hint", diff.UnifiedDiff().Header("a.txt", "b.txt").String())
	snapshotAssert(t, "similar__udiff__unified_diff_newline_hint-2", diff.
		UnifiedDiff().
		MissingNewlineHint(false).
		Header("a.txt", "b.txt").
		String())
}
