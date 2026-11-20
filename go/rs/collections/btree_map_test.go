// Port of alloc 1.99.1 src/collections/btree/map/tests.rs

package collections

// Rust's map is a B-tree whose nodes hold up to node::CAPACITY (11) entries;
// the port is a balanced binary tree (btree_node.go). The sizes the tests
// use to reach a given B-tree height (MIN_INSERTS_HEIGHT_1 and _2) are kept,
// and `map.height()` is asked only what a caller can observe: whether the
// map has a root at all (None or Some), which decides whether `range`
// checks its bounds. check() is the port's check of its own tree.
//
// Not ported:
//   - test_levels and test_insert_into_full_height_1, which check the node
//     layout itself;
//   - test_iter_mixed, test_iter_entering_root_twice and
//     test_iter_descending_to_same_node_twice, which drive one iterator from
//     both ends, and the size_hint assertions of test_iter and
//     test_iter_rev: the port's sequences run in one direction and have no
//     size hint;
//   - test_range_finding_ill_order_in_range_ord, which looks a range up by a
//     borrowed part of the key: the port's bounds are keys;
//   - test_append_ord_chaos, which pins what Rust's leaf search does with
//     keys whose order is not transitive (Cyclic3): it scans a node's keys
//     from the left, so a second B meets C first and lands before it,
//     giving a map of four keys where the port's tree compares B with its
//     root first, finds the existing B, and keeps three. Rust leaves the
//     behaviour under such an order unspecified;
//   - test_try_insert, the cursor tests, the merge tests and
//     test_id_based_merge: try_insert, cursors and merge are unstable, and
//     Mjolnir uses none of them;
//   - the tests of drops and leaks after a panic (test_clear_drop_panic_leak,
//     test_clone_panic_leak_*, test_append_drop_leak,
//     test_into_iter_drop_leak_*, test_extract_if::drop_panic_leak) and
//     test_extract_if::pred_panic_reuse: Go has no destructors, and a Go
//     sequence ranged again starts over instead of resuming;
//   - assert_covariance, assert_sync and assert_send, which are checks of
//     the compiler.

import (
	"iter"
	"math"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const (
	// btreeNodeCapacity is node::CAPACITY in Rust's B-tree.
	btreeNodeCapacity = 11
	// Minimum number of elements to insert, to guarantee a tree with 2
	// levels in Rust, i.e., a tree who's root is an internal node at height
	// 1, with edges to leaf nodes.
	minInsertsHeight1 = btreeNodeCapacity + 1
	// Minimum number of elements to insert in ascending order, to guarantee
	// a tree with 3 levels in Rust.
	minInsertsHeight2 = 89
)

// hasRoot is `map.height().is_some()`.
func (m BTreeMap[K, V]) hasRoot() bool { return m.rooted }

// optionOf is the Option<&V> that get and friends return in Rust.
func optionOf[V any](value *V, ok bool) rs.Option[V] {
	if !ok {
		return rs.None[V]()
	}
	return rs.Some(*value)
}

// optionPair is the Option<(&K, &V)> of first_key_value and friends.
func optionPair[K, V any](key K, value *V, ok bool) rs.Option[rs.Tuple2[K, V]] {
	if !ok {
		return rs.None[rs.Tuple2[K, V]]()
	}
	return rs.Some(rs.T2(key, *value))
}

// derefValues is `values().copied()`.
func derefValues[V any](seq iter.Seq[*V]) iter.Seq[V] {
	return func(yield func(V) bool) {
		for value := range seq {
			if !yield(*value) {
				return
			}
		}
	}
}

// derefPairs is a map borrowed for extend: `map.extend(&other)`.
func derefPairs[K, V any](seq iter.Seq2[K, *V]) iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		for key, value := range seq {
			if !yield(key, *value) {
				return
			}
		}
	}
}

// Ensures the testing infrastructure usually notices order violations.
func TestBTreeMap_test_check_ord_chaos(t *testing.T) {
	gov := &governor{}
	m := BTreeMapFrom(rs.T2(governed{1, gov}, rs.Unit{}), rs.T2(governed{2, gov}, rs.Unit{}))
	gov.flip()
	if m.checkInvariants() == nil {
		t.Fatal("check did not notice the order violation")
	}
}

// Ensures the testing infrastructure doesn't always mind order violations.
func TestBTreeMap_test_check_invariants_ord_chaos(t *testing.T) {
	gov := &governor{}
	m := BTreeMapFrom(rs.T2(governed{1, gov}, rs.Unit{}), rs.T2(governed{2, gov}, rs.Unit{}))
	gov.flip()
	m.checkStructure(t)
}

func TestBTreeMap_test_basic_large(t *testing.T) {
	var m BTreeMap[int, int]
	size := 10000
	size = size + (size % 2) // round up to even number
	expectEq(t, m.Len(), 0)

	for i := range size {
		expectEq(t, m.Insert(i, 10*i), rs.None[int]())
		expectEq(t, m.Len(), i+1)
	}

	expectEq(t, optionPair(m.FirstKeyValue()), rs.Some(rs.T2(0, 0)))
	expectEq(t, optionPair(m.LastKeyValue()), rs.Some(rs.T2(size-1, 10*(size-1))))
	expectEq(t, m.FirstEntry().Unwrap().Key(), 0)
	expectEq(t, m.LastEntry().Unwrap().Key(), size-1)

	for i := range size {
		expectEq(t, optionOf(m.Get(i)).Unwrap(), i*10)
	}

	for i := size; i < size*2; i++ {
		expectEq(t, optionOf(m.Get(i)), rs.None[int]())
	}

	for i := range size {
		expectEq(t, m.Insert(i, 100*i), rs.Some(10*i))
		expectEq(t, m.Len(), size)
	}

	for i := range size {
		expectEq(t, optionOf(m.Get(i)).Unwrap(), i*100)
	}

	for i := range size / 2 {
		expectEq(t, m.Remove(i*2), rs.Some(i*200))
		expectEq(t, m.Len(), size-i-1)
	}

	for i := range size / 2 {
		expectEq(t, optionOf(m.Get(2*i)), rs.None[int]())
		expectEq(t, optionOf(m.Get(2*i+1)).Unwrap(), i*200+100)
	}

	for i := range size / 2 {
		expectEq(t, m.Remove(2*i), rs.None[int]())
		expectEq(t, m.Remove(2*i+1), rs.Some(i*200+100))
		expectEq(t, m.Len(), size/2-i-1)
	}
	m.check(t)
}

