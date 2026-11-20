// Port of core 1.99.1 src/slice/sort/unstable/quicksort.rs

package rs

// This module contains an unstable quicksort and two partition implementations.

// sortQuicksort sorts `v` recursively.
//
// If the slice had a predecessor in the original array, it is specified as `ancestorPivot`.
//
// `limit` is the number of allowed imbalanced partitions before switching to `heapsort`. If zero,
// this function will immediately switch to heapsort.
func sortQuicksort[T any](v []T, ancestorPivot *T, limit uint32, layout SortLayout, isLess func(a, b *T) bool) {
	for {
		if len(v) <= layout.smallSortThreshold() {
			sortSmallSortFor(layout, v, isLess)
			return
		}

		// If too many bad pivot choices were made, simply fall back to heapsort in order to
		// guarantee `O(N x log(N))` worst-case.
		if limit == 0 {
			sortHeapsort(v, isLess)
			return
		}

		limit--

		// Choose a pivot and try guessing whether the slice is already sorted.
		pivotPos := sortChoosePivot(v, isLess)

		// If the chosen pivot is equal to the predecessor, then it's the smallest element in the
		// slice. Partition the slice into elements equal to and elements greater than the pivot.
		// This case is usually hit when the slice contains many duplicate elements.
		if p := ancestorPivot; p != nil {
			if !isLess(p, &v[pivotPos]) {
				numLt := sortPartition(v, pivotPos, layout, func(a, b *T) bool { return !isLess(b, a) })

				// Continue sorting elements greater than the pivot. We know that `num_lt` contains
				// the pivot. So we can continue after `num_lt`.
				v = v[numLt+1:]
				ancestorPivot = nil
				continue
			}
		}

		// Partition the slice.
		numLt := sortPartition(v, pivotPos, layout, isLess)

		// Split the slice into `left`, `pivot`, and `right`.
		left, right := v[:numLt], v[numLt:]
		pivot := &right[0]
		right = right[1:]

		// Recurse into the left side. We have a fixed recursion limit, testing shows no real
		// benefit for recursing into the shorter side.
		sortQuicksort(left, ancestorPivot, limit, layout, isLess)

		// Continue with the right side.
		v = right
		ancestorPivot = pivot
	}
}

// sortPartition takes the input slice `v` and re-arranges elements such that when the call
// returns normally all elements that compare true for `is_less(elem, pivot)` where `pivot ==
// v[pivot_pos]` are on the left side of `v` followed by the other elements, notionally considered
// greater or equal to `pivot`.
//
// Returns the number of elements that are compared true for `is_less(elem, pivot)`.
//
// If `is_less` does not implement a total order the resulting order and return value are
// unspecified. All original elements will remain in `v` and any possible modifications via
// interior mutability will be observable. Same is true if `is_less` panics or `v.len()`
// exceeds `scratch.len()`.
func sortPartition[T any](v []T, pivot int, layout SortLayout, isLess func(a, b *T) bool) int {
	n := len(v)

	// Allows for panic-free code-gen by proving this property to the compiler.
	if n == 0 {
		return 0
	}

	if pivot >= n {
		sortAbort()
	}

	// Place the pivot at the beginning of slice.
	v[0], v[pivot] = v[pivot], v[0]
	pivotRef, vWithoutPivot := &v[0], v[1:]

	// The partition is chosen by the element type's size, as
	// `inst_partition` chooses it.
	const maxBranchlessPartitionSize = 96
	var numLt int
	if layout.Size <= maxBranchlessPartitionSize {
		// Specialize for types that are relatively cheap to copy, where branchless optimizations
		// have large leverage e.g. `u64` and `String`.
		numLt = sortPartitionLomutoBranchlessCyclic(vWithoutPivot, pivotRef, isLess)
	} else {
		numLt = sortPartitionHoareBranchyCyclic(vWithoutPivot, pivotRef, isLess)
	}

	if numLt >= n {
		sortAbort()
	}

	// Place the pivot between the two partitions.
	v[0], v[numLt] = v[numLt], v[0]

	return numLt
}

// sortPartitionHoareBranchyCyclic: see sortPartition.
//
// Optimized for large types that are expensive to move. The pair of
// out-of-order elements is swapped through a cyclic permutation that keeps
// one element aside (the gap); should is_less panic, the deferred write puts
// it back, as Rust's GapGuard does, so v stays a permutation of its input.
func sortPartitionHoareBranchyCyclic[T any](v []T, pivot *T, isLess func(a, b *T) bool) int {
	n := len(v)

	if n == 0 {
		return 0
	}

	var gapValue T
	gapPos := -1
	defer func() {
		// `gap_opt` goes out of scope and overwrites the last wrong-side element on the right side
		// with the first wrong-side element of the left side that was initially overwritten by the
		// first wrong-side element on the right side element.
		if gapPos >= 0 {
			v[gapPos] = gapValue
		}
	}()

	left := 0
	right := n

	for {
		// Find the first element greater than the pivot.
		for left < right && isLess(&v[left], pivot) {
			left++
		}

		// Find the last element equal to the pivot.
		for {
			right--
			if left >= right || isLess(&v[right], pivot) {
				break
			}
		}

		if left >= right {
			break
		}

		// Swap the found pair of out-of-order elements via cyclic permutation.
		isFirstSwapPair := gapPos < 0

		if isFirstSwapPair {
			gapValue = v[left]
			gapPos = right
		}

		if !isFirstSwapPair {
			v[gapPos] = v[left]
		}
		gapPos = right
		v[left] = v[right]

		left++
	}

	return left
}

// sortPartitionLomutoBranchlessCyclic: see sortPartition.
//
// Novel partition implementation by Lukas Bergdoll and Orson Peters. Branchless Lomuto
// partition paired with a cyclic permutation.
// https://github.com/Voultapher/sort-research-rs/blob/main/writeup/lomcyc_partition/text.md
//
// The first element is kept aside (the gap value) while every other element
// moves; it is compared last. Should is_less panic, the deferred write puts
// it back where the gap is, as Rust's GapGuardRaw does. Rust unrolls the
// loop by two for elements of up to 16 bytes, which does not change the
// order of the steps.
func sortPartitionLomutoBranchlessCyclic[T any](v []T, pivot *T, isLess func(a, b *T) bool) int {
	n := len(v)

	if n == 0 {
		return 0
	}

	gapValue := v[0]
	gapPos := 0
	numLt := 0
	done := false
	defer func() {
		if !done {
			v[gapPos] = gapValue
		}
	}()

	loopBody := func(right *T, rightPos int) {
		rightIsLt := isLess(right, pivot)
		left := numLt

		v[gapPos] = v[left]
		v[left] = *right

		gapPos = rightPos
		if rightIsLt {
			numLt++
		}
	}

	for right := 1; right < n; right++ {
		loopBody(&v[right], right)
	}

	// The last step takes the gap value as the right element; after it no
	// gap is left to fill.
	rightIsLt := isLess(&gapValue, pivot)
	left := numLt
	v[gapPos] = v[left]
	v[left] = gapValue
	done = true
	if rightIsLt {
		numLt++
	}

	return numLt
}
