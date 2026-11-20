// Port of similar 2.7.0 src/algorithms/patience.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestAlgorithmsPatience_test_patience(t *testing.T) {
	a := []int{11, 1, 2, 2, 3, 4, 4, 4, 5, 47, 19}
	b := []int{10, 1, 2, 2, 8, 9, 4, 4, 7, 47, 18}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsPatienceDiff(d, a, fullRange(a), b, fullRange(b)))

	snapshotAssert(t, "similar__algorithms__patience__patience", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsPatience_test_patience_out_of_bounds_bug(t *testing.T) {
	// this used to be a bug
	a := []int{1, 2, 3, 4}
	b := []int{1, 2, 3}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsPatienceDiff(d, a, fullRange(a), b, fullRange(b)))

	snapshotAssert(t, "similar__algorithms__patience__patience_out_of_bounds_bug", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsPatience_test_finish_called(t *testing.T) {
	d := &hasRunFinish{F0: false}
	slice := []int{1, 2}
	slice2 := []int{1, 2, 3}
	rs.UnwrapUnit(AlgorithmsPatienceDiff(d, slice, fullRange(slice), slice2, fullRange(slice2)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	slice = []int{1, 2}
	rs.UnwrapUnit(AlgorithmsPatienceDiff(d, slice, fullRange(slice), slice, fullRange(slice)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	empty := []uint8{}
	rs.UnwrapUnit(AlgorithmsPatienceDiff(d, empty, fullRange(empty), empty, fullRange(empty)))
	rs.Assert(d.F0)
}
