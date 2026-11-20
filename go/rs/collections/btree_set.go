// Port of alloc 1.99.1 src/collections/btree/set.rs (with merge_iter.rs and
// set_val.rs)

package collections

import (
	"iter"
	"reflect"
	"slices"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// btreeSetVal is SetValZST in set_val.rs: the value type of the map inside a
// BTreeSet, distinct from every type a caller can name.
type btreeSetVal struct{}

// BTreeSet is Rust's BTreeSet<T>: an ordered set whose elements are kept and
// iterated in order. Elements are ordered by rs.Cmp, and by nothing else.
//
// The zero value is an empty set. Like BTreeMap it is changed through a
// pointer, and a plain assignment shares the storage instead of copying it;
// `set.clone()` is rs.Clone(s), which rebuilds the set through
// rs.ReflectSeq as BTreeMap describes. Elements are passed and
// returned by value: an element of a set cannot be changed in place. The
// sequences yield elements in order and must not have their set changed
// while they run, except that the element just yielded may be removed.
type BTreeSet[T any] struct {
	m BTreeMap[T, btreeSetVal]
}

// This constant is used by functions that compare two sets.
// It estimates the relative size at which searching performs better
// than iterating, based on the benchmarks in
// https://github.com/ssomers/rust_bench_btreeset_intersection.
// It's used to divide rather than multiply sizes, to rule out overflow,
// and it's a power of two to make that division cheap.
const btreeSetIterPerformanceTippingSizeDiff = 16

// BTreeSetNew is BTreeSet::new: an empty set.
func BTreeSetNew[T any]() BTreeSet[T] {
	var empty BTreeSet[T]
	return empty
}

// BTreeSetDefault is BTreeSet::default: an empty set.
func BTreeSetDefault[T any]() BTreeSet[T] {
	var empty BTreeSet[T]
	return empty
}

// BTreeSetFrom is BTreeSet::from([a, b, ..]). Of several equal elements the
// last one is kept.
func BTreeSetFrom[T any](elements ...T) BTreeSet[T] {
	if len(elements) == 0 {
		return BTreeSetNew[T]()
	}
	// use stable sort to preserve the insertion order.
	sorted := slices.Clone(elements)
	slices.SortStableFunc(sorted, rs.Cmp[T])
	return btreeSetFromSortedIter(slices.Values(sorted))
}

// BTreeSetFromIter is `iter.collect::<BTreeSet<T>>()` (FromIterator). Of
// several equal elements the last one is kept; that differs from inserting
// them one by one, which keeps the first.
func BTreeSetFromIter[T any](seq iter.Seq[T]) BTreeSet[T] {
	inputs := slices.Collect(seq)
	if len(inputs) == 0 {
		return BTreeSetNew[T]()
	}
	// use stable sort to preserve the insertion order.
	slices.SortStableFunc(inputs, rs.Cmp[T])
	return btreeSetFromSortedIter(slices.Values(inputs))
}

// btreeSetFromSortedIter is BTreeSet::from_sorted_iter.
func btreeSetFromSortedIter[T any](sorted iter.Seq[T]) BTreeSet[T] {
	var pairs []rs.Tuple2[T, btreeSetVal]
	for element := range sorted {
		pairs = append(pairs, rs.Tuple2[T, btreeSetVal]{F0: element, F1: btreeSetVal{}})
	}
	return BTreeSet[T]{m: btreeBulkBuildFromSortedIter(pairs)}
}

// Equal is BTreeSet's PartialEq: the same number of elements, equal (by
// rs.Eq) element for element.
func (s BTreeSet[T]) Equal(other *BTreeSet[T]) bool {
	if s.m.length != other.m.length {
		return false
	}
	a, b := s.m.firstNode(), other.m.firstNode()
	for a != nil && b != nil {
		if !rs.Eq(a.key, b.key) {
			return false
		}
		a, b = a.next(), b.next()
	}
	return true
}

// Compare is BTreeSet's Ord: the elements compared in order by rs.Cmp, and
// the shorter set first when one is a prefix of the other.
func (s BTreeSet[T]) Compare(other *BTreeSet[T]) int {
	a, b := s.m.firstNode(), other.m.firstNode()
	for {
		switch {
		case a == nil && b == nil:
			return 0
		case a == nil:
			return -1
		case b == nil:
			return 1
		}
		if order := rs.Cmp(a.key, b.key); order != 0 {
			return order
		}
		a, b = a.next(), b.next()
	}
}

// Range is BTreeSet::range: the elements between the two bounds, in order
// (see Bound for how a Rust range is written). Like Rust it checks the
// bounds when it is called, and panics when the start is greater than the
// end or when both are excluded and equal.
func (s BTreeSet[T]) Range(lo, hi Bound[T]) iter.Seq[T] {
	first, last, failure := s.m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(T) bool) {
		btreeAscend(first, last, func(n *btreeNode[T, btreeSetVal]) bool { return yield(n.key) })
	}
}

