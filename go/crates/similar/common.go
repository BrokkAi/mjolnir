// Port of similar 2.7.0 src/common.rs

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// Creates a diff between old and new with the given algorithm capturing the ops.
//
// This is like [`diff`](crate::algorithms::diff) but instead of using an
// arbitrary hook this will always use [`Compact`] + [`Replace`] + [`Capture`]
// and return the captured [`DiffOp`]s.
func CaptureDiff[T comparable](
	alg Algorithm,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) []DiffOp {
	return CaptureDiffDeadline(alg, old, oldRange, new_, newRange, rs.None[rstime.Instant]())
}

// Creates a diff between old and new with the given algorithm capturing the ops.
//
// Works like [`capture_diff`] but with an optional deadline.
func CaptureDiffDeadline[T comparable](
	alg Algorithm,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) []DiffOp {
	d := AlgorithmsCompactNew(AlgorithmsReplaceNew(AlgorithmsCaptureNew()), old, new_)
	rs.UnwrapUnit(AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, deadline))
	return d.IntoInner().IntoInner().IntoOps()
}

// Creates a diff between old and new with the given algorithm capturing the ops.
func CaptureDiffSlices[T comparable](alg Algorithm, old []T, new_ []T) []DiffOp {
	return CaptureDiffSlicesDeadline(alg, old, new_, rs.None[rstime.Instant]())
}

// Creates a diff between old and new with the given algorithm capturing the ops.
//
// Works like [`capture_diff_slices`] but with an optional deadline.
func CaptureDiffSlicesDeadline[T comparable](
	alg Algorithm,
	old []T,
	new_ []T,
	deadline rs.Option[rstime.Instant],
) []DiffOp {
	return CaptureDiffDeadline(alg, old, Range{Start: 0, End: len(old)}, new_, Range{Start: 0, End: len(new_)}, deadline)
}

// Return a measure of similarity in the range `0..=1`.
//
// A ratio of `1.0` means the two sequences are a complete match, a
// ratio of `0.0` would indicate completely distinct sequences.  The input
// is the sequence of diff operations and the length of the old and new
// sequence.
func GetDiffRatio(ops []DiffOp, oldLen int, newLen int) float32 {
	matches := 0
	for _, op := range ops {
		if op, ok := op.(*DiffOp_Equal); ok {
			matches += op.Len
		}
	}
	len_ := oldLen + newLen
	if len_ == 0 {
		return 1.0
	}
	// The conversions keep each step in single precision, as Rust's f32
	// arithmetic is.
	return float32(float32(2.0*float32(matches)) / float32(len_))
}

// Isolate change clusters by eliminating ranges with no changes.
//
// This will leave holes behind in long periods of equal ranges so that
// you can build things like unified diffs.
//
// Rust takes `ops` by value. The slice passed here is used up the same way:
// its first and last elements may be replaced, so pass a copy to keep it.
func GroupDiffOps(ops []DiffOp, n int) [][]DiffOp {
	if len(ops) == 0 {
		return [][]DiffOp{}
	}

	pendingGroup := []DiffOp{}
	rv := [][]DiffOp{}

	if first, ok := ops[0].(*DiffOp_Equal); ok {
		offset := usizeSaturatingSub(first.Len, n)
		ops[0] = &DiffOp_Equal{
			OldIndex: first.OldIndex + offset,
			NewIndex: first.NewIndex + offset,
			Len:      first.Len - offset,
		}
	}

	if last, ok := ops[len(ops)-1].(*DiffOp_Equal); ok {
		ops[len(ops)-1] = &DiffOp_Equal{
			OldIndex: last.OldIndex,
			NewIndex: last.NewIndex,
			Len:      last.Len - usizeSaturatingSub(last.Len, n),
		}
	}

	for _, op := range ops {
		if equal, ok := op.(*DiffOp_Equal); ok {
			oldIndex, newIndex, len_ := equal.OldIndex, equal.NewIndex, equal.Len
			// End the current group and start a new one whenever
			// there is a large range with no changes.
			if len_ > n*2 {
				pendingGroup = append(pendingGroup, &DiffOp_Equal{
					OldIndex: oldIndex,
					NewIndex: newIndex,
					Len:      n,
				})
				rv = append(rv, pendingGroup)
				offset := usizeSaturatingSub(len_, n)
				pendingGroup = []DiffOp{&DiffOp_Equal{
					OldIndex: oldIndex + offset,
					NewIndex: newIndex + offset,
					Len:      len_ - offset,
				}}
				continue
			}
		}
		pendingGroup = append(pendingGroup, op)
	}

	// match &pending_group[..] { &[] | &[DiffOp::Equal { .. }] => {} _ => rv.push(pending_group) }
	if len(pendingGroup) == 0 {
	} else if len(pendingGroup) == 1 && rs.Is[*DiffOp_Equal](pendingGroup[0]) {
	} else {
		rv = append(rv, pendingGroup)
	}

	return rv
}
