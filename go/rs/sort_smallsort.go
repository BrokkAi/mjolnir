// Port of core 1.99.1 src/slice/sort/shared/smallsort.rs (the parts the
// unstable sort uses)

package rs

// This module contains a variety of sort implementations that are optimized for small lengths.
//
// Rust picks the small-sort routine by specialization on the element type
// (UnstableSmallSortTypeImpl): the sorting network for Copy types of at most
// 8 bytes, the general small-sort for other types without interior
// mutability that fit the stack scratch buffer, and insertion sort for the
// rest. smallSortThreshold and smallSort make the same choice from a
// SortLayout.

// Optimal number of comparisons, and good perf.
const sortSmallSortFallbackThreshold = 16

// From a comparison perspective 20 was ~2% more efficient for fully random input, but for
// wall-clock performance choosing 32 yielded better performance overall.
const sortSmallSortGeneralThreshold = 32

// [`small_sort_general`] uses [`sort8_stable`] as primitive and does a kind of ping-pong merge,
// where the output of the first two [`sort8_stable`] calls is stored at the end of the scratch
// buffer. This simplifies panic handling and avoids additional copies. This affects the required
// scratch buffer size.
const sortSmallSortGeneralScratchLen = sortSmallSortGeneralThreshold + 16

const sortSmallSortNetworkThreshold = 32
const sortSmallSortNetworkScratchLen = sortSmallSortNetworkThreshold

// Using a stack array, could cause a stack overflow if the type `T` is very large. To be
// conservative we limit the usage of small-sorts that require a stack array to types that fit
// within this limit.
const sortMaxStackArraySize = 4096

type sortSmallSortKind uint8

const (
	sortSmallSortFallback sortSmallSortKind = iota
	sortSmallSortGeneral
	sortSmallSortNetwork
)

// smallSortKind is the specialization of UnstableSmallSortTypeImpl and
// UnstableSmallSortFreezeTypeImpl for the element type.
func (l SortLayout) smallSortKind() sortSmallSortKind {
	if !l.Freeze {
		return sortSmallSortFallback
	}
	if l.Copy && sortHasEfficientInPlaceSwap(l) && l.Size*sortSmallSortNetworkScratchLen <= sortMaxStackArraySize {
		return sortSmallSortNetwork
	}
	if l.Size*sortSmallSortGeneralScratchLen <= sortMaxStackArraySize {
		return sortSmallSortGeneral
	}
	return sortSmallSortFallback
}

// smallSortThreshold is `T::small_sort_threshold()`: for which input length
// <= return value of this function, is it valid to call `small_sort`.
func (l SortLayout) smallSortThreshold() int {
	switch l.smallSortKind() {
	case sortSmallSortNetwork:
		return sortSmallSortNetworkThreshold
	case sortSmallSortGeneral:
		return sortSmallSortGeneralThreshold
	}
	return sortSmallSortFallbackThreshold
}

// sortSmallSortFor is `T::small_sort(v, is_less)`: sorts `v` using strategies
// optimized for small sizes.
func sortSmallSortFor[T any](l SortLayout, v []T, isLess func(a, b *T) bool) {
	switch l.smallSortKind() {
	case sortSmallSortNetwork:
		sortSmallSortNetworkRun(v, isLess)
	case sortSmallSortGeneral:
		sortSmallSortGeneralRun(v, l, isLess)
	default:
		sortSmallSortFallbackRun(v, isLess)
	}
}

func sortSmallSortFallbackRun[T any](v []T, isLess func(a, b *T) bool) {
	if len(v) >= 2 {
		sortInsertionSortShiftLeft(v, 1, isLess)
	}
}

