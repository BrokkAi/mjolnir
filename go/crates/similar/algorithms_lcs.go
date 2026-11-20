// Port of similar 2.7.0 src/algorithms/lcs.rs

// LCS diff algorithm.
//
// * time: `O((NM)D log (M)D)`
// * space `O(MN)`

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// LCS diff algorithm.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
//
// This diff is done with an optional deadline that defines the maximal
// execution time permitted before it bails and falls back to an very bad
// approximation.  Deadlines with LCS do not make a lot of sense and should
// not be used.
func AlgorithmsLcsDiff[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) error {
	return AlgorithmsLcsDiffDeadline(d, old, oldRange, new_, newRange, rs.None[rstime.Instant]())
}

// LCS diff algorithm.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
//
// This diff is done with an optional deadline that defines the maximal
// execution time permitted before it bails and falls back to an approximation.
func AlgorithmsLcsDiffDeadline[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) error {
	if algorithmsUtilsIsEmptyRange(newRange) {
		if err := d.Delete(oldRange.Start, oldRange.Len(), newRange.Start); err != nil {
			return err
		}
		if err := d.Finish(); err != nil {
			return err
		}
		return nil
	} else if algorithmsUtilsIsEmptyRange(oldRange) {
		if err := d.Insert(oldRange.Start, newRange.Start, newRange.Len()); err != nil {
			return err
		}
		if err := d.Finish(); err != nil {
			return err
		}
		return nil
	}

	commonPrefixLen := algorithmsUtilsCommonPrefixLen(old, oldRange, new_, newRange)
	commonSuffixLen := algorithmsUtilsCommonSuffixLen(
		old,
		Range{Start: oldRange.Start + commonPrefixLen, End: oldRange.End},
		new_,
		Range{Start: newRange.Start + commonPrefixLen, End: newRange.End},
	)

	// If the sequences are not different then we're done
	if commonPrefixLen == oldRange.Len() && (oldRange.Len() == newRange.Len()) {
		if err := d.Equal(0, 0, oldRange.Len()); err != nil {
			return err
		}
		if err := d.Finish(); err != nil {
			return err
		}
		return nil
	}

	maybeTable := algorithmsLcsMakeTable(
		old,
		Range{Start: commonPrefixLen, End: oldRange.Len() - commonSuffixLen},
		new_,
		Range{Start: commonPrefixLen, End: newRange.Len() - commonSuffixLen},
		deadline,
	)
	oldIdx := 0
	newIdx := 0
	newLen := newRange.Len() - commonPrefixLen - commonSuffixLen
	oldLen := oldRange.Len() - commonPrefixLen - commonSuffixLen

	if commonPrefixLen > 0 {
		if err := d.Equal(oldRange.Start, newRange.Start, commonPrefixLen); err != nil {
			return err
		}
	}

	if table, ok := maybeTable.Get(); ok {
		for newIdx < newLen && oldIdx < oldLen {
			oldOrigIdx := oldRange.Start + commonPrefixLen + oldIdx
			newOrigIdx := newRange.Start + commonPrefixLen + newIdx

			if algorithmsUtilsIndex(new_, newOrigIdx) == algorithmsUtilsIndex(old, oldOrigIdx) {
				if err := d.Equal(oldOrigIdx, newOrigIdx, 1); err != nil {
					return err
				}
				oldIdx += 1
				newIdx += 1
			} else if table[rs.T2(newIdx, oldIdx+1)] >= table[rs.T2(newIdx+1, oldIdx)] {
				if err := d.Delete(oldOrigIdx, 1, newOrigIdx); err != nil {
					return err
				}
				oldIdx += 1
			} else {
				if err := d.Insert(oldOrigIdx, newOrigIdx, 1); err != nil {
					return err
				}
				newIdx += 1
			}
		}
	} else {
		oldOrigIdx := oldRange.Start + commonPrefixLen + oldIdx
		newOrigIdx := newRange.Start + commonPrefixLen + newIdx
		if err := d.Delete(oldOrigIdx, oldLen, newOrigIdx); err != nil {
			return err
		}
		if err := d.Insert(oldOrigIdx, newOrigIdx, newLen); err != nil {
			return err
		}
	}

	if oldIdx < oldLen {
		if err := d.Delete(
			oldRange.Start+commonPrefixLen+oldIdx,
			oldLen-oldIdx,
			newRange.Start+commonPrefixLen+newIdx,
		); err != nil {
			return err
		}
		oldIdx += oldLen - oldIdx
	}

	if newIdx < newLen {
		if err := d.Insert(
			oldRange.Start+commonPrefixLen+oldIdx,
			newRange.Start+commonPrefixLen+newIdx,
			newLen-newIdx,
		); err != nil {
			return err
		}
	}

	if commonSuffixLen > 0 {
		if err := d.Equal(
			oldRange.Start+oldLen+commonPrefixLen,
			newRange.Start+newLen+commonPrefixLen,
			commonSuffixLen,
		); err != nil {
			return err
		}
	}

	return d.Finish()
}

// The table is a `BTreeMap<(usize, usize), u32>` in Rust. It is only ever
// read by key, with a missing key standing for zero, so a Go map (whose
// missing keys read as zero) holds it.
func algorithmsLcsMakeTable[T comparable](
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) rs.Option[map[rs.Tuple2[int, int]]uint32] {
	oldLen := oldRange.Len()
	newLen := newRange.Len()
	table := map[rs.Tuple2[int, int]]uint32{}

	for i := newLen - 1; i >= 0; i-- {
		// are we running for too long?  give up on the table
		if deadlineSupportDeadlineExceeded(deadline) {
			return rs.None[map[rs.Tuple2[int, int]]uint32]()
		}

		for j := oldLen - 1; j >= 0; j-- {
			var val uint32
			if algorithmsUtilsIndex(new_, i) == algorithmsUtilsIndex(old, j) {
				val = table[rs.T2(i+1, j+1)] + 1
			} else {
				val = max(table[rs.T2(i+1, j)], table[rs.T2(i, j+1)])
			}
			if val > 0 {
				table[rs.T2(i, j)] = val
			}
		}
	}

	return rs.Some(table)
}
