// Port of alloc 1.99.1 src/slice.rs (sort, sort_by, sort_by_key,
// sort_by_cached_key)

package rs

import (
	"cmp"
	"slices"
)

// The stable sorts leave equal elements in their original order, so every
// correct stable sort leaves the same order and these use Go's. Rust's
// driftsort calls the comparison in a different sequence and may panic with
// "user-provided comparison function does not correctly implement a total
// order" for a comparison that is not one; neither is visible to code whose
// comparison is a total order.

// SortStable is `v.sort()`.
func SortStable[T cmp.Ordered](v []T) {
	slices.SortStableFunc(v, cmp.Compare[T])
}

// SortStableBy is `v.sort_by(compare)`. compare returns an Ordering or an
// int.
func SortStableBy[T any, O CompareResult](v []T, compare func(a, b T) O) {
	slices.SortStableFunc(v, func(a, b T) int { return int(compare(a, b)) })
}

// SortStableByKey is `v.sort_by_key(key)`. The keys are compared with Cmp.
func SortStableByKey[T, K any](v []T, key func(T) K) {
	slices.SortStableFunc(v, func(a, b T) int { return Cmp(key(a), key(b)) })
}

// SortStableByCachedKey is `v.sort_by_cached_key(key)`: key is called once
// for each element, in order, and the keys are compared with Cmp.
func SortStableByCachedKey[T, K any](v []T, key func(T) K) {
	n := len(v)
	if n < 2 {
		return
	}

	keys := make([]K, n)
	for i := range v {
		keys[i] = key(v[i])
	}
	// Rust sorts (key, index) pairs, which are unique, so the result is the
	// stable order by key.
	indices := make([]int, n)
	for i := range indices {
		indices[i] = i
	}
	slices.SortStableFunc(indices, func(a, b int) int { return Cmp(keys[a], keys[b]) })
	sorted := make([]T, n)
	for i, index := range indices {
		sorted[i] = v[index]
	}
	copy(v, sorted)
}
