// Port of similar 2.7.0 src/algorithms/myers.rs

// Myers' diff algorithm.
//
// * time: `O((N+M)D)`
// * space `O(N+M)`
//
// See [the original article by Eugene W. Myers](http://www.xmailserver.org/diff2.pdf)
// describing it.
//
// The implementation of this algorithm is based on the implementation by
// Brandon Williams.
//
// # Heuristics
//
// At present this implementation of Myers' does not implement any more advanced
// heuristics that would solve some pathological cases.  For instance passing two
// large and completely distinct sequences to the algorithm will make it spin
// without making reasonable progress.  Currently the only protection in the
// library against this is to pass a deadline to the diffing algorithm.
//
// For potential improvements here see [similar#15](https://github.com/mitsuhiko/similar/issues/15).

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// Myers' diff algorithm.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
func AlgorithmsMyersDiff[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) error {
	return AlgorithmsMyersDiffDeadline(d, old, oldRange, new_, newRange, rs.None[rstime.Instant]())
}

// Myers' diff algorithm with deadline.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
//
// This diff is done with an optional deadline that defines the maximal
// execution time permitted before it bails and falls back to an approximation.
func AlgorithmsMyersDiffDeadline[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) error {
	maxD := algorithmsMyersMaxD(oldRange.Len(), newRange.Len())
	vb := algorithmsMyersVNew(maxD)
	vf := algorithmsMyersVNew(maxD)
	if err := algorithmsMyersConquer(
		d, old, oldRange, new_, newRange, &vf, &vb, deadline,
	); err != nil {
		return err
	}
	return d.Finish()
}

// A D-path is a path which starts at (0,0) that has exactly D non-diagonal
// edges. All D-paths consist of a (D - 1)-path followed by a non-diagonal edge
// and then a possibly empty sequence of diagonal edges called a snake.

// `V` contains the endpoints of the furthest reaching `D-paths`. For each
// recorded endpoint `(x,y)` in diagonal `k`, we only need to retain `x` because
// `y` can be computed from `x - k`. In other words, `V` is an array of integers
// where `V[k]` contains the row index of the endpoint of the furthest reaching
// path in diagonal `k`.
//
// We can't use a traditional Vec to represent `V` since we use `k` as an index
// and it can take on negative values. So instead `V` is represented as a
// light-weight wrapper around a Vec plus an `offset` which is the maximum value
// `k` can take on in order to map negative `k`'s back to a value >= 0.
type algorithmsMyersV struct {
	offset int
	v      []int // Look into initializing this to -1 and storing isize
}

func algorithmsMyersVNew(maxD int) algorithmsMyersV {
	return algorithmsMyersV{
		offset: maxD,
		v:      make([]int, 2*maxD),
	}
}

func (self *algorithmsMyersV) len() int {
	return len(self.v)
}

// index is `impl Index<isize> for V`.
func (self *algorithmsMyersV) index(index int) int {
	return self.v[index+self.offset]
}

// indexMut is `impl IndexMut<isize> for V`.
func (self *algorithmsMyersV) indexMut(index int) *int {
	return &self.v[index+self.offset]
}

func algorithmsMyersMaxD(len1 int, len2 int) int {
	// XXX look into reducing the need to have the additional '+ 1'
	return (len1+len2+1)/2 + 1
}

func algorithmsMyersSplitAt(range_ Range, at int) (Range, Range) {
	return Range{Start: range_.Start, End: at}, Range{Start: at, End: range_.End}
}

