// Port of core 1.99.1 src/ops/range.rs (Bound, which std::collections
// re-exports as std::collections::Bound) and src/slice/index.rs
// (into_slice_range, the index-range check VecDeque uses).

package collections

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Bound is Rust's std::collections::Bound<T> (core::ops::Bound): one end of
// a range. Every method of this package that takes a Rust range takes two
// bounds, the start and the end:
//
//	m.range(a..b)     m.Range(collections.Included(a), collections.Excluded(b))
//	m.range(a..=b)    m.Range(collections.Included(a), collections.Included(b))
//	m.range(a..)      m.Range(collections.Included(a), collections.Unbounded[K]())
//	m.range(..b)      m.Range(collections.Unbounded[K](), collections.Excluded(b))
//	m.range(..=b)     m.Range(collections.Unbounded[K](), collections.Included(b))
//	m.range(..)       m.Range(collections.Unbounded[K](), collections.Unbounded[K]())
//	m.range((Bound::Excluded(a), Bound::Unbounded))
//	                  m.Range(collections.Excluded(a), collections.Unbounded[K]())
//
// It is an enum in the port's usual form: a sealed interface whose values are
// pointers to the variant structs. A nil Bound is a porting error and panics.
type Bound[T any] interface {
	isBound()
	// bound gives the variant and its value without a type switch, and ties
	// the interface to T so that a Bound[string] is not a Bound[int].
	bound() (boundKind, T)
}

// Bound_Included is Bound::Included: the range reaches the value itself.
type Bound_Included[T any] struct{ F0 T }

// Bound_Excluded is Bound::Excluded: the range stops short of the value.
type Bound_Excluded[T any] struct{ F0 T }

// Bound_Unbounded is Bound::Unbounded: the range has no end on this side.
type Bound_Unbounded[T any] struct{}

func (*Bound_Included[T]) isBound()  {}
func (*Bound_Excluded[T]) isBound()  {}
func (*Bound_Unbounded[T]) isBound() {}

type boundKind uint8

const (
	boundIncluded boundKind = iota
	boundExcluded
	boundUnbounded
)

func (b *Bound_Included[T]) bound() (boundKind, T) { return boundIncluded, b.F0 }
func (b *Bound_Excluded[T]) bound() (boundKind, T) { return boundExcluded, b.F0 }
func (b *Bound_Unbounded[T]) bound() (boundKind, T) {
	var zero T
	return boundUnbounded, zero
}

var _ = rs.RegisterEnum[Bound[int]]("Bound",
	rs.Variant[Bound_Included[int]]("Included", rs.ShapeTuple),
	rs.Variant[Bound_Excluded[int]]("Excluded", rs.ShapeTuple),
	rs.Variant[Bound_Unbounded[int]]("Unbounded", rs.ShapeUnit),
)

// Included returns Bound::Included(value).
func Included[T any](value T) Bound[T] { return &Bound_Included[T]{F0: value} }

// Excluded returns Bound::Excluded(value).
func Excluded[T any](value T) Bound[T] { return &Bound_Excluded[T]{F0: value} }

// Unbounded returns Bound::Unbounded.
func Unbounded[T any]() Bound[T] { return &Bound_Unbounded[T]{} }

// unpackBound reads a bound a caller passed. Nothing in Rust is nil, so a nil
// bound is a mistake in the port and is reported as one.
func unpackBound[T any](b Bound[T]) (boundKind, T) {
	if b == nil {
		rs.PanicAt(2, "collections: a nil Bound was passed as a range end; use collections.Unbounded")
	}
	return b.bound()
}

// sliceRange is core::slice::index::into_slice_range: it turns a pair of
// bounds over indexes into the half-open range start..end of a sequence of
// length len. The second result is the panic message Rust gives when the
// range does not fit, and is empty when it does.
func sliceRange(length int, lo, hi Bound[int]) (start, end int, failure string) {
	loKind, loValue := unpackBound(lo)
	hiKind, hiValue := unpackBound(hi)

	switch hiKind {
	case boundIncluded:
		if hiValue >= length {
			return 0, 0, sliceIndexFail(0, hiValue, length)
		}
		// Cannot overflow because `end < len` implies `end < usize::MAX`.
		end = hiValue + 1
	case boundExcluded:
		if hiValue > length {
			return 0, 0, sliceIndexFail(0, hiValue, length)
		}
		end = hiValue
	case boundUnbounded:
		end = length
	}

	switch loKind {
	case boundExcluded:
		if loValue >= end {
			return 0, 0, sliceIndexFail(loValue, end, length)
		}
		// Cannot overflow because `start < end` implies `start < usize::MAX`.
		start = loValue + 1
	case boundIncluded:
		if loValue > end {
			return 0, 0, sliceIndexFail(loValue, end, length)
		}
		start = loValue
	case boundUnbounded:
		start = 0
	}
	return start, end, ""
}

// sliceIndexFail is core::slice::index::slice_index_fail: the message for a
// range that does not fit a sequence.
func sliceIndexFail(start, end, length int) string {
	if start > length {
		return "range start index " + strconv.Itoa(start) + " out of range for slice of length " + strconv.Itoa(length)
	}
	if end > length {
		return "range end index " + strconv.Itoa(end) + " out of range for slice of length " + strconv.Itoa(length)
	}
	if start > end {
		return "slice index starts at " + strconv.Itoa(start) + " but ends at " + strconv.Itoa(end)
	}
	// Only reachable if the range was a `RangeInclusive` or a
	// `RangeToInclusive`, with `end == len`.
	return "range end index " + strconv.Itoa(end) + " out of range for slice of length " + strconv.Itoa(length)
}
