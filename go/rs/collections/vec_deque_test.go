// Port of alloc 1.99.1 src/collections/vec_deque/tests.rs

package collections

// The tests that place the head of the ring at every position set the
// deque's fields as Rust's do. Rust forgets the old elements when it sets
// `len = 0`; the port also zeroes the buffer there, which is what the port
// keeps outside the live range.
//
// Not ported:
//   - the benchmarks;
//   - test_try_reserve_exact and test_try_reserve: try_reserve is not
//     provided, and Mjolnir does not call it on a deque;
//   - test_vec_deque_truncate_drop: Go has no destructors;
//   - issue_80303, which hashes deques through the Hash trait: the port
//     hashes a value by hand with the Hasher methods (HashMapDefaultHasher
//     says how), and the oracle compares that for a deque;
//   - the extract_if tests and drain_to_empty_test: VecDeque::extract_if is
//     unstable and not provided.

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// placeHead is `deque.head = WrappedIndex::from_arbitrary_number(head);
// deque.len = 0;`.
func (d *VecDeque[T]) placeHead(head int) {
	clear(d.buf)
	d.head = head
	d.length = 0
}

// countingFrom is `(0..).take(n).collect::<VecDeque<_>>()`, and with a
// start, `(start..start + n)`.
func countingFrom(start, n int) VecDeque[int] {
	return VecDequeFromIter(slices.Values(intRange(start, start+n)))
}

func TestVecDeque_test_swap_front_back_remove(t *testing.T) {
	test := func(back bool) {
		// This test checks that every single combination of tail position
		// and length is tested. Capacity 15 should be large enough to cover
		// every case.
		tester := VecDequeWithCapacity[int](15)
		usableCap := tester.Capacity()
		finalLen := usableCap / 2

		for length := range finalLen {
			var expected VecDeque[int]
			if back {
				expected = countingFrom(0, length)
			} else {
				reversed := intRange(0, length)
				slices.Reverse(reversed)
				expected = VecDequeFromIter(slices.Values(reversed))
			}
			for headPos := range usableCap {
				tester.placeHead(headPos)
				if back {
					for i := range length * 2 {
						tester.PushFront(i)
					}
					for i := range length {
						expectEq(t, tester.SwapRemoveBack(i), rs.Some(length*2-1-i))
					}
				} else {
					for i := range length * 2 {
						tester.PushBack(i)
					}
					for i := range length {
						idx := tester.Len() - 1 - i
						expectEq(t, tester.SwapRemoveFront(idx), rs.Some(length*2-1-i))
					}
				}
				expect(t, tester.head <= tester.Capacity())
				expect(t, tester.length <= tester.Capacity())
				expectEq(t, tester, expected)
			}
		}
	}
	test(true)
	test(false)
}

func TestVecDeque_test_insert(t *testing.T) {
	// This test checks that every single combination of tail position,
	// length, and insertion position is tested. Capacity 15 should be large
	// enough to cover every case.

	tester := VecDequeWithCapacity[int](15)
	// can't guarantee we got 15, so have to get what we got.
	// 15 would be great, but we will definitely get 2^k - 1, for k >= 4, or
	// else this test isn't covering what it wants to
	capacity := tester.Capacity()

	// len is the length *after* insertion
	for length := 1; length < capacity; length++ {
		// 0, 1, 2, .., len - 1
		expected := countingFrom(0, length)
		for headPos := range capacity {
			for toInsert := range length {
				tester.placeHead(headPos)
				for i := range length {
					if i != toInsert {
						tester.PushBack(i)
					}
				}
				tester.Insert(toInsert, toInsert)
				expect(t, tester.head <= tester.Capacity())
				expect(t, tester.length <= tester.Capacity())
				expectEq(t, tester, expected)
			}
		}
	}
}

