// Port of similar 2.7.0 src/algorithms/utils.rs

package similar

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The Rust functions take any `Index<usize>` lookups, of two different types
// when `New::Output: PartialEq<Old::Output>`. Here both sequences are slices
// of one comparable type, which is all the text differ needs.

// algorithmsUtilsIndex is `lookup[index]` on a slice or a Vec, with the
// panic Rust raises when the index is past the end. The crate can be made to
// read past the end (the LCS algorithm does when its deadline has passed),
// and the message is what a caller then sees.
func algorithmsUtilsIndex[T any](lookup []T, index int) T {
	if index < 0 || index >= len(lookup) {
		rs.PanicAt(1, "index out of bounds: the len is "+strconv.Itoa(len(lookup))+" but the index is "+strconv.FormatUint(uint64(uint(index)), 10))
	}
	return lookup[index]
}

// algorithmsUtilsInBounds reports whether every index of a (non-empty) range
// can be read from lookup.
func algorithmsUtilsInBounds[T any](lookup []T, range_ Range) bool {
	return 0 <= range_.Start && range_.End <= len(lookup)
}

// Utility function to check if a range is empty that works on older rust versions
func algorithmsUtilsIsEmptyRange(range_ Range) bool {
	return !(range_.Start < range_.End)
}

// Represents an item in the vector returned by [`unique`].
//
// It compares like the underlying item does it was created from but
// carries the index it was originally created from.
type algorithmsUtilsUniqueItem[T comparable] struct {
	lookup []T
	index  int
}

// Returns the value.
func (self algorithmsUtilsUniqueItem[T]) value() T {
	return algorithmsUtilsIndex(self.lookup, self.index)
}

// Returns the original index.
func (self algorithmsUtilsUniqueItem[T]) originalIndex() int {
	return self.index
}

// Returns only unique items in the sequence as vector.
//
// Each item is wrapped in a [`UniqueItem`] so that both the value and the
// index can be extracted.
func algorithmsUtilsUnique[T comparable](lookup []T, range_ Range) []algorithmsUtilsUniqueItem[T] {
	// Rust collects from a HashMap and then sorts by the original index.
	// Recording the items in first-seen order gives that order directly.
	byItem := map[T]rs.Option[int]{}
	var order []T
	for index := range_.Start; index < range_.End; index++ {
		item := algorithmsUtilsIndex(lookup, index)
		entry, occupied := byItem[item]
		if !occupied {
			byItem[item] = rs.Some(index)
			order = append(order, item)
		} else if entry.IsSome() {
			byItem[item] = rs.None[int]()
		}
	}
	rv := []algorithmsUtilsUniqueItem[T]{}
	for _, item := range order {
		if index, ok := byItem[item].Get(); ok {
			rv = append(rv, algorithmsUtilsUniqueItem[T]{lookup: lookup, index: index})
		}
	}
	return rv
}

// Given two lookups and ranges calculates the length of the common prefix.
func algorithmsUtilsCommonPrefixLen[T comparable](old []T, oldRange Range, new_ []T, newRange Range) int {
	if algorithmsUtilsIsEmptyRange(oldRange) || algorithmsUtilsIsEmptyRange(newRange) {
		return 0
	}
	count := 0
	if algorithmsUtilsInBounds(old, oldRange) && algorithmsUtilsInBounds(new_, newRange) {
		// Both ranges lie inside their slices, so no read can fail: compare
		// the two windows directly. This is the loop below without its
		// per-element checks; Myers spends most of its time here.
		a := new_[newRange.Start:newRange.End]
		b := old[oldRange.Start:oldRange.End]
		if len(b) < len(a) {
			a = a[:len(b)]
		}
		for count < len(a) && a[count] == b[count] {
			count++
		}
		return count
	}
	for x0, x1 := newRange.Start, oldRange.Start; x0 < newRange.End && x1 < oldRange.End; x0, x1 = x0+1, x1+1 {
		if algorithmsUtilsIndex(new_, x0) != algorithmsUtilsIndex(old, x1) {
			break
		}
		count++
	}
	return count
}

