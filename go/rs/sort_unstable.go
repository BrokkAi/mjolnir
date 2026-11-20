// Port of core 1.99.1 src/slice/sort/unstable/mod.rs, and of the
// sort_unstable methods of src/slice/mod.rs

package rs

import (
	"cmp"
	"reflect"
	"slices"
)

// Sorting.
//
// Rust's `sort`, `sort_by`, `sort_by_key` and `sort_by_cached_key` are
// stable: rs.SortStable, rs.SortStableBy, rs.SortStableByKey and
// rs.SortStableByCachedKey (sort_stable.go). Any stable sort leaves the same
// order, so those wrap Go's own.
//
// `sort_unstable`, `sort_unstable_by` and `sort_unstable_by_key` are
// rs.SortUnstable, rs.SortUnstableBy and rs.SortUnstableByKey. They are a
// port of Rust's own algorithm (ipnsort: insertion sort up to 20 elements,
// then quicksort with Rust's pivot choice and partitions, its small-sort
// routines and its heapsort fallback), because the order an unstable sort
// leaves equal elements in is visible and differs between algorithms.
//
// A comparison function returns an Ordering or an int (negative, zero or
// positive), so that rs.OrdCmp, rs.Cmp, cmp.Compare and strings.Compare can
// all be passed:
//
//	rs.SortUnstableBy(terminal, func(a, b rs.Tuple2[uint64, string]) rs.Ordering {
//		return rs.OrdCmp(b.F0, a.F0).ThenWith(func() rs.Ordering { return rs.OrdCmp(b.F1, a.F1) })
//	})
//
// A key function's results are compared with rs.Cmp, which orders numbers,
// strings, options, tuples, rs.Reverse and derived Ord structs as Rust does.
// A path is a string in this port, but Rust orders paths by component: sort
// paths with a comparison function from rspath, not by their text.

// CompareResult is what a comparison function passed to the sorting and
// searching functions returns: an Ordering, or an int that is negative, zero
// or positive.
type CompareResult interface{ ~int | ~int8 }

// SortLayout describes the Rust element type of a slice being sorted with
// sort_unstable. Rust chooses its small-sort routine and its partition from
// the element type's size and from whether it is Copy and Freeze, and the
// choice decides the order equal elements end up in. The plain functions
// derive a layout from the Go element type (SortLayoutOf); the ...Layout
// functions take one, for an element type the derivation gets wrong.
type SortLayout struct {
	// Size is `size_of::<T>()` in bytes.
	Size int
	// Copy is whether T is Copy.
	Copy bool
	// Freeze is whether T holds no interior mutability (Cell, RefCell, a
	// Mutex or an atomic held inline rather than behind a pointer).
	Freeze bool
}

// SortLayoutOf returns the layout the unstable sort assumes for the Rust
// type a Go element type T stands for:
//
//   - a bool, integer, float or rune: its own size, Copy;
//   - a string: String (24 bytes, not Copy); a `&str` (16 bytes, Copy) sorts
//     differently once 16 or more equal elements meet in a small sort;
//   - a pointer, func or channel: a reference (8 bytes, Copy); pass the
//     layout of a Box or Arc (8 bytes, not Copy) yourself;
//   - a slice: Vec (24 bytes, not Copy); a map: HashMap (48 bytes, not Copy);
//     an interface: a boxed trait object (16 bytes, not Copy);
//   - an array: its elements';
//   - a struct: its fields laid out as Rust lays out a struct (largest
//     alignment first, size rounded up to the alignment); Copy only for the
//     tuples (Tuple2 ... Tuple5) of Copy fields, since whether any other
//     struct derives Copy is not visible from Go; Option and Reverse as Rust
//     lays them out; Cell and RefCell are not Freeze.
//
// Every type is Freeze unless it holds a Cell or RefCell.
func SortLayoutOf[T any]() SortLayout {
	layout, _ := sortLayoutOfType(reflect.TypeFor[T]())
	return layout
}