// RangeRev is `set.range(..).rev()`: the elements of Range from the greatest
// down.
func (s BTreeSet[T]) RangeRev(lo, hi Bound[T]) iter.Seq[T] {
	first, last, failure := s.m.rangeSearch(lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(T) bool) {
		btreeDescend(first, last, func(n *btreeNode[T, btreeSetVal]) bool { return yield(n.key) })
	}
}

// Difference is BTreeSet::difference: the elements of s that are not in
// other, in order.
func (s BTreeSet[T]) Difference(other *BTreeSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		selfIter := s.m.iter()
		selfMin, selfMax := s.m.firstNode(), s.m.lastNode()
		otherMin, otherMax := other.m.firstNode(), other.m.lastNode()
		if selfMin == nil || otherMin == nil {
			// simply produce all elements in `self`
			btreeSetDrain(&selfIter, yield)
			return
		}
		minToMax, maxToMin := rs.Cmp(selfMin.key, otherMax.key), rs.Cmp(selfMax.key, otherMin.key)
		switch {
		case minToMax > 0 || maxToMin < 0:
			btreeSetDrain(&selfIter, yield)
		case minToMax == 0:
			selfIter.next()
			btreeSetDrain(&selfIter, yield)
		case maxToMin == 0:
			selfIter.nextBack()
			btreeSetDrain(&selfIter, yield)
		case s.Len() <= other.Len()/btreeSetIterPerformanceTippingSizeDiff:
			// iterate `self`, look up in `other`
			for selfNext := selfIter.next(); selfNext != nil; selfNext = selfIter.next() {
				if !other.Contains(selfNext.key) && !yield(selfNext.key) {
					return
				}
			}
		default:
			// iterate all of `self` and some of `other`, spotting matches
			// along the way
			otherIter := other.m.iter()
			otherPeeked := otherIter.next()
			for selfNext := selfIter.next(); selfNext != nil; {
				order := -1
				if otherPeeked != nil {
					order = rs.Cmp(selfNext.key, otherPeeked.key)
				}
				switch {
				case order < 0:
					if !yield(selfNext.key) {
						return
					}
					selfNext = selfIter.next()
				case order == 0:
					selfNext = selfIter.next()
					otherPeeked = otherIter.next()
				default:
					otherPeeked = otherIter.next()
				}
			}
		}
	}
}

// btreeSetDrain yields what is left of it.
func btreeSetDrain[T any](it *btreeIter[T, btreeSetVal], yield func(T) bool) {
	for n := it.next(); n != nil; n = it.next() {
		if !yield(n.key) {
			return
		}
	}
}

// btreeMergeIter is MergeIterInner in merge_iter.rs: the core of an iterator
// that merges the output of two strictly ascending iterators, for instance a
// union or a symmetric difference.
type btreeMergeIter[T any] struct {
	a, b        btreeIter[T, btreeSetVal]
	peeked      *btreeNode[T, btreeSetVal]
	peekedFromA bool
}

