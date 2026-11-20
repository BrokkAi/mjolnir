// Port of coretests 1.99.1 tests/ops.rs (the tests of Range and
// RangeInclusive)
//
// test_range and test_range_inclusive step the ranges as iterators; Go
// loops over a range instead, so they check Len and IsEmpty, which is what
// the iteration counts. Left out: the RangeFrom, RangeTo, RangeFull and
// RangeToInclusive tests and test_range_bounds_contains, whose types and
// Bound pairs product code does not use.

package rs_test

import (
	"math"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestOps_test_range(t *testing.T) {
	r := rs.Range[int]{Start: 2, End: 10}
	sliceTestEq(t, r.Len(), 8)
	count := 0
	for ri := r.Start; ri < r.End; ri++ {
		sliceTestTrue(t, r.Contains(ri), "ri >= 2 && ri < 10")
		count++
	}
	sliceTestEq(t, count, 8)
}

func TestOps_test_range_inclusive(t *testing.T) {
	sliceTestEq(t, rs.RangeInclusiveNew[int8](1, 2).Len(), 2)
	sliceTestEq(t, rs.RangeInclusiveNew[int8](127, 127).Len(), 1)
	sliceTestEq(t, rs.RangeInclusiveNew[int8](-128, -128).Len(), 1)

	// degenerate
	r := rs.RangeInclusiveNew(1, -1)
	sliceTestEq(t, r.Len(), 0)
	sliceTestTrue(t, r.IsEmpty(), "(1..=-1).is_empty()")
}

func TestOps_test_range_contains(t *testing.T) {
	r := rs.Range[uint32]{Start: 1, End: 5}
	sliceTestTrue(t, !r.Contains(0), "!(1u32..5).contains(&0u32)")
	sliceTestTrue(t, r.Contains(1), "(1u32..5).contains(&1u32)")
	sliceTestTrue(t, r.Contains(4), "(1u32..5).contains(&4u32)")
	sliceTestTrue(t, !r.Contains(5), "!(1u32..5).contains(&5u32)")
	sliceTestTrue(t, !r.Contains(6), "!(1u32..5).contains(&6u32)")
}

func TestOps_test_range_to_contains(t *testing.T) {
	r := rs.RangeInclusiveNew[uint32](1, 5)
	sliceTestTrue(t, !r.Contains(0), "!(1u32..=5).contains(&0)")
	sliceTestTrue(t, r.Contains(1), "(1u32..=5).contains(&1)")
	sliceTestTrue(t, r.Contains(4), "(1u32..=5).contains(&4)")
	sliceTestTrue(t, r.Contains(5), "(1u32..=5).contains(&5)")
	sliceTestTrue(t, !r.Contains(6), "!(1u32..=5).contains(&6)")
}

func TestOps_test_range_is_empty(t *testing.T) {
	nan := float32(math.NaN())
	inf := float32(math.Inf(1))
	epsilon := math.Nextafter32(1, 2) - 1
	negZero := float32(math.Copysign(0, -1))

	sliceTestTrue(t, !(rs.Range[float64]{Start: 0.0, End: 10.0}).IsEmpty(), "!(0.0..10.0).is_empty()")
	sliceTestTrue(t, (rs.Range[float32]{Start: negZero, End: 0.0}).IsEmpty(), "(-0.0..0.0).is_empty()")
	sliceTestTrue(t, (rs.Range[float64]{Start: 10.0, End: 0.0}).IsEmpty(), "(10.0..0.0).is_empty()")

	sliceTestTrue(t, !(rs.Range[float32]{Start: -inf, End: inf}).IsEmpty(), "!(NEG_INFINITY..INFINITY).is_empty()")
	sliceTestTrue(t, (rs.Range[float32]{Start: epsilon, End: nan}).IsEmpty(), "(EPSILON..NAN).is_empty()")
	sliceTestTrue(t, (rs.Range[float32]{Start: nan, End: epsilon}).IsEmpty(), "(NAN..EPSILON).is_empty()")
	sliceTestTrue(t, (rs.Range[float32]{Start: nan, End: nan}).IsEmpty(), "(NAN..NAN).is_empty()")

	sliceTestTrue(t, !rs.RangeInclusiveNew(0.0, 10.0).IsEmpty(), "!(0.0..=10.0).is_empty()")
	sliceTestTrue(t, !rs.RangeInclusiveNew(negZero, 0.0).IsEmpty(), "!(-0.0..=0.0).is_empty()")
	sliceTestTrue(t, rs.RangeInclusiveNew(10.0, 0.0).IsEmpty(), "(10.0..=0.0).is_empty()")

	sliceTestTrue(t, !rs.RangeInclusiveNew(-inf, inf).IsEmpty(), "!(NEG_INFINITY..=INFINITY).is_empty()")
	sliceTestTrue(t, rs.RangeInclusiveNew(epsilon, nan).IsEmpty(), "(EPSILON..=NAN).is_empty()")
	sliceTestTrue(t, rs.RangeInclusiveNew(nan, epsilon).IsEmpty(), "(NAN..=EPSILON).is_empty()")
	sliceTestTrue(t, rs.RangeInclusiveNew(nan, nan).IsEmpty(), "(NAN..=NAN).is_empty()")
}

// The Debug forms, as rustc 1.99.1 prints `format!("{:?} {:?} {:#?} {:x?}",
// 5..30, 5u8..=30, 1..2, 10..=255)`: "5..30 5..=30 1..2 a..=ff".
func TestOps_range_debug(t *testing.T) {
	got := rs.Format("{:?} {:?} {:#?} {:x?}", rs.Range[int]{Start: 5, End: 30}, rs.RangeInclusiveNew[uint8](5, 30),
		rs.Range[int]{Start: 1, End: 2}, rs.RangeInclusiveNew(10, 255))
	sliceTestEq(t, got, "5..30 5..=30 1..2 a..=ff")
}
