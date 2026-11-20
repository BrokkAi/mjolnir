// Port of std 1.99.1 src/collections/hash/set.rs

package collections

import (
	"iter"
	"reflect"
	"slices"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// HashSet is Rust's HashSet<T>: a set without an order. It is a HashMap
// whose values carry nothing, and what HashMap says about its zero value,
// about copies, about how elements are told apart (by ==, by HashKey, or
// by rs.Cmp), and about iteration order holds for it. Elements are passed
// and returned by value.
type HashSet[T any] struct {
	base HashMap[T, struct{}]
}

// HashSetNew is HashSet::new: an empty set.
func HashSetNew[T any]() HashSet[T] {
	var empty HashSet[T]
	return empty
}

// HashSetDefault is HashSet::default: an empty set.
func HashSetDefault[T any]() HashSet[T] {
	var empty HashSet[T]
	return empty
}

// HashSetWithCapacity is HashSet::with_capacity: an empty set with space for
// at least capacity elements.
func HashSetWithCapacity[T any](capacity int) HashSet[T] {
	return HashSet[T]{base: HashMapWithCapacity[T, struct{}](capacity)}
}

// HashSetFrom is HashSet::from([a, b, ..]). Of several equal elements the
// first one is kept.
func HashSetFrom[T any](elements ...T) HashSet[T] {
	set := HashSetNew[T]()
	for _, element := range elements {
		set.Insert(element)
	}
	return set
}

// HashSetFromIter is `iter.collect::<HashSet<T>>()` (FromIterator). Of
// several equal elements the first one is kept.
func HashSetFromIter[T any](seq iter.Seq[T]) HashSet[T] {
	set := HashSetNew[T]()
	set.Extend(seq)
	return set
}

// Iter is HashSet::iter (`for x in &set`): every element, in no particular
// order.
func (s HashSet[T]) Iter() iter.Seq[T] {
	return s.base.Keys()
}

// IterSorted yields what Iter yields, in the order of the elements by
// rs.Cmp. It has no counterpart in Rust: it is for the callers whose Rust
// code depends on nothing but a consistent order, and whose output must be
// the same on every run.
func (s HashSet[T]) IterSorted() iter.Seq[T] {
	return func(yield func(T) bool) {
		elements := slices.Collect(s.Iter())
		slices.SortFunc(elements, rs.Cmp[T])
		for _, element := range elements {
			if !yield(element) {
				return
			}
		}
	}
}

// IntoIter is HashSet::into_iter (`for x in set`): every element in no
// particular order, for a set that is not used again.
func (s HashSet[T]) IntoIter() iter.Seq[T] {
	return s.base.Keys()
}

// Len is HashSet::len: the number of elements.
func (s HashSet[T]) Len() int {
	return s.base.Len()
}

// IsEmpty is HashSet::is_empty.
func (s HashSet[T]) IsEmpty() bool {
	return s.base.Len() == 0
}

// Drain is HashSet::drain: it clears the set, returning all elements in no
// particular order. The set is empty by the time Drain returns; in Rust it
// cannot be touched until the iterator is dropped, and is empty then.
func (s *HashSet[T]) Drain() iter.Seq[T] {
	drained := HashSet[T]{base: s.base.takeAll()}
	return drained.Iter()
}

// ExtractIf is HashSet::extract_if: a sequence that visits every element in
// no particular order and removes and yields those for which pred returns
// true. It is lazy as in Rust: elements are removed as the sequence is
// consumed, and an element not reached stays in the set.
func (s *HashSet[T]) ExtractIf(pred func(T) bool) iter.Seq[T] {
	extracted := s.base.ExtractIf(func(element T, _ *struct{}) bool { return pred(element) })
	return func(yield func(T) bool) {
		for element := range extracted {
			if !yield(element) {
				return
			}
		}
	}
}

// Retain is HashSet::retain: it keeps the elements for which f returns
// true, visiting them in no particular order.
func (s *HashSet[T]) Retain(f func(T) bool) {
	s.base.Retain(func(element T, _ *struct{}) bool { return f(element) })
}

// Clear is HashSet::clear: it removes every element.
func (s *HashSet[T]) Clear() {
	s.base.Clear()
}

// Reserve is HashSet::reserve. A Go map grows on its own, so this does
// nothing.
func (s *HashSet[T]) Reserve(additional int) {}

// ShrinkToFit is HashSet::shrink_to_fit. A Go map does not give memory
// back, so this does nothing.
func (s *HashSet[T]) ShrinkToFit() {}

// ShrinkTo is HashSet::shrink_to. A Go map does not give memory back, so
// this does nothing.
func (s *HashSet[T]) ShrinkTo(minCapacity int) {}

// Difference is HashSet::difference: the elements of s that are not in
// other, in no particular order.
func (s HashSet[T]) Difference(other *HashSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		for element := range s.Iter() {
			if !other.Contains(element) && !yield(element) {
				return
			}
		}
	}
}

