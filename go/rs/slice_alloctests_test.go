// Port of alloctests 1.99.1 tests/slice.rs and tests/vec.rs (the tests of
// what slice.go, slice_vec.go and iter.go cover)
//
// Left out: tests of capacity, pointers and allocation (Go slices do not
// report them the same way); tests that count drops or panic in Drop
// (test_vec_truncate_fail, test_vec_dedup_panicking, test_dedup_unique's
// boxes, test_extend's DropCounter), which have no Go counterpart; the
// iterator objects' size hints and double-ended stepping; the methods not
// ported (try_remove, extend_from_within, partition_dedup, the _mut
// chunk iterators, split, splitn, rsplit); test_join_nocopy, which joins
// Strings (strs covers str joining). test_retain_pred_panic_* check
// that no element is lost or doubled after the panic, which is what Rust's
// reference counts show there. max_insert, max_remove and max_swap_remove
// use the largest int, Go's usize::MAX.

package rs_test

import (
	"math"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestSliceAlloc_test_get(t *testing.T) {
	a := []int32{11}
	sliceTestEq(t, rs.Get(a, 1), rs.None[int32]())
	a = []int32{11, 12}
	sliceTestEq(t, rs.Get(a, 1).Unwrap(), int32(12))
	a = []int32{11, 12, 13}
	sliceTestEq(t, rs.Get(a, 1).Unwrap(), int32(12))
}

func TestSliceAlloc_test_first(t *testing.T) {
	a := []int32{}
	sliceTestEq(t, rs.First(a), rs.None[int32]())
	a = []int32{11}
	sliceTestEq(t, rs.First(a).Unwrap(), int32(11))
	a = []int32{11, 12}
	sliceTestEq(t, rs.First(a).Unwrap(), int32(11))
}

func TestSliceAlloc_test_first_mut(t *testing.T) {
	a := []int32{}
	_, ok := rs.FirstMut(a)
	sliceTestTrue(t, !ok, "a.first_mut() is None")
	a = []int32{11}
	first, _ := rs.FirstMut(a)
	sliceTestEq(t, *first, int32(11))
	a = []int32{11, 12}
	first, _ = rs.FirstMut(a)
	sliceTestEq(t, *first, int32(11))
}

func TestSliceAlloc_test_split_first(t *testing.T) {
	a := []int32{11}
	b := []int32{}
	sliceTestTrue(t, rs.SplitFirst(b).IsNone(), "b.split_first().is_none()")
	sliceTestEq(t, rs.SplitFirst(a), rs.Some(rs.T2(int32(11), b)))
	a = []int32{11, 12}
	b = []int32{12}
	sliceTestEq(t, rs.SplitFirst(a), rs.Some(rs.T2(int32(11), b)))
}

func TestSliceAlloc_test_split_last(t *testing.T) {
	a := []int32{11}
	b := []int32{}
	sliceTestTrue(t, rs.SplitLast(b).IsNone(), "b.split_last().is_none()")
	sliceTestEq(t, rs.SplitLast(a), rs.Some(rs.T2(int32(11), b)))
	a = []int32{11, 12}
	b = []int32{11}
	sliceTestEq(t, rs.SplitLast(a), rs.Some(rs.T2(int32(12), b)))
}

func TestSliceAlloc_test_last(t *testing.T) {
	a := []int32{}
	sliceTestEq(t, rs.Last(a), rs.None[int32]())
	a = []int32{11}
	sliceTestEq(t, rs.Last(a).Unwrap(), int32(11))
	a = []int32{11, 12}
	sliceTestEq(t, rs.Last(a).Unwrap(), int32(12))
}

func TestSliceAlloc_test_last_mut(t *testing.T) {
	a := []int32{}
	_, ok := rs.LastMut(a)
	sliceTestTrue(t, !ok, "a.last_mut() is None")
	a = []int32{11}
	last, _ := rs.LastMut(a)
	sliceTestEq(t, *last, int32(11))
	a = []int32{11, 12}
	last, _ = rs.LastMut(a)
	sliceTestEq(t, *last, int32(12))
}

func TestSliceAlloc_test_pop(t *testing.T) {
	v := []int32{5}
	e := rs.Pop(&v)
	sliceTestEq(t, len(v), 0)
	sliceTestEq(t, e, rs.Some(int32(5)))
	sliceTestEq(t, rs.Pop(&v), rs.None[int32]())
	sliceTestEq(t, rs.Pop(&v), rs.None[int32]())
}

func TestSliceAlloc_test_swap_remove(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	e := rs.SwapRemove(&v, 0)
	sliceTestEq(t, e, int32(1))
	sliceTestEq(t, v, []int32{5, 2, 3, 4})
	e = rs.SwapRemove(&v, 3)
	sliceTestEq(t, e, int32(4))
	sliceTestEq(t, v, []int32{5, 2, 3})
}

func TestSliceAlloc_test_swap_remove_fail(t *testing.T) {
	v := []int32{1}
	rs.SwapRemove(&v, 0)
	sliceTestPanics(t, "swap_remove index (is 0) should be < len (is 0)", func() { rs.SwapRemove(&v, 0) })
}

func TestSliceAlloc_test_push(t *testing.T) {
	v := []int32{}
	rs.Push(&v, 1)
	sliceTestEq(t, len(v), 1)
	sliceTestEq(t, v[0], int32(1))
	rs.Push(&v, 2)
	sliceTestEq(t, len(v), 2)
	sliceTestEq(t, v[0], int32(1))
	sliceTestEq(t, v[1], int32(2))
}

func TestSliceAlloc_test_truncate(t *testing.T) {
	six, five, four := 6, 5, 4
	v := []*int{&six, &five, &four}
	rs.Truncate(&v, 1)
	sliceTestEq(t, len(v), 1)
	sliceTestEq(t, *v[0], 6)
}

func TestSliceAlloc_test_retain(t *testing.T) {
	v := []int{1, 2, 3, 4, 5}
	rs.Retain(&v, func(n *int) bool { return *n%2 == 1 })
	sliceTestEq(t, v, []int{1, 3, 5})
}

func TestSliceAlloc_test_reverse(t *testing.T) {
	v := []int32{10, 20}
	sliceTestEq(t, v[0], int32(10))
	sliceTestEq(t, v[1], int32(20))
	rs.SliceReverse(v)
	sliceTestEq(t, v[0], int32(20))
	sliceTestEq(t, v[1], int32(10))

	v3 := []int32{}
	rs.SliceReverse(v3)
	sliceTestTrue(t, len(v3) == 0, "v3.is_empty()")

	v8, want8 := []int8{}, []int8{}
	for i := int8(-50); i < 51; i++ {
		v8 = append(v8, i)
		want8 = append([]int8{i}, want8...)
	}
	rs.SliceReverse(v8)
	sliceTestEq(t, v8, want8)

	v16, want16 := []int16{}, []int16{}
	for i := int16(-50); i < 51; i++ {
		v16 = append(v16, i)
		want16 = append([]int16{i}, want16...)
	}
	rs.SliceReverse(v16)
	sliceTestEq(t, v16, want16)
}

func TestSliceAlloc_test_rotate_left(t *testing.T) {
	expected := []int32{1, 2, 3, 4, 5}

	// no-ops
	v := []int32{1, 2, 3, 4, 5}
	rs.SliceRotateLeft(v, 0)
	sliceTestEq(t, v, expected)
	rs.SliceRotateLeft(v, 5)
	sliceTestEq(t, v, expected)
	v = []int32{}
	rs.SliceRotateLeft(v, 0)
	sliceTestEq(t, v, []int32{})

	// happy path
	v = []int32{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13}
	rs.SliceRotateLeft(v, 7)
	sliceTestEq(t, v, []int32{8, 9, 10, 11, 12, 13, 1, 2, 3, 4, 5, 6, 7})

	v = make([]int32, 1000)
	for i := range v {
		v[i] = int32(i)
	}
	rs.SliceRotateLeft(v[:500], 100)
	for i := range 500 {
		sliceTestEq(t, v[i], int32((i+100)%500))
	}
	rs.SliceRotateLeft(v[500:], 300)
	for i := range 500 {
		sliceTestEq(t, v[500+i], int32(500+(i+300)%500))
	}

	sliceTestPanics(t, "assertion failed: mid <= self.len()", func() { rs.SliceRotateLeft([]int32{1, 2}, 3) })
}

func TestSliceAlloc_test_rotate_right(t *testing.T) {
	expected := []int32{1, 2, 3, 4, 5}

	// no-ops
	v := []int32{1, 2, 3, 4, 5}
	rs.SliceRotateRight(v, 0)
	sliceTestEq(t, v, expected)
	rs.SliceRotateRight(v, 5)
	sliceTestEq(t, v, expected)
	v = []int32{}
	rs.SliceRotateRight(v, 0)
	sliceTestEq(t, v, []int32{})

	// happy path
	v = []int32{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13}
	rs.SliceRotateRight(v, 6)
	sliceTestEq(t, v, []int32{8, 9, 10, 11, 12, 13, 1, 2, 3, 4, 5, 6, 7})

	v = make([]int32, 1000)
	for i := range v {
		v[i] = int32(i)
	}
	rs.SliceRotateRight(v[:500], 100)
	for i := range 500 {
		sliceTestEq(t, v[i], int32((i+400)%500))
	}
	rs.SliceRotateRight(v[500:], 300)
	for i := range 500 {
		sliceTestEq(t, v[500+i], int32(500+(i+200)%500))
	}

	sliceTestPanics(t, "assertion failed: k <= self.len()", func() { rs.SliceRotateRight([]int32{1, 2}, 3) })
}

func TestSliceAlloc_test_concat(t *testing.T) {
	sliceTestEq(t, rs.Concat([][]int32{}), []int32{})
	sliceTestEq(t, rs.Concat([][]int32{{1}, {2, 3}}), []int32{1, 2, 3})
	sliceTestEq(t, rs.Join([][]int32{{1}, {2, 3}}, 0), []int32{1, 0, 2, 3})
	sliceTestEq(t, rs.Join([][]int32{{1}, {2}, {3}}, 0), []int32{1, 0, 2, 0, 3})
}

func TestSliceAlloc_test_join(t *testing.T) {
	sliceTestEq(t, rs.Join([][]int32{}, 0), []int32{})
	sliceTestEq(t, rs.Join([][]int32{{1}, {2, 3}}, 0), []int32{1, 0, 2, 3})
	sliceTestEq(t, rs.Join([][]int32{{1}, {2}, {3}}, 0), []int32{1, 0, 2, 0, 3})
}

func TestSliceAlloc_test_insert(t *testing.T) {
	a := []int32{1, 2, 4}
	rs.Insert(&a, 2, 3)
	sliceTestEq(t, a, []int32{1, 2, 3, 4})

	a = []int32{1, 2, 3}
	rs.Insert(&a, 0, 0)
	sliceTestEq(t, a, []int32{0, 1, 2, 3})

	a = []int32{1, 2, 3}
	rs.Insert(&a, 3, 4)
	sliceTestEq(t, a, []int32{1, 2, 3, 4})

	a = []int32{}
	rs.Insert(&a, 0, 1)
	sliceTestEq(t, a, []int32{1})
}

func TestSliceAlloc_test_insert_oob(t *testing.T) {
	a := []int32{1, 2, 3}
	sliceTestPanics(t, "insertion index (is 4) should be <= len (is 3)", func() { rs.Insert(&a, 4, 5) })
}

func TestSliceAlloc_test_remove(t *testing.T) {
	a := []int32{1, 2, 3, 4}
	sliceTestEq(t, rs.Remove(&a, 2), int32(3))
	sliceTestEq(t, a, []int32{1, 2, 4})
	sliceTestEq(t, rs.Remove(&a, 2), int32(4))
	sliceTestEq(t, a, []int32{1, 2})
	sliceTestEq(t, rs.Remove(&a, 0), int32(1))
	sliceTestEq(t, a, []int32{2})
	sliceTestEq(t, rs.Remove(&a, 0), int32(2))
	sliceTestEq(t, a, []int32{})
}

func TestSliceAlloc_test_remove_fail(t *testing.T) {
	a := []int32{1}
	rs.Remove(&a, 0)
	sliceTestPanics(t, "removal index (is 0) should be < len (is 0)", func() { rs.Remove(&a, 0) })
}

func TestSliceAlloc_test_windows_iterator(t *testing.T) {
	v := []int32{1, 2, 3, 4}
	sliceTestEq(t, rs.Windows(v, 2), [][]int32{{1, 2}, {2, 3}, {3, 4}})
	sliceTestEq(t, rs.Windows(v, 3), [][]int32{{1, 2, 3}, {2, 3, 4}})
	sliceTestTrue(t, len(rs.Windows(v, 6)) == 0, "v.windows(6).next().is_none()")
	sliceTestEq(t, rs.Rev(rs.Windows(v, 2)), [][]int32{{3, 4}, {2, 3}, {1, 2}})
}

func TestSliceAlloc_test_windows_iterator_0(t *testing.T) {
	sliceTestPanics(t, "window size must be non-zero", func() { rs.Windows([]int32{1, 2, 3, 4}, 0) })
}

func TestSliceAlloc_test_chunks_iterator(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	sliceTestEq(t, len(rs.Chunks(v, 2)), 3)
	sliceTestEq(t, rs.Chunks(v, 2), [][]int32{{1, 2}, {3, 4}, {5}})
	sliceTestEq(t, rs.Chunks(v, 3), [][]int32{{1, 2, 3}, {4, 5}})
	sliceTestEq(t, rs.Chunks(v, 6), [][]int32{{1, 2, 3, 4, 5}})
	sliceTestEq(t, rs.Rev(rs.Chunks(v, 2)), [][]int32{{5}, {3, 4}, {1, 2}})
}

func TestSliceAlloc_test_chunks_iterator_0(t *testing.T) {
	sliceTestPanics(t, "chunk size must be non-zero", func() { rs.Chunks([]int32{1, 2, 3, 4}, 0) })
}

func TestSliceAlloc_test_chunks_exact_iterator(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	chunks, _ := rs.ChunksExact(v, 2)
	sliceTestEq(t, len(chunks), 2)
	sliceTestEq(t, chunks, [][]int32{{1, 2}, {3, 4}})
	chunks, _ = rs.ChunksExact(v, 3)
	sliceTestEq(t, chunks, [][]int32{{1, 2, 3}})
	chunks, _ = rs.ChunksExact(v, 6)
	sliceTestEq(t, chunks, [][]int32{})
	chunks, _ = rs.ChunksExact(v, 2)
	sliceTestEq(t, rs.Rev(chunks), [][]int32{{3, 4}, {1, 2}})
}

func TestSliceAlloc_test_chunks_exact_iterator_0(t *testing.T) {
	sliceTestPanics(t, "chunk size must be non-zero", func() { rs.ChunksExact([]int32{1, 2, 3, 4}, 0) })
}

func TestSliceAlloc_test_rchunks_iterator(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	sliceTestEq(t, len(rs.Rchunks(v, 2)), 3)
	sliceTestEq(t, rs.Rchunks(v, 2), [][]int32{{4, 5}, {2, 3}, {1}})
	sliceTestEq(t, rs.Rchunks(v, 3), [][]int32{{3, 4, 5}, {1, 2}})
	sliceTestEq(t, rs.Rchunks(v, 6), [][]int32{{1, 2, 3, 4, 5}})
	sliceTestEq(t, rs.Rev(rs.Rchunks(v, 2)), [][]int32{{1}, {2, 3}, {4, 5}})
}

func TestSliceAlloc_test_rchunks_iterator_0(t *testing.T) {
	sliceTestPanics(t, "chunk size must be non-zero", func() { rs.Rchunks([]int32{1, 2, 3, 4}, 0) })
}

func TestSliceAlloc_test_starts_with(t *testing.T) {
	b := func(s string) []byte { return []byte(s) }
	sliceTestTrue(t, rs.StartsWith(b("foobar"), b("foo")), `b"foobar".starts_with(b"foo")`)
	sliceTestTrue(t, !rs.StartsWith(b("foobar"), b("oob")), `!b"foobar".starts_with(b"oob")`)
	sliceTestTrue(t, !rs.StartsWith(b("foobar"), b("bar")), `!b"foobar".starts_with(b"bar")`)
	sliceTestTrue(t, !rs.StartsWith(b("foo"), b("foobar")), `!b"foo".starts_with(b"foobar")`)
	sliceTestTrue(t, !rs.StartsWith(b("bar"), b("foobar")), `!b"bar".starts_with(b"foobar")`)
	sliceTestTrue(t, rs.StartsWith(b("foobar"), b("foobar")), `b"foobar".starts_with(b"foobar")`)
	empty := []byte{}
	sliceTestTrue(t, rs.StartsWith(empty, empty), "empty.starts_with(empty)")
	sliceTestTrue(t, !rs.StartsWith(empty, b("foo")), `!empty.starts_with(b"foo")`)
	sliceTestTrue(t, rs.StartsWith(b("foobar"), empty), `b"foobar".starts_with(empty)`)
}

func TestSliceAlloc_test_ends_with(t *testing.T) {
	b := func(s string) []byte { return []byte(s) }
	sliceTestTrue(t, rs.EndsWith(b("foobar"), b("bar")), `b"foobar".ends_with(b"bar")`)
	sliceTestTrue(t, !rs.EndsWith(b("foobar"), b("oba")), `!b"foobar".ends_with(b"oba")`)
	sliceTestTrue(t, !rs.EndsWith(b("foobar"), b("foo")), `!b"foobar".ends_with(b"foo")`)
	sliceTestTrue(t, !rs.EndsWith(b("foo"), b("foobar")), `!b"foo".ends_with(b"foobar")`)
	sliceTestTrue(t, !rs.EndsWith(b("bar"), b("foobar")), `!b"bar".ends_with(b"foobar")`)
	sliceTestTrue(t, rs.EndsWith(b("foobar"), b("foobar")), `b"foobar".ends_with(b"foobar")`)
	empty := []byte{}
	sliceTestTrue(t, rs.EndsWith(empty, empty), "empty.ends_with(empty)")
	sliceTestTrue(t, !rs.EndsWith(empty, b("foo")), `!empty.ends_with(b"foo")`)
	sliceTestTrue(t, rs.EndsWith(b("foobar"), empty), `b"foobar".ends_with(empty)`)
}

func TestSliceAlloc_test_get_mut(t *testing.T) {
	v := []int32{0, 1, 2}
	_, ok := rs.GetMut(v, 3)
	sliceTestTrue(t, !ok, "v.get_mut(3) is None")
	if e, ok := rs.GetMut(v, 1); ok {
		*e = 7
	}
	sliceTestEq(t, v[1], int32(7))
	e, ok := rs.GetMut(v, 2)
	sliceTestTrue(t, ok && *e == 2, "v.get_mut(2) == Some(&mut 2)")
}

func TestSliceAlloc_test_mut_last(t *testing.T) {
	x := []int32{1, 2, 3, 4, 5}
	h, _ := rs.LastMut(x)
	sliceTestEq(t, *h, int32(5))
	_, ok := rs.LastMut([]int32{})
	sliceTestTrue(t, !ok, "y.last_mut().is_none()")
}

func TestSliceAlloc_test_chunk_by(t *testing.T) {
	slice := []int32{1, 1, 1, 3, 3, 2, 2, 2, 1, 0}
	chunks := rs.ChunkBy(slice, func(a, b int32) bool { return a == b })
	sliceTestEq(t, chunks, [][]int32{{1, 1, 1}, {3, 3}, {2, 2, 2}, {1}, {0}})
	sliceTestEq(t, rs.Rev(chunks), [][]int32{{0}, {1}, {2, 2, 2}, {3, 3}, {1, 1, 1}})
}

func TestSliceAlloc_repeat_generic_slice(t *testing.T) {
	sliceTestEq(t, rs.Repeat([]int32{1, 2}, 2), []int32{1, 2, 1, 2})
	sliceTestEq(t, rs.Repeat([]int32{1, 2, 3, 4}, 0), []int32{})
	sliceTestEq(t, rs.Repeat([]int32{1, 2, 3, 4}, 1), []int32{1, 2, 3, 4})
	sliceTestEq(t, rs.Repeat([]int32{1, 2, 3, 4}, 3), []int32{1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4})
}

// --- tests/vec.rs ---

func TestVec_test_extend(t *testing.T) {
	v, w := []int{}, []int{}
	rs.Extend(&v, rs.Clone(w))
	sliceTestEq(t, v, []int{})
	rs.Extend(&v, []int{0, 1, 2})
	for i := range 3 {
		w = append(w, i)
	}
	sliceTestEq(t, v, w)
	rs.Extend(&v, []int{3, 4, 5, 6, 7, 8, 9})
	for i := 3; i < 10; i++ {
		w = append(w, i)
	}
	sliceTestEq(t, v, w)
	rs.Extend(&v, rs.Clone(w))
	sliceTestEq(t, v, rs.Chain(w, w))
}

func TestVec_test_extend_from_slice(t *testing.T) {
	v := []int{1, 2, 3, 4, 5}
	rs.ExtendFromSlice(&v, []int{6, 7, 8, 9, 0})
	sliceTestEq(t, v, []int{1, 2, 3, 4, 5, 6, 7, 8, 9, 0})
}

func TestVec_test_retain(t *testing.T) {
	vec := []int{1, 2, 3, 4}
	rs.Retain(&vec, func(x *int) bool { return *x%2 == 0 })
	sliceTestEq(t, vec, []int{2, 4})
}

func TestVec_test_retain_predicate_order(t *testing.T) {
	for _, toKeep := range []bool{true, false} {
		executions := 0
		vec := []int{1, 2, 3, 4}
		nextExpected := 1
		rs.Retain(&vec, func(x *int) bool {
			sliceTestEq(t, *x, nextExpected)
			nextExpected++
			executions++
			return toKeep
		})
		sliceTestEq(t, executions, 4)
	}
}

func sliceTestRetainPanics(f func(*int) bool) []int {
	v := []int{0, 1, 2, 3, 4}
	func() {
		defer func() { _ = recover() }()
		rs.Retain(&v, f)
	}()
	return v
}

func TestVec_test_retain_pred_panic_with_hole(t *testing.T) {
	v := sliceTestRetainPanics(func(r *int) bool {
		switch *r {
		case 0, 2:
			return true
		case 1:
			return false
		}
		panic("explicit panic")
	})
	sliceTestEq(t, v, []int{0, 2, 3, 4})
}

func TestVec_test_retain_pred_panic_no_hole(t *testing.T) {
	v := sliceTestRetainPanics(func(r *int) bool {
		if *r <= 2 {
			return true
		}
		panic("explicit panic")
	})
	sliceTestEq(t, v, []int{0, 1, 2, 3, 4})
}

func TestVec_test_dedup(t *testing.T) {
	check := func(a, b []int32) {
		t.Helper()
		rs.Dedup(&a)
		sliceTestEq(t, a, b)
	}
	check([]int32{}, []int32{})
	check([]int32{1}, []int32{1})
	check([]int32{1, 1}, []int32{1})
	check([]int32{1, 2, 3}, []int32{1, 2, 3})
	check([]int32{1, 1, 2, 3}, []int32{1, 2, 3})
	check([]int32{1, 2, 2, 3}, []int32{1, 2, 3})
	check([]int32{1, 2, 3, 3}, []int32{1, 2, 3})
	check([]int32{1, 1, 2, 2, 2, 3, 3}, []int32{1, 2, 3})
}

func TestVec_test_dedup_by_key(t *testing.T) {
	check := func(a, b []int32) {
		t.Helper()
		rs.DedupByKey(&a, func(i *int32) int32 { return *i / 10 })
		sliceTestEq(t, a, b)
	}
	check([]int32{}, []int32{})
	check([]int32{10}, []int32{10})
	check([]int32{10, 11}, []int32{10})
	check([]int32{10, 20, 30}, []int32{10, 20, 30})
	check([]int32{10, 11, 20, 30}, []int32{10, 20, 30})
	check([]int32{10, 20, 21, 30}, []int32{10, 20, 30})
	check([]int32{10, 20, 30, 31}, []int32{10, 20, 30})
	check([]int32{10, 11, 20, 21, 22, 30, 31}, []int32{10, 20, 30})
}

func TestVec_test_dedup_by(t *testing.T) {
	vec := []string{"foo", "bar", "Bar", "baz", "bar"}
	rs.DedupBy(&vec, func(a, b *string) bool { return strings.EqualFold(*a, *b) })
	sliceTestEq(t, vec, []string{"foo", "bar", "baz", "bar"})

	pairs := []rs.Tuple2[string, int]{rs.T2("foo", 1), rs.T2("foo", 2), rs.T2("bar", 3), rs.T2("bar", 4), rs.T2("bar", 5)}
	rs.DedupBy(&pairs, func(a, b *rs.Tuple2[string, int]) bool {
		if a.F0 == b.F0 {
			b.F1 += a.F1
			return true
		}
		return false
	})
	sliceTestEq(t, pairs, []rs.Tuple2[string, int]{rs.T2("foo", 3), rs.T2("bar", 12)})
}

func TestVec_test_dedup_unique(t *testing.T) {
	box := func(n int) *int { return &n }
	v0 := []*int{box(1), box(1), box(2), box(3)}
	rs.Dedup(&v0)
	sliceTestEq(t, len(v0), 3)
	v1 := []*int{box(1), box(2), box(2), box(3)}
	rs.Dedup(&v1)
	sliceTestEq(t, len(v1), 3)
	v2 := []*int{box(1), box(2), box(3), box(3)}
	rs.Dedup(&v2)
	sliceTestEq(t, len(v2), 3)
}

func TestVec_test_partition(t *testing.T) {
	check := func(v []int32, x int32, yes, no []int32) {
		t.Helper()
		gotYes, gotNo := rs.Partition(v, func(e int32) bool { return e < x })
		sliceTestEq(t, gotYes, yes)
		sliceTestEq(t, gotNo, no)
	}
	check([]int32{}, 3, []int32{}, []int32{})
	check([]int32{1, 2, 3}, 4, []int32{1, 2, 3}, []int32{})
	check([]int32{1, 2, 3}, 2, []int32{1}, []int32{2, 3})
	check([]int32{1, 2, 3}, 0, []int32{}, []int32{1, 2, 3})
}

func TestVec_test_swap_remove_empty(t *testing.T) {
	vec := []int32{}
	sliceTestPanics(t, "swap_remove index (is 0) should be < len (is 0)", func() { rs.SwapRemove(&vec, 0) })
}

func TestVec_test_drain_empty_vec(t *testing.T) {
	vec, vec2 := []int32{}, []int32{}
	for _, i := range rs.Drain(&vec, 0, len(vec)) {
		vec2 = append(vec2, i)
	}
	sliceTestTrue(t, len(vec) == 0, "vec.is_empty()")
	sliceTestTrue(t, len(vec2) == 0, "vec2.is_empty()")
}

func TestVec_test_drain_items(t *testing.T) {
	vec, vec2 := []int32{1, 2, 3}, []int32{}
	for _, i := range rs.Drain(&vec, 0, len(vec)) {
		vec2 = append(vec2, i)
	}
	sliceTestEq(t, vec, []int32{})
	sliceTestEq(t, vec2, []int32{1, 2, 3})
}

func TestVec_test_drain_out_of_bounds(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	sliceTestPanics(t, "range end index 6 out of range for slice of length 5", func() { rs.Drain(&v, 5, 6) })
}

func TestVec_test_drain_range(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	rs.Drain(&v, 4, len(v))
	sliceTestEq(t, v, []int32{1, 2, 3, 4})

	strs := func() []string { return rs.Map([]int{1, 2, 3, 4, 5}, strconv.Itoa) }
	s := strs()
	rs.Drain(&s, 1, 4)
	sliceTestEq(t, s, []string{"1", "5"})

	s = strs()
	rs.Rev(rs.Drain(&s, 1, 4))
	sliceTestEq(t, s, []string{"1", "5"})

	u := make([]struct{}, 5)
	rs.Drain(&u, 1, 4)
	sliceTestEq(t, u, []struct{}{{}, {}})
}

func TestVec_test_drain_inclusive_range(t *testing.T) {
	v := []rune{'a', 'b', 'c', 'd', 'e'}
	rs.Drain(&v, 1, 3+1)
	sliceTestEq(t, v, []rune{'a', 'e'})

	strs := func(n int) []string {
		out := []string{}
		for i := 0; i <= n; i++ {
			out = append(out, strconv.Itoa(i))
		}
		return out
	}
	s := strs(5)
	rs.Drain(&s, 1, 5+1)
	sliceTestEq(t, s, []string{"0"})

	s = strs(5)
	rs.Drain(&s, 0, 5+1)
	sliceTestEq(t, s, []string{})

	s = strs(5)
	rs.Drain(&s, 0, 3+1)
	sliceTestEq(t, s, []string{"4", "5"})

	s = strs(1)
	rs.Drain(&s, 0, 0+1)
	sliceTestEq(t, s, []string{"1"})
}

// `v.drain(5..=5)` is the range (5, 6).
func TestVec_test_drain_inclusive_out_of_bounds(t *testing.T) {
	v := []int32{1, 2, 3, 4, 5}
	sliceTestPanics(t, "range end index 6 out of range for slice of length 5", func() { rs.Drain(&v, 5, 5+1) })
}

func TestVec_test_append(t *testing.T) {
	vec := []int32{1, 2, 3}
	vec2 := []int32{4, 5, 6}
	rs.Append(&vec, &vec2)
	sliceTestEq(t, vec, []int32{1, 2, 3, 4, 5, 6})
	sliceTestEq(t, vec2, []int32{})
}

func TestVec_test_split_off(t *testing.T) {
	vec := []int32{1, 2, 3, 4, 5, 6}
	splitOff := rs.SplitOff(&vec, 4)
	sliceTestEq(t, vec, []int32{1, 2, 3, 4})
	sliceTestEq(t, splitOff, []int32{5, 6})
}

func TestVec_test_split_off_take_all(t *testing.T) {
	vec := make([]int32, 0, 1000)
	vec = append(vec, 1, 2, 3, 4, 5, 6)
	splitOff := rs.SplitOff(&vec, 0)
	sliceTestEq(t, vec, []int32{})
	sliceTestEq(t, splitOff, []int32{1, 2, 3, 4, 5, 6})
	sliceTestTrue(t, cap(vec) == 1000, "vec.capacity() == orig_capacity")
}

func TestVec_test_vec_swap(t *testing.T) {
	a := []int{0, 1, 2, 3, 4, 5, 6}
	rs.SliceSwap(a, 2, 4)
	sliceTestEq(t, a[2], 4)
	sliceTestEq(t, a[4], 2)
	n := 42
	rs.Swap(&n, &a[0])
	sliceTestEq(t, a[0], 42)
	sliceTestEq(t, n, 0)
}

func TestVec_test_vec_dedup_by(t *testing.T) {
	vec := []int32{1, -1, 2, 3, 1, -5, 5, -2, 2}
	rs.DedupBy(&vec, func(a, b *int32) bool { return rs.Abs(*a) == rs.Abs(*b) })
	sliceTestEq(t, vec, []int32{1, 2, 3, 1, -5, -2})
}

func TestVec_test_vec_dedup_empty(t *testing.T) {
	vec := []int32{}
	rs.Dedup(&vec)
	sliceTestEq(t, vec, []int32{})
}

func TestVec_test_vec_dedup_one(t *testing.T) {
	vec := []int32{12}
	rs.Dedup(&vec)
	sliceTestEq(t, vec, []int32{12})
}

func TestVec_test_vec_dedup_multiple_ident(t *testing.T) {
	vec := []int32{12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11}
	rs.Dedup(&vec)
	sliceTestEq(t, vec, []int32{12, 11})
}

// sliceTestFoo is test_vec_dedup_partialeq's Foo, equal by its first field.
type sliceTestFoo struct{ F0, F1 int32 }

func (f *sliceTestFoo) Equal(other *sliceTestFoo) bool { return f.F0 == other.F0 }

func TestVec_test_vec_dedup_partialeq(t *testing.T) {
	vec := []sliceTestFoo{{0, 1}, {0, 5}, {1, 7}, {1, 9}}
	rs.Dedup(&vec)
	sliceTestEq(t, vec, []sliceTestFoo{{0, 1}, {1, 7}})
}

func TestVec_test_vec_dedup(t *testing.T) {
	for x := range uint8(255) {
		vec := make([]bool, 0, 8)
		for bit := range 8 {
			vec = append(vec, (x>>bit)&1 == 1)
		}
		// partition_dedup's first part, as Rust's test takes it.
		dedup := []bool{}
		for i, b := range vec {
			if i == 0 || b != vec[i-1] {
				dedup = append(dedup, b)
			}
		}
		rs.Dedup(&vec)
		sliceTestEq(t, vec, dedup)
	}
}

func TestVec_test_pop_if(t *testing.T) {
	v := []int32{1, 2, 3, 4}
	pred := func(x *int32) bool { return *x%2 == 0 }
	sliceTestEq(t, rs.PopIf(&v, pred), rs.Some(int32(4)))
	sliceTestEq(t, v, []int32{1, 2, 3})
	sliceTestEq(t, rs.PopIf(&v, pred), rs.None[int32]())
	sliceTestEq(t, v, []int32{1, 2, 3})
}

func TestVec_test_pop_if_empty(t *testing.T) {
	v := []int32{}
	sliceTestEq(t, rs.PopIf(&v, func(*int32) bool { return true }), rs.None[int32]())
	sliceTestTrue(t, len(v) == 0, "v.is_empty()")
}

func TestVec_test_pop_if_mutates(t *testing.T) {
	v := []int32{1}
	pred := func(x *int32) bool {
		*x++
		return false
	}
	sliceTestEq(t, rs.PopIf(&v, pred), rs.None[int32]())
	sliceTestEq(t, v, []int32{2})
}

func TestVec_max_insert(t *testing.T) {
	v := []int32{0}
	sliceTestPanics(t, "insertion index (is "+strconv.Itoa(math.MaxInt)+") should be <= len (is 1)", func() {
		rs.Insert(&v, math.MaxInt, 1)
	})
}

func TestVec_max_remove(t *testing.T) {
	v := []int32{0}
	sliceTestPanics(t, "removal index (is "+strconv.Itoa(math.MaxInt)+") should be < len (is 1)", func() {
		rs.Remove(&v, math.MaxInt)
	})
}

func TestVec_max_swap_remove(t *testing.T) {
	v := []int32{0}
	sliceTestPanics(t, "swap_remove index (is "+strconv.Itoa(math.MaxInt)+") should be < len (is 1)", func() {
		rs.SwapRemove(&v, math.MaxInt)
	})
}