// A `Snake` is a sequence of diagonal edges in the edit graph.  Normally
// a snake has a start end end point (and it is possible for a snake to have
// a length of zero, meaning the start and end points are the same) however
// we do not need the end point which is why it's not implemented here.
//
// The divide part of a divide-and-conquer strategy. A D-path has D+1 snakes
// some of which may be empty. The divide step requires finding the ceil(D/2) +
// 1 or middle snake of an optimal D-path. The idea for doing so is to
// simultaneously run the basic algorithm in both the forward and reverse
// directions until furthest reaching forward and reverse paths starting at
// opposing corners 'overlap'.
func algorithmsMyersFindMiddleSnake[T comparable](
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	vf *algorithmsMyersV,
	vb *algorithmsMyersV,
	deadline rs.Option[rstime.Instant],
) rs.Option[rs.Tuple2[int, int]] {
	n := oldRange.Len()
	m := newRange.Len()

	// By Lemma 1 in the paper, the optimal edit script length is odd or even as
	// `delta` is odd or even.
	delta := n - m
	odd := delta&1 == 1

	// The initial point at (0, -1)
	*vf.indexMut(1) = 0
	// The initial point at (N, M+1)
	*vb.indexMut(1) = 0

	// We only need to explore ceil(D/2) + 1
	dMax := algorithmsMyersMaxD(n, m)
	if !(vf.len() >= dMax) {
		rs.PanicAt(0, "assertion failed: vf.len() >= d_max")
	}
	if !(vb.len() >= dMax) {
		rs.PanicAt(0, "assertion failed: vb.len() >= d_max")
	}

	// A diff that runs out of time gives a coarser answer, so the speed of
	// the loop below, where a diff of unlike inputs spends its time, is part
	// of matching Rust. Two things in it are written out by hand for that
	// reason, and neither changes what it computes:
	//
	//   - `vf[k]` and `vb[k]` are V's Index impl, the element at
	//     `k + offset`, read through locals.
	//   - When both ranges lie inside their slices, which is always unless
	//     the caller passed a range past the end, the common prefix and
	//     suffix are counted in place over the two windows `oldW` and `newW`
	//     instead of through common_prefix_len and common_suffix_len.
	fv, fo := vf.v, vf.offset
	bv, bo := vb.v, vb.offset
	inBounds := algorithmsUtilsInBounds(old, oldRange) && algorithmsUtilsInBounds(new_, newRange)
	var oldW, newW []T
	if inBounds {
		oldW = old[oldRange.Start:oldRange.End]
		newW = new_[newRange.Start:newRange.End]
	}

	for d := 0; d < dMax; d++ {
		// are we running for too long?
		if deadlineSupportDeadlineExceeded(deadline) {
			break
		}

		// Forward path
		for k := d; k >= -d; k -= 2 {
			var x int
			if k == -d || (k != d && fv[k-1+fo] < fv[k+1+fo]) {
				x = fv[k+1+fo]
			} else {
				x = fv[k-1+fo] + 1
			}
			y := x - k

			// The coordinate of the start of a snake
			x0, y0 := x, y
			//  While these sequences are identical, keep moving through the
			//  graph with no cost
			if x < oldRange.Len() && y < newRange.Len() {
				var advance int
				if inBounds && y >= 0 {
					a, b := newW[y:], oldW[x:]
					if len(b) < len(a) {
						a = a[:len(b)]
					}
					for advance < len(a) && a[advance] == b[advance] {
						advance++
					}
				} else {
					advance = algorithmsUtilsCommonPrefixLen(
						old,
						Range{Start: oldRange.Start + x, End: oldRange.End},
						new_,
						Range{Start: newRange.Start + y, End: newRange.End},
					)
				}
				x += advance
			}

			// This is the new best x value
			fv[k+fo] = x

			// Only check for connections from the forward search when N - M is
			// odd and when there is a reciprocal k line coming from the other
			// direction.
			if odd && algorithmsMyersAbs(k-delta) <= (d-1) {
				// TODO optimize this so we don't have to compare against n
				if fv[k+fo]+bv[-(k-delta)+bo] >= n {
					// Return the snake
					return rs.Some(rs.T2(x0+oldRange.Start, y0+newRange.Start))
				}
			}
		}

		// Backward path
		for k := d; k >= -d; k -= 2 {
			var x int
			if k == -d || (k != d && bv[k-1+bo] < bv[k+1+bo]) {
				x = bv[k+1+bo]
			} else {
				x = bv[k-1+bo] + 1
			}
			y := x - k

			// The coordinate of the start of a snake
			if x < n && y < m {
				var advance int
				if inBounds && y >= 0 {
					a, b := newW[:m-y], oldW[:n-x]
					for advance < len(a) && advance < len(b) && a[len(a)-1-advance] == b[len(b)-1-advance] {
						advance++
					}
				} else {
					advance = algorithmsUtilsCommonSuffixLen(
						old,
						Range{Start: oldRange.Start, End: oldRange.Start + n - x},
						new_,
						Range{Start: newRange.Start, End: newRange.Start + m - y},
					)
				}
				x += advance
				y += advance
			}

			// This is the new best x value
			bv[k+bo] = x

			if !odd && algorithmsMyersAbs(k-delta) <= d {
				// TODO optimize this so we don't have to compare against n
				if bv[k+bo]+fv[-(k-delta)+fo] >= n {
					// Return the snake
					return rs.Some(rs.T2(n-x+oldRange.Start, m-y+newRange.Start))
				}
			}
		}

		// TODO: Maybe there's an opportunity to optimize and bail early?
	}

	// deadline reached
	return rs.None[rs.Tuple2[int, int]]()
}