func TestBTreeMap_test_basic_small(t *testing.T) {
	var m BTreeMap[int, int]
	// Empty, root is absent (None):
	expectEq(t, m.Remove(1), rs.None[int]())
	expectEq(t, m.Len(), 0)
	expectEq(t, optionOf(m.Get(1)), rs.None[int]())
	expectEq(t, optionOf(m.GetMut(1)), rs.None[int]())
	expectEq(t, optionPair(m.FirstKeyValue()), rs.None[rs.Tuple2[int, int]]())
	expectEq(t, optionPair(m.LastKeyValue()), rs.None[rs.Tuple2[int, int]]())
	expectEq(t, count(m.Keys()), 0)
	expectEq(t, count(m.Values()), 0)
	expectEq(t, count2(m.Range(Unbounded[int](), Unbounded[int]())), 0)
	expectEq(t, count2(m.Range(Unbounded[int](), Excluded(1))), 0)
	expectEq(t, count2(m.Range(Included(1), Unbounded[int]())), 0)
	expectEq(t, count2(m.Range(Included(1), Included(1))), 0)
	expectEq(t, count2(m.Range(Included(1), Excluded(2))), 0)
	expect(t, !m.hasRoot(), "height is None")
	expectEq(t, m.Insert(1, 1), rs.None[int]())
	expect(t, m.hasRoot(), "height is Some(0)")
	m.check(t)

	// 1 key-value pair:
	expectEq(t, m.Len(), 1)
	expectEq(t, optionOf(m.Get(1)), rs.Some(1))
	expectEq(t, optionOf(m.GetMut(1)), rs.Some(1))
	expectEq(t, optionPair(m.FirstKeyValue()), rs.Some(rs.T2(1, 1)))
	expectEq(t, optionPair(m.LastKeyValue()), rs.Some(rs.T2(1, 1)))
	expectEq(t, collect(m.Keys()), []int{1})
	expectEq(t, collect(derefValues(m.Values())), []int{1})
	expectEq(t, m.Insert(1, 2), rs.Some(1))
	expectEq(t, m.Len(), 1)
	expectEq(t, optionOf(m.Get(1)), rs.Some(2))
	expectEq(t, optionOf(m.GetMut(1)), rs.Some(2))
	expectEq(t, optionPair(m.FirstKeyValue()), rs.Some(rs.T2(1, 2)))
	expectEq(t, optionPair(m.LastKeyValue()), rs.Some(rs.T2(1, 2)))
	expectEq(t, collect(m.Keys()), []int{1})
	expectEq(t, collect(derefValues(m.Values())), []int{2})
	expectEq(t, m.Insert(2, 4), rs.None[int]())
	expect(t, m.hasRoot(), "height is Some(0)")
	m.check(t)

	// 2 key-value pairs:
	expectEq(t, m.Len(), 2)
	expectEq(t, optionOf(m.Get(2)), rs.Some(4))
	expectEq(t, optionOf(m.GetMut(2)), rs.Some(4))
	expectEq(t, optionPair(m.FirstKeyValue()), rs.Some(rs.T2(1, 2)))
	expectEq(t, optionPair(m.LastKeyValue()), rs.Some(rs.T2(2, 4)))
	expectEq(t, collect(m.Keys()), []int{1, 2})
	expectEq(t, collect(derefValues(m.Values())), []int{2, 4})
	expectEq(t, m.Remove(1), rs.Some(2))
	expect(t, m.hasRoot(), "height is Some(0)")
	m.check(t)

	// 1 key-value pair:
	expectEq(t, m.Len(), 1)
	expectEq(t, optionOf(m.Get(1)), rs.None[int]())
	expectEq(t, optionOf(m.GetMut(1)), rs.None[int]())
	expectEq(t, optionOf(m.Get(2)), rs.Some(4))
	expectEq(t, optionOf(m.GetMut(2)), rs.Some(4))
	expectEq(t, optionPair(m.FirstKeyValue()), rs.Some(rs.T2(2, 4)))
	expectEq(t, optionPair(m.LastKeyValue()), rs.Some(rs.T2(2, 4)))
	expectEq(t, collect(m.Keys()), []int{2})
	expectEq(t, collect(derefValues(m.Values())), []int{4})
	expectEq(t, m.Remove(2), rs.Some(4))
	expect(t, m.hasRoot(), "height is Some(0)")
	m.check(t)

	// Empty but root is owned (Some(...)):
	expectEq(t, m.Len(), 0)
	expectEq(t, optionOf(m.Get(1)), rs.None[int]())
	expectEq(t, optionOf(m.GetMut(1)), rs.None[int]())
	expectEq(t, optionPair(m.FirstKeyValue()), rs.None[rs.Tuple2[int, int]]())
	expectEq(t, optionPair(m.LastKeyValue()), rs.None[rs.Tuple2[int, int]]())
	expectEq(t, count(m.Keys()), 0)
	expectEq(t, count(m.Values()), 0)
	expectEq(t, count2(m.Range(Unbounded[int](), Unbounded[int]())), 0)
	expectEq(t, count2(m.Range(Unbounded[int](), Excluded(1))), 0)
	expectEq(t, count2(m.Range(Included(1), Unbounded[int]())), 0)
	expectEq(t, count2(m.Range(Included(1), Included(1))), 0)
	expectEq(t, count2(m.Range(Included(1), Excluded(2))), 0)
	expectEq(t, m.Remove(1), rs.None[int]())
	expect(t, m.hasRoot(), "height is Some(0)")
	m.check(t)
}

func TestBTreeMap_test_iter(t *testing.T) {
	size := 10000
	m := BTreeMapFromIter(identityPairs(0, size))

	test := func(got []rs.Tuple2[int, int]) {
		t.Helper()
		expectEq(t, len(got), size)
		for i := range size {
			expectEq(t, got[i], rs.T2(i, i))
		}
	}
	test(collectPairs(m.Iter()))
	test(collectPairs(m.IterMut()))
	test(collectOwned(m.IntoIter()))
}

func TestBTreeMap_test_iter_rev(t *testing.T) {
	size := 10000
	m := BTreeMapFromIter(identityPairs(0, size))

	test := func(got []rs.Tuple2[int, int]) {
		t.Helper()
		expectEq(t, len(got), size)
		for i := range size {
			expectEq(t, got[i], rs.T2(size-i-1, size-i-1))
		}
	}
	test(collectPairs(m.IterRev()))
	test(collectPairs(m.IterMutRev()))
	test(collectOwned(m.IntoIterRev()))
}

// Specifically tests iter_mut's ability to mutate the value of pairs
// in-line.
func doTestIterMutMutation[T any](t *testing.T, size int, from func(int) T) {
	t.Helper()
	zero := from(0)
	m := BTreeMapFromIter(func(yield func(T, T) bool) {
		for i := range size {
			if !yield(from(i), zero) {
				return
			}
		}
	})

	// Forward and backward iteration sees enough pairs (also tested
	// elsewhere)
	expectEq(t, count2(m.IterMut()), size)
	expectEq(t, count2(m.IterMutRev()), size)

	// Iterate forwards, trying to mutate to unique values
	i := 0
	for k, v := range m.IterMut() {
		expectEq(t, k, from(i))
		expectEq(t, *v, zero)
		*v = from(i + 1)
		i++
	}

	// Iterate backwards, checking that mutations succeeded and trying to
	// mutate again
	i = 0
	for k, v := range m.IterMutRev() {
		expectEq(t, k, from(size-i-1))
		expectEq(t, *v, from(size-i))
		*v = from(2*size - i)
		i++
	}

	// Check that backward mutations succeeded
	i = 0
	for k, v := range m.IterMut() {
		expectEq(t, k, from(i))
		expectEq(t, *v, from(size+i+1))
		i++
	}
	m.check(t)
}

// align32 is Align32, which Rust aligns to 32 bytes; Go cannot ask for an
// alignment, so it is the plain struct.
type align32 struct{ value int }

func TestBTreeMap_test_iter_mut_mutation(t *testing.T) {
	// Check many alignments and trees with roots at various heights.
	u8 := func(i int) uint8 { return uint8(i) }
	u16 := func(i int) uint16 { return uint16(i) }
	u32 := func(i int) uint32 { return uint32(i) }
	u64 := func(i int) uint64 { return uint64(i) }
	u128 := func(i int) rs.U128 { return rs.U128FromU64(uint64(i)) }
	aligned := func(i int) align32 { return align32{i} }
	doTestIterMutMutation(t, 0, u8)
	doTestIterMutMutation(t, 1, u8)
	doTestIterMutMutation(t, minInsertsHeight1, u8)
	doTestIterMutMutation(t, minInsertsHeight2, u8)
	doTestIterMutMutation(t, 1, u16)
	doTestIterMutMutation(t, minInsertsHeight1, u16)
	doTestIterMutMutation(t, minInsertsHeight2, u16)
	doTestIterMutMutation(t, 1, u32)
	doTestIterMutMutation(t, minInsertsHeight1, u32)
	doTestIterMutMutation(t, minInsertsHeight2, u32)
	doTestIterMutMutation(t, 1, u64)
	doTestIterMutMutation(t, minInsertsHeight1, u64)
	doTestIterMutMutation(t, minInsertsHeight2, u64)
	doTestIterMutMutation(t, 1, u128)
	doTestIterMutMutation(t, minInsertsHeight1, u128)
	doTestIterMutMutation(t, minInsertsHeight2, u128)
	doTestIterMutMutation(t, 1, aligned)
	doTestIterMutMutation(t, minInsertsHeight1, aligned)
	doTestIterMutMutation(t, minInsertsHeight2, aligned)
}

// testAllRefs gathers all pointers from a sequence of them and uses them
// all, twice, to be sure we got all interleavings.
func testAllRefs[T any](dummy *T, seq iter.Seq[*T]) {
	refs := collect(seq)
	for _, r := range refs {
		*dummy, *r = *r, *dummy
	}
	for _, r := range refs {
		*dummy, *r = *r, *dummy
	}
}

func TestBTreeMap_test_values_mut(t *testing.T) {
	a := BTreeMapFromIter(identityPairs(0, minInsertsHeight2))
	dummy := 13
	testAllRefs(&dummy, a.ValuesMut())
	a.check(t)
}

