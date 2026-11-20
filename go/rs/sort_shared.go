// Port of core 1.99.1 src/slice/sort/shared/mod.rs and src/slice/sort/shared/pivot.rs

package rs

// sortFindExistingRun finds a run of sorted elements starting at the beginning of the slice.
//
// Returns the length of the run, and a bool that is false when the run
// is ascending, and true if the run strictly descending.
func sortFindExistingRun[T any](v []T, isLess func(a, b *T) bool) (int, bool) {
	n := len(v)
	if n < 2 {
		return n, false
	}

	runLen := 2
	strictlyDescending := isLess(&v[1], &v[0])
	if strictlyDescending {
		for runLen < n && isLess(&v[runLen], &v[runLen-1]) {
			runLen++
		}
	} else {
		for runLen < n && !isLess(&v[runLen], &v[runLen-1]) {
			runLen++
		}
	}
	return runLen, strictlyDescending
}

// This module contains the logic for pivot selection.

// Recursively select a pseudomedian if above this threshold.
const sortPseudoMedianRecThreshold = 64

// sortChoosePivot selects a pivot from `v`. Algorithm taken from glidesort by Orson Peters.
//
// This chooses a pivot by sampling an adaptive amount of points, approximating
// the quality of a median of sqrt(n) elements.
func sortChoosePivot[T any](v []T, isLess func(a, b *T) bool) int {
	n := len(v)
	if n < 8 {
		sortAbort()
	}

	lenDiv8 := n / 8

	a := 0           // [0, floor(n/8))
	b := lenDiv8 * 4 // [4*floor(n/8), 5*floor(n/8))
	c := lenDiv8 * 7 // [7*floor(n/8), 8*floor(n/8))

	if n < sortPseudoMedianRecThreshold {
		return sortMedian3(v, a, b, c, isLess)
	}
	return sortMedian3Rec(v, a, b, c, lenDiv8, isLess)
}

// sortMedian3Rec calculates an approximate median of 3 elements from sections a, b, c, or
// recursively from an approximation of each, if they're large enough. By
// dividing the size of each section by 8 when recursing we have logarithmic
// recursion depth and overall sample from f(n) = 3*f(n/8) -> f(n) =
// O(n^(log(3)/log(8))) ~= O(n^0.528) elements.
func sortMedian3Rec[T any](v []T, a, b, c, n int, isLess func(a, b *T) bool) int {
	if n*8 >= sortPseudoMedianRecThreshold {
		n8 := n / 8
		a = sortMedian3Rec(v, a, a+n8*4, a+n8*7, n8, isLess)
		b = sortMedian3Rec(v, b, b+n8*4, b+n8*7, n8, isLess)
		c = sortMedian3Rec(v, c, c+n8*4, c+n8*7, n8, isLess)
	}
	return sortMedian3(v, a, b, c, isLess)
}

// sortMedian3 calculates the median of 3 elements.
func sortMedian3[T any](v []T, a, b, c int, isLess func(a, b *T) bool) int {
	// Compiler tends to make this branchless when sensible, and avoids the
	// third comparison when not.
	x := isLess(&v[a], &v[b])
	y := isLess(&v[a], &v[c])
	if x == y {
		// If x=y=0 then b, c <= a. In this case we want to return max(b, c).
		// If x=y=1 then a < b, c. In this case we want to return min(b, c).
		// By toggling the outcome of b < c using XOR x we get this behavior.
		z := isLess(&v[b], &v[c])
		if z != x {
			return c
		}
		return b
	}
	// Either c <= a < b or b <= a < c, thus a is our median.
	return a
}