// sortSmallSortGeneralRun is small_sort_general together with
// small_sort_general_with_scratch.
func sortSmallSortGeneralRun[T any](v []T, l SortLayout, isLess func(a, b *T) bool) {
	n := len(v)
	if n < 2 {
		return
	}

	scratch := make([]T, sortSmallSortGeneralScratchLen)

	if len(scratch) < n+16 {
		sortAbort()
	}

	lenDiv2 := n / 2

	var presortedLen int
	if l.Size <= 16 && n >= 16 {
		sortSort8Stable(v[0:], scratch[0:], scratch[n:], isLess)
		sortSort8Stable(v[lenDiv2:], scratch[lenDiv2:], scratch[n+8:], isLess)

		presortedLen = 8
	} else if n >= 8 {
		sortSort4Stable(v[0:], scratch[0:], isLess)
		sortSort4Stable(v[lenDiv2:], scratch[lenDiv2:], isLess)

		presortedLen = 4
	} else {
		scratch[0] = v[0]
		scratch[lenDiv2] = v[lenDiv2]

		presortedLen = 1
	}

	for _, offset := range [2]int{0, lenDiv2} {
		// At this point dst is initialized with presorted_len elements.
		// We extend this to desired_len, src is valid for desired_len elements.
		src := v[offset:]
		dst := scratch[offset:]
		desiredLen := n - lenDiv2
		if offset == 0 {
			desiredLen = lenDiv2
		}

		for i := presortedLen; i < desiredLen; i++ {
			dst[i] = src[i]
			sortInsertTail(dst, 0, i, isLess)
		}
	}

	// The merge writes back into v. Should is_less panic, the deferred copy
	// restores v from scratch, which holds every element exactly once, as
	// Rust's CopyOnDrop guard does.
	done := false
	defer func() {
		if !done {
			copy(v, scratch[:n])
		}
	}()
	sortBidirectionalMerge(scratch[:n], v, isLess)
	done = true
}

func sortSmallSortNetworkRun[T any](v []T, isLess func(a, b *T) bool) {
	// This implementation is tuned to be efficient for integer types.

	n := len(v)
	if n < 2 {
		return
	}

	if n > sortSmallSortNetworkScratchLen {
		sortAbort()
	}

	lenDiv2 := n / 2
	noMerge := n < 18

	initialRegionLen := lenDiv2
	if noMerge {
		initialRegionLen = n
	}
	region := v[:initialRegionLen]
	regionStart := 0

	// Avoid compiler unrolling, we *really* don't want that to happen here for binary-size reasons.
	for {
		presortedLen := 1
		if len(region) >= 13 {
			sortSort13Optimal(region, isLess)
			presortedLen = 13
		} else if len(region) >= 9 {
			sortSort9Optimal(region, isLess)
			presortedLen = 9
		}

		sortInsertionSortShiftLeft(region, presortedLen, isLess)

		if noMerge {
			return
		}

		if regionStart != 0 {
			break
		}

		region = v[lenDiv2:]
		regionStart = lenDiv2
	}

	// Should is_less panic v was not modified in the merge and retains its
	// original input.
	scratch := make([]T, n)
	sortBidirectionalMerge(v, scratch, isLess)
	copy(v, scratch)
}

// sortSwapIfLess swaps two values in the slice at the position `a_pos` and
// `b_pos` if the value at position `b_pos` is less than the one at position
// `a_pos`.
func sortSwapIfLess[T any](v []T, aPos, bPos int, isLess func(a, b *T) bool) {
	// Important to only swap if it is more and not if it is equal. is_less should return false for
	// equal, so we don't swap.
	if isLess(&v[bPos], &v[aPos]) {
		v[aPos], v[bPos] = v[bPos], v[aPos]
	}
}

// sortSort9Optimal sorts the first 9 elements of `v` with a fast fixed function.
func sortSort9Optimal[T any](v []T, isLess func(a, b *T) bool) {
	if len(v) < 9 {
		sortAbort()
	}

	// Optimal sorting network see:
	// https://bertdobbelaere.github.io/sorting_networks.html.
	for _, pair := range sortNetwork9 {
		sortSwapIfLess(v, int(pair[0]), int(pair[1]), isLess)
	}
}

