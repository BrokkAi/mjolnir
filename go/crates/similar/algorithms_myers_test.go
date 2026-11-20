// Port of similar 2.7.0 src/algorithms/myers.rs

package similar

import (
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

func TestAlgorithmsMyers_test_find_middle_snake(t *testing.T) {
	a := []byte("ABCABBA")
	b := []byte("CBABAC")
	maxD := algorithmsMyersMaxD(len(a), len(b))
	vf := algorithmsMyersVNew(maxD)
	vb := algorithmsMyersVNew(maxD)
	snake := algorithmsMyersFindMiddleSnake(a, fullRange(a), b, fullRange(b), &vf, &vb, rs.None[rstime.Instant]()).Unwrap()
	xStart, yStart := snake.F0, snake.F1
	assertEq(t, xStart, 4)
	assertEq(t, yStart, 1)
}

func TestAlgorithmsMyers_test_diff(t *testing.T) {
	a := []int{0, 1, 2, 3, 4}
	b := []int{0, 1, 2, 9, 4}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__myers__diff", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsMyers_test_contiguous(t *testing.T) {
	a := []int{0, 1, 2, 3, 4, 4, 4, 5}
	b := []int{0, 1, 2, 8, 9, 4, 4, 7}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__myers__contiguous", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsMyers_test_pat(t *testing.T) {
	a := []int{0, 1, 3, 4, 5}
	b := []int{0, 1, 4, 5, 8, 9}

	d := AlgorithmsCaptureNew()
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, a, fullRange(a), b, fullRange(b)))
	snapshotAssert(t, "similar__algorithms__myers__pat", debugOps(d.Ops()))
}

func TestAlgorithmsMyers_test_deadline_reached(t *testing.T) {
	a := make([]int, 100)
	b := make([]int, 100)
	for i := range 100 {
		a[i] = i
		b[i] = i
	}
	b[10] = 99
	b[50] = 99
	b[25] = 99

	// The Rust test wraps both sequences in an index that sleeps a
	// millisecond on every read and gives the diff 50ms, so the deadline has
	// passed by the time the common prefix and suffix have been read and the
	// search for a middle snake starts. A Go slice cannot be made slow; the
	// deadline here has passed before the diff starts, which leaves the
	// search in the same position.

	// don't give it enough time to do anything interesting
	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsMyersDiffDeadline(
		d,
		a,
		fullRange(a),
		b,
		fullRange(b),
		rs.Some(rstime.InstantNow().Sub(50*time.Millisecond)),
	))
	snapshotAssert(t, "similar__algorithms__myers__deadline_reached", debugOps(d.IntoInner().Ops()))
}

func TestAlgorithmsMyers_test_finish_called(t *testing.T) {
	d := &hasRunFinish{F0: false}
	slice := []int{1, 2}
	slice2 := []int{1, 2, 3}
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, slice, fullRange(slice), slice2, fullRange(slice2)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	slice = []int{1, 2}
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, slice, fullRange(slice), slice, fullRange(slice)))
	rs.Assert(d.F0)

	d = &hasRunFinish{F0: false}
	empty := []uint8{}
	rs.UnwrapUnit(AlgorithmsMyersDiff(d, empty, fullRange(empty), empty, fullRange(empty)))
	rs.Assert(d.F0)
}