// SymmetricDifference is HashSet::symmetric_difference: the elements that
// are in s or in other but not in both, in no particular order (those of s
// first).
func (s HashSet[T]) SymmetricDifference(other *HashSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		for element := range s.Difference(other) {
			if !yield(element) {
				return
			}
		}
		for element := range other.Difference(&s) {
			if !yield(element) {
				return
			}
		}
	}
}

// Intersection is HashSet::intersection: the elements that are in both s
// and other, in no particular order. They are taken from the smaller of the
// two sets (from s when the sets have the same size).
func (s HashSet[T]) Intersection(other *HashSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		small, large := &s, other
		if s.Len() > other.Len() {
			small, large = other, &s
		}
		for element := range small.Iter() {
			if large.Contains(element) && !yield(element) {
				return
			}
		}
	}
}

// Union is HashSet::union: the elements that are in s or in other, each
// once, in no particular order. An element in both is taken from the larger
// of the two sets (from s when the sets have the same size).
func (s HashSet[T]) Union(other *HashSet[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		large, small := &s, other
		if s.Len() < other.Len() {
			large, small = other, &s
		}
		for element := range large.Iter() {
			if !yield(element) {
				return
			}
		}
		for element := range small.Difference(large) {
			if !yield(element) {
				return
			}
		}
	}
}

// Contains is HashSet::contains.
func (s HashSet[T]) Contains(value T) bool {
	return s.base.find(value) != nil
}

// Get is HashSet::get: the element of the set that is equal to value, and
// whether there is one.
func (s HashSet[T]) Get(value T) (T, bool) {
	if entry := s.base.find(value); entry != nil {
		return entry.key, true
	}
	var zero T
	return zero, false
}

// IsDisjoint is HashSet::is_disjoint: whether s has no element in common
// with other.
func (s HashSet[T]) IsDisjoint(other *HashSet[T]) bool {
	small, large := &s, other
	if s.Len() > other.Len() {
		small, large = other, &s
	}
	for element := range small.Iter() {
		if large.Contains(element) {
			return false
		}
	}
	return true
}

// IsSubset is HashSet::is_subset: whether other contains every element of
// s.
func (s HashSet[T]) IsSubset(other *HashSet[T]) bool {
	if s.Len() > other.Len() {
		return false
	}
	for element := range s.Iter() {
		if !other.Contains(element) {
			return false
		}
	}
	return true
}

// IsSuperset is HashSet::is_superset: whether s contains every element of
// other.
func (s HashSet[T]) IsSuperset(other *HashSet[T]) bool {
	return other.IsSubset(&s)
}

// Insert is HashSet::insert: it adds value and reports whether it was new.
// When an equal element is already present the set is left as it is: the
// stored element is not replaced.
func (s *HashSet[T]) Insert(value T) bool {
	return s.base.Insert(value, struct{}{}).IsNone()
}