// algorithmsMyersAbs is `isize::abs`.
func algorithmsMyersAbs(value int) int {
	if value < 0 {
		return -value
	}
	return value
}

func algorithmsMyersConquer[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	vf *algorithmsMyersV,
	vb *algorithmsMyersV,
	deadline rs.Option[rstime.Instant],
) error {
	// Check for common prefix
	commonPrefixLen := algorithmsUtilsCommonPrefixLen(old, oldRange, new_, newRange)
	if commonPrefixLen > 0 {
		if err := d.Equal(oldRange.Start, newRange.Start, commonPrefixLen); err != nil {
			return err
		}
	}
	oldRange.Start += commonPrefixLen
	newRange.Start += commonPrefixLen

	// Check for common suffix
	commonSuffixLen := algorithmsUtilsCommonSuffixLen(old, oldRange, new_, newRange)
	commonSuffix := rs.T2(
		oldRange.End-commonSuffixLen,
		newRange.End-commonSuffixLen,
	)
	oldRange.End -= commonSuffixLen
	newRange.End -= commonSuffixLen

	if algorithmsUtilsIsEmptyRange(oldRange) && algorithmsUtilsIsEmptyRange(newRange) {
		// Do nothing
	} else if algorithmsUtilsIsEmptyRange(newRange) {
		if err := d.Delete(oldRange.Start, oldRange.Len(), newRange.Start); err != nil {
			return err
		}
	} else if algorithmsUtilsIsEmptyRange(oldRange) {
		if err := d.Insert(oldRange.Start, newRange.Start, newRange.Len()); err != nil {
			return err
		}
	} else if snake, ok := algorithmsMyersFindMiddleSnake(
		old,
		oldRange,
		new_,
		newRange,
		vf,
		vb,
		deadline,
	).Get(); ok {
		xStart, yStart := snake.F0, snake.F1
		oldA, oldB := algorithmsMyersSplitAt(oldRange, xStart)
		newA, newB := algorithmsMyersSplitAt(newRange, yStart)
		if err := algorithmsMyersConquer(d, old, oldA, new_, newA, vf, vb, deadline); err != nil {
			return err
		}
		if err := algorithmsMyersConquer(d, old, oldB, new_, newB, vf, vb, deadline); err != nil {
			return err
		}
	} else {
		if err := d.Delete(
			oldRange.Start,
			oldRange.End-oldRange.Start,
			newRange.Start,
		); err != nil {
			return err
		}
		if err := d.Insert(
			oldRange.Start,
			newRange.Start,
			newRange.End-newRange.Start,
		); err != nil {
			return err
		}
	}

	if commonSuffixLen > 0 {
		if err := d.Equal(commonSuffix.F0, commonSuffix.F1, commonSuffixLen); err != nil {
			return err
		}
	}

	return nil
}