func TestVecDeque_test_get(t *testing.T) {
	var tester VecDeque[int]
	tester.PushBack(1)
	tester.PushBack(2)
	tester.PushBack(3)

	expectEq(t, tester.Len(), 3)

	expectEq(t, optionOf(tester.Get(1)), rs.Some(2))
	expectEq(t, optionOf(tester.Get(2)), rs.Some(3))
	expectEq(t, optionOf(tester.Get(0)), rs.Some(1))
	expectEq(t, optionOf(tester.Get(3)), rs.None[int]())

	tester.Remove(0)

	expectEq(t, tester.Len(), 2)
	expectEq(t, optionOf(tester.Get(0)), rs.Some(2))
	expectEq(t, optionOf(tester.Get(1)), rs.Some(3))
	expectEq(t, optionOf(tester.Get(2)), rs.None[int]())
}

func TestVecDeque_test_get_mut(t *testing.T) {
	var tester VecDeque[int]
	tester.PushBack(1)
	tester.PushBack(2)
	tester.PushBack(3)

	expectEq(t, tester.Len(), 3)

	if elem, ok := tester.GetMut(0); ok {
		expectEq(t, *elem, 1)
		*elem = 10
	}

	if elem, ok := tester.GetMut(2); ok {
		expectEq(t, *elem, 3)
		*elem = 30
	}

	expectEq(t, optionOf(tester.Get(0)), rs.Some(10))
	expectEq(t, optionOf(tester.Get(2)), rs.Some(30))
	expectEq(t, optionOf(tester.GetMut(3)), rs.None[int]())

	tester.Remove(2)

	expectEq(t, tester.Len(), 2)
	expectEq(t, optionOf(tester.Get(0)), rs.Some(10))
	expectEq(t, optionOf(tester.Get(1)), rs.Some(2))
	expectEq(t, optionOf(tester.Get(2)), rs.None[int]())
}

func TestVecDeque_test_swap(t *testing.T) {
	var tester VecDeque[int]
	tester.PushBack(1)
	tester.PushBack(2)
	tester.PushBack(3)

	expect(t, tester.EqualSlice([]int{1, 2, 3}))

	tester.Swap(0, 0)
	expect(t, tester.EqualSlice([]int{1, 2, 3}))
	tester.Swap(0, 1)
	expect(t, tester.EqualSlice([]int{2, 1, 3}))
	tester.Swap(2, 1)
	expect(t, tester.EqualSlice([]int{2, 3, 1}))
	tester.Swap(1, 2)
	expect(t, tester.EqualSlice([]int{2, 1, 3}))
	tester.Swap(0, 2)
	expect(t, tester.EqualSlice([]int{3, 1, 2}))
	tester.Swap(2, 2)
	expect(t, tester.EqualSlice([]int{3, 1, 2}))
}

func TestVecDeque_test_swap_panic(t *testing.T) {
	var tester VecDeque[int]
	tester.PushBack(1)
	tester.PushBack(2)
	tester.PushBack(3)
	shouldPanic(t, "assertion failed: j < self.len()", func() { tester.Swap(2, 3) })
}

func TestVecDeque_test_reserve_exact(t *testing.T) {
	tester := VecDequeWithCapacity[int32](1)
	expectEq(t, tester.Capacity(), 1)
	tester.ReserveExact(50)
	expectEq(t, tester.Capacity(), 50)
	tester.ReserveExact(40)
	// reserving won't shrink the buffer
	expectEq(t, tester.Capacity(), 50)
	tester.ReserveExact(200)
	expectEq(t, tester.Capacity(), 200)
}

func TestVecDeque_test_reserve_exact_panic(t *testing.T) {
	var tester VecDeque[int32]
	shouldPanic(t, "capacity overflow", func() { tester.ReserveExact(rs.UsizeMax) })
}

func TestVecDeque_test_contains(t *testing.T) {
	var tester VecDeque[int]
	tester.PushBack(1)
	tester.PushBack(2)
	tester.PushBack(3)

	expect(t, tester.Contains(1))
	expect(t, tester.Contains(3))
	expect(t, !tester.Contains(0))
	expect(t, !tester.Contains(4))
	tester.Remove(0)
	expect(t, !tester.Contains(1))
	expect(t, tester.Contains(2))
	expect(t, tester.Contains(3))
}