// nexts returns the next pair of items stemming from the pair of sources
// being merged. If both returned nodes are set, their elements are equal and
// occur in both sources. If one is set, its element doesn't occur in the
// other source. If neither is set, iteration has finished.
func (it *btreeMergeIter[T]) nexts() (aNext, bNext *btreeNode[T, btreeSetVal]) {
	switch {
	case it.peeked != nil && it.peekedFromA:
		aNext, it.peeked = it.peeked, nil
		bNext = it.b.next()
	case it.peeked != nil:
		bNext, it.peeked = it.peeked, nil
		aNext = it.a.next()
	default:
		aNext = it.a.next()
		bNext = it.b.next()
	}
	if aNext != nil && bNext != nil {
		order := rs.Cmp(aNext.key, bNext.key)
		switch {
		case order < 0:
			it.peeked, it.peekedFromA = bNext, false
			bNext = nil
		case order > 0:
			it.peeked, it.peekedFromA = aNext, true
			aNext = nil
		}
	}
	return aNext, bNext
}

// SymmetricDifference is BTreeSet::symmetric_difference: the elements that
// are in s or in other but not in both, in order.
func (s BTreeSet[T]) SymmetricDifference(other *BTreeSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		merge := btreeMergeIter[T]{a: s.m.iter(), b: other.m.iter(), peeked: nil, peekedFromA: false}
		for {
			aNext, bNext := merge.nexts()
			switch {
			case aNext != nil && bNext != nil:
				continue
			case aNext != nil:
				if !yield(aNext.key) {
					return
				}
			case bNext != nil:
				if !yield(bNext.key) {
					return
				}
			default:
				return
			}
		}
	}
}

// Intersection is BTreeSet::intersection: the elements that are in both s
// and other, in order.
func (s BTreeSet[T]) Intersection(other *BTreeSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		selfMin, selfMax := s.m.firstNode(), s.m.lastNode()
		otherMin, otherMax := other.m.firstNode(), other.m.lastNode()
		if selfMin == nil || otherMin == nil {
			return
		}
		minToMax, maxToMin := rs.Cmp(selfMin.key, otherMax.key), rs.Cmp(selfMax.key, otherMin.key)
		switch {
		case minToMax > 0 || maxToMin < 0:
			// return emptiness
		case minToMax == 0:
			yield(selfMin.key)
		case maxToMin == 0:
			yield(selfMax.key)
		case s.Len() <= other.Len()/btreeSetIterPerformanceTippingSizeDiff:
			btreeSetSearchIntersection(s.m.iter(), other, yield)
		case other.Len() <= s.Len()/btreeSetIterPerformanceTippingSizeDiff:
			btreeSetSearchIntersection(other.m.iter(), &s, yield)
		default:
			// iterate similarly sized sets jointly, spotting matches along
			// the way
			a, b := s.m.iter(), other.m.iter()
			aNext, bNext := a.next(), b.next()
			for aNext != nil && bNext != nil {
				order := rs.Cmp(aNext.key, bNext.key)
				switch {
				case order < 0:
					aNext = a.next()
				case order > 0:
					bNext = b.next()
				default:
					if !yield(aNext.key) {
						return
					}
					aNext, bNext = a.next(), b.next()
				}
			}
		}
	}
}

// btreeSetSearchIntersection iterates a small set and looks each element up
// in the large set. The elements it yields are the small set's.
func btreeSetSearchIntersection[T any](smallIter btreeIter[T, btreeSetVal], largeSet *BTreeSet[T], yield func(T) bool) {
	for smallNext := smallIter.next(); smallNext != nil; smallNext = smallIter.next() {
		if largeSet.Contains(smallNext.key) && !yield(smallNext.key) {
			return
		}
	}
}