var sortNetwork9 = [...][2]uint8{
	{0, 3}, {1, 7}, {2, 5}, {4, 8}, {0, 7}, {2, 4}, {3, 8}, {5, 6}, {0, 2},
	{1, 3}, {4, 5}, {7, 8}, {1, 4}, {3, 6}, {5, 7}, {0, 1}, {2, 4}, {3, 5},
	{6, 8}, {2, 3}, {4, 5}, {6, 7}, {1, 2}, {3, 4}, {5, 6},
}

// sortSort13Optimal sorts the first 13 elements of `v` with a fast fixed function.
func sortSort13Optimal[T any](v []T, isLess func(a, b *T) bool) {
	if len(v) < 13 {
		sortAbort()
	}

	// Optimal sorting network see:
	// https://bertdobbelaere.github.io/sorting_networks.html.
	for _, pair := range sortNetwork13 {
		sortSwapIfLess(v, int(pair[0]), int(pair[1]), isLess)
	}
}

var sortNetwork13 = [...][2]uint8{
	{0, 12}, {1, 10}, {2, 9}, {3, 7}, {5, 11}, {6, 8}, {1, 6}, {2, 3}, {4, 11},
	{7, 9}, {8, 10}, {0, 4}, {1, 2}, {3, 6}, {7, 8}, {9, 10}, {11, 12}, {4, 6},
	{5, 9}, {8, 11}, {10, 12}, {0, 5}, {3, 8}, {4, 7}, {6, 11}, {9, 10}, {0, 1},
	{2, 5}, {6, 9}, {7, 8}, {10, 11}, {1, 3}, {2, 4}, {5, 6}, {9, 10}, {1, 2},
	{3, 4}, {5, 7}, {6, 8}, {2, 3}, {4, 5}, {6, 7}, {8, 9}, {3, 4}, {5, 6},
}

// sortInsertTail sorts range [begin, tail] assuming [begin, tail) is already sorted.
//
// The element at tail is taken out while the larger ones shift up; should
// is_less panic, the deferred write puts it into the gap, as Rust's gap
// guard does.
func sortInsertTail[T any](v []T, begin, tail int, isLess func(a, b *T) bool) {
	sift := tail - 1
	if !isLess(&v[tail], &v[sift]) {
		return
	}

	tmp := v[tail]
	gapDst := tail
	defer func() { v[gapDst] = tmp }()

	for {
		v[gapDst] = v[sift]
		gapDst = sift

		if sift == begin {
			break
		}

		sift--
		if !isLess(&tmp, &v[sift]) {
			break
		}
	}
}

// sortInsertionSortShiftLeft sorts `v` assuming `v[..offset]` is already sorted.
func sortInsertionSortShiftLeft[T any](v []T, offset int, isLess func(a, b *T) bool) {
	n := len(v)
	if offset == 0 || offset > n {
		sortAbort()
	}

	for tail := offset; tail != n; tail++ {
		sortInsertTail(v, 0, tail, isLess)
	}
}

// sortSort4Stable sorts src[0..4] into dst[0..4].
//
// It does 5 instead of 6 comparisons compared to a stable transposition 4
// element sorting-network, and always copies each element exactly once.
func sortSort4Stable[T any](src, dst []T, isLess func(a, b *T) bool) {
	// Stably create two pairs a <= b and c <= d.
	c1 := isLess(&src[1], &src[0])
	c2 := isLess(&src[3], &src[2])
	a := sortB2i(c1)
	b := sortB2i(!c1)
	c := 2 + sortB2i(c2)
	d := 2 + sortB2i(!c2)

	// Compare (a, c) and (b, d) to identify max/min. We're left with two
	// unknown elements, but because we are a stable sort we must know which
	// one is leftmost and which one is rightmost.
	// c3, c4 | min max unknown_left unknown_right
	//  0,  0 |  a   d    b         c
	//  0,  1 |  a   b    c         d
	//  1,  0 |  c   d    a         b
	//  1,  1 |  c   b    a         d
	c3 := isLess(&src[c], &src[a])
	c4 := isLess(&src[d], &src[b])
	minimum := sortSelect(c3, c, a)
	maximum := sortSelect(c4, b, d)
	unknownLeft := sortSelect(c3, a, sortSelect(c4, c, b))
	unknownRight := sortSelect(c4, d, sortSelect(c3, b, c))

	// Sort the last two unknown elements.
	c5 := isLess(&src[unknownRight], &src[unknownLeft])
	lo := sortSelect(c5, unknownRight, unknownLeft)
	hi := sortSelect(c5, unknownLeft, unknownRight)

	dst[0] = src[minimum]
	dst[1] = src[lo]
	dst[2] = src[hi]
	dst[3] = src[maximum]
}