func TestVecDeque_test_rotate_left_right(t *testing.T) {
	tester := VecDequeFromIter(slices.Values(intRange(1, 11)))
	tester.Reserve(1)

	expectEq(t, tester.Len(), 10)

	tester.RotateLeft(0)
	expect(t, tester.EqualSlice([]int{1, 2, 3, 4, 5, 6, 7, 8, 9, 10}))

	tester.RotateRight(0)
	expect(t, tester.EqualSlice([]int{1, 2, 3, 4, 5, 6, 7, 8, 9, 10}))

	tester.RotateLeft(3)
	expect(t, tester.EqualSlice([]int{4, 5, 6, 7, 8, 9, 10, 1, 2, 3}))

	tester.RotateRight(5)
	expect(t, tester.EqualSlice([]int{9, 10, 1, 2, 3, 4, 5, 6, 7, 8}))

	tester.RotateLeft(tester.Len())
	expect(t, tester.EqualSlice([]int{9, 10, 1, 2, 3, 4, 5, 6, 7, 8}))

	tester.RotateRight(tester.Len())
	expect(t, tester.EqualSlice([]int{9, 10, 1, 2, 3, 4, 5, 6, 7, 8}))

	tester.RotateLeft(1)
	expect(t, tester.EqualSlice([]int{10, 1, 2, 3, 4, 5, 6, 7, 8, 9}))
}

func TestVecDeque_test_rotate_left_panic(t *testing.T) {
	tester := VecDequeFromIter(slices.Values(intRange(1, 11)))
	shouldPanic(t, "assertion failed: n <= self.len()", func() { tester.RotateLeft(tester.Len() + 1) })
}

func TestVecDeque_test_rotate_right_panic(t *testing.T) {
	tester := VecDequeFromIter(slices.Values(intRange(1, 11)))
	shouldPanic(t, "assertion failed: n <= self.len()", func() { tester.RotateRight(tester.Len() + 1) })
}

// found is the Result<usize, usize> of the binary searches.
type found struct {
	at int
	ok bool
}

func searched(at int, ok bool) found { return found{at, ok} }

func TestVecDeque_test_binary_search(t *testing.T) {
	// If the given VecDeque is not sorted, the returned result is
	// unspecified and meaningless, as this method performs a binary search.

	tester := VecDequeFrom(0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55)

	expectEq(t, searched(tester.BinarySearch(0)), found{0, true})
	expectEq(t, searched(tester.BinarySearch(5)), found{5, true})
	expectEq(t, searched(tester.BinarySearch(55)), found{10, true})
	expectEq(t, searched(tester.BinarySearch(4)), found{5, false})
	expectEq(t, searched(tester.BinarySearch(-1)), found{0, false})
	at, ok := tester.BinarySearch(1)
	expect(t, ok && 1 <= at && at <= 2)

	tester = VecDequeFrom(1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3)
	expectEq(t, searched(tester.BinarySearch(1)), found{0, true})
	at, ok = tester.BinarySearch(2)
	expect(t, ok && 1 <= at && at <= 4)
	at, ok = tester.BinarySearch(3)
	expect(t, ok && 5 <= at && at <= 13)
	expectEq(t, searched(tester.BinarySearch(-2)), found{0, false})
	expectEq(t, searched(tester.BinarySearch(0)), found{0, false})
	expectEq(t, searched(tester.BinarySearch(4)), found{14, false})
	expectEq(t, searched(tester.BinarySearch(5)), found{14, false})
}

func TestVecDeque_test_binary_search_by(t *testing.T) {
	// If the given VecDeque is not sorted, the returned result is
	// unspecified and meaningless, as this method performs a binary search.

	tester := VecDequeFrom(0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55)
	by := func(target int) func(*int) int { return func(x *int) int { return rs.Cmp(*x, target) } }

	expectEq(t, searched(tester.BinarySearchBy(by(0))), found{0, true})
	expectEq(t, searched(tester.BinarySearchBy(by(5))), found{5, true})
	expectEq(t, searched(tester.BinarySearchBy(by(55))), found{10, true})
	expectEq(t, searched(tester.BinarySearchBy(by(4))), found{5, false})
	expectEq(t, searched(tester.BinarySearchBy(by(-1))), found{0, false})
	at, ok := tester.BinarySearchBy(by(1))
	expect(t, ok && 1 <= at && at <= 2)
}