// Union is BTreeSet::union: the elements that are in s or in other, in
// order, each once. An element in both is taken from s.
func (s BTreeSet[T]) Union(other *BTreeSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		merge := btreeMergeIter[T]{a: s.m.iter(), b: other.m.iter(), peeked: nil, peekedFromA: false}
		for {
			aNext, bNext := merge.nexts()
			switch {
			case aNext != nil:
				if !yield(aNext.key) {
					return
				}
			case bNext != nil:
				if !yield(bNext.key) {
					return
				}
			default:
				return
			}
		}
	}
}

// Clear is BTreeSet::clear: it removes every element.
func (s *BTreeSet[T]) Clear() {
	s.m.Clear()
}

// Contains is BTreeSet::contains.
func (s BTreeSet[T]) Contains(value T) bool {
	return s.m.find(value) != nil
}

// Get is BTreeSet::get: the element of the set that is equal to value, and
// whether there is one.
func (s BTreeSet[T]) Get(value T) (T, bool) {
	if n := s.m.find(value); n != nil {
		return n.key, true
	}
	var zero T
	return zero, false
}

// IsDisjoint is BTreeSet::is_disjoint: whether s has no element in common
// with other.
func (s BTreeSet[T]) IsDisjoint(other *BTreeSet[T]) bool {
	for range s.Intersection(other) {
		return false
	}
	return true
}

// IsSubset is BTreeSet::is_subset: whether other contains every element of
// s.
func (s BTreeSet[T]) IsSubset(other *BTreeSet[T]) bool {
	// Same result as self.difference(other).next().is_none()
	// but the code below is faster (hugely in some cases).
	if s.Len() > other.Len() {
		return false // self has more elements than other
	}
	selfMin, selfMax := s.m.firstNode(), s.m.lastNode()
	if selfMin == nil {
		return true // self is empty
	}
	otherMin, otherMax := other.m.firstNode(), other.m.lastNode()
	if otherMin == nil {
		return false // other is empty
	}
	selfIter := s.m.iter()
	switch order := rs.Cmp(selfMin.key, otherMin.key); {
	case order < 0:
		return false // other does not contain self_min
	case order == 0:
		selfIter.next() // self_min is contained in other, so remove it from consideration
		// other_min is now not in self_iter (used below)
	default:
		// other_min is not in self_iter (used below)
	}
	switch order := rs.Cmp(selfMax.key, otherMax.key); {
	case order > 0:
		return false // other does not contain self_max
	case order == 0:
		selfIter.nextBack() // self_max is contained in other, so remove it from consideration
		// other_max is now not in self_iter (used below)
	default:
		// other_max is not in self_iter (used below)
	}
	if selfIter.length <= other.Len()/btreeSetIterPerformanceTippingSizeDiff {
		for element := selfIter.next(); element != nil; element = selfIter.next() {
			if !other.Contains(element.key) {
				return false
			}
		}
		return true
	}
	otherIter := other.m.iter()
	// remove other_min and other_max as they are not in self_iter (see above)
	otherIter.next()
	otherIter.nextBack()
	// custom `self_iter.all(|e| other.contains(e))`
	for self1 := selfIter.next(); self1 != nil; self1 = selfIter.next() {
		contained := false
	search:
		for other1 := otherIter.next(); other1 != nil; other1 = otherIter.next() {
			order := rs.Cmp(other1.key, self1.key)
			switch {
			// happens up to `ITER_PERFORMANCE_TIPPING_SIZE_DIFF * self.len() - 1` times
			case order < 0:
				continue // skip over elements that are smaller
			// happens `self.len()` times
			case order == 0:
				contained = true // self1 is in other
				break search
			// happens only once
			default:
				return false // self1 is not in other
			}
		}
		if !contained {
			return false
		}
	}
	return true
}

// IsSuperset is BTreeSet::is_superset: whether s contains every element of
// other.
func (s BTreeSet[T]) IsSuperset(other *BTreeSet[T]) bool {
	return other.IsSubset(&s)
}