// Replace is HashSet::replace: it adds value, replacing the element equal
// to it if there is one, and returns the replaced element.
func (s *HashSet[T]) Replace(value T) rs.Option[T] {
	if entry := s.base.find(value); entry != nil {
		old := entry.key
		s.base.replaceKey(entry, value)
		return rs.Some(old)
	}
	s.base.store(&hashMapEntry[T, struct{}]{key: value, value: struct{}{}})
	return rs.None[T]()
}

// Remove is HashSet::remove: it removes value and reports whether it was
// present.
func (s *HashSet[T]) Remove(value T) bool {
	return s.base.Remove(value).IsSome()
}

// Take is HashSet::take: it removes and returns the element of the set that
// is equal to value.
func (s *HashSet[T]) Take(value T) rs.Option[T] {
	if entry := s.base.find(value); entry != nil {
		s.base.erase(value)
		return rs.Some(entry.key)
	}
	return rs.None[T]()
}

// Equal is HashSet's PartialEq: the same number of elements, and every
// element of s in other.
func (s HashSet[T]) Equal(other *HashSet[T]) bool {
	if s.Len() != other.Len() {
		return false
	}
	for element := range s.Iter() {
		if !other.Contains(element) {
			return false
		}
	}
	return true
}

// Extend is HashSet::extend: it inserts every element of seq, as Insert
// does.
func (s *HashSet[T]) Extend(seq iter.Seq[T]) {
	for element := range seq {
		s.Insert(element)
	}
}

// Bitor is `&a | &b`: a new set with clones of the elements that are in s or
// in rhs.
func (s HashSet[T]) Bitor(rhs *HashSet[T]) HashSet[T] {
	return hashSetCollectCloned(s.Union(rhs))
}

// Bitand is `&a & &b`: a new set with clones of the elements that are in
// both s and rhs.
func (s HashSet[T]) Bitand(rhs *HashSet[T]) HashSet[T] {
	return hashSetCollectCloned(s.Intersection(rhs))
}

// Bitxor is `&a ^ &b`: a new set with clones of the elements that are in s
// or in rhs but not in both.
func (s HashSet[T]) Bitxor(rhs *HashSet[T]) HashSet[T] {
	return hashSetCollectCloned(s.SymmetricDifference(rhs))
}

// Sub is `&a - &b`: a new set with clones of the elements of s that are not
// in rhs.
func (s HashSet[T]) Sub(rhs *HashSet[T]) HashSet[T] {
	return hashSetCollectCloned(s.Difference(rhs))
}

// hashSetCollectCloned is `iter.cloned().collect()`.
func hashSetCollectCloned[T any](seq iter.Seq[T]) HashSet[T] {
	set := HashSetNew[T]()
	for element := range seq {
		set.Insert(rs.Clone(element))
	}
	return set
}

// The methods below describe the set to rs's formatter, structural clone and
// comparison, and to go/crates/serde (rs.ReflectSeq).

// ReflectLen returns the number of elements.
func (s *HashSet[T]) ReflectLen() int { return s.base.Len() }

// ReflectElemType returns the type T.
func (s *HashSet[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }

// ReflectElements calls yield with a pointer to each element, in no
// particular order. The pointers are for reading: an element must not be
// changed in place.
func (s *HashSet[T]) ReflectElements(yield func(element any) bool) {
	s.base.each(func(entry *hashMapEntry[T, struct{}]) bool { return yield(&entry.key) })
}

// ReflectPush inserts element (a T), as Insert does.
func (s *HashSet[T]) ReflectPush(element any) {
	s.Insert(reflectCast[T](element))
}

// ReflectIsSet reports that the collection is a set.
func (s *HashSet[T]) ReflectIsSet() bool { return true }

// ReflectSorted reports that iteration is not in element order.
func (s *HashSet[T]) ReflectSorted() bool { return false }

var _ rs.ReflectSeq = (*HashSet[int])(nil)