func TestVecDeque_test_binary_search_key(t *testing.T) {
	// If the given VecDeque is not sorted, the returned result is
	// unspecified and meaningless, as this method performs a binary search.

	tester := VecDequeFrom(
		rs.T2(-1, 0),
		rs.T2(2, 10),
		rs.T2(6, 5),
		rs.T2(7, 1),
		rs.T2(8, 10),
		rs.T2(10, 2),
		rs.T2(20, 3),
		rs.T2(24, 5),
		rs.T2(25, 18),
		rs.T2(28, 13),
		rs.T2(31, 21),
		rs.T2(32, 4),
		rs.T2(54, 25),
	)
	first := func(pair *rs.Tuple2[int, int]) int { return pair.F0 }

	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, -1, first)), found{0, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 8, first)), found{4, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 25, first)), found{8, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 54, first)), found{12, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, -2, first)), found{0, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 1, first)), found{1, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 4, first)), found{2, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 13, first)), found{6, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 55, first)), found{13, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 100, first)), found{13, false})

	tester = VecDequeFrom(
		rs.T2(0, 0),
		rs.T2(2, 1),
		rs.T2(6, 1),
		rs.T2(5, 1),
		rs.T2(3, 1),
		rs.T2(1, 2),
		rs.T2(2, 3),
		rs.T2(4, 5),
		rs.T2(5, 8),
		rs.T2(8, 13),
		rs.T2(1, 21),
		rs.T2(2, 34),
		rs.T2(4, 55),
	)
	second := func(pair *rs.Tuple2[int, int]) int { return pair.F1 }

	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 0, second)), found{0, true})
	at, ok := VecDequeBinarySearchByKey(&tester, 1, second)
	expect(t, ok && 1 <= at && at <= 4)
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 8, second)), found{8, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 13, second)), found{9, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 55, second)), found{12, true})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, -1, second)), found{0, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 4, second)), found{7, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 56, second)), found{13, false})
	expectEq(t, searched(VecDequeBinarySearchByKey(&tester, 100, second)), found{13, false})
}

// slicesOf is `deque.as_slices()` as one value, for assert_eq!.
func slicesOf[T any](d *VecDeque[T]) rs.Tuple2[[]T, []T] {
	front, back := d.AsSlices()
	return rs.T2(front, back)
}

func TestVecDeque_make_contiguous_big_head(t *testing.T) {
	tester := VecDequeWithCapacity[int](15)

	for i := range 3 {
		tester.PushBack(i)
	}

	for i := 3; i < 10; i++ {
		tester.PushFront(i)
	}

	// 012......9876543
	expectEq(t, tester.Capacity(), 15)
	expectEq(t, rs.T2([]int{9, 8, 7, 6, 5, 4, 3}, []int{0, 1, 2}), slicesOf(&tester))

	expectedStart := len(slicesOf(&tester).F1)
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t, rs.T2([]int{9, 8, 7, 6, 5, 4, 3, 0, 1, 2}, []int{}), slicesOf(&tester))
}

func TestVecDeque_make_contiguous_big_tail(t *testing.T) {
	tester := VecDequeWithCapacity[int](15)

	for i := range 8 {
		tester.PushBack(i)
	}

	for i := 8; i < 10; i++ {
		tester.PushFront(i)
	}

	// 01234567......98
	expectedStart := 0
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t, rs.T2([]int{9, 8, 0, 1, 2, 3, 4, 5, 6, 7}, []int{}), slicesOf(&tester))
}

// chars is `b'A'..b'I'` and the like as chars.
func chars(from, to byte) []rune {
	var runes []rune
	for b := from; b < to; b++ {
		runes = append(runes, rune(b))
	}
	return runes
}