// First is BTreeSet::first: the smallest element.
func (s BTreeSet[T]) First() (T, bool) {
	if n := s.m.firstNode(); n != nil {
		return n.key, true
	}
	var zero T
	return zero, false
}

// Last is BTreeSet::last: the greatest element.
func (s BTreeSet[T]) Last() (T, bool) {
	if n := s.m.lastNode(); n != nil {
		return n.key, true
	}
	var zero T
	return zero, false
}

// PopFirst is BTreeSet::pop_first: it removes and returns the smallest
// element.
func (s *BTreeSet[T]) PopFirst() rs.Option[T] {
	if n := s.m.firstNode(); n != nil {
		s.m.unlink(n)
		return rs.Some(n.key)
	}
	return rs.None[T]()
}

// PopLast is BTreeSet::pop_last: it removes and returns the greatest
// element.
func (s *BTreeSet[T]) PopLast() rs.Option[T] {
	if n := s.m.lastNode(); n != nil {
		s.m.unlink(n)
		return rs.Some(n.key)
	}
	return rs.None[T]()
}

// Insert is BTreeSet::insert: it adds value and reports whether it was new.
// When an equal element is already present the set is left as it is: the
// stored element is not replaced.
func (s *BTreeSet[T]) Insert(value T) bool {
	return s.m.Insert(value, btreeSetVal{}).IsNone()
}

// Replace is BTreeSet::replace: it adds value, replacing the element equal
// to it if there is one, and returns the replaced element.
func (s *BTreeSet[T]) Replace(value T) rs.Option[T] {
	// Rust creates the root before it searches.
	s.m.rooted = true
	found, parent, right := s.m.locate(value)
	if found != nil {
		old := found.key
		found.key = value
		return rs.Some(old)
	}
	s.m.link(&btreeNode[T, btreeSetVal]{
		key: value, value: btreeSetVal{},
		left: nil, right: nil, parent: nil, height: 1,
	}, parent, right)
	return rs.None[T]()
}

// Remove is BTreeSet::remove: it removes value and reports whether it was
// present.
func (s *BTreeSet[T]) Remove(value T) bool {
	if n := s.m.find(value); n != nil {
		s.m.unlink(n)
		return true
	}
	return false
}

// Take is BTreeSet::take: it removes and returns the element of the set that
// is equal to value.
func (s *BTreeSet[T]) Take(value T) rs.Option[T] {
	if n := s.m.find(value); n != nil {
		s.m.unlink(n)
		return rs.Some(n.key)
	}
	return rs.None[T]()
}

// Retain is BTreeSet::retain: it keeps the elements for which f returns
// true, visiting them in order.
func (s *BTreeSet[T]) Retain(f func(T) bool) {
	s.m.Retain(func(element T, _ *btreeSetVal) bool { return f(element) })
}

// Append is BTreeSet::append: it moves every element of other into s and
// leaves other empty. Of two equal elements, s keeps its own.
func (s *BTreeSet[T]) Append(other *BTreeSet[T]) {
	s.m.Append(&other.m)
}

// SplitOff is BTreeSet::split_off: it moves every element at or after value
// into a new set, which it returns. It takes time in proportion to the size
// of the set.
func (s *BTreeSet[T]) SplitOff(value T) BTreeSet[T] {
	return BTreeSet[T]{m: s.m.SplitOff(value)}
}

// ExtractIf is BTreeSet::extract_if: a sequence that visits the elements
// between the two bounds in order and removes and yields those for which
// pred returns true. It is lazy as in Rust: elements are removed as the
// sequence is consumed, and an element not reached stays in the set.
func (s *BTreeSet[T]) ExtractIf(lo, hi Bound[T], pred func(T) bool) iter.Seq[T] {
	extracted := s.m.ExtractIf(lo, hi, func(element T, _ *btreeSetVal) bool { return pred(element) })
	return func(yield func(T) bool) {
		for element := range extracted {
			if !yield(element) {
				return
			}
		}
	}
}

