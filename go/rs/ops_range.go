// Port of core 1.99.1 src/ops/range.rs (Range and RangeInclusive)

package rs

import (
	"reflect"
	"strconv"
)

// Range is core::ops::Range, `start..end`: the values from Start up to but
// not including End. `std::ops::Range<usize>` is rs.Range[int].
//
// The struct and its fields are Rust's. A Rust Range is also the iterator
// over its values; in Go that is a loop, `for i := r.Start; i < r.End; i++`.
type Range[T any] struct {
	Start T `rs:"start"`
	End   T `rs:"end"`
}

var _ = RegisterStruct[Range[int]]("Range")

// Contains is Range::contains: Start <= item and item < End, as PartialOrd
// decides them (a float NaN is in no range).
func (r Range[T]) Contains(item T) bool {
	return rangeLe(r.Start, item) && rangeLt(item, r.End)
}

// IsEmpty is Range::is_empty: whether Start is not less than End. A range
// with a NaN bound is empty.
func (r Range[T]) IsEmpty() bool {
	return !rangeLt(r.Start, r.End)
}

// Len is `ExactSizeIterator::len` of a range of integers: End - Start, or
// zero when End is not above Start. Rust has it for the integer types of up
// to 32 bits and for usize and isize; Go code may call it on any integer
// range.
func (r Range[T]) Len() int {
	return rangeStepsBetween(r.Start, r.End)
}

// FmtDebug is `impl Debug for Range`: each bound with its own Debug, under
// the formatter's flags, around "..", as in `5..30`.
func (r Range[T]) FmtDebug(f *Formatter) error {
	if err := DebugFmt(r.Start, f); err != nil {
		return err
	}
	if err := f.WriteStr(".."); err != nil {
		return err
	}
	return DebugFmt(r.End, f)
}

// RangeInclusive is core::ops::RangeInclusive, `start..=end`: the values
// from Start up to and including End. `RangeInclusive::new(a, b)` is
// rs.RangeInclusiveNew(a, b) or the literal; `r.start()` and `r.end()` are
// the fields.
//
// Rust keeps a third field, `exhausted`, which only iterating the range
// sets. Iteration is a Go loop (`for i := r.Start; i <= r.End; i++`, minding
// End at the type's maximum), so nothing sets it and it is left out; a
// RangeInclusive here is never exhausted.
type RangeInclusive[T any] struct {
	Start T `rs:"start"`
	End   T `rs:"end"`
}

var _ = RegisterStruct[RangeInclusive[int]]("RangeInclusive")

// RangeInclusiveNew is `RangeInclusive::new(start, end)`.
func RangeInclusiveNew[T any](start, end T) RangeInclusive[T] {
	return RangeInclusive[T]{Start: start, End: end}
}

// IntoInner is RangeInclusive::into_inner: the two bounds.
func (r RangeInclusive[T]) IntoInner() (T, T) {
	return r.Start, r.End
}

// Contains is RangeInclusive::contains: Start <= item and item <= End, as
// PartialOrd decides them.
func (r RangeInclusive[T]) Contains(item T) bool {
	return rangeLe(r.Start, item) && rangeLe(item, r.End)
}

// IsEmpty is RangeInclusive::is_empty: whether Start is not less than or
// equal to End.
func (r RangeInclusive[T]) IsEmpty() bool {
	return !rangeLe(r.Start, r.End)
}

// Len is `ExactSizeIterator::len` of an inclusive range of integers: zero
// when it is empty, else End - Start + 1. Rust has it for u8, i8, u16 and
// i16; Go code may call it on any integer range whose length fits an int.
func (r RangeInclusive[T]) Len() int {
	if r.IsEmpty() {
		return 0
	}
	return rangeStepsBetween(r.Start, r.End) + 1
}

// FmtDebug is `impl Debug for RangeInclusive`: each bound with its own
// Debug, under the formatter's flags, around "..=", as in `5..=30`.
func (r RangeInclusive[T]) FmtDebug(f *Formatter) error {
	if err := DebugFmt(r.Start, f); err != nil {
		return err
	}
	if err := f.WriteStr("..="); err != nil {
		return err
	}
	return DebugFmt(r.End, f)
}

// rangeLe is `a <= b` for PartialOrd: false when the two are unordered.
func rangeLe[T any](a, b T) bool {
	order, ok := PartialCmp(a, b).Get()
	return ok && order <= 0
}

// rangeLt is `a < b` for PartialOrd.
func rangeLt[T any](a, b T) bool {
	order, ok := PartialCmp(a, b).Get()
	return ok && order < 0
}

// rangeStepsBetween is `Step::steps_between(&start, &end)` for an integer
// type: how many steps lead from start to end, or zero when end is below
// start. It panics for a type that is not an integer, where Rust has no
// len, and when the count does not fit an int.
func rangeStepsBetween[T any](start, end T) int {
	a, b := reflect.ValueOf(start), reflect.ValueOf(end)
	var steps uint64
	switch a.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		x, y := a.Int(), b.Int()
		if y <= x {
			return 0
		}
		steps = uint64(y) - uint64(x)
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		x, y := a.Uint(), b.Uint()
		if y <= x {
			return 0
		}
		steps = y - x
	default:
		PanicAt(2, "len() of a range of "+a.Type().String()+", which is not an integer type")
	}
	if steps > uint64(UsizeMax) {
		PanicAt(2, "len() of a range of "+strconv.FormatUint(steps, 10)+" values, more than a usize of this port holds")
	}
	return int(steps)
}