func TestVecDeque_make_contiguous_small_free(t *testing.T) {
	tester := VecDequeWithCapacity[rune](16)

	for _, c := range chars('A', 'I') {
		tester.PushBack(c)
	}

	for _, c := range chars('I', 'N') {
		tester.PushFront(c)
	}

	expect(t, tester.EqualSlice([]rune{'M', 'L', 'K', 'J', 'I', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'}))

	// ABCDEFGH...MLKJI
	expectedStart := 0
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t,
		rs.T2([]rune{'M', 'L', 'K', 'J', 'I', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'}, []rune{}),
		slicesOf(&tester),
	)

	tester.Clear()
	for _, c := range chars('I', 'N') {
		tester.PushBack(c)
	}

	for _, c := range chars('A', 'I') {
		tester.PushFront(c)
	}

	// IJKLM...HGFEDCBA
	expectedStart = 3
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t,
		rs.T2([]rune{'H', 'G', 'F', 'E', 'D', 'C', 'B', 'A', 'I', 'J', 'K', 'L', 'M'}, []rune{}),
		slicesOf(&tester),
	)
}

func TestVecDeque_make_contiguous_head_to_end(t *testing.T) {
	tester := VecDequeWithCapacity[rune](16)

	for _, c := range chars('A', 'L') {
		tester.PushBack(c)
	}

	for _, c := range chars('L', 'Q') {
		tester.PushFront(c)
	}

	expect(t, tester.EqualSlice([]rune{'P', 'O', 'N', 'M', 'L', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K'}))

	// ABCDEFGHIJKPONML
	expectedStart := 0
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t,
		rs.T2([]rune{'P', 'O', 'N', 'M', 'L', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K'}, []rune{}),
		slicesOf(&tester),
	)

	tester.Clear()
	for _, c := range chars('L', 'Q') {
		tester.PushBack(c)
	}

	for _, c := range chars('A', 'L') {
		tester.PushFront(c)
	}

	// LMNOPKJIHGFEDCBA
	expectedStart = 0
	tester.MakeContiguous()
	expectEq(t, tester.head, expectedStart)
	expectEq(t,
		rs.T2([]rune{'K', 'J', 'I', 'H', 'G', 'F', 'E', 'D', 'C', 'B', 'A', 'L', 'M', 'N', 'O', 'P'}, []rune{}),
		slicesOf(&tester),
	)
}

func TestVecDeque_make_contiguous_head_to_end_2(t *testing.T) {
	// Another test case for #79808, taken from #80293.

	dq := VecDequeFromIter(slices.Values(intRange(0, 6)))
	dq.PopFront()
	dq.PopFront()
	dq.PushBack(6)
	dq.PushBack(7)
	dq.PushBack(8)
	dq.MakeContiguous()
	collected := collect(derefValues(dq.Iter()))
	expectEq(t, slicesOf(&dq), rs.T2(collected, []int{}))
}

func TestVecDeque_test_remove(t *testing.T) {
	// This test checks that every single combination of tail position,
	// length, and removal position is tested. Capacity 15 should be large
	// enough to cover every case.

	tester := VecDequeWithCapacity[int](15)
	// can't guarantee we got 15, so have to get what we got.
	// 15 would be great, but we will definitely get 2^k - 1, for k >= 4, or
	// else this test isn't covering what it wants to
	capacity := tester.Capacity()

	// len is the length *after* removal
	for length := 0; length < capacity-1; length++ {
		// 0, 1, 2, .., len - 1
		expected := countingFrom(0, length)
		for headPos := range capacity {
			for toRemove := 0; toRemove <= length; toRemove++ {
				tester.placeHead(headPos)
				for i := range length {
					if i == toRemove {
						tester.PushBack(1234)
					}
					tester.PushBack(i)
				}
				if toRemove == length {
					tester.PushBack(1234)
				}
				tester.Remove(toRemove)
				expect(t, tester.head <= tester.Capacity())
				expect(t, tester.length <= tester.Capacity())
				expectEq(t, tester, expected)
			}
		}
	}
}

func TestVecDeque_test_range(t *testing.T) {
	tester := VecDequeWithCapacity[int](7)

	capacity := tester.Capacity()
	for length := 0; length <= capacity; length++ {
		for head := 0; head <= capacity; head++ {
			for start := 0; start <= length; start++ {
				for end := start; end <= length; end++ {
					tester.placeHead(head)
					for i := range length {
						tester.PushBack(i)
					}

					// Check that we iterate over the correct values
					got := VecDequeFromIter(derefValues(tester.Range(Included(start), Excluded(end))))
					expectEq(t, got, countingFrom(start, end-start))
				}
			}
		}
	}
}

func TestVecDeque_test_range_mut(t *testing.T) {
	tester := VecDequeWithCapacity[int](7)

	capacity := tester.Capacity()
	for length := 0; length <= capacity; length++ {
		for head := 0; head <= capacity; head++ {
			for start := 0; start <= length; start++ {
				for end := start; end <= length; end++ {
					tester.placeHead(head)
					for i := range length {
						tester.PushBack(i)
					}

					headWas := tester.head
					lenWas := tester.length

					// Check that we iterate over the correct values
					got := VecDequeFromIter(derefValues(tester.RangeMut(Included(start), Excluded(end))))
					expectEq(t, got, countingFrom(start, end-start))

					// We shouldn't have changed the capacity or made the
					// head or tail out of bounds
					expectEq(t, tester.Capacity(), capacity)
					expectEq(t, tester.head, headWas)
					expectEq(t, tester.length, lenWas)
				}
			}
		}
	}
}

func TestVecDeque_test_drain(t *testing.T) {
	tester := VecDequeWithCapacity[int](7)

	capacity := tester.Capacity()
	for length := 0; length <= capacity; length++ {
		for head := range capacity {
			for drainStart := 0; drainStart <= length; drainStart++ {
				for drainEnd := drainStart; drainEnd <= length; drainEnd++ {
					tester.placeHead(head)
					for i := range length {
						tester.PushBack(i)
					}

					// Check that we drain the correct values
					drained := VecDequeFromIter(tester.Drain(Included(drainStart), Excluded(drainEnd)))
					expectEq(t, drained, countingFrom(drainStart, drainEnd-drainStart))

					// We shouldn't have changed the capacity or made the
					// head or tail out of bounds
					expectEq(t, tester.Capacity(), capacity)
					expect(t, tester.head <= tester.Capacity())
					expect(t, tester.length <= tester.Capacity())

					// We should see the correct values in the VecDeque
					expected := VecDequeFromIter(slices.Values(append(intRange(0, drainStart), intRange(drainEnd, length)...)))
					expectEq(t, expected, tester)
				}
			}
		}
	}
}

func TestVecDeque_issue_108453(t *testing.T) {
	deque := VecDequeWithCapacity[uint8](10)

	deque.PushBack(1)
	deque.PushBack(2)
	deque.PushBack(3)

	deque.PushFront(10)
	deque.PushFront(9)

	deque.ShrinkTo(9)

	expectEq(t, collect(deque.IntoIter()), []uint8{9, 10, 1, 2, 3})
}

func TestVecDeque_test_shrink_to(t *testing.T) {
	// test deques with capacity 16 with all possible head positions,
	// lengths and target capacities.
	capacity := 16

	for length := range capacity {
		for head := range capacity {
			expected := countingFrom(1, length)

			for targetCap := length; targetCap < capacity; targetCap++ {
				deque := VecDequeWithCapacity[int](capacity)
				// currently, `with_capacity` always allocates the exact
				// capacity if it's greater than 8.
				expectEq(t, deque.Capacity(), capacity)

				// we can let the head point anywhere in the buffer since the
				// deque is empty.
				deque.placeHead(head)
				deque.Extend(slices.Values(intRange(1, length+1)))

				deque.ShrinkTo(targetCap)

				expectEq(t, deque, expected)
			}
		}
	}
}

func TestVecDeque_test_shrink_to_fit(t *testing.T) {
	// This test checks that every single combination of head and tail
	// position, is tested. Capacity 15 should be large enough to cover
	// every case.

	tester := VecDequeWithCapacity[int](15)
	// can't guarantee we got 15, so have to get what we got.
	// 15 would be great, but we will definitely get 2^k - 1, for k >= 4, or
	// else this test isn't covering what it wants to
	capacity := tester.Capacity()
	tester.Reserve(63)
	maxCap := tester.Capacity()

	for length := 0; length <= capacity; length++ {
		// 0, 1, 2, .., len - 1
		expected := countingFrom(0, length)
		for headPos := 0; headPos <= maxCap; headPos++ {
			tester.Reserve(headPos)
			tester.placeHead(headPos)
			tester.Reserve(63)
			for i := range length {
				tester.PushBack(i)
			}
			tester.ShrinkToFit()
			expect(t, tester.Capacity() <= capacity)
			expect(t, tester.head <= tester.Capacity())
			expect(t, tester.length <= tester.Capacity())
			expectEq(t, tester, expected)
		}
	}
}

func TestVecDeque_test_split_off(t *testing.T) {
	// This test checks that every single combination of tail position,
	// length, and split position is tested. Capacity 15 should be large
	// enough to cover every case.

	tester := VecDequeWithCapacity[int](15)
	// can't guarantee we got 15, so have to get what we got.
	// 15 would be great, but we will definitely get 2^k - 1, for k >= 4, or
	// else this test isn't covering what it wants to
	capacity := tester.Capacity()

	// len is the length *before* splitting
	for length := range capacity {
		// index to split at
		for at := 0; at <= length; at++ {
			// 0, 1, 2, .., at - 1 (may be empty)
			expectedSelf := countingFrom(0, at)
			// at, at + 1, .., len - 1 (may be empty)
			expectedOther := countingFrom(at, length-at)

			for headPos := range capacity {
				tester.placeHead(headPos)
				for i := range length {
					tester.PushBack(i)
				}
				result := tester.SplitOff(at)
				expect(t, tester.head <= tester.Capacity())
				expect(t, tester.length <= tester.Capacity())
				expect(t, result.head <= result.Capacity())
				expect(t, result.length <= result.Capacity())
				expectEq(t, tester, expectedSelf)
				expectEq(t, result, expectedOther)
			}
		}
	}
}

func TestVecDeque_test_from_vec(t *testing.T) {
	for capacity := range 35 {
		for length := 0; length <= capacity; length++ {
			vec := make([]int, 0, capacity)
			vec = append(vec, intRange(0, length)...)

			vd := VecDequeFrom(slices.Clone(vec)...)
			expectEq(t, vd.Len(), len(vec))
			expectEq(t, collect(vd.IntoIter()), vec)
		}
	}
}

func TestVecDeque_test_extend_basic(t *testing.T) {
	testExtendImpl(t, false)
}

// Rust's Extend takes another path for a TrustedLen iterator, reserving
// its whole length at once; a Go sequence has no length, so both tests run
// the one Extend of the port, and the contents must agree either way.
func TestVecDeque_test_extend_trusted_len(t *testing.T) {
	testExtendImpl(t, true)
}

func testExtendImpl(t *testing.T, trustedLen bool) {
	t.Helper()
	type vecDequeTester struct {
		test       VecDeque[int]
		expected   VecDeque[int]
		trustedLen bool
	}

	tester := &vecDequeTester{test: VecDequeNew[int](), expected: VecDequeNew[int](), trustedLen: trustedLen}

	testExtend := func(lo, hi int) {
		t.Helper()
		tester.test.Extend(slices.Values(intRange(lo, hi)))

		for _, item := range intRange(lo, hi) {
			tester.expected.PushBack(item)
		}

		expectEq(t, tester.test, tester.expected)
	}

	drain := func(lo, hi Bound[int]) {
		t.Helper()
		for range tester.test.Drain(lo, hi) {
		}
		for range tester.expected.Drain(lo, hi) {
		}

		expectEq(t, tester.test, tester.expected)
	}

	clearBoth := func() {
		tester.test.Clear()
		tester.expected.Clear()
	}

	remainingCapacity := func() int { return tester.test.Capacity() - tester.test.Len() }

	// Initial capacity
	testExtend(0, remainingCapacity())

	// Grow
	testExtend(1024, 2048)

	// Wrap around
	drain(Unbounded[int](), Excluded(128))

	testExtend(0, remainingCapacity())

	// Continue
	drain(Included(256), Unbounded[int]())
	testExtend(4096, 8196)

	clearBoth()

	// Start again
	testExtend(0, 32)
}

func TestVecDeque_test_from_array(t *testing.T) {
	test := func(n int) {
		t.Helper()
		array := make([]int, n)

		for i := range n {
			array[i] = i
		}

		deq := VecDequeFrom(array...)

		for i := range n {
			expectEq(t, *deq.Index(i), i)
		}

		expectEq(t, deq.Len(), n)
	}
	test(0)
	test(1)
	test(2)
	test(32)
	test(35)
}

func TestVecDeque_test_vec_from_vecdeque(t *testing.T) {
	createVecAndTestConvert := func(capacity, offset, length int) {
		t.Helper()
		vd := VecDequeWithCapacity[int](capacity)
		for range offset {
			vd.PushBack(0)
			vd.PopFront()
		}
		vd.Extend(slices.Values(intRange(0, length)))

		vec := VecFromVecDeque(rs.Clone(vd))
		expectEq(t, len(vec), vd.Len())
		expectEq(t, vec, collect(vd.IntoIter()))
	}

	maxPwr := 7

	for capPwr := range maxPwr {
		// Make capacity as a (2^x)-1, so that the ring size is 2^x
		capacity := 1<<capPwr - 1

		// In these cases there is enough free space to solve it with copies
		for length := 0; length < (capacity+1)/2; length++ {
			// Test contiguous cases
			for offset := 0; offset < capacity-length; offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}

			// Test cases where block at end of buffer is bigger than block
			// at start
			for offset := capacity - length; offset < capacity-(length/2); offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}

			// Test cases where block at start of buffer is bigger than block
			// at end
			for offset := capacity - (length / 2); offset < capacity; offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}
		}

		// Now there's not (necessarily) space to straighten the ring with
		// simple copies, the ring will use swapping when:
		// (cap + 1 - offset) > (cap + 1 - len) && (len - (cap + 1 - offset)) > (cap + 1 - len))
		//  right block size  >   free space    &&      left block size       >    free space
		for length := (capacity + 1) / 2; length < capacity; length++ {
			// Test contiguous cases
			for offset := 0; offset < capacity-length; offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}

			// Test cases where block at end of buffer is bigger than block
			// at start
			for offset := capacity - length; offset < capacity-(length/2); offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}

			// Test cases where block at start of buffer is bigger than block
			// at end
			for offset := capacity - (length / 2); offset < capacity; offset++ {
				createVecAndTestConvert(capacity, offset, length)
			}
		}
	}
}

// clone_from is `self.clear(); self.extend(source.iter().cloned())` for a
// VecDeque; the port has no CloneFrom (Mjolnir calls it on no deque), and
// the test asserts only the contents, which rs.Clone gives.
func TestVecDeque_test_clone_from(t *testing.T) {
	m := slices.Repeat([]int{1}, 8)
	n := slices.Repeat([]int{2}, 12)
	limit := 8
	for pfv := range limit {
		for pfu := range limit {
			for longer := range 2 {
				vr, ur := m, n
				if longer != 0 {
					vr, ur = n, m
				}
				v := VecDequeFrom(slices.Clone(vr)...)
				for range pfv {
					v.PushFront(1)
				}
				u := VecDequeFrom(slices.Clone(ur)...)
				for range pfu {
					u.PushFront(2)
				}
				v = rs.Clone(u)
				expectEq(t, &v, &u)
			}
		}
	}
}

func TestVecDeque_issue_53529(t *testing.T) {
	box := func(value int) *int { return &value }

	var dst VecDeque[*int]
	dst.PushFront(box(1))
	dst.PushFront(box(2))
	expectEq(t, *dst.PopBack().Unwrap(), 1)

	var src VecDeque[*int]
	src.PushFront(box(2))
	dst.Append(&src)
	for a := range dst.IntoIter() {
		expectEq(t, *a, 2)
	}
}