// sortSort8Stable sorts src[0..8] into dst[0..8], using scratch[0..8].
func sortSort8Stable[T any](src, dst, scratch []T, isLess func(a, b *T) bool) {
	sortSort4Stable(src[0:], scratch[0:], isLess)
	sortSort4Stable(src[4:], scratch[4:], isLess)

	// scratch[0..8] is now initialized, allowing us to merge back into dst.
	sortBidirectionalMerge(scratch[:8], dst, isLess)
}

// sortBidirectionalMerge merges src into dst, assuming src[..len / 2] and
// src[len / 2..] are sorted.
//
// Original idea for bi-directional merging by Igor van den Hoven (quadsort),
// adapted to only use merge up and down. In contrast to the original
// parity_merge function, it performs 2 writes instead of 4 per iteration.
//
// In each iteration one of left or right moves up one position, and one of
// left_rev or right_rev moves down one position, whereas dst always moves
// up one position and dst_rev always moves down one position. Assuming
// the input was sorted and the comparison function is correctly implemented
// at the end we will have left == left_rev + 1, and right == right_rev + 1,
// fully consuming the input having written it to dst.
func sortBidirectionalMerge[T any](src, dst []T, isLess func(a, b *T) bool) {
	n := len(src)
	lenDiv2 := n / 2

	left := 0
	right := lenDiv2
	out := 0

	leftRev := lenDiv2 - 1
	rightRev := n - 1
	outRev := n - 1

	for range lenDiv2 {
		// merge_up
		if !isLess(&src[right], &src[left]) {
			dst[out] = src[left]
			left++
		} else {
			dst[out] = src[right]
			right++
		}
		out++

		// merge_down
		if !isLess(&src[rightRev], &src[leftRev]) {
			dst[outRev] = src[rightRev]
			rightRev--
		} else {
			dst[outRev] = src[leftRev]
			leftRev--
		}
		outRev--
	}

	leftEnd := leftRev + 1
	rightEnd := rightRev + 1

	// Odd length, so one element is left unconsumed in the input.
	if n%2 != 0 {
		leftNonempty := left < leftEnd
		if leftNonempty {
			dst[out] = src[left]
			left++
		} else {
			dst[out] = src[right]
			right++
		}
	}

	// We now should have consumed the full input exactly once. This can only fail if the
	// user-provided comparison function fails to implement a strict weak ordering. In that case
	// we panic and never access the inconsistent state in dst.
	if left != leftEnd || right != rightEnd {
		sortPanicOnOrdViolation()
	}
}

// sortPanicOnOrdViolation is panic_on_ord_violation.
func sortPanicOnOrdViolation() {
	// This is indicative of a logic bug in the user-provided comparison function or Ord
	// implementation. They are expected to implement a total order as explained in the Ord
	// documentation.
	PanicAt(2, "user-provided comparison function does not correctly implement a total order")
}

// sortHasEfficientInPlaceSwap is has_efficient_in_place_swap: a heuristic
// that holds true on all tested 64-bit capable architectures.
func sortHasEfficientInPlaceSwap(l SortLayout) bool {
	return l.Size <= 8 // size_of::<u64>()
}

func sortB2i(b bool) int {
	if b {
		return 1
	}
	return 0
}

// sortSelect is `hint::select_unpredictable(condition, a, b)`.
func sortSelect(condition bool, a, b int) int {
	if condition {
		return a
	}
	return b
}