// sortLayoutOfType returns the layout of t and its alignment.
func sortLayoutOfType(t reflect.Type) (SortLayout, int) {
	switch t.Kind() {
	case reflect.Bool, reflect.Int8, reflect.Uint8:
		return SortLayout{Size: 1, Copy: true, Freeze: true}, 1
	case reflect.Int16, reflect.Uint16:
		return SortLayout{Size: 2, Copy: true, Freeze: true}, 2
	case reflect.Int32, reflect.Uint32, reflect.Float32:
		return SortLayout{Size: 4, Copy: true, Freeze: true}, 4
	case reflect.Int, reflect.Uint, reflect.Uintptr, reflect.Int64, reflect.Uint64, reflect.Float64:
		return SortLayout{Size: 8, Copy: true, Freeze: true}, 8
	case reflect.Complex64:
		return SortLayout{Size: 8, Copy: true, Freeze: true}, 4
	case reflect.Complex128:
		return SortLayout{Size: 16, Copy: true, Freeze: true}, 8
	case reflect.String, reflect.Slice:
		return SortLayout{Size: 24, Copy: false, Freeze: true}, 8
	case reflect.Map:
		return SortLayout{Size: 48, Copy: false, Freeze: true}, 8
	case reflect.Interface:
		return SortLayout{Size: 16, Copy: false, Freeze: true}, 8
	case reflect.Pointer, reflect.UnsafePointer, reflect.Func, reflect.Chan:
		return SortLayout{Size: 8, Copy: true, Freeze: true}, 8
	case reflect.Array:
		element, align := sortLayoutOfType(t.Elem())
		return SortLayout{Size: element.Size * t.Len(), Copy: element.Copy, Freeze: element.Freeze}, align
	case reflect.Struct:
		return sortLayoutOfStruct(t)
	}
	return SortLayout{Size: int(t.Size()), Copy: false, Freeze: true}, max(t.Align(), 1)
}

var (
	sortCellType    = reflect.TypeFor[Cell[int]]()
	sortRefCellType = reflect.TypeFor[RefCell[int]]()
	sortOptionType  = reflect.TypeFor[Option[int]]()
	sortReverseType = reflect.TypeFor[Reverse[int]]()
	sortTupleTypes  = []reflect.Type{
		reflect.TypeFor[Tuple2[int, int]](),
		reflect.TypeFor[Tuple3[int, int, int]](),
		reflect.TypeFor[Tuple4[int, int, int, int]](),
		reflect.TypeFor[Tuple5[int, int, int, int, int]](),
	}
)

func sortLayoutOfStruct(t reflect.Type) (SortLayout, int) {
	key := typeKey(t)
	switch {
	case key == typeKey(sortCellType) || key == typeKey(sortRefCellType):
		inner, align := sortLayoutOfType(t.Field(0).Type)
		if key == typeKey(sortRefCellType) {
			// RefCell<T> keeps a borrow counter (an isize) beside T.
			align = max(align, 8)
			return SortLayout{Size: sortRoundUp(inner.Size+8, align), Copy: false, Freeze: false}, align
		}
		return SortLayout{Size: inner.Size, Copy: false, Freeze: false}, align
	case key == typeKey(sortReverseType):
		return sortLayoutOfType(t.Field(0).Type)
	case key == typeKey(sortOptionType):
		valueField, _ := t.FieldByName("value")
		inner, align := sortLayoutOfType(valueField.Type)
		switch valueField.Type.Kind() {
		case reflect.String, reflect.Slice, reflect.Map, reflect.Pointer, reflect.Func, reflect.Interface:
			// None takes the null pointer of these, so the option is no
			// larger than the value.
			return inner, align
		}
		return SortLayout{Size: sortRoundUp(inner.Size+align, align), Copy: inner.Copy, Freeze: inner.Freeze}, align
	}
	tuple := false
	for _, tupleType := range sortTupleTypes {
		if key == typeKey(tupleType) {
			tuple = true
		}
	}
	layout := SortLayout{Copy: tuple, Freeze: true}
	align := 1
	for i := range t.NumField() {
		field, fieldAlign := sortLayoutOfType(t.Field(i).Type)
		// Rust orders the fields of a struct by alignment, so no padding
		// falls between them; only the end is rounded up.
		layout.Size += field.Size
		layout.Copy = layout.Copy && field.Copy
		layout.Freeze = layout.Freeze && field.Freeze
		align = max(align, fieldAlign)
	}
	layout.Size = sortRoundUp(layout.Size, align)
	return layout, align
}