func TestBTreeMap_test_values_mut_mutation(t *testing.T) {
	var a BTreeMap[int, string]
	a.Insert(1, "hello")
	a.Insert(2, "goodbye")

	for value := range a.ValuesMut() {
		*value += "!"
	}

	values := collect(derefValues(a.Values()))
	expectEq(t, values, []string{"hello!", "goodbye!"})
	a.check(t)
}

func TestBTreeMap_test_iter_min_max(t *testing.T) {
	var a BTreeMap[int, int]
	all := func() (Bound[int], Bound[int]) { return Unbounded[int](), Unbounded[int]() }
	none := rs.None[rs.Tuple2[int, int]]()
	expectEq(t, seqMin(pairsOf(a.Iter())), none)
	expectEq(t, seqMax(pairsOf(a.Iter())), none)
	expectEq(t, seqMin(pairsOf(a.IterMut())), none)
	expectEq(t, seqMax(pairsOf(a.IterMut())), none)
	expectEq(t, seqMin(pairsOf(a.Range(all()))), none)
	expectEq(t, seqMax(pairsOf(a.Range(all()))), none)
	expectEq(t, seqMin(pairsOf(a.RangeMut(all()))), none)
	expectEq(t, seqMax(pairsOf(a.RangeMut(all()))), none)
	expectEq(t, seqMin(a.Keys()), rs.None[int]())
	expectEq(t, seqMax(a.Keys()), rs.None[int]())
	expectEq(t, seqMin(derefValues(a.Values())), rs.None[int]())
	expectEq(t, seqMax(derefValues(a.Values())), rs.None[int]())
	expectEq(t, seqMin(derefValues(a.ValuesMut())), rs.None[int]())
	expectEq(t, seqMax(derefValues(a.ValuesMut())), rs.None[int]())
	a.Insert(1, 42)
	a.Insert(2, 24)
	expectEq(t, seqMin(pairsOf(a.Iter())), rs.Some(rs.T2(1, 42)))
	expectEq(t, seqMax(pairsOf(a.Iter())), rs.Some(rs.T2(2, 24)))
	expectEq(t, seqMin(pairsOf(a.IterMut())), rs.Some(rs.T2(1, 42)))
	expectEq(t, seqMax(pairsOf(a.IterMut())), rs.Some(rs.T2(2, 24)))
	expectEq(t, seqMin(pairsOf(a.Range(all()))), rs.Some(rs.T2(1, 42)))
	expectEq(t, seqMax(pairsOf(a.Range(all()))), rs.Some(rs.T2(2, 24)))
	expectEq(t, seqMin(pairsOf(a.RangeMut(all()))), rs.Some(rs.T2(1, 42)))
	expectEq(t, seqMax(pairsOf(a.RangeMut(all()))), rs.Some(rs.T2(2, 24)))
	expectEq(t, seqMin(a.Keys()), rs.Some(1))
	expectEq(t, seqMax(a.Keys()), rs.Some(2))
	expectEq(t, seqMin(derefValues(a.Values())), rs.Some(24))
	expectEq(t, seqMax(derefValues(a.Values())), rs.Some(42))
	expectEq(t, seqMin(derefValues(a.ValuesMut())), rs.Some(24))
	expectEq(t, seqMax(derefValues(a.ValuesMut())), rs.Some(42))
	a.check(t)
}

func rangeKeys(t *testing.T, m *BTreeMap[int32, int32], lo, hi Bound[int32]) []int32 {
	t.Helper()
	keys := []int32{}
	for k, v := range m.Range(lo, hi) {
		expectEq(t, k, *v)
		keys = append(keys, k)
	}
	return keys
}

// testRangeAround runs the assertions test_range_small and test_range_large
// share, over a map of 1..=size.
func testRangeAround(t *testing.T, size int32) *BTreeMap[int32, int32] {
	t.Helper()
	all := intRange(int32(1), size+1)
	first, last := []int32{all[0]}, []int32{all[size-1]}
	m := BTreeMapFromIter(identityPairs(int32(1), size+1))
	u := Unbounded[int32]()
	empty := []int32{}

	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Excluded(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Included(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Included(size)), all)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), u), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Excluded(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Included(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Included(size)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), u), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), Excluded(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), Included(size+1)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), Included(size)), all)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), u), all)
	expectEq(t, rangeKeys(t, &m, u, Excluded(size+1)), all)
	expectEq(t, rangeKeys(t, &m, u, Included(size+1)), all)
	expectEq(t, rangeKeys(t, &m, u, Included(size)), all)
	expectEq(t, rangeKeys(t, &m, u, u), all)

	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Excluded[int32](1)), empty)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Included[int32](0)), empty)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Included[int32](0)), empty)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Excluded[int32](1)), empty)
	expectEq(t, rangeKeys(t, &m, u, Excluded[int32](1)), empty)
	expectEq(t, rangeKeys(t, &m, u, Included[int32](0)), empty)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Excluded[int32](2)), first)
	expectEq(t, rangeKeys(t, &m, Excluded[int32](0), Included[int32](1)), first)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Excluded[int32](2)), first)
	expectEq(t, rangeKeys(t, &m, Included[int32](0), Included[int32](1)), first)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), Excluded[int32](2)), first)
	expectEq(t, rangeKeys(t, &m, Included[int32](1), Included[int32](1)), first)
	expectEq(t, rangeKeys(t, &m, u, Excluded[int32](2)), first)
	expectEq(t, rangeKeys(t, &m, u, Included[int32](1)), first)
	expectEq(t, rangeKeys(t, &m, Excluded(size-1), Excluded(size+1)), last)
	expectEq(t, rangeKeys(t, &m, Excluded(size-1), Included(size+1)), last)
	expectEq(t, rangeKeys(t, &m, Excluded(size-1), Included(size)), last)
	expectEq(t, rangeKeys(t, &m, Excluded(size-1), u), last)
	expectEq(t, rangeKeys(t, &m, Included(size), Excluded(size+1)), last)
	expectEq(t, rangeKeys(t, &m, Included(size), Included(size+1)), last)
	expectEq(t, rangeKeys(t, &m, Included(size), Included(size)), last)
	expectEq(t, rangeKeys(t, &m, Included(size), u), last)
	expectEq(t, rangeKeys(t, &m, Excluded(size), Excluded(size+1)), empty)
	expectEq(t, rangeKeys(t, &m, Excluded(size), Included(size)), empty)
	expectEq(t, rangeKeys(t, &m, Excluded(size), u), empty)
	expectEq(t, rangeKeys(t, &m, Included(size+1), Excluded(size+1)), empty)
	expectEq(t, rangeKeys(t, &m, Included(size+1), Included(size+1)), empty)
	expectEq(t, rangeKeys(t, &m, Included(size+1), u), empty)
	return &m
}

func TestBTreeMap_test_range_small(t *testing.T) {
	m := testRangeAround(t, 4)
	u := Unbounded[int32]()

	expectEq(t, rangeKeys(t, m, u, Excluded[int32](3)), []int32{1, 2})
	expectEq(t, rangeKeys(t, m, Included[int32](3), u), []int32{3, 4})
	expectEq(t, rangeKeys(t, m, Included[int32](2), Included[int32](3)), []int32{2, 3})
}

func TestBTreeMap_test_range_height_1(t *testing.T) {
	// Tests tree with a root and 2 leaves. We test around the middle of the
	// keys because one of those is the single key in the root node.
	m := BTreeMapFromIter(identityPairs(int32(0), minInsertsHeight1))
	middle := int32(minInsertsHeight1 / 2)
	empty := []int32{}
	for root := middle - 2; root <= middle+2; root++ {
		expectEq(t, rangeKeys(t, &m, Excluded(root), Excluded(root+1)), empty)
		expectEq(t, rangeKeys(t, &m, Excluded(root), Included(root+1)), []int32{root + 1})
		expectEq(t, rangeKeys(t, &m, Included(root), Excluded(root+1)), []int32{root})
		expectEq(t, rangeKeys(t, &m, Included(root), Included(root+1)), []int32{root, root + 1})

		expectEq(t, rangeKeys(t, &m, Excluded(root-1), Excluded(root)), empty)
		expectEq(t, rangeKeys(t, &m, Included(root-1), Excluded(root)), []int32{root - 1})
		expectEq(t, rangeKeys(t, &m, Excluded(root-1), Included(root)), []int32{root})
		expectEq(t, rangeKeys(t, &m, Included(root-1), Included(root)), []int32{root - 1, root})
	}
}

