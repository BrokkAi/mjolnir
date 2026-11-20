// Port of similar 2.7.0 src/algorithms/lcs.rs

package similar

import (
	"maps"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

func TestAlgorithmsLcs_test_table(t *testing.T) {
	table := algorithmsLcsMakeTable([]int{2, 3}, Range{Start: 0, End: 2}, []int{0, 1, 2}, Range{Start: 0, End: 3}, rs.None[rstime.Instant]()).Unwrap()
	expected := map[rs.Tuple2[int, int]]uint32{}
	expected[rs.T2(1, 0)] = 1
	expected[rs.T2(0, 0)] = 1
	expected[rs.T2(2, 0)] = 1
	if !maps.Equal(table, expected) {
		t.Fatalf("assertion `left == right` failed\n  left: %v\n right: %v", table, expected)
	}
}

func TestAlgorithmsLcs_test_diff(t *testing.T) {
	a := []int{0, 1, 2, 3, 4}
	b := []int{0, 1, 2, 9, 4}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__lcs__diff", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsLcs_test_contiguous(t *testing.T) {
	a := []int{0, 1, 2, 3, 4, 4, 4, 5}
	b := []int{0, 1, 2, 8, 9, 4, 4, 7}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__lcs__contiguous", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsLcs_test_pat(t *testing.T) {
	a := []int{0, 1, 3, 4, 5}
	b := []int{0, 1, 4, 5, 8, 9}

	d := AlgorithmsCaptureNew()
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__lcs__pat", debugOps(d.Ops()))
}

func TestAlgorithmsLcs_test_same(t *testing.T) {
	a := []int{0, 1, 2, 3, 4, 4, 4, 5}
	b := []int{0, 1, 2, 3, 4, 4, 4, 5}

	d := AlgorithmsCaptureNew()
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__lcs__same", debugOps(d.Ops()))
}

func TestAlgorithmsLcs_test_finish_called(t *testing.T) {
	d := &hasRunFinish{F0: false}
	slice := []int{1, 2}
	slice2 := []int{1, 2, 3}
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, slice, fullRange(slice), slice2, fullRange(slice2)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	slice = []int{1, 2}
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, slice, fullRange(slice), slice, fullRange(slice)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	empty := []uint8{}
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, empty, fullRange(empty), empty, fullRange(empty)))
	rs.Assert(d.F0)
}

func TestAlgorithmsLcs_test_bad_range_regression(t *testing.T) {
	d := AlgorithmsCaptureNew()
	rs.UnwrapUnit(AlgorithmsLcsDiff(d, []int{0}, Range{Start: 0, End: 1}, []int{0, 0}, Range{Start: 0, End: 2}))
	assertEq(t, debugOps(d.IntoOps()), debugOps([]DiffOp{
		&DiffOp_Equal{
			OldIndex: 0,
			NewIndex: 0,
			Len:      1,
		},
		&DiffOp_Insert{
			OldIndex: 1,
			NewIndex: 1,
			NewLen:   1,
		},
	}))
}
