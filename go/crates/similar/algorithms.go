// Port of similar 2.7.0 src/algorithms/mod.rs

// Various diff (longest common subsequence) algorithms.
//
// The implementations of the algorithms in this module are relatively low
// level and expose the most generic bounds possible for the algorithm.  To
// use them you would typically use the higher level API if possible but
// direct access to these algorithms can be useful in some cases.
//
// All these algorithms provide a `diff` function which takes two indexable
// objects (for instance slices) and a [`DiffHook`].  As the
// diff is generated the diff hook is invoked.  Note that the diff hook does
// not get access to the actual values but only the indexes.  This is why the
// diff hook is not used outside of the raw algorithm implementations as for
// most situations access to the values is useful of required.
//
// The algorithms module really is the most low-level module in similar and
// generally not the place to start.

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// Creates a diff between old and new with the given algorithm.
//
// Diffs `old`, between indices `old_range` and `new` between indices `new_range`.
func AlgorithmsDiff[T comparable](
	alg Algorithm,
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) error {
	return AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, rs.None[rstime.Instant]())
}

// Creates a diff between old and new with the given algorithm with deadline.
//
// Diffs `old`, between indices `old_range` and `new` between indices `new_range`.
//
// This diff is done with an optional deadline that defines the maximal
// execution time permitted before it bails and falls back to an approximation.
// Note that not all algorithms behave well if they reach the deadline (LCS
// for instance produces a very simplistic diff when the deadline is reached
// in all cases).
func AlgorithmsDiffDeadline[T comparable](
	alg Algorithm,
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) error {
	switch alg {
	case Algorithm_Myers:
		return AlgorithmsMyersDiffDeadline(d, old, oldRange, new_, newRange, deadline)
	case Algorithm_Patience:
		return AlgorithmsPatienceDiffDeadline(d, old, oldRange, new_, newRange, deadline)
	case Algorithm_Lcs:
		return AlgorithmsLcsDiffDeadline(d, old, oldRange, new_, newRange, deadline)
	default:
		panic(rs.Unreachable(alg))
	}
}

// Shortcut for diffing slices with a specific algorithm.
func AlgorithmsDiffSlices[T comparable](alg Algorithm, d AlgorithmsDiffHook, old []T, new_ []T) error {
	return AlgorithmsDiff(alg, d, old, Range{Start: 0, End: len(old)}, new_, Range{Start: 0, End: len(new_)})
}

// Shortcut for diffing slices with a specific algorithm.
func AlgorithmsDiffSlicesDeadline[T comparable](
	alg Algorithm,
	d AlgorithmsDiffHook,
	old []T,
	new_ []T,
	deadline rs.Option[rstime.Instant],
) error {
	return AlgorithmsDiffDeadline(alg, d, old, Range{Start: 0, End: len(old)}, new_, Range{Start: 0, End: len(new_)}, deadline)
}