func TestBTreeMap_test_range_large(t *testing.T) {
	m := testRangeAround(t, 200)
	u := Unbounded[int32]()

	check := func(lhs, rhs []rs.Tuple2[int32, int32]) {
		t.Helper()
		expectEq(t, lhs, rhs)
	}

	check(collectPairs(m.Range(u, Included[int32](100))), collectPairs(m.Range(u, Excluded[int32](101))))
	check(collectPairs(m.Range(Included[int32](5), Included[int32](8))),
		[]rs.Tuple2[int32, int32]{rs.T2[int32, int32](5, 5), rs.T2[int32, int32](6, 6), rs.T2[int32, int32](7, 7), rs.T2[int32, int32](8, 8)})
	check(collectPairs(m.Range(Included[int32](-1), Included[int32](2))),
		[]rs.Tuple2[int32, int32]{rs.T2[int32, int32](1, 1), rs.T2[int32, int32](2, 2)})
}

func TestBTreeMap_test_range_inclusive_max_value(t *testing.T) {
	maximum := uint64(math.MaxUint64)
	m := BTreeMapFrom(rs.T2(maximum, 0))
	expectEq(t, collectPairs(m.Range(Included(maximum), Included(maximum))), []rs.Tuple2[uint64, int]{rs.T2(maximum, 0)})
}

func TestBTreeMap_test_range_equal_empty_cases(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	expectEq(t, count2(m.Range(Included(2), Excluded(2))), 0)
	expectEq(t, count2(m.Range(Excluded(2), Included(2))), 0)
}

func TestBTreeMap_test_range_equal_excluded(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	shouldPanic(t, "", func() { _ = m.Range(Excluded(2), Excluded(2)) })
}

func TestBTreeMap_test_range_backwards_1(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	shouldPanic(t, "", func() { _ = m.Range(Included(3), Included(2)) })
}

func TestBTreeMap_test_range_backwards_2(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	shouldPanic(t, "", func() { _ = m.Range(Included(3), Excluded(2)) })
}

func TestBTreeMap_test_range_backwards_3(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	shouldPanic(t, "", func() { _ = m.Range(Excluded(3), Included(2)) })
}

func TestBTreeMap_test_range_backwards_4(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 5))
	shouldPanic(t, "", func() { _ = m.Range(Excluded(3), Excluded(2)) })
}

func TestBTreeMap_test_range_finding_ill_order_in_map(t *testing.T) {
	var m BTreeMap[cyclic3, rs.Unit]
	m.Insert(cyclicB, rs.Unit{})
	// Lacking static_assert, call `range` conditionally, to emphasise that
	// we cause a different panic than `test_range_backwards_1` does.
	// A more refined `should_panic` would be welcome.
	if cyclicC.Compare(cyclicA) < 0 {
		for range m.Range(Included(cyclicC), Included(cyclicA)) {
		}
	}
}

func TestBTreeMap_test_range_1000(t *testing.T) {
	size := uint32(1000)
	m := BTreeMapFromIter(identityPairs(uint32(0), size))

	test := func(lo, hi Bound[uint32]) {
		t.Helper()
		kvs := collectPairs(m.Range(lo, hi))
		expectEq(t, len(kvs), int(size))
		for i, kv := range kvs {
			expectEq(t, kv, rs.T2(uint32(i), uint32(i)))
		}
	}
	test(Included[uint32](0), Excluded(size))
	test(Unbounded[uint32](), Excluded(size))
	test(Included[uint32](0), Included(size-1))
	test(Unbounded[uint32](), Included(size-1))
	test(Included[uint32](0), Unbounded[uint32]())
	test(Unbounded[uint32](), Unbounded[uint32]())
}

func TestBTreeMap_test_range_borrowed_key(t *testing.T) {
	var m BTreeMap[string, int]
	m.Insert("aardvark", 1)
	m.Insert("baboon", 2)
	m.Insert("coyote", 3)
	m.Insert("dingo", 4)
	// NOTE: would like to use simply "b".."d" here...
	expectEq(t, collectPairs(m.Range(Included("b"), Excluded("d"))),
		[]rs.Tuple2[string, int]{rs.T2("baboon", 2), rs.T2("coyote", 3)})
}

func TestBTreeMap_test_range(t *testing.T) {
	size := 200
	step := 1
	m := BTreeMapFromIter(identityPairs(0, size))

	for i := 0; i < size; i += step {
		for j := i; j < size; j += step {
			kvs := collectPairs(m.Range(Included(i), Included(j)))
			expectEq(t, len(kvs), j-i+1)
			for at, kv := range kvs {
				expectEq(t, kv, rs.T2(i+at, i+at))
			}
		}
	}
}

func TestBTreeMap_test_range_mut(t *testing.T) {
	size := 200
	step := 1
	m := BTreeMapFromIter(identityPairs(0, size))

	for i := 0; i < size; i += step {
		for j := i; j < size; j += step {
			kvs := collectPairs(m.RangeMut(Included(i), Included(j)))
			expectEq(t, len(kvs), j-i+1)
			for at, kv := range kvs {
				expectEq(t, kv, rs.T2(i+at, i+at))
			}
		}
	}
	m.check(t)
}

func TestBTreeMap_test_range_panic_1(t *testing.T) {
	var m BTreeMap[int, string]
	m.Insert(3, "a")
	m.Insert(5, "b")
	m.Insert(8, "c")

	shouldPanic(t, "range start is greater than range end in BTreeMap", func() {
		_ = m.Range(Included(8), Included(3))
	})
}

func TestBTreeMap_test_range_panic_2(t *testing.T) {
	var m BTreeMap[int, string]
	m.Insert(3, "a")
	m.Insert(5, "b")
	m.Insert(8, "c")

	shouldPanic(t, "range start and end are equal and excluded in BTreeMap", func() {
		_ = m.Range(Excluded(5), Excluded(5))
	})
}

func TestBTreeMap_test_range_panic_3(t *testing.T) {
	var m BTreeMap[int32, rs.Unit]
	m.Insert(3, rs.Unit{})
	m.Insert(5, rs.Unit{})
	m.Insert(8, rs.Unit{})

	shouldPanic(t, "range start and end are equal and excluded in BTreeMap", func() {
		_ = m.Range(Excluded[int32](5), Excluded[int32](5))
	})
}

func TestBTreeMap_test_retain(t *testing.T) {
	m := BTreeMapFromIter(func(yield func(int, int) bool) {
		for x := range 100 {
			if !yield(x, x*10) {
				return
			}
		}
	})

	m.Retain(func(k int, _ *int) bool { return k%2 == 0 })
	expectEq(t, m.Len(), 50)
	expectEq(t, *m.Index(2), 20)
	expectEq(t, *m.Index(4), 40)
	expectEq(t, *m.Index(6), 60)
}

// extractAll is `map.extract_if(.., pred).for_each(drop)`.
func extractAll[K, V any](m *BTreeMap[K, V], pred func(K, *V) bool) {
	for range m.ExtractIf(Unbounded[K](), Unbounded[K](), pred) {
	}
}

func TestBTreeMap_test_extract_if_empty(t *testing.T) {
	var m BTreeMap[int32, int32]
	extractAll(&m, func(int32, *int32) bool { panic(rs.Unreachable("there's nothing to decide on")) })
	expect(t, !m.hasRoot(), "height is None")
	m.check(t)
}

// Explicitly consumes the iterator, where most test cases drop it
// instantly.
func TestBTreeMap_test_extract_if_consumed_keeping_all(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 3))
	expectEq(t, collectOwned(m.ExtractIf(Unbounded[int](), Unbounded[int](), func(int, *int) bool { return false })),
		[]rs.Tuple2[int, int]{})
	m.check(t)
}

// Explicitly consumes the iterator, where most test cases drop it
// instantly.
func TestBTreeMap_test_extract_if_consumed_removing_all(t *testing.T) {
	pairs := collectOwned(identityPairs(0, 3))
	m := BTreeMapFromIter(identityPairs(0, 3))
	expectEq(t, collectOwned(m.ExtractIf(Unbounded[int](), Unbounded[int](), func(int, *int) bool { return true })), pairs)
	expect(t, m.IsEmpty())
	m.check(t)
}

