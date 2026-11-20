// Port of core 1.99.1 src/slice/sort/unstable/heapsort.rs

package rs

// This module contains a branchless heapsort as fallback for unstable quicksort.

// sortHeapsort sorts `v` using heapsort, which guarantees *O*(*n* \* log(*n*)) worst-case.
func sortHeapsort[T any](v []T, isLess func(a, b *T) bool) {
	n := len(v)

	for i := n + n/2 - 1; i >= 0; i-- {
		var siftIdx int
		if i >= n {
			siftIdx = i - n
		} else {
			v[0], v[i] = v[i], v[0]
			siftIdx = 0
		}

		// The above calculation ensures that `sift_idx` is either 0 or
		// `(len..(len + (len / 2))) - len`, which simplifies to `0..(len / 2)`.
		sortSiftDown(v[:min(i, n)], siftIdx, isLess)
	}
}

// sortSiftDown restores the heap below node. This binary heap respects the
// invariant `parent >= child`.
func sortSiftDown[T any](v []T, node int, isLess func(a, b *T) bool) {
	n := len(v)

	for {
		// Children of `node`.
		child := 2*node + 1
		if child >= n {
			break
		}

		// Choose the greater child.
		if child+1 < n {
			// We need a branch to be sure not to out-of-bounds index,
			// but it's highly predictable.  The comparison, however,
			// is better done branchless, especially for primitives.
			if isLess(&v[child], &v[child+1]) {
				child++
			}
		}

		// Stop if the invariant holds at `node`.
		if !isLess(&v[node], &v[child]) {
			break
		}

		v[node], v[child] = v[child], v[node]

		node = child
	}
}
