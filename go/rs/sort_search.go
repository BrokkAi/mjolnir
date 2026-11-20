// Port of core 1.99.1 src/slice/mod.rs (binary_search, binary_search_by,
// binary_search_by_key, partition_point, is_sorted, is_sorted_by,
// is_sorted_by_key)

package rs

import "cmp"

// The binary searches return Rust's Result<usize, usize> as (index, found):
// Ok(i) is (i, true) and Err(i) is (i, false). They probe the same elements
// in the same order as Rust's, which decides which of several equal
// elements is found.

// BinarySearch is `v.binary_search(&x)`.
func BinarySearch[T cmp.Ordered](v []T, x T) (int, bool) {
	return BinarySearchBy(v, func(p T) int { return cmp.Compare(p, x) })
}

// BinarySearchBy is `v.binary_search_by(f)`: f says how an element is
// ordered against the one sought, as an Ordering or an int.
func BinarySearchBy[T any, O CompareResult](v []T, f func(T) O) (int, bool) {
	size := len(v)
	if size == 0 {
		return 0, false
	}
	base := 0

	// This loop intentionally doesn't have an early exit if the comparison
	// returns Equal. We want the number of loop iterations to depend *only*
	// on the size of the input slice so that the CPU can reliably predict
	// the loop count.
	for size > 1 {
		half := size / 2
		mid := base + half

		order := f(v[mid])

		if !(order > 0) {
			base = mid
		}

		// This is imprecise in the case where `size` is odd and the
		// comparison returns Greater: the mid element still gets included
		// by `size` even though it's known to be larger than the element
		// being searched for.
		//
		// This is fine though: we gain more performance by keeping the
		// loop iteration count invariant (and thus predictable) than we
		// lose from considering one additional element.
		size -= half
	}

	order := f(v[base])
	if order == 0 {
		return base, true
	}
	if order < 0 {
		return base + 1, false
	}
	return base, false
}

// BinarySearchByKey is `v.binary_search_by_key(&b, f)`. The keys are
// compared with Cmp.
func BinarySearchByKey[T, K any](v []T, b K, f func(T) K) (int, bool) {
	return BinarySearchBy(v, func(k T) int { return Cmp(f(k), b) })
}

// PartitionPoint is `v.partition_point(pred)`.
func PartitionPoint[T any](v []T, pred func(T) bool) int {
	index, _ := BinarySearchBy(v, func(x T) Ordering {
		if pred(x) {
			return Ordering_Less
		}
		return Ordering_Greater
	})
	return index
}

// IsSorted is `v.is_sorted()`: every element is <= the next. A float NaN
// makes it false, as in Rust.
func IsSorted[T cmp.Ordered](v []T) bool {
	for i := 1; i < len(v); i++ {
		if !(v[i-1] <= v[i]) {
			return false
		}
	}
	return true
}

// IsSortedBy is `v.is_sorted_by(compare)`: compare(a, b) holds for every
// element a and the one after it. It stops at the first pair that fails.
func IsSortedBy[T any](v []T, compare func(a, b T) bool) bool {
	for i := 1; i < len(v); i++ {
		if !compare(v[i-1], v[i]) {
			return false
		}
	}
	return true
}

// IsSortedByKey is `v.is_sorted_by_key(f)`: f is called once for each
// element, in order, until a key is less than the one before it.
func IsSortedByKey[T any, K cmp.Ordered](v []T, f func(T) K) bool {
	if len(v) == 0 {
		return true
	}
	last := f(v[0])
	for _, element := range v[1:] {
		current := f(element)
		if !(last <= current) {
			return false
		}
		last = current
	}
	return true
}