func TestBTreeMap_test_extract_if_consumed_removing_some(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 3))
	for x := range 3 {
		for y := range 3 {
			m := m.Clone()
			expectEq(t, collectOwned(m.ExtractIf(Included(x), Excluded(y), func(int, *int) bool { return true })),
				collectOwned(identityPairs(x, max(x, y))))
			for i := range 3 {
				expectNe(t, m.ContainsKey(i), x <= i && i < y)
			}
		}
	}
	for x := range 3 {
		for y := range 2 {
			m := m.Clone()
			expectEq(t, collectOwned(m.ExtractIf(Included(x), Included(y), func(int, *int) bool { return true })),
				collectOwned(identityPairs(x, max(x, y+1))))
			for i := range 3 {
				expectNe(t, m.ContainsKey(i), x <= i && i <= y)
			}
		}
	}
}

// Explicitly consumes the iterator and modifies values through it.
func TestBTreeMap_test_extract_if_mutating_and_keeping(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 3))
	expectEq(t, collectOwned(m.ExtractIf(Unbounded[int](), Unbounded[int](), func(_ int, v *int) bool {
		*v += 6
		return false
	})), []rs.Tuple2[int, int]{})
	expectEq(t, collect(m.Keys()), intRange(0, 3))
	expectEq(t, collect(derefValues(m.Values())), intRange(6, 9))
	m.check(t)
}

// Explicitly consumes the iterator and modifies values through it.
func TestBTreeMap_test_extract_if_mutating_and_removing(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 3))
	expectEq(t, collectOwned(m.ExtractIf(Unbounded[int](), Unbounded[int](), func(_ int, v *int) bool {
		*v += 6
		return true
	})), []rs.Tuple2[int, int]{rs.T2(0, 6), rs.T2(1, 7), rs.T2(2, 8)})
	expect(t, m.IsEmpty())
	m.check(t)
}

func TestBTreeMap_test_extract_if_underfull_keeping_all(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 3))
	extractAll(&m, func(int, *int) bool { return false })
	expectEq(t, collect(m.Keys()), intRange(0, 3))
	m.check(t)
}

// extractRemovingOne, extractKeepingOne, extractRemovingAll and
// extractKeepingAll are the bodies of the underfull_*, height_0_*,
// height_1_* and height_2_* tests, over a map of 0..size.
func extractRemovingOne(t *testing.T, size, step int) {
	t.Helper()
	for doomed := 0; doomed < size; doomed += step {
		m := BTreeMapFromIter(identityPairs(0, size))
		extractAll(&m, func(i int, _ *int) bool { return i == doomed })
		expectEq(t, m.Len(), size-1)
		m.check(t)
	}
}

func extractKeepingOne(t *testing.T, size, step int) {
	t.Helper()
	for sacred := 0; sacred < size; sacred += step {
		m := BTreeMapFromIter(identityPairs(0, size))
		extractAll(&m, func(i int, _ *int) bool { return i != sacred })
		expectEq(t, collect(m.Keys()), []int{sacred})
		m.check(t)
	}
}

func extractRemovingAll(t *testing.T, size int) {
	t.Helper()
	m := BTreeMapFromIter(identityPairs(0, size))
	extractAll(&m, func(int, *int) bool { return true })
	expect(t, m.IsEmpty())
	m.check(t)
}

func TestBTreeMap_test_extract_if_underfull_removing_one(t *testing.T) {
	extractRemovingOne(t, 3, 1)
}

func TestBTreeMap_test_extract_if_underfull_keeping_one(t *testing.T) {
	extractKeepingOne(t, 3, 1)
}

func TestBTreeMap_test_extract_if_underfull_removing_all(t *testing.T) {
	extractRemovingAll(t, 3)
}

func TestBTreeMap_test_extract_if_height_0_keeping_all(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, btreeNodeCapacity))
	extractAll(&m, func(int, *int) bool { return false })
	expectEq(t, collect(m.Keys()), intRange(0, btreeNodeCapacity))
	m.check(t)
}

func TestBTreeMap_test_extract_if_height_0_removing_one(t *testing.T) {
	extractRemovingOne(t, btreeNodeCapacity, 1)
}

func TestBTreeMap_test_extract_if_height_0_keeping_one(t *testing.T) {
	extractKeepingOne(t, btreeNodeCapacity, 1)
}

func TestBTreeMap_test_extract_if_height_0_removing_all(t *testing.T) {
	extractRemovingAll(t, btreeNodeCapacity)
}

func TestBTreeMap_test_extract_if_height_0_keeping_half(t *testing.T) {
	m := BTreeMapFromIter(identityPairs(0, 16))
	expectEq(t, count2(m.ExtractIf(Unbounded[int](), Unbounded[int](), func(i int, _ *int) bool { return i%2 == 0 })), 8)
	expectEq(t, m.Len(), 8)
	m.check(t)
}

func TestBTreeMap_test_extract_if_height_1_removing_all(t *testing.T) {
	extractRemovingAll(t, minInsertsHeight1)
}

func TestBTreeMap_test_extract_if_height_1_removing_one(t *testing.T) {
	extractRemovingOne(t, minInsertsHeight1, 1)
}

func TestBTreeMap_test_extract_if_height_1_keeping_one(t *testing.T) {
	extractKeepingOne(t, minInsertsHeight1, 1)
}

func TestBTreeMap_test_extract_if_height_2_removing_one(t *testing.T) {
	extractRemovingOne(t, minInsertsHeight2, 12)
}

func TestBTreeMap_test_extract_if_height_2_keeping_one(t *testing.T) {
	extractKeepingOne(t, minInsertsHeight2, 12)
}

func TestBTreeMap_test_extract_if_height_2_removing_all(t *testing.T) {
	extractRemovingAll(t, minInsertsHeight2)
}

// The drop counts of the Rust test have no counterpart: Go has no
// destructors.
func TestBTreeMap_test_extract_if_pred_panic_leak(t *testing.T) {
	a := &crashTestDummy{id: 0}
	b := &crashTestDummy{id: 1}
	c := &crashTestDummy{id: 2}
	var m BTreeMap[dummyInstance, rs.Unit]
	m.Insert(a.spawn(panicNever), rs.Unit{})
	m.Insert(b.spawn(panicInQuery), rs.Unit{})
	m.Insert(c.spawn(panicInQuery), rs.Unit{})

	_, panicked := panicMessage(func() {
		extractAll(&m, func(dummy dummyInstance, _ *rs.Unit) bool { return dummy.query(true) })
	})
	expect(t, panicked, "unwrap_err")

	expectEq(t, a.queried, 1)
	expectEq(t, b.queried, 1)
	expectEq(t, c.queried, 0)
	expectEq(t, m.Len(), 2)
	expectEq(t, m.FirstEntry().Unwrap().Key().id(), 1)
	expectEq(t, m.LastEntry().Unwrap().Key().id(), 2)
	m.check(t)
}

// Rust checks that lookups through the Borrow trait compile; the port's
// lookups take the key type, so what is left to check is that equal keys
// held in different storage find each other.
func TestBTreeMap_test_borrow(t *testing.T) {
	{
		var m BTreeMap[string, int]
		m.Insert("0", 1)
		expectEq(t, *m.Index("0"), 1)
	}

	{
		var m BTreeMap[*int, int]
		boxed := 0
		m.Insert(&boxed, 1)
		other := 0
		expectEq(t, *m.Index(&other), 1)
	}

	{
		var m BTreeMap[[]int32, int]
		m.Insert([]int32{0, 1}, 1)
		expectEq(t, *m.Index([]int32{0, 1}), 1)
	}

	{
		var m BTreeMap[*int32, int]
		rc := int32(0)
		m.Insert(&rc, 1)
		other := int32(0)
		expectEq(t, *m.Index(&other), 1)
	}
}

