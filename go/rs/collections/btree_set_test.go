// Port of alloc 1.99.1 src/collections/btree/set/tests.rs

package collections

// Not ported:
//   - the size_hint tests (test_intersection_size_hint,
//     test_difference_size_hint, test_symmetric_difference_size_hint,
//     test_union_size_hint): the port's sequences have no size hint;
//   - test_extract_if_drop_panic_leak and the drop counts of
//     test_extract_if_pred_panic_leak: Go has no destructors;
//   - assert_covariance, assert_sync, assert_send and assert_derives, which
//     are checks of the compiler.

import (
	"iter"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestBTreeSet_test_clone_eq(t *testing.T) {
	var m BTreeSet[int]

	m.Insert(1)
	m.Insert(2)

	expectEq(t, rs.Clone(m), m)
}

func TestBTreeSet_test_iter_min_max(t *testing.T) {
	var a BTreeSet[int]
	empty := func() *BTreeSet[int] {
		set := BTreeSetNew[int]()
		return &set
	}
	all := func() (Bound[int], Bound[int]) { return Unbounded[int](), Unbounded[int]() }
	none := rs.None[int]()
	expectEq(t, seqMin(a.Iter()), none)
	expectEq(t, seqMax(a.Iter()), none)
	expectEq(t, seqMin(a.Range(all())), none)
	expectEq(t, seqMax(a.Range(all())), none)
	expectEq(t, seqMin(a.Difference(empty())), none)
	expectEq(t, seqMax(a.Difference(empty())), none)
	expectEq(t, seqMin(a.Intersection(&a)), none)
	expectEq(t, seqMax(a.Intersection(&a)), none)
	expectEq(t, seqMin(a.SymmetricDifference(empty())), none)
	expectEq(t, seqMax(a.SymmetricDifference(empty())), none)
	expectEq(t, seqMin(a.Union(&a)), none)
	expectEq(t, seqMax(a.Union(&a)), none)
	a.Insert(1)
	a.Insert(2)
	expectEq(t, seqMin(a.Iter()), rs.Some(1))
	expectEq(t, seqMax(a.Iter()), rs.Some(2))
	expectEq(t, seqMin(a.Range(all())), rs.Some(1))
	expectEq(t, seqMax(a.Range(all())), rs.Some(2))
	expectEq(t, seqMin(a.Difference(empty())), rs.Some(1))
	expectEq(t, seqMax(a.Difference(empty())), rs.Some(2))
	expectEq(t, seqMin(a.Intersection(&a)), rs.Some(1))
	expectEq(t, seqMax(a.Intersection(&a)), rs.Some(2))
	expectEq(t, seqMin(a.SymmetricDifference(empty())), rs.Some(1))
	expectEq(t, seqMax(a.SymmetricDifference(empty())), rs.Some(2))
	expectEq(t, seqMin(a.Union(&a)), rs.Some(1))
	expectEq(t, seqMax(a.Union(&a)), rs.Some(2))
}

// checkSets builds sets from a and b and has f stream its result through
// the callback, which compares each element with expected; f stops when
// the callback returns false, as `all` does.
func checkSets(t *testing.T, a, b, expected []int32, f func(x, y *BTreeSet[int32], each func(int32) bool) bool) {
	t.Helper()
	var setA, setB BTreeSet[int32]

	for _, x := range a {
		expect(t, setA.Insert(x))
	}
	for _, y := range b {
		expect(t, setB.Insert(y))
	}

	i := 0
	f(&setA, &setB, func(x int32) bool {
		if i < len(expected) {
			expectEq(t, x, expected[i])
		}
		i++
		return true
	})
	expectEq(t, i, len(expected))
}

// all is Iterator::all.
func all[T any](seq iter.Seq[T], f func(T) bool) bool {
	for element := range seq {
		if !f(element) {
			return false
		}
	}
	return true
}

func TestBTreeSet_test_intersection(t *testing.T) {
	checkIntersection := func(a, b, expected []int32) {
		t.Helper()
		checkSets(t, a, b, expected, func(x, y *BTreeSet[int32], f func(int32) bool) bool { return all(x.Intersection(y), f) })
	}

	checkIntersection([]int32{}, []int32{}, []int32{})
	checkIntersection([]int32{1, 2, 3}, []int32{}, []int32{})
	checkIntersection([]int32{}, []int32{1, 2, 3}, []int32{})
	checkIntersection([]int32{2}, []int32{1, 2, 3}, []int32{2})
	checkIntersection([]int32{1, 2, 3}, []int32{2}, []int32{2})
	checkIntersection([]int32{11, 1, 3, 77, 103, 5, -5}, []int32{2, 11, 77, -9, -42, 5, 3}, []int32{3, 5, 11, 77})

	large := intRange(int32(0), 100)
	checkIntersection([]int32{}, large, []int32{})
	checkIntersection(large, []int32{}, []int32{})
	checkIntersection([]int32{-1}, large, []int32{})
	checkIntersection(large, []int32{-1}, []int32{})
	checkIntersection([]int32{0}, large, []int32{0})
	checkIntersection(large, []int32{0}, []int32{0})
	checkIntersection([]int32{99}, large, []int32{99})
	checkIntersection(large, []int32{99}, []int32{99})
	checkIntersection([]int32{100}, large, []int32{})
	checkIntersection(large, []int32{100}, []int32{})
	checkIntersection([]int32{11, 5000, 1, 3, 77, 8924}, large, []int32{1, 3, 11, 77})
}

func TestBTreeSet_test_difference(t *testing.T) {
	checkDifference := func(a, b, expected []int32) {
		t.Helper()
		checkSets(t, a, b, expected, func(x, y *BTreeSet[int32], f func(int32) bool) bool { return all(x.Difference(y), f) })
	}

	checkDifference([]int32{}, []int32{}, []int32{})
	checkDifference([]int32{1, 12}, []int32{}, []int32{1, 12})
	checkDifference([]int32{}, []int32{1, 2, 3, 9}, []int32{})
	checkDifference([]int32{1, 3, 5, 9, 11}, []int32{3, 9}, []int32{1, 5, 11})
	checkDifference([]int32{1, 3, 5, 9, 11}, []int32{3, 6, 9}, []int32{1, 5, 11})
	checkDifference([]int32{1, 3, 5, 9, 11}, []int32{0, 1}, []int32{3, 5, 9, 11})
	checkDifference([]int32{1, 3, 5, 9, 11}, []int32{11, 12}, []int32{1, 3, 5, 9})
	checkDifference(
		[]int32{-5, 11, 22, 33, 40, 42},
		[]int32{-12, -5, 14, 23, 34, 38, 39, 50},
		[]int32{11, 22, 33, 40, 42},
	)

	large := intRange(int32(0), 100)
	checkDifference([]int32{}, large, []int32{})
	checkDifference([]int32{-1}, large, []int32{-1})
	checkDifference([]int32{0}, large, []int32{})
	checkDifference([]int32{99}, large, []int32{})
	checkDifference([]int32{100}, large, []int32{100})
	checkDifference([]int32{11, 5000, 1, 3, 77, 8924}, large, []int32{5000, 8924})
	checkDifference(large, []int32{}, large)
	checkDifference(large, []int32{-1}, large)
	checkDifference(large, []int32{100}, large)
}

func TestBTreeSet_test_symmetric_difference(t *testing.T) {
	checkSymmetricDifference := func(a, b, expected []int32) {
		t.Helper()
		checkSets(t, a, b, expected, func(x, y *BTreeSet[int32], f func(int32) bool) bool {
			return all(x.SymmetricDifference(y), f)
		})
	}

	checkSymmetricDifference([]int32{}, []int32{}, []int32{})
	checkSymmetricDifference([]int32{1, 2, 3}, []int32{2}, []int32{1, 3})
	checkSymmetricDifference([]int32{2}, []int32{1, 2, 3}, []int32{1, 3})
	checkSymmetricDifference([]int32{1, 3, 5, 9, 11}, []int32{-2, 3, 9, 14, 22}, []int32{-2, 1, 5, 11, 14, 22})
}

func TestBTreeSet_test_union(t *testing.T) {
	checkUnion := func(a, b, expected []int32) {
		t.Helper()
		checkSets(t, a, b, expected, func(x, y *BTreeSet[int32], f func(int32) bool) bool { return all(x.Union(y), f) })
	}

	checkUnion([]int32{}, []int32{}, []int32{})
	checkUnion([]int32{1, 2, 3}, []int32{2}, []int32{1, 2, 3})
	checkUnion([]int32{2}, []int32{1, 2, 3}, []int32{1, 2, 3})
	checkUnion(
		[]int32{1, 3, 5, 9, 11, 16, 19, 24},
		[]int32{-2, 1, 5, 9, 13, 19},
		[]int32{-2, 1, 3, 5, 9, 11, 13, 16, 19, 24},
	)
}

// Only tests the simple function definition with respect to intersection
func TestBTreeSet_test_is_disjoint(t *testing.T) {
	one := BTreeSetFrom(1)
	two := BTreeSetFrom(2)
	expect(t, one.IsDisjoint(&two))
}

// Also implicitly tests the trivial function definition of is_superset
func TestBTreeSet_test_is_subset(t *testing.T) {
	isSubset := func(a, b []int32) bool {
		setA := BTreeSetFromIter(slices.Values(a))
		setB := BTreeSetFromIter(slices.Values(b))
		return setA.IsSubset(&setB)
	}

	expectEq(t, isSubset([]int32{}, []int32{}), true)
	expectEq(t, isSubset([]int32{}, []int32{1, 2}), true)
	expectEq(t, isSubset([]int32{0}, []int32{1, 2}), false)
	expectEq(t, isSubset([]int32{1}, []int32{1, 2}), true)
	expectEq(t, isSubset([]int32{2}, []int32{1, 2}), true)
	expectEq(t, isSubset([]int32{3}, []int32{1, 2}), false)
	expectEq(t, isSubset([]int32{1, 2}, []int32{1}), false)
	expectEq(t, isSubset([]int32{1, 2}, []int32{1, 2}), true)
	expectEq(t, isSubset([]int32{1, 2}, []int32{2, 3}), false)
	expectEq(t,
		isSubset([]int32{-5, 11, 22, 33, 40, 42}, []int32{-12, -5, 11, 14, 22, 23, 33, 34, 38, 39, 40, 42}),
		true,
	)
	expectEq(t, isSubset([]int32{-5, 11, 22, 33, 40, 42}, []int32{-12, -5, 11, 14, 22, 23, 34, 38}), false)

	large := intRange(int32(0), 100)
	expectEq(t, isSubset([]int32{}, large), true)
	expectEq(t, isSubset(large, []int32{}), false)
	expectEq(t, isSubset([]int32{-1}, large), false)
	expectEq(t, isSubset([]int32{0}, large), true)
	expectEq(t, isSubset([]int32{1, 2}, large), true)
	expectEq(t, isSubset([]int32{99, 100}, large), false)
}

func TestBTreeSet_test_is_superset(t *testing.T) {
	isSuperset := func(a, b []int32) bool {
		setA := BTreeSetFromIter(slices.Values(a))
		setB := BTreeSetFromIter(slices.Values(b))
		return setA.IsSuperset(&setB)
	}

	expectEq(t, isSuperset([]int32{}, []int32{}), true)
	expectEq(t, isSuperset([]int32{}, []int32{1, 2}), false)
	expectEq(t, isSuperset([]int32{0}, []int32{1, 2}), false)
	expectEq(t, isSuperset([]int32{1}, []int32{1, 2}), false)
	expectEq(t, isSuperset([]int32{4}, []int32{1, 2}), false)
	expectEq(t, isSuperset([]int32{1, 4}, []int32{1, 2}), false)
	expectEq(t, isSuperset([]int32{1, 2}, []int32{1, 2}), true)
	expectEq(t, isSuperset([]int32{1, 2, 3}, []int32{1, 3}), true)
	expectEq(t, isSuperset([]int32{1, 2, 3}, []int32{}), true)
	expectEq(t, isSuperset([]int32{-1, 1, 2, 3}, []int32{-1, 3}), true)

	large := intRange(int32(0), 100)
	expectEq(t, isSuperset([]int32{}, large), false)
	expectEq(t, isSuperset(large, []int32{}), true)
	expectEq(t, isSuperset(large, []int32{1}), true)
	expectEq(t, isSuperset(large, []int32{50, 99}), true)
	expectEq(t, isSuperset(large, []int32{100}), false)
	expectEq(t, isSuperset(large, []int32{0, 99}), true)
	expectEq(t, isSuperset([]int32{-1}, large), false)
	expectEq(t, isSuperset([]int32{0}, large), false)
	expectEq(t, isSuperset([]int32{99, 100}, large), false)
}

func TestBTreeSet_test_retain(t *testing.T) {
	set := BTreeSetFrom(1, 2, 3, 4, 5, 6)
	set.Retain(func(k int) bool { return k%2 == 0 })
	expectEq(t, set.Len(), 3)
	expect(t, set.Contains(2))
	expect(t, set.Contains(4))
	expect(t, set.Contains(6))
}

func TestBTreeSet_test_extract_if(t *testing.T) {
	x := BTreeSetFrom(1)
	y := BTreeSetFrom(1)

	for range x.ExtractIf(Unbounded[int](), Unbounded[int](), func(int) bool { return true }) {
	}
	for range y.ExtractIf(Unbounded[int](), Unbounded[int](), func(int) bool { return false }) {
	}
	expectEq(t, x.Len(), 0)
	expectEq(t, y.Len(), 1)
}

// The drop counts of the Rust test have no counterpart: Go has no
// destructors.
func TestBTreeSet_test_extract_if_pred_panic_leak(t *testing.T) {
	a := &crashTestDummy{id: 0}
	b := &crashTestDummy{id: 1}
	c := &crashTestDummy{id: 2}
	var set BTreeSet[dummyInstance]
	set.Insert(a.spawn(panicNever))
	set.Insert(b.spawn(panicInQuery))
	set.Insert(c.spawn(panicInQuery))

	panicMessage(func() {
		for range set.ExtractIf(Unbounded[dummyInstance](), Unbounded[dummyInstance](), func(dummy dummyInstance) bool { return dummy.query(true) }) {
		}
	})

	expectEq(t, a.queried, 1)
	expectEq(t, b.queried, 1)
	expectEq(t, c.queried, 0)
	expectEq(t, set.Len(), 2)
	first, _ := set.First()
	expectEq(t, first.id(), 1)
	last, _ := set.Last()
	expectEq(t, last.id(), 2)
}

func TestBTreeSet_test_clear(t *testing.T) {
	var x BTreeSet[int]
	x.Insert(1)

	x.Clear()
	expect(t, x.IsEmpty())
}

func TestBTreeSet_test_remove(t *testing.T) {
	var x BTreeSet[int]
	expect(t, x.IsEmpty())

	x.Insert(1)
	x.Insert(2)
	x.Insert(3)
	x.Insert(4)

	expectEq(t, x.Remove(2), true)
	expectEq(t, x.Remove(0), false)
	expectEq(t, x.Remove(5), false)
	expectEq(t, x.Remove(1), true)
	expectEq(t, x.Remove(2), false)
	expectEq(t, x.Remove(3), true)
	expectEq(t, x.Remove(4), true)
	expectEq(t, x.Remove(4), false)
	expect(t, x.IsEmpty())
}

func TestBTreeSet_test_zip(t *testing.T) {
	var x BTreeSet[int]
	x.Insert(5)
	x.Insert(12)
	x.Insert(11)

	var y BTreeSet[string]
	y.Insert("foo")
	y.Insert("bar")

	nextX, stopX := iter.Pull(x.Iter())
	defer stopX()
	nextY, stopY := iter.Pull(y.Iter())
	defer stopY()
	z := func() rs.Option[rs.Tuple2[int, string]] {
		a, okA := nextX()
		if !okA {
			return rs.None[rs.Tuple2[int, string]]()
		}
		b, okB := nextY()
		if !okB {
			return rs.None[rs.Tuple2[int, string]]()
		}
		return rs.Some(rs.T2(a, b))
	}

	expectEq(t, z().Unwrap(), rs.T2(5, "bar"))
	expectEq(t, z().Unwrap(), rs.T2(11, "foo"))
	expect(t, z().IsNone())
}

func TestBTreeSet_test_from_iter(t *testing.T) {
	xs := []int{1, 2, 3, 4, 5, 6, 7, 8, 9}

	set := BTreeSetFromIter(slices.Values(xs))

	for _, x := range xs {
		expect(t, set.Contains(x))
	}
}

func TestBTreeSet_test_show(t *testing.T) {
	var set BTreeSet[int32]
	empty := BTreeSetNew[int32]()

	set.Insert(1)
	set.Insert(2)

	setStr := rs.Format("{set:?}", rs.Arg("set", &set))

	expectEq(t, setStr, "{1, 2}")
	expectEq(t, rs.Format("{empty:?}", rs.Arg("empty", &empty)), "{}")
}

func TestBTreeSet_test_extend_ref(t *testing.T) {
	var a BTreeSet[int32]
	a.Insert(1)

	a.Extend(slices.Values([]int32{2, 3, 4}))

	expectEq(t, a.Len(), 4)
	expect(t, a.Contains(1))
	expect(t, a.Contains(2))
	expect(t, a.Contains(3))
	expect(t, a.Contains(4))

	var b BTreeSet[int32]
	b.Insert(5)
	b.Insert(6)

	a.Extend(b.Iter())

	expectEq(t, a.Len(), 6)
	expect(t, a.Contains(1))
	expect(t, a.Contains(2))
	expect(t, a.Contains(3))
	expect(t, a.Contains(4))
	expect(t, a.Contains(5))
	expect(t, a.Contains(6))
}

// foo is the Foo of test_recovery: equal and ordered by its first field
// only.
type foo struct {
	name  string
	other int32
}

func (f foo) Equal(other foo) bool { return f.name == other.name }

func (f foo) Compare(other foo) int { return rs.Cmp(f.name, other.name) }

func TestBTreeSet_test_recovery(t *testing.T) {
	// The second field takes part in no comparison, so the tests compare
	// both fields themselves where Rust's Foo hides the second.
	same := func(got rs.Option[foo], want rs.Option[foo]) {
		t.Helper()
		gotFoo, gotOk := got.Get()
		wantFoo, wantOk := want.Get()
		if gotOk != wantOk || gotFoo != wantFoo {
			t.Fatalf("got %v, want %v", got, want)
		}
	}

	var s BTreeSet[foo]
	same(s.Replace(foo{"a", 1}), rs.None[foo]())
	expectEq(t, s.Len(), 1)
	same(s.Replace(foo{"a", 2}), rs.Some(foo{"a", 1}))
	expectEq(t, s.Len(), 1)

	{
		elements := collect(s.Iter())
		expectEq(t, len(elements), 1)
		same(rs.Some(elements[0]), rs.Some(foo{"a", 2}))
	}

	same(rs.OptionOf(s.Get(foo{"a", 1})), rs.Some(foo{"a", 2}))
	same(s.Take(foo{"a", 1}), rs.Some(foo{"a", 2}))
	expectEq(t, s.Len(), 0)

	same(rs.OptionOf(s.Get(foo{"a", 1})), rs.None[foo]())
	same(s.Take(foo{"a", 1}), rs.None[foo]())

	expectEq(t, count(s.Iter()), 0)
}

// What needs no order of the elements works without one: rs.Cmp is never
// asked about a nonOrd, which it could not order.
func TestBTreeSet_test_ord_absence(t *testing.T) {
	setFn := func(set BTreeSet[nonOrd]) {
		_ = set.IsEmpty()
		_ = set.Len()
		set.Clear()
		_ = set.Iter()
		_ = set.IntoIter()
	}

	setDebug := func(set BTreeSet[nonOrd]) {
		_ = rs.Format("{:?}", &set)
	}

	setClone := func(set BTreeSet[nonOrd]) {
		set = rs.Clone(set)
	}

	setFn(BTreeSetNew[nonOrd]())
	setDebug(BTreeSetNew[nonOrd]())
	setClone(BTreeSetDefault[nonOrd]())
}

func TestBTreeSet_test_append(t *testing.T) {
	var a BTreeSet[int]
	a.Insert(1)
	a.Insert(2)
	a.Insert(3)

	var b BTreeSet[int]
	b.Insert(3)
	b.Insert(4)
	b.Insert(5)

	a.Append(&b)

	expectEq(t, a.Len(), 5)
	expectEq(t, b.Len(), 0)

	expectEq(t, a.Contains(1), true)
	expectEq(t, a.Contains(2), true)
	expectEq(t, a.Contains(3), true)
	expectEq(t, a.Contains(4), true)
	expectEq(t, a.Contains(5), true)
}

func TestBTreeSet_test_first_last(t *testing.T) {
	var a BTreeSet[int]
	first := func() rs.Option[int] { return rs.OptionOf(a.First()) }
	last := func() rs.Option[int] { return rs.OptionOf(a.Last()) }
	expectEq(t, first(), rs.None[int]())
	expectEq(t, last(), rs.None[int]())
	a.Insert(1)
	expectEq(t, first(), rs.Some(1))
	expectEq(t, last(), rs.Some(1))
	a.Insert(2)
	expectEq(t, first(), rs.Some(1))
	expectEq(t, last(), rs.Some(2))
	for i := 3; i <= 12; i++ {
		a.Insert(i)
	}
	expectEq(t, first(), rs.Some(1))
	expectEq(t, last(), rs.Some(12))
	expectEq(t, a.PopFirst(), rs.Some(1))
	expectEq(t, a.PopLast(), rs.Some(12))
	expectEq(t, a.PopFirst(), rs.Some(2))
	expectEq(t, a.PopLast(), rs.Some(11))
	expectEq(t, a.PopFirst(), rs.Some(3))
	expectEq(t, a.PopLast(), rs.Some(10))
	expectEq(t, a.PopFirst(), rs.Some(4))
	expectEq(t, a.PopFirst(), rs.Some(5))
	expectEq(t, a.PopFirst(), rs.Some(6))
	expectEq(t, a.PopFirst(), rs.Some(7))
	expectEq(t, a.PopFirst(), rs.Some(8))
	cloned := rs.Clone(a)
	expectEq(t, cloned.PopLast(), rs.Some(9))
	expectEq(t, a.PopFirst(), rs.Some(9))
	expectEq(t, a.PopFirst(), rs.None[int]())
	expectEq(t, a.PopLast(), rs.None[int]())
}

// Unlike the function with the same name in map/tests, returns no values.
// Which also means it returns different predetermined pseudo-random keys,
// and the test cases using this function explore slightly different trees.
func randKeys(length int) []uint32 {
	rng := newDeterministicRng()
	data := make([]uint32, 0, length)
	for range length {
		data = append(data, rng.next())
	}
	return data
}

func TestBTreeSet_test_split_off_empty_right(t *testing.T) {
	data := randKeys(173)

	set := BTreeSetFromIter(slices.Values(data))
	right := set.SplitOff(slices.Max(data) + 1)

	data = sorted(data)
	expectEq(t, collect(set.IntoIter()), data)
	expectEq(t, collect(right.IntoIter()), []uint32{})
}

func TestBTreeSet_test_split_off_empty_left(t *testing.T) {
	data := randKeys(314)

	set := BTreeSetFromIter(slices.Values(data))
	right := set.SplitOff(slices.Min(data))

	data = sorted(data)
	expectEq(t, collect(set.IntoIter()), []uint32{})
	expectEq(t, collect(right.IntoIter()), data)
}

func TestBTreeSet_test_split_off_large_random_sorted(t *testing.T) {
	data := randKeys(1529)
	// special case with maximum height.
	data = sorted(data)

	set := BTreeSetFromIter(slices.Values(data))
	key := data[len(data)/2]
	right := set.SplitOff(key)

	below := []uint32{}
	atOrAbove := []uint32{}
	for _, x := range data {
		if x < key {
			below = append(below, x)
		} else {
			atOrAbove = append(atOrAbove, x)
		}
	}
	expectEq(t, collect(set.IntoIter()), below)
	expectEq(t, collect(right.IntoIter()), atOrAbove)
}

func TestBTreeSet_from_array(t *testing.T) {
	set := BTreeSetFrom(1, 2, 3, 4)
	unorderedDuplicates := BTreeSetFrom(4, 1, 4, 3, 2)
	expectEq(t, set, unorderedDuplicates)
}

func TestBTreeSet_test_range_panic_1(t *testing.T) {
	var set BTreeSet[int]
	set.Insert(3)
	set.Insert(5)
	set.Insert(8)

	shouldPanic(t, "range start is greater than range end in BTreeSet", func() {
		_ = set.Range(Included(8), Included(3))
	})
}

func TestBTreeSet_test_range_panic_2(t *testing.T) {
	var set BTreeSet[int]
	set.Insert(3)
	set.Insert(5)
	set.Insert(8)

	shouldPanic(t, "range start and end are equal and excluded in BTreeSet", func() {
		_ = set.Range(Excluded(5), Excluded(5))
	})
}
