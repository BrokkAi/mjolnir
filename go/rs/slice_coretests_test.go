// Port of coretests 1.99.1 tests/slice.rs (the tests of what slice.go,
// sort_search.go and iter.go cover)
//
// Left out: the tests of the iterator objects' own methods (nth, nth_back,
// advance_by, size_hint, next_back on chunks and windows), which the Go
// functions return as slices; the lines of test_binary_search and
// test_windows_count over `[(); usize::MAX]`, which test_binary_search_by_overflow
// covers with Go's largest slice of empty structs; the methods not ported
// (select_nth_unstable, partition_dedup, split_*_chunk, get_disjoint_mut,
// array_windows, the _mut iterators).

package rs_test

import (
	"math"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func sliceTestEq[T any](t *testing.T, got, want T) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func sliceTestTrue(t *testing.T, condition bool, what string) {
	t.Helper()
	if !condition {
		t.Errorf("assertion failed: %s", what)
	}
}

// sliceTestPanics is #[should_panic(expected = want)].
func sliceTestPanics(t *testing.T, want string, f func()) {
	t.Helper()
	defer func() {
		t.Helper()
		recovered := recover()
		if recovered == nil {
			t.Errorf("no panic, want %q", want)
			return
		}
		if message := rs.PanicInfoOf(recovered).Message; message != want {
			t.Errorf("panic %q, want %q", message, want)
		}
	}()
	f()
}

func sliceTestSearch(t *testing.T, b []int32, x int32, index int, found bool) {
	t.Helper()
	gotIndex, gotFound := rs.BinarySearch(b, x)
	if gotIndex != index || gotFound != found {
		t.Errorf("%v.binary_search(%d) = (%d, %v), want (%d, %v)", b, x, gotIndex, gotFound, index, found)
	}
}

func TestSlice_test_position(t *testing.T) {
	b := []int32{1, 2, 3, 5, 5}
	sliceTestEq(t, rs.Position(b, func(v int32) bool { return v == 9 }), rs.None[int]())
	sliceTestEq(t, rs.Position(b, func(v int32) bool { return v == 5 }), rs.Some(3))
	sliceTestEq(t, rs.Position(b, func(v int32) bool { return v == 3 }), rs.Some(2))
	sliceTestEq(t, rs.Position(b, func(v int32) bool { return v == 0 }), rs.None[int]())
}

func TestSlice_test_rposition(t *testing.T) {
	b := []int32{1, 2, 3, 5, 5}
	sliceTestEq(t, rs.Rposition(b, func(v int32) bool { return v == 9 }), rs.None[int]())
	sliceTestEq(t, rs.Rposition(b, func(v int32) bool { return v == 5 }), rs.Some(4))
	sliceTestEq(t, rs.Rposition(b, func(v int32) bool { return v == 3 }), rs.Some(2))
	sliceTestEq(t, rs.Rposition(b, func(v int32) bool { return v == 0 }), rs.None[int]())
}

func TestSlice_test_binary_search(t *testing.T) {
	sliceTestSearch(t, []int32{}, 5, 0, false)

	b := []int32{4}
	sliceTestSearch(t, b, 3, 0, false)
	sliceTestSearch(t, b, 4, 0, true)
	sliceTestSearch(t, b, 5, 1, false)

	b = []int32{1, 2, 4, 6, 8, 9}
	sliceTestSearch(t, b, 5, 3, false)
	sliceTestSearch(t, b, 6, 3, true)
	sliceTestSearch(t, b, 7, 4, false)
	sliceTestSearch(t, b, 8, 4, true)

	b = []int32{1, 2, 4, 5, 6, 8}
	sliceTestSearch(t, b, 9, 6, false)

	b = []int32{1, 2, 4, 6, 7, 8, 9}
	sliceTestSearch(t, b, 6, 3, true)
	sliceTestSearch(t, b, 5, 3, false)
	sliceTestSearch(t, b, 8, 5, true)

	b = []int32{1, 2, 4, 5, 6, 8, 9}
	sliceTestSearch(t, b, 7, 5, false)
	sliceTestSearch(t, b, 0, 0, false)

	b = []int32{1, 3, 3, 3, 7}
	sliceTestSearch(t, b, 0, 0, false)
	sliceTestSearch(t, b, 1, 0, true)
	sliceTestSearch(t, b, 2, 1, false)
	index, found := rs.BinarySearch(b, 3)
	sliceTestTrue(t, found && index >= 1 && index <= 3, "binary_search(&3) is Ok(1..=3)")
	sliceTestSearch(t, b, 4, 4, false)
	sliceTestSearch(t, b, 5, 4, false)
	sliceTestSearch(t, b, 6, 4, false)
	sliceTestSearch(t, b, 7, 4, true)
	sliceTestSearch(t, b, 8, 5, false)
}

// The slice of usize::MAX unit values becomes Go's largest slice of empty
// structs, whose length is UsizeMax in this port.
func TestSlice_test_binary_search_by_overflow(t *testing.T) {
	b := make([]struct{}, math.MaxInt)
	index, found := rs.BinarySearchBy(b, func(struct{}) rs.Ordering { return rs.Ordering_Equal })
	sliceTestEq(t, [2]any{index, found}, [2]any{math.MaxInt - 1, true})
	index, found = rs.BinarySearchBy(b, func(struct{}) rs.Ordering { return rs.Ordering_Greater })
	sliceTestEq(t, [2]any{index, found}, [2]any{0, false})
	index, found = rs.BinarySearchBy(b, func(struct{}) rs.Ordering { return rs.Ordering_Less })
	sliceTestEq(t, [2]any{index, found}, [2]any{math.MaxInt, false})
}

// Test implementation specific behavior when finding equivalent elements.
func TestSlice_test_binary_search_implementation_details(t *testing.T) {
	b := []int32{1, 1, 2, 2, 3, 3, 3}
	sliceTestSearch(t, b, 1, 1, true)
	sliceTestSearch(t, b, 2, 3, true)
	sliceTestSearch(t, b, 3, 6, true)
	b = []int32{1, 1, 1, 1, 1, 3, 3, 3, 3}
	sliceTestSearch(t, b, 1, 4, true)
	sliceTestSearch(t, b, 3, 8, true)
	b = []int32{1, 1, 1, 1, 3, 3, 3, 3, 3}
	sliceTestSearch(t, b, 1, 3, true)
	sliceTestSearch(t, b, 3, 8, true)
}

func TestSlice_test_partition_point(t *testing.T) {
	check := func(b []int32, x int32, want int) {
		t.Helper()
		if got := rs.PartitionPoint(b, func(e int32) bool { return e < x }); got != want {
			t.Errorf("%v.partition_point(|&e| e < %d) = %d, want %d", b, x, got, want)
		}
	}
	check([]int32{}, 5, 0)

	b := []int32{4}
	check(b, 3, 0)
	check(b, 4, 0)
	check(b, 5, 1)

	b = []int32{1, 2, 4, 6, 8, 9}
	check(b, 5, 3)
	check(b, 6, 3)
	check(b, 7, 4)
	check(b, 8, 4)

	b = []int32{1, 2, 4, 5, 6, 8}
	check(b, 9, 6)

	b = []int32{1, 2, 4, 6, 7, 8, 9}
	check(b, 6, 3)
	check(b, 5, 3)
	check(b, 8, 5)

	b = []int32{1, 2, 4, 5, 6, 8, 9}
	check(b, 7, 5)
	check(b, 0, 0)

	b = []int32{1, 3, 3, 3, 7}
	check(b, 0, 0)
	check(b, 1, 0)
	check(b, 2, 1)
	check(b, 3, 1)
	check(b, 4, 4)
	check(b, 5, 4)
	check(b, 6, 4)
	check(b, 7, 4)
	check(b, 8, 5)
}

func TestSlice_test_iterator_last(t *testing.T) {
	v := []int32{0, 1, 2, 3, 4}
	sliceTestEq(t, rs.Last(v).Unwrap(), int32(4))
	sliceTestEq(t, rs.Last(v[:1]).Unwrap(), int32(0))
}

func TestSlice_test_chunks_count(t *testing.T) {
	sliceTestEq(t, len(rs.Chunks([]int32{0, 1, 2, 3, 4, 5}, 3)), 2)
	sliceTestEq(t, len(rs.Chunks([]int32{0, 1, 2, 3, 4}, 2)), 3)
	sliceTestEq(t, len(rs.Chunks([]int32{}, 2)), 0)
}

func TestSlice_test_chunks_last(t *testing.T) {
	sliceTestEq(t, rs.Last(rs.Chunks([]int32{0, 1, 2, 3, 4, 5}, 2)).Unwrap()[1], int32(5))
	sliceTestEq(t, rs.Last(rs.Chunks([]int32{0, 1, 2, 3, 4}, 2)).Unwrap()[0], int32(4))
}

func TestSlice_test_chunks_zip(t *testing.T) {
	v1 := []int32{0, 1, 2, 3, 4}
	v2 := []int32{6, 7, 8, 9, 10}
	res := rs.Map(rs.Zip(rs.Chunks(v1, 2), rs.Chunks(v2, 2)), func(p rs.Tuple2[[]int32, []int32]) int32 {
		return rs.Sum(p.F0) + rs.Sum(p.F1)
	})
	sliceTestEq(t, res, []int32{14, 22, 14})
}

func TestSlice_test_chunks_exact_count(t *testing.T) {
	chunks, _ := rs.ChunksExact([]int32{0, 1, 2, 3, 4, 5}, 3)
	sliceTestEq(t, len(chunks), 2)
	chunks, _ = rs.ChunksExact([]int32{0, 1, 2, 3, 4}, 2)
	sliceTestEq(t, len(chunks), 2)
	chunks, _ = rs.ChunksExact([]int32{}, 2)
	sliceTestEq(t, len(chunks), 0)
}

func TestSlice_test_chunks_exact_last(t *testing.T) {
	chunks, _ := rs.ChunksExact([]int32{0, 1, 2, 3, 4, 5}, 2)
	sliceTestEq(t, rs.Last(chunks).Unwrap(), []int32{4, 5})
	chunks, _ = rs.ChunksExact([]int32{0, 1, 2, 3, 4}, 2)
	sliceTestEq(t, rs.Last(chunks).Unwrap(), []int32{2, 3})
}

func TestSlice_test_chunks_exact_remainder(t *testing.T) {
	_, remainder := rs.ChunksExact([]int32{0, 1, 2, 3, 4}, 2)
	sliceTestEq(t, remainder, []int32{4})
}

func TestSlice_test_rchunks_count(t *testing.T) {
	sliceTestEq(t, len(rs.Rchunks([]int32{0, 1, 2, 3, 4, 5}, 3)), 2)
	sliceTestEq(t, len(rs.Rchunks([]int32{0, 1, 2, 3, 4}, 2)), 3)
	sliceTestEq(t, len(rs.Rchunks([]int32{}, 2)), 0)
}

func TestSlice_test_rchunks_last(t *testing.T) {
	sliceTestEq(t, rs.Last(rs.Rchunks([]int32{0, 1, 2, 3, 4, 5}, 2)).Unwrap()[1], int32(1))
	sliceTestEq(t, rs.Last(rs.Rchunks([]int32{0, 1, 2, 3, 4}, 2)).Unwrap()[0], int32(0))
}

func TestSlice_test_windows_count(t *testing.T) {
	sliceTestEq(t, len(rs.Windows([]int32{0, 1, 2, 3, 4, 5}, 3)), 4)
	sliceTestEq(t, len(rs.Windows([]int32{0, 1, 2, 3, 4}, 6)), 0)
	sliceTestEq(t, len(rs.Windows([]int32{}, 2)), 0)
}

func TestSlice_test_windows_last(t *testing.T) {
	sliceTestEq(t, rs.Last(rs.Windows([]int32{0, 1, 2, 3, 4, 5}, 2)).Unwrap()[1], int32(5))
	sliceTestEq(t, rs.Last(rs.Windows([]int32{0, 1, 2, 3, 4}, 2)).Unwrap()[0], int32(3))
}

func TestSlice_test_windows_zip(t *testing.T) {
	v1 := []int32{0, 1, 2, 3, 4}
	v2 := []int32{6, 7, 8, 9, 10}
	res := rs.Map(rs.Zip(rs.Windows(v1, 2), rs.Windows(v2, 2)), func(p rs.Tuple2[[]int32, []int32]) int32 {
		return rs.Sum(p.F0) + rs.Sum(p.F1)
	})
	sliceTestEq(t, res, []int32{14, 18, 22, 26})
}

func TestSlice_test_rotate_left(t *testing.T) {
	const n = 600
	a := make([]int, n)
	for i := range a {
		a[i] = i
	}
	rs.SliceRotateLeft(a, 42)
	k := n - 42
	for i := range n {
		if a[(i+k)%n] != i {
			t.Fatalf("a[%d] = %d, want %d", (i+k)%n, a[(i+k)%n], i)
		}
	}
}

func TestSlice_test_rotate_right(t *testing.T) {
	const n = 600
	a := make([]int, n)
	for i := range a {
		a[i] = i
	}
	rs.SliceRotateRight(a, 42)
	for i := range n {
		if a[(i+42)%n] != i {
			t.Fatalf("a[%d] = %d, want %d", (i+42)%n, a[(i+42)%n], i)
		}
	}
}

func TestSlice_brute_force_rotate_test_0(t *testing.T) {
	// In case of edge cases involving multiple algorithms
	for n := range 300 {
		for s := range n {
			v := make([]int, n)
			for i := range v {
				v[i] = i
			}
			rs.SliceRotateRight(v, s)
			for i := range v {
				if want := (len(v) + i - s) % len(v); v[i] != want {
					t.Fatalf("len %d, rotate_right(%d): v[%d] = %d, want %d", n, s, i, v[i], want)
				}
			}
		}
	}
}

func TestSlice_brute_force_rotate_test_1(t *testing.T) {
	for n := range 30 {
		for s := range n {
			v := make([][4]int, n)
			for i := range v {
				v[i] = [4]int{i, 0, 0, 0}
			}
			rs.SliceRotateRight(v, s)
			for i := range v {
				if want := (len(v) + i - s) % len(v); v[i][0] != want {
					t.Fatalf("len %d, rotate_right(%d): v[%d][0] = %d, want %d", n, s, i, v[i][0], want)
				}
			}
		}
	}
}

func TestSlice_test_is_sorted(t *testing.T) {
	nan := float32(math.NaN())
	negNan := -nan

	// Tests on integers
	sliceTestTrue(t, rs.IsSorted([]int32{1, 2, 2, 9}), "[1, 2, 2, 9].is_sorted()")
	sliceTestTrue(t, !rs.IsSorted([]int32{1, 3, 2}), "![1, 3, 2].is_sorted()")
	sliceTestTrue(t, rs.IsSorted([]int32{0}), "[0].is_sorted()")
	sliceTestTrue(t, rs.IsSorted([]int32{0, 0}), "[0, 0].is_sorted()")
	sliceTestTrue(t, rs.IsSorted([]int32{}), "empty.is_sorted()")

	// Tests on floats
	sliceTestTrue(t, rs.IsSorted([]float32{1.0, 2.0, 2.0, 9.0}), "[1.0, 2.0, 2.0, 9.0].is_sorted()")
	sliceTestTrue(t, !rs.IsSorted([]float32{1.0, 3.0, 2.0}), "![1.0, 3.0, 2.0].is_sorted()")
	sliceTestTrue(t, rs.IsSorted([]float32{0.0}), "[0.0].is_sorted()")
	sliceTestTrue(t, rs.IsSorted([]float32{0.0, 0.0}), "[0.0, 0.0].is_sorted()")
	// Test cases with NaNs
	sliceTestTrue(t, rs.IsSorted([]float32{nan}), "[NAN].is_sorted()")
	for _, v := range [][]float32{
		{nan, nan},
		{0.0, 1.0, nan},
		{nan, nan, nan},
		{1.0, nan, 2.0},
		{2.0, nan, 1.0},
		{2.0, nan, 1.0, 7.0},
		{2.0, nan, 1.0, 0.0},
		{negNan, -1.0, 0.0, 1.0, nan},
		{nan, negNan, -1.0, 0.0, 1.0},
		{1.0, nan, negNan, -1.0, 0.0},
		{0.0, 1.0, nan, negNan, -1.0},
		{-1.0, 0.0, 1.0, nan, negNan},
	} {
		sliceTestTrue(t, !rs.IsSorted(v), "a slice holding NaN next to another value is not sorted")
	}

	// Tests for is_sorted_by
	sliceTestTrue(t, !rs.IsSorted([]int32{6, 2, 8, 5, 1, -60, 1337}), "![6, 2, 8, 5, 1, -60, 1337].is_sorted()")
	sliceTestTrue(t, rs.IsSortedBy([]int32{6, 2, 8, 5, 1, -60, 1337}, func(_, _ int32) bool { return true }),
		"is_sorted_by(|_, _| true)")

	// Tests for is_sorted_by_key
	sliceTestTrue(t, rs.IsSorted([]int32{-2, -1, 0, 3}), "[-2, -1, 0, 3].is_sorted()")
	sliceTestTrue(t, !rs.IsSortedByKey([]int32{-2, -1, 0, 3}, func(n int32) int32 { return rs.Abs(n) }),
		"![-2i32, -1, 0, 3].is_sorted_by_key(|n| n.abs())")
	sliceTestTrue(t, !rs.IsSorted([]string{"c", "bb", "aaa"}), `!["c", "bb", "aaa"].is_sorted()`)
	sliceTestTrue(t, rs.IsSortedByKey([]string{"c", "bb", "aaa"}, func(s string) int { return len(s) }),
		`["c", "bb", "aaa"].is_sorted_by_key(|s| s.len())`)
}

func TestSlice_test_swap(t *testing.T) {
	x := []string{"a", "b", "c", "d"}
	rs.SliceSwap(x, 1, 3)
	sliceTestEq(t, x, []string{"a", "d", "c", "b"})
	rs.SliceSwap(x, 0, 3)
	sliceTestEq(t, x, []string{"b", "d", "c", "a"})
}

func TestSliceSwapPanics_index_a_equals_len(t *testing.T) {
	sliceTestPanics(t, "index out of bounds: the len is 4 but the index is 4", func() {
		rs.SliceSwap([]string{"a", "b", "c", "d"}, 4, 2)
	})
}

func TestSliceSwapPanics_index_b_equals_len(t *testing.T) {
	sliceTestPanics(t, "index out of bounds: the len is 4 but the index is 4", func() {
		rs.SliceSwap([]string{"a", "b", "c", "d"}, 2, 4)
	})
}

func TestSliceSwapPanics_index_a_greater_than_len(t *testing.T) {
	sliceTestPanics(t, "index out of bounds: the len is 4 but the index is 5", func() {
		rs.SliceSwap([]string{"a", "b", "c", "d"}, 5, 2)
	})
}

func TestSliceSwapPanics_index_b_greater_than_len(t *testing.T) {
	sliceTestPanics(t, "index out of bounds: the len is 4 but the index is 5", func() {
		rs.SliceSwap([]string{"a", "b", "c", "d"}, 2, 5)
	})
}