func TestBTreeMap_test_entry(t *testing.T) {
	xs := []rs.Tuple2[int, int]{rs.T2(1, 10), rs.T2(2, 20), rs.T2(3, 30), rs.T2(4, 40), rs.T2(5, 50), rs.T2(6, 60)}

	m := BTreeMapFrom(xs...)

	// Existing key (insert)
	switch view := m.Entry(1).(type) {
	case *BTreeMapEntry_Vacant[int, int]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Occupied[int, int]:
		expectEq(t, *view.F0.Get(), 10)
		expectEq(t, view.F0.Insert(100), 10)
	default:
		panic(rs.Unreachable(view))
	}
	expectEq(t, optionOf(m.Get(1)).Unwrap(), 100)
	expectEq(t, m.Len(), 6)

	// Existing key (update)
	switch view := m.Entry(2).(type) {
	case *BTreeMapEntry_Vacant[int, int]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Occupied[int, int]:
		v := view.F0.GetMut()
		*v *= 10
	default:
		panic(rs.Unreachable(view))
	}
	expectEq(t, optionOf(m.Get(2)).Unwrap(), 200)
	expectEq(t, m.Len(), 6)
	m.check(t)

	// Existing key (take)
	switch view := m.Entry(3).(type) {
	case *BTreeMapEntry_Vacant[int, int]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Occupied[int, int]:
		expectEq(t, view.F0.Remove(), 30)
	default:
		panic(rs.Unreachable(view))
	}
	expectEq(t, optionOf(m.Get(3)), rs.None[int]())
	expectEq(t, m.Len(), 5)
	m.check(t)

	// Inexistent key (insert)
	switch view := m.Entry(10).(type) {
	case *BTreeMapEntry_Occupied[int, int]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Vacant[int, int]:
		expectEq(t, *view.F0.Insert(1000), 1000)
	default:
		panic(rs.Unreachable(view))
	}
	expectEq(t, optionOf(m.Get(10)).Unwrap(), 1000)
	expectEq(t, m.Len(), 6)
	m.check(t)
}

func TestBTreeMap_test_extend_ref(t *testing.T) {
	var a BTreeMap[int, string]
	a.Insert(1, "one")
	var b BTreeMap[int, string]
	b.Insert(2, "two")
	b.Insert(3, "three")

	a.Extend(derefPairs(b.Iter()))

	expectEq(t, a.Len(), 3)
	expectEq(t, *a.Index(1), "one")
	expectEq(t, *a.Index(2), "two")
	expectEq(t, *a.Index(3), "three")
	a.check(t)
}

func TestBTreeMap_test_zst(t *testing.T) {
	var m BTreeMap[rs.Unit, rs.Unit]
	expectEq(t, m.Len(), 0)

	expectEq(t, m.Insert(rs.Unit{}, rs.Unit{}), rs.None[rs.Unit]())
	expectEq(t, m.Len(), 1)

	expectEq(t, m.Insert(rs.Unit{}, rs.Unit{}), rs.Some(rs.Unit{}))
	expectEq(t, m.Len(), 1)
	expectEq(t, count2(m.Iter()), 1)

	m.Clear()
	expectEq(t, m.Len(), 0)

	for range 100 {
		m.Insert(rs.Unit{}, rs.Unit{})
	}

	expectEq(t, m.Len(), 1)
	expectEq(t, count2(m.Iter()), 1)
	m.check(t)
}

// badZst is the Bad of test_bad_zst: a zero-sized key with a nonsensical
// ordering.
type badZst struct{}

func (badZst) Equal(badZst) bool { return false }

func (badZst) Compare(badZst) int { return -1 }

// This test's only purpose is to ensure that zero-sized keys with
// nonsensical orderings do not cause segfaults when used with zero-sized
// values. All other map behavior is undefined.
func TestBTreeMap_test_bad_zst(t *testing.T) {
	var m BTreeMap[badZst, badZst]

	for range 100 {
		m.Insert(badZst{}, badZst{})
	}
	m.check(t)
}

func TestBTreeMap_test_clear(t *testing.T) {
	var m BTreeMap[int, rs.Unit]
	for _, length := range []int{minInsertsHeight1, minInsertsHeight2, 0, btreeNodeCapacity} {
		for i := range length {
			m.Insert(i, rs.Unit{})
		}
		expectEq(t, m.Len(), length)
		m.Clear()
		m.check(t)
		expect(t, !m.hasRoot(), "height is None")
	}
}

func TestBTreeMap_test_clone(t *testing.T) {
	var m BTreeMap[int, int]
	size := minInsertsHeight1
	expectEq(t, m.Len(), 0)

	for i := range size {
		expectEq(t, m.Insert(i, 10*i), rs.None[int]())
		expectEq(t, m.Len(), i+1)
		m.check(t)
		expectEq(t, m, m.Clone())
	}

	for i := range size {
		expectEq(t, m.Insert(i, 100*i), rs.Some(10*i))
		expectEq(t, m.Len(), size)
		m.check(t)
		expectEq(t, m, m.Clone())
	}

	for i := range size / 2 {
		expectEq(t, m.Remove(i*2), rs.Some(i*200))
		expectEq(t, m.Len(), size-i-1)
		m.check(t)
		expectEq(t, m, m.Clone())
	}

	for i := range size / 2 {
		expectEq(t, m.Remove(2*i), rs.None[int]())
		expectEq(t, m.Remove(2*i+1), rs.Some(i*200+100))
		expectEq(t, m.Len(), size/2-i-1)
		m.check(t)
		expectEq(t, m, m.Clone())
	}

	// Test a tree with 2 semi-full levels and a tree with 3 levels.
	m = BTreeMapFromIter(identityPairs(1, minInsertsHeight2))
	expectEq(t, m.Len(), minInsertsHeight2-1)
	expectEq(t, m, m.Clone())
	m.Insert(0, 0)
	expectEq(t, m.Len(), minInsertsHeight2)
	expectEq(t, m, m.Clone())
	m.check(t)
}

// clone_from is not overridden for BTreeMap: it is `*self = source.clone()`.
func TestBTreeMap_test_clone_from(t *testing.T) {
	var map1 BTreeMap[int, int]
	maxSize := minInsertsHeight1

	// Range to max_size inclusive, because i is the size of map1 being
	// tested.
	for i := 0; i <= maxSize; i++ {
		var map2 BTreeMap[int, int]
		for j := range i {
			map1Copy := map2.Clone()
			map1Copy = map1.Clone() // small cloned from large
			expectEq(t, map1Copy, map1)
			map2Copy := map1.Clone()
			map2Copy = map2.Clone() // large cloned from small
			expectEq(t, map2Copy, map2)
			map2.Insert(100*j+1, 2*j+1)
		}
		map2 = map1.Clone() // same length
		map2.check(t)
		expectEq(t, map2, map1)
		map1.Insert(i, 10*i)
		map1.check(t)
	}
}

// nonOrd is NonOrd of test_ord_absence: a key type without an order.
type nonOrd struct{}

// What needs no order of the keys works without one: rs.Cmp is never asked
// about a nonOrd, which it could not order.
func TestBTreeMap_test_ord_absence(t *testing.T) {
	mapFn := func(m BTreeMap[nonOrd, rs.Unit]) {
		_ = m.IsEmpty()
		_ = m.Len()
		m.Clear()
		_ = m.Iter()
		_ = m.IterMut()
		_ = m.Keys()
		_ = m.Values()
		_ = m.ValuesMut()
		_ = m.IntoValues()
	}

	mapDebug := func(m BTreeMap[nonOrd, rs.Unit]) {
		_ = rs.Format("{:?}", &m)
	}

	mapClone := func(m BTreeMap[nonOrd, rs.Unit]) {
		m = m.Clone()
	}

	mapFn(BTreeMapNew[nonOrd, rs.Unit]())
	mapDebug(BTreeMapNew[nonOrd, rs.Unit]())
	mapClone(BTreeMapDefault[nonOrd, rs.Unit]())
}

func TestBTreeMap_test_occupied_entry_key(t *testing.T) {
	var a BTreeMap[string, string]
	key := "hello there"
	value := "value goes here"
	expect(t, !a.hasRoot(), "height is None")
	a.Insert(key, value)
	expectEq(t, a.Len(), 1)
	expectEq(t, *a.Index(key), value)

	switch e := a.Entry(key).(type) {
	case *BTreeMapEntry_Vacant[string, string]:
		panic(rs.Panicf("explicit panic"))
	case *BTreeMapEntry_Occupied[string, string]:
		expectEq(t, key, e.F0.Key())
	default:
		panic(rs.Unreachable(e))
	}
	expectEq(t, a.Len(), 1)
	expectEq(t, *a.Index(key), value)
	a.check(t)
}