// Iter is BTreeSet::iter (`for x in &set`): every element in order.
func (s BTreeSet[T]) Iter() iter.Seq[T] {
	return s.m.Keys()
}

// IterRev is `set.iter().rev()`: every element from the greatest down.
func (s BTreeSet[T]) IterRev() iter.Seq[T] {
	return s.m.KeysRev()
}

// Len is BTreeSet::len: the number of elements.
func (s BTreeSet[T]) Len() int {
	return s.m.length
}

// IsEmpty is BTreeSet::is_empty.
func (s BTreeSet[T]) IsEmpty() bool {
	return s.m.length == 0
}

// IntoIter is BTreeSet::into_iter (`for x in set`): every element in order,
// for a set that is not used again.
func (s BTreeSet[T]) IntoIter() iter.Seq[T] {
	return s.m.Keys()
}

// IntoIterRev is `set.into_iter().rev()`.
func (s BTreeSet[T]) IntoIterRev() iter.Seq[T] {
	return s.m.KeysRev()
}

// Extend is BTreeSet::extend: it inserts every element of seq, as Insert
// does.
func (s *BTreeSet[T]) Extend(seq iter.Seq[T]) {
	for element := range seq {
		s.Insert(element)
	}
}

// Sub is `&a - &b`: a new set with clones of the elements of s that are not
// in rhs.
func (s BTreeSet[T]) Sub(rhs *BTreeSet[T]) BTreeSet[T] {
	return btreeSetFromSortedIter(btreeSetCloned(s.Difference(rhs)))
}

// Bitxor is `&a ^ &b`: a new set with clones of the elements that are in s
// or in rhs but not in both.
func (s BTreeSet[T]) Bitxor(rhs *BTreeSet[T]) BTreeSet[T] {
	return btreeSetFromSortedIter(btreeSetCloned(s.SymmetricDifference(rhs)))
}

// Bitand is `&a & &b`: a new set with clones of the elements that are in
// both s and rhs.
func (s BTreeSet[T]) Bitand(rhs *BTreeSet[T]) BTreeSet[T] {
	return btreeSetFromSortedIter(btreeSetCloned(s.Intersection(rhs)))
}

// Bitor is `&a | &b`: a new set with clones of the elements that are in s or
// in rhs.
func (s BTreeSet[T]) Bitor(rhs *BTreeSet[T]) BTreeSet[T] {
	return btreeSetFromSortedIter(btreeSetCloned(s.Union(rhs)))
}

// btreeSetCloned is Iterator::cloned.
func btreeSetCloned[T any](seq iter.Seq[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		for element := range seq {
			if !yield(rs.Clone(element)) {
				return
			}
		}
	}
}

// The methods below describe the set to rs's formatter, structural clone and
// comparison, and to go/crates/serde (rs.ReflectSeq).

// ReflectLen returns the number of elements.
func (s *BTreeSet[T]) ReflectLen() int { return s.m.length }

// ReflectElemType returns the type T.
func (s *BTreeSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }

// ReflectElements calls yield with a pointer to each element, in order. The
// pointers are for reading: an element must not be changed in place.
func (s *BTreeSet[T]) ReflectElements(yield func(element any) bool) {
	btreeAscend(s.m.firstNode(), nil, func(n *btreeNode[T, btreeSetVal]) bool { return yield(&n.key) })
}

// ReflectPush inserts element (a T), as Insert does.
func (s *BTreeSet[T]) ReflectPush(element any) {
	s.Insert(reflectCast[T](element))
}

// ReflectIsSet reports that the collection is a set.
func (s *BTreeSet[T]) ReflectIsSet() bool { return true }

// ReflectSorted reports that iteration is in element order.
func (s *BTreeSet[T]) ReflectSorted() bool { return true }

var _ rs.ReflectSeq = (*BTreeSet[int])(nil)