func sortRoundUp(size, align int) int {
	return (size + align - 1) / align * align
}

// SortUnstable is `v.sort_unstable()`.
func SortUnstable[T cmp.Ordered](v []T) {
	sortUnstable(v, SortLayoutOf[T](), func(a, b *T) bool { return *a < *b })
}

// SortUnstableBy is `v.sort_unstable_by(compare)`. compare returns an
// Ordering or an int.
func SortUnstableBy[T any, O CompareResult](v []T, compare func(a, b T) O) {
	SortUnstableByLayout(v, SortLayoutOf[T](), compare)
}

// SortUnstableByKey is `v.sort_unstable_by_key(key)`. The keys are compared
// with Cmp; key is called twice for each comparison, as Rust calls it.
func SortUnstableByKey[T, K any](v []T, key func(T) K) {
	SortUnstableByKeyLayout(v, SortLayoutOf[T](), key)
}

// SortUnstableByLayout is SortUnstableBy for elements whose Rust layout
// SortLayoutOf does not derive correctly.
func SortUnstableByLayout[T any, O CompareResult](v []T, layout SortLayout, compare func(a, b T) O) {
	sortUnstable(v, layout, func(a, b *T) bool { return compare(*a, *b) < 0 })
}

// SortUnstableByKeyLayout is SortUnstableByKey for elements whose Rust
// layout SortLayoutOf does not derive correctly.
func SortUnstableByKeyLayout[T, K any](v []T, layout SortLayout, key func(T) K) {
	sortUnstable(v, layout, func(a, b *T) bool { return Cmp(key(*a), key(*b)) < 0 })
}

// sortUnstable is core::slice::sort::unstable::sort.
//
// Unstable sort called ipnsort by Lukas Bergdoll and Orson Peters.
// Design document:
// <https://github.com/Voultapher/sort-research-rs/blob/main/writeup/ipnsort_introduction/text.md>
func sortUnstable[T any](v []T, layout SortLayout, isLess func(a, b *T) bool) {
	// Arrays of zero-sized types are always all-equal, and thus sorted.
	if layout.Size == 0 {
		return
	}

	// Instrumenting the standard library showed that 90+% of the calls to sort
	// by rustc are either of size 0 or 1.
	n := len(v)
	if n < 2 {
		return
	}

	// More advanced sorting methods than insertion sort are faster if called in
	// a hot loop for small inputs, but for general-purpose code the small
	// binary size of insertion sort is more important. The instruction cache in
	// modern processors is very valuable, and for a single sort call in general
	// purpose code any gains from an advanced method are cancelled by i-cache
	// misses during the sort, and thrashing the i-cache for surrounding code.
	const maxLenAlwaysInsertionSort = 20
	if n <= maxLenAlwaysInsertionSort {
		sortInsertionSortShiftLeft(v, 1, isLess)
		return
	}

	sortIpnsort(v, layout, isLess)
}

// sortIpnsort is core::slice::sort::unstable::ipnsort.
func sortIpnsort[T any](v []T, layout SortLayout, isLess func(a, b *T) bool) {
	n := len(v)
	runLen, wasReversed := sortFindExistingRun(v, isLess)

	if runLen == n {
		if wasReversed {
			slices.Reverse(v)
		}

		// It would be possible to a do in-place merging here for a long existing streak. But that
		// makes the implementation a lot bigger, users can use `slice::sort` for that use-case.
		return
	}

	// Limit the number of imbalanced partitions to `2 * floor(log2(len))`.
	// The binary OR by one is used to eliminate the zero-check in the logarithm.
	limit := 2 * ilog2(uint64(n|1))
	sortQuicksort(v, nil, limit, layout, isLess)
}

// sortAbort stands for `intrinsics::abort()`, which the sort reaches only
// when its own bookkeeping is wrong.
func sortAbort() {
	panic("rs: sort: internal invariant broken (Rust aborts the process here)")
}