func TestBTreeMap_test_vacant_entry_key(t *testing.T) {
	var a BTreeMap[string, string]
	key := "hello there"
	value := "value goes here"

	expect(t, !a.hasRoot(), "height is None")
	switch e := a.Entry(key).(type) {
	case *BTreeMapEntry_Occupied[string, string]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Vacant[string, string]:
		expectEq(t, key, e.F0.Key())
		e.F0.Insert(value)
	default:
		panic(rs.Unreachable(e))
	}
	expectEq(t, a.Len(), 1)
	expectEq(t, *a.Index(key), value)
	a.check(t)
}

func TestBTreeMap_test_vacant_entry_no_insert(t *testing.T) {
	var a BTreeMap[string, rs.Unit]
	key := "hello there"

	// Non-allocated
	expect(t, !a.hasRoot(), "height is None")
	switch e := a.Entry(key).(type) {
	case *BTreeMapEntry_Occupied[string, rs.Unit]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Vacant[string, rs.Unit]:
		expectEq(t, key, e.F0.Key())
	default:
		panic(rs.Unreachable(e))
	}
	// Ensures the tree has no root.
	expect(t, !a.hasRoot(), "height is None")
	a.check(t)

	// Allocated but still empty
	a.Insert(key, rs.Unit{})
	a.Remove(key)
	expect(t, a.hasRoot(), "height is Some(0)")
	expect(t, a.IsEmpty())
	switch e := a.Entry(key).(type) {
	case *BTreeMapEntry_Occupied[string, rs.Unit]:
		panic(rs.Unreachable())
	case *BTreeMapEntry_Vacant[string, rs.Unit]:
		expectEq(t, key, e.F0.Key())
	default:
		panic(rs.Unreachable(e))
	}
	// Ensures the allocated root is not changed.
	expect(t, a.hasRoot(), "height is Some(0)")
	expect(t, a.IsEmpty())
	a.check(t)
}

func TestBTreeMap_test_first_last_entry(t *testing.T) {
	var a BTreeMap[int, int]
	expect(t, a.FirstEntry().IsNone())
	expect(t, a.LastEntry().IsNone())
	a.Insert(1, 42)
	expectEq(t, a.FirstEntry().Unwrap().Key(), 1)
	expectEq(t, a.LastEntry().Unwrap().Key(), 1)
	a.Insert(2, 24)
	expectEq(t, a.FirstEntry().Unwrap().Key(), 1)
	expectEq(t, a.LastEntry().Unwrap().Key(), 2)
	a.Insert(0, 6)
	expectEq(t, a.FirstEntry().Unwrap().Key(), 0)
	expectEq(t, a.LastEntry().Unwrap().Key(), 2)
	k1, v1 := a.FirstEntry().Unwrap().RemoveEntry()
	expectEq(t, k1, 0)
	expectEq(t, v1, 6)
	k2, v2 := a.LastEntry().Unwrap().RemoveEntry()
	expectEq(t, k2, 2)
	expectEq(t, v2, 24)
	expectEq(t, a.FirstEntry().Unwrap().Key(), 1)
	expectEq(t, a.LastEntry().Unwrap().Key(), 1)
	a.check(t)
}

func TestBTreeMap_test_pop_first_last(t *testing.T) {
	var m BTreeMap[int, int]
	none := rs.None[rs.Tuple2[int, int]]()
	expectEq(t, m.PopFirst(), none)
	expectEq(t, m.PopLast(), none)

	m.Insert(1, 10)
	m.Insert(2, 20)
	m.Insert(3, 30)
	m.Insert(4, 40)

	expectEq(t, m.Len(), 4)

	pair := m.PopFirst().Unwrap()
	expectEq(t, pair.F0, 1)
	expectEq(t, pair.F1, 10)
	expectEq(t, m.Len(), 3)

	pair = m.PopFirst().Unwrap()
	expectEq(t, pair.F0, 2)
	expectEq(t, pair.F1, 20)
	expectEq(t, m.Len(), 2)
	pair = m.PopLast().Unwrap()
	expectEq(t, pair.F0, 4)
	expectEq(t, pair.F1, 40)
	expectEq(t, m.Len(), 1)

	m.Insert(5, 50)
	m.Insert(6, 60)
	expectEq(t, m.Len(), 3)

	pair = m.PopFirst().Unwrap()
	expectEq(t, pair.F0, 3)
	expectEq(t, pair.F1, 30)
	expectEq(t, m.Len(), 2)

	pair = m.PopLast().Unwrap()
	expectEq(t, pair.F0, 6)
	expectEq(t, pair.F1, 60)
	expectEq(t, m.Len(), 1)

	pair = m.PopLast().Unwrap()
	expectEq(t, pair.F0, 5)
	expectEq(t, pair.F1, 50)
	expectEq(t, m.Len(), 0)

	expectEq(t, m.PopFirst(), none)
	expectEq(t, m.PopLast(), none)

	m.Insert(7, 70)
	m.Insert(8, 80)

	pair = m.PopLast().Unwrap()
	expectEq(t, pair.F0, 8)
	expectEq(t, pair.F1, 80)
	expectEq(t, m.Len(), 1)

	pair = m.PopLast().Unwrap()
	expectEq(t, pair.F0, 7)
	expectEq(t, pair.F1, 70)
	expectEq(t, m.Len(), 0)

	expectEq(t, m.PopFirst(), none)
	expectEq(t, m.PopLast(), none)
}

func TestBTreeMap_test_get_key_value(t *testing.T) {
	var m BTreeMap[int, int]
	none := rs.None[rs.Tuple2[int, int]]()

	expect(t, m.IsEmpty())
	expectEq(t, optionPair(m.GetKeyValue(1)), none)
	expectEq(t, optionPair(m.GetKeyValue(2)), none)

	m.Insert(1, 10)
	m.Insert(2, 20)
	m.Insert(3, 30)

	expectEq(t, m.Len(), 3)
	expectEq(t, optionPair(m.GetKeyValue(1)), rs.Some(rs.T2(1, 10)))
	expectEq(t, optionPair(m.GetKeyValue(3)), rs.Some(rs.T2(3, 30)))
	expectEq(t, optionPair(m.GetKeyValue(4)), none)

	m.Remove(3)

	expectEq(t, m.Len(), 2)
	expectEq(t, optionPair(m.GetKeyValue(3)), none)
	expectEq(t, optionPair(m.GetKeyValue(2)), rs.Some(rs.T2(2, 20)))
}

func TestBTreeMap_test_insert_into_full_height_0(t *testing.T) {
	size := btreeNodeCapacity
	for pos := 0; pos <= size; pos++ {
		m := BTreeMapFromIter(func(yield func(int, rs.Unit) bool) {
			for i := range size {
				if !yield(i*2+1, rs.Unit{}) {
					return
				}
			}
		})
		expect(t, m.Insert(pos*2, rs.Unit{}).IsNone())
		m.check(t)
	}
}

// createAppendTest is create_append_test!.
func createAppendTest(t *testing.T, length int) {
	var a BTreeMap[int, int]
	for i := range 8 {
		a.Insert(i, i)
	}

	var b BTreeMap[int, int]
	for i := 5; i < length; i++ {
		b.Insert(i, 2*i)
	}

	a.Append(&b)

	expectEq(t, a.Len(), length)
	expectEq(t, b.Len(), 0)

	for i := range length {
		if i < 5 {
			expectEq(t, *a.Index(i), i)
		} else {
			expectEq(t, *a.Index(i), 2*i)
		}
	}

	a.check(t)
	expectEq(t, a.Remove(length-1), rs.Some(2*(length-1)))
	expectEq(t, a.Insert(length-1, 20), rs.None[int]())
	a.check(t)
}

// These are mostly for testing the algorithm that "fixes" the right edge
// after insertion.
// Single node.
func TestBTreeMap_test_append_9(t *testing.T) { createAppendTest(t, 9) }

// Two leafs that don't need fixing.
func TestBTreeMap_test_append_17(t *testing.T) { createAppendTest(t, 17) }

// Two leafs where the second one ends up underfull and needs stealing at
// the end.
func TestBTreeMap_test_append_14(t *testing.T) { createAppendTest(t, 14) }

// Two leafs where the second one ends up empty because the insertion
// finished at the root.
func TestBTreeMap_test_append_12(t *testing.T) { createAppendTest(t, 12) }

// Three levels; insertion finished at the root.
func TestBTreeMap_test_append_144(t *testing.T) { createAppendTest(t, 144) }