// Given two lookups and ranges calculates the length of common suffix.
func algorithmsUtilsCommonSuffixLen[T comparable](old []T, oldRange Range, new_ []T, newRange Range) int {
	if algorithmsUtilsIsEmptyRange(oldRange) || algorithmsUtilsIsEmptyRange(newRange) {
		return 0
	}
	count := 0
	if algorithmsUtilsInBounds(old, oldRange) && algorithmsUtilsInBounds(new_, newRange) {
		// As in the prefix: the same comparisons without the per-element
		// checks, from the ends of the two windows.
		a := new_[newRange.Start:newRange.End]
		b := old[oldRange.Start:oldRange.End]
		for count < len(a) && count < len(b) && a[len(a)-1-count] == b[len(b)-1-count] {
			count++
		}
		return count
	}
	for x0, x1 := newRange.End-1, oldRange.End-1; x0 >= newRange.Start && x1 >= oldRange.Start; x0, x1 = x0-1, x1-1 {
		if algorithmsUtilsIndex(new_, x0) != algorithmsUtilsIndex(old, x1) {
			break
		}
		count++
	}
	return count
}

// AlgorithmsIdentifyDistinctInt is the bound on IdentifyDistinct's `Int`:
// `Add<Output = Int> + From<u8> + Default + Copy`.
type AlgorithmsIdentifyDistinctInt interface {
	~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uint | ~int16 | ~int32 | ~int64 | ~int
}

// A utility struct to convert distinct items to unique integers.
//
// This can be helpful on larger inputs to speed up the comparisons
// performed by doing a first pass where the data set gets reduced
// to (small) integers.
//
// The idea is that instead of passing two sequences to a diffling algorithm
// you first pass it via [`IdentifyDistinct`]:
//
//	old := []string{"foo", "bar", "baz"}
//	new := []string{"foo", "blah", "baz"}
//	h := similar.AlgorithmsIdentifyDistinctNew[uint32](old, similar.Range{0, len(old)}, new, similar.Range{0, len(new)})
//	ops := similar.CaptureDiff(
//		similar.Algorithm_Myers,
//		h.OldLookup(),
//		h.OldRange(),
//		h.NewLookup(),
//		h.NewRange(),
//	)
//
// The indexes are the same as with the passed source ranges.
type AlgorithmsIdentifyDistinct[Int AlgorithmsIdentifyDistinctInt] struct {
	old algorithmsUtilsOffsetLookup[Int]
	new algorithmsUtilsOffsetLookup[Int]
}

// algorithmsUtilsOffsetLookup holds the integers of one side. In Rust it is
// an `Index<usize>` that subtracts the range's start from every index. The
// algorithms here take slices, so vec is laid out to be indexed with the
// source index directly: its first offset elements are padding (and reading
// one of them gives zero where Rust panics).
type algorithmsUtilsOffsetLookup[Int any] struct {
	offset int
	vec    []Int
}

// Creates an int hasher for two sequences.
func AlgorithmsIdentifyDistinctNew[Int AlgorithmsIdentifyDistinctInt, T comparable](
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) *AlgorithmsIdentifyDistinct[Int] {
	// Rust keys the map by an enum of "old item" and "new item" whose
	// equality and hash are those of the items, which is a map keyed by the
	// item.
	map_ := map[T]Int{}
	oldStart := oldRange.Start
	newStart := newRange.Start
	oldSeq := make([]Int, oldStart, oldStart+oldRange.Len())
	newSeq := make([]Int, newStart, newStart+newRange.Len())
	var nextId Int
	var step Int = 1

	for idx := oldRange.Start; idx < oldRange.End; idx++ {
		item := algorithmsUtilsIndex(old, idx)
		id, occupied := map_[item]
		if !occupied {
			id = nextId
			nextId = nextId + step
			map_[item] = id
		}
		oldSeq = append(oldSeq, id)
	}

	for idx := newRange.Start; idx < newRange.End; idx++ {
		item := algorithmsUtilsIndex(new_, idx)
		id, occupied := map_[item]
		if !occupied {
			id = nextId
			nextId = nextId + step
			map_[item] = id
		}
		newSeq = append(newSeq, id)
	}

	return &AlgorithmsIdentifyDistinct[Int]{
		old: algorithmsUtilsOffsetLookup[Int]{
			offset: oldStart,
			vec:    oldSeq,
		},
		new: algorithmsUtilsOffsetLookup[Int]{
			offset: newStart,
			vec:    newSeq,
		},
	}
}

// Returns a lookup for the old side.
func (self *AlgorithmsIdentifyDistinct[Int]) OldLookup() []Int {
	return self.old.vec
}

// Returns a lookup for the new side.
func (self *AlgorithmsIdentifyDistinct[Int]) NewLookup() []Int {
	return self.new.vec
}

// Convenience method to get back the old range.
func (self *AlgorithmsIdentifyDistinct[Int]) OldRange() Range {
	return Range{Start: self.old.offset, End: len(self.old.vec)}
}

// Convenience method to get back the new range.
func (self *AlgorithmsIdentifyDistinct[Int]) NewRange() Range {
	return Range{Start: self.new.offset, End: len(self.new.vec)}
}