// Three levels; insertion finished at leaf while there is an empty node on
// the second level.
func TestBTreeMap_test_append_145(t *testing.T) { createAppendTest(t, 145) }

// Tests for several randomly chosen sizes.
func TestBTreeMap_test_append_170(t *testing.T) { createAppendTest(t, 170) }

func TestBTreeMap_test_append_181(t *testing.T) { createAppendTest(t, 181) }

func TestBTreeMap_test_append_239(t *testing.T) { createAppendTest(t, 239) }

func TestBTreeMap_test_append_1700(t *testing.T) { createAppendTest(t, 1700) }

func randData(length int) []rs.Tuple2[uint32, uint32] {
	rng := newDeterministicRng()
	data := make([]rs.Tuple2[uint32, uint32], 0, length)
	for range length {
		key := rng.next()
		data = append(data, rs.T2(key, rng.next()))
	}
	return data
}

func pairSeqOf[K, V any](pairs []rs.Tuple2[K, V]) iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		for _, pair := range pairs {
			if !yield(pair.F0, pair.F1) {
				return
			}
		}
	}
}

func TestBTreeMap_test_split_off_empty_right(t *testing.T) {
	data := randData(173)

	m := BTreeMapFromIter(pairSeqOf(data))
	right := m.SplitOff(slices.MaxFunc(data, rs.Cmp[rs.Tuple2[uint32, uint32]]).F0 + 1)
	m.check(t)
	right.check(t)

	data = sorted(data)
	expectEq(t, collectOwned(m.IntoIter()), data)
	expectEq(t, collectOwned(right.IntoIter()), []rs.Tuple2[uint32, uint32]{})
}

func TestBTreeMap_test_split_off_empty_left(t *testing.T) {
	data := randData(314)

	m := BTreeMapFromIter(pairSeqOf(data))
	right := m.SplitOff(slices.MinFunc(data, rs.Cmp[rs.Tuple2[uint32, uint32]]).F0)
	m.check(t)
	right.check(t)

	data = sorted(data)
	expectEq(t, collectOwned(m.IntoIter()), []rs.Tuple2[uint32, uint32]{})
	expectEq(t, collectOwned(right.IntoIter()), data)
}

// In a tree with 3 levels, if all but a part of the first leaf node is
// split off, make sure fix_top eliminates both top levels.
func TestBTreeMap_test_split_off_tiny_left_height_2(t *testing.T) {
	left := BTreeMapFromIter(identityPairs(0, minInsertsHeight2))
	right := left.SplitOff(1)
	left.check(t)
	right.check(t)
	expectEq(t, left.Len(), 1)
	expectEq(t, right.Len(), minInsertsHeight2-1)
	expectEq(t, optionPair(left.FirstKeyValue()).Unwrap().F0, 0)
	expectEq(t, optionPair(right.FirstKeyValue()).Unwrap().F0, 1)
}

// In a tree with 3 levels, if only part of the last leaf node is split off,
// make sure fix_top eliminates both top levels.
func TestBTreeMap_test_split_off_tiny_right_height_2(t *testing.T) {
	last := minInsertsHeight2 - 1
	left := BTreeMapFromIter(identityPairs(0, minInsertsHeight2))
	expectEq(t, optionPair(left.LastKeyValue()).Unwrap().F0, last)
	right := left.SplitOff(last)
	left.check(t)
	right.check(t)
	expectEq(t, left.Len(), minInsertsHeight2-1)
	expectEq(t, right.Len(), 1)
	expectEq(t, optionPair(left.LastKeyValue()).Unwrap().F0, last-1)
	expectEq(t, optionPair(right.LastKeyValue()).Unwrap().F0, last)
}

func TestBTreeMap_test_split_off_halfway(t *testing.T) {
	rng := newDeterministicRng()
	for _, length := range []int{btreeNodeCapacity, 25, 50, 75, 100} {
		data := make([]rs.Tuple2[uint32, rs.Unit], 0, length)
		for range length {
			data = append(data, rs.T2(rng.next(), rs.Unit{}))
		}
		// Insertion in non-ascending order creates some variation in node
		// length.
		m := BTreeMapFromIter(pairSeqOf(data))
		data = sorted(data)
		var smallKeys, largeKeys []uint32
		for index, kv := range data {
			if index < length/2 {
				smallKeys = append(smallKeys, kv.F0)
			} else {
				largeKeys = append(largeKeys, kv.F0)
			}
		}
		splitKey := largeKeys[0]
		right := m.SplitOff(splitKey)
		m.check(t)
		right.check(t)
		expectEq(t, collect(m.Keys()), smallKeys)
		expectEq(t, collect(right.Keys()), largeKeys)
	}
}

func TestBTreeMap_test_split_off_large_random_sorted(t *testing.T) {
	data := randData(1529)
	// special case with maximum height.
	data = sorted(data)

	m := BTreeMapFromIter(pairSeqOf(data))
	key := data[len(data)/2].F0
	right := m.SplitOff(key)
	m.check(t)
	right.check(t)

	below := []rs.Tuple2[uint32, uint32]{}
	atOrAbove := []rs.Tuple2[uint32, uint32]{}
	for _, x := range data {
		if x.F0 < key {
			below = append(below, x)
		} else {
			atOrAbove = append(atOrAbove, x)
		}
	}
	expectEq(t, collectOwned(m.IntoIter()), below)
	expectEq(t, collectOwned(right.IntoIter()), atOrAbove)
}

func TestBTreeMap_test_into_keys(t *testing.T) {
	m := BTreeMapFrom(rs.T2(1, 'a'), rs.T2(2, 'b'), rs.T2(3, 'c'))
	keys := collect(m.IntoKeys())

	expectEq(t, len(keys), 3)
	expect(t, slices.Contains(keys, 1))
	expect(t, slices.Contains(keys, 2))
	expect(t, slices.Contains(keys, 3))
}

func TestBTreeMap_test_into_values(t *testing.T) {
	m := BTreeMapFrom(rs.T2(1, 'a'), rs.T2(2, 'b'), rs.T2(3, 'c'))
	values := collect(m.IntoValues())

	expectEq(t, len(values), 3)
	expect(t, slices.Contains(values, 'a'))
	expect(t, slices.Contains(values, 'b'))
	expect(t, slices.Contains(values, 'c'))
}

func TestBTreeMap_test_insert_remove_intertwined(t *testing.T) {
	loops := 1_000_000
	var m BTreeMap[int, int]
	i := 1
	offset := 165 // somewhat arbitrarily chosen to cover some code paths
	for range loops {
		i = (i + offset) & 0xFF
		m.Insert(i, i)
		m.Remove(0xFF - i)
	}
	m.check(t)
}

func TestBTreeMap_test_insert_remove_intertwined_ord_chaos(t *testing.T) {
	loops := 1_000_000
	gov := &governor{}
	var m BTreeMap[governed, rs.Unit]
	i := 1
	offset := 165 // more arbitrarily copied from above
	for range loops {
		i = (i + offset) & 0xFF
		m.Insert(governed{i, gov}, rs.Unit{})
		m.Remove(governed{0xFF - i, gov})
		gov.flip()
	}
	m.checkStructure(t)
}

func TestBTreeMap_from_array(t *testing.T) {
	m := BTreeMapFrom(rs.T2(1, 2), rs.T2(3, 4))
	unorderedDuplicates := BTreeMapFrom(rs.T2(3, 4), rs.T2(1, 2), rs.T2(1, 2))
	expectEq(t, m, unorderedDuplicates)
}

func TestBTreeMap_test_id_based_insert(t *testing.T) {
	var lhs, rhs BTreeMap[idBased, string]

	lhs.Insert(idBased{id: 0, name: "lhs_k"}, "lhs_v")
	rhs.Insert(idBased{id: 0, name: "rhs_k"}, "rhs_v")

	for k, v := range rhs.IntoIter() {
		lhs.Insert(k, v)
	}

	expectEq(t, lhs.PopFirst().Unwrap().F0.name, "lhs_k")
}

func TestBTreeMap_test_id_based_append(t *testing.T) {
	var lhs, rhs BTreeMap[idBased, string]

	lhs.Insert(idBased{id: 0, name: "lhs_k"}, "lhs_v")
	rhs.Insert(idBased{id: 0, name: "rhs_k"}, "rhs_v")

	lhs.Append(&rhs)

	expectEq(t, lhs.PopFirst().Unwrap().F0.name, "lhs_k")
}
