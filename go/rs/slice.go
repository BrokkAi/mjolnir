// Port of core 1.99.1 src/slice/mod.rs, src/slice/index.rs and
// src/slice/iter.rs (the methods of [T] that Go lacks or does differently),
// and of alloc 1.99.1 src/slice.rs (concat, join, repeat)

package rs

import (
	"reflect"
	"strconv"
)

// Slice methods.
//
// A method of Rust's [T] is the function of the same name over a Go slice:
// `v.first()` is rs.First(v), `v.windows(2)` is rs.Windows(v, 2). A name
// that rs already uses for something else takes the prefix Slice:
// rs.SliceSwap (rs.Swap is mem::swap), rs.SliceReverse (rs.Reverse is
// cmp::Reverse), rs.SliceRotateLeft (rs.RotateLeft is the integer method).
//
// Methods returning an element return a copy, as `.copied()` or a Rust
// move would; the ones returning `&mut T` (`first_mut`, `get_mut`,
// `last_mut`) return a pointer into the slice and whether there is one, as
// the collections do. A method returning sub-slices (`chunks`, `windows`,
// `split_first`) returns sub-slices of v, which share its elements as
// Rust's borrows do. Equality is Eq's (derived PartialEq), and a Clone the
// Rust method makes is Clone's.
//
// Where Rust panics, these panic with Rust's message.

// sliceIndexFail is core::slice::index::slice_index_fail.
func sliceIndexFail(skip int, start, end, n int) {
	switch {
	case start > n:
		PanicAt(skip+1, "range start index "+strconv.Itoa(start)+" out of range for slice of length "+strconv.Itoa(n))
	case end > n:
		PanicAt(skip+1, "range end index "+strconv.Itoa(end)+" out of range for slice of length "+strconv.Itoa(n))
	case start > end:
		PanicAt(skip+1, "slice index starts at "+strconv.Itoa(start)+" but ends at "+strconv.Itoa(end))
	}
	PanicAt(skip+1, "range end index "+strconv.Itoa(end)+" out of range for slice of length "+strconv.Itoa(n))
}

// sliceRange is core::slice::range for `start..end`: it panics as Rust does
// when the range does not fit a slice of length n.
func sliceRange(skip int, start, end, n int) {
	if end > n {
		sliceIndexFail(skip+1, 0, end, n)
	}
	if start > end {
		sliceIndexFail(skip+1, start, end, n)
	}
}

// sliceRangeInclusive is core::slice::range for `start..=end`: it returns
// the exclusive end, and panics as Rust does when the range does not fit.
func sliceRangeInclusive(skip int, start, end, n int) int {
	if end >= n {
		sliceIndexFail(skip+1, 0, end, n)
	}
	exclusive := end + 1
	if start > exclusive {
		sliceIndexFail(skip+1, start, exclusive, n)
	}
	return exclusive
}

// sliceIndexCheck panics as Rust's indexing `v[index]` does.
func sliceIndexCheck(skip int, index, n int) {
	if index < 0 || index >= n {
		PanicAt(skip+1, "index out of bounds: the len is "+strconv.Itoa(n)+" but the index is "+strconv.Itoa(index))
	}
}

// sliceElementsArePlain reports whether copying a T copies all of it, so
// that Clone has nothing to add: numbers, bools, strings (immutable in Go)
// and arrays and structs of those.
func sliceElementsArePlain(t reflect.Type) bool {
	switch t.Kind() {
	case reflect.Bool, reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr,
		reflect.Float32, reflect.Float64, reflect.Complex64, reflect.Complex128, reflect.String:
		return true
	case reflect.Array:
		return sliceElementsArePlain(t.Elem())
	case reflect.Struct:
		for i := range t.NumField() {
			if !sliceElementsArePlain(t.Field(i).Type) {
				return false
			}
		}
		return true
	}
	return false
}

// sliceCloneElements replaces each element of v by its Clone, for the
// methods whose Rust form clones (concat, join, fill, resize,
// extend_from_slice).
func sliceCloneElements[T any](v []T) {
	if sliceElementsArePlain(reflect.TypeFor[T]()) {
		return
	}
	for i := range v {
		v[i] = Clone(v[i])
	}
}

// First is `v.first()` (and `v.iter().next()`): the first element, or None.
func First[T any](v []T) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	return Some(v[0])
}

// FirstMut is `v.first_mut()`: a pointer to the first element, and whether
// there is one.
func FirstMut[T any](v []T) (*T, bool) {
	if len(v) == 0 {
		return nil, false
	}
	return &v[0], true
}

// Last is `v.last()`, and `v.iter().last()`: the last element, or None.
func Last[T any](v []T) Option[T] {
	if len(v) == 0 {
		return None[T]()
	}
	return Some(v[len(v)-1])
}

// LastMut is `v.last_mut()`: a pointer to the last element, and whether
// there is one.
func LastMut[T any](v []T) (*T, bool) {
	if len(v) == 0 {
		return nil, false
	}
	return &v[len(v)-1], true
}

// Get is `v.get(index)`: the element at index, or None when index is out of
// range (a negative index, which no usize is, included).
func Get[T any](v []T, index int) Option[T] {
	if index < 0 || index >= len(v) {
		return None[T]()
	}
	return Some(v[index])
}

// GetMut is `v.get_mut(index)`: a pointer to the element at index, and
// whether there is one.
func GetMut[T any](v []T, index int) (*T, bool) {
	if index < 0 || index >= len(v) {
		return nil, false
	}
	return &v[index], true
}

// GetRange is `v.get(start..end)`: the sub-slice, or None when the range
// does not fit.
func GetRange[T any](v []T, r Range[int]) Option[[]T] {
	if r.Start < 0 || r.Start > r.End || r.End > len(v) {
		return None[[]T]()
	}
	return Some(v[r.Start:r.End])
}

// SplitFirst is `v.split_first()`: the first element and the rest.
func SplitFirst[T any](v []T) Option[Tuple2[T, []T]] {
	if len(v) == 0 {
		return None[Tuple2[T, []T]]()
	}
	return Some(T2(v[0], v[1:]))
}

// SplitLast is `v.split_last()`: the last element and the rest.
func SplitLast[T any](v []T) Option[Tuple2[T, []T]] {
	if len(v) == 0 {
		return None[Tuple2[T, []T]]()
	}
	return Some(T2(v[len(v)-1], v[:len(v)-1]))
}

// SplitAt is `v.split_at(mid)`. It panics with "mid > len" when mid is past
// the end.
func SplitAt[T any](v []T, mid int) ([]T, []T) {
	if mid < 0 || mid > len(v) {
		PanicAt(1, "mid > len")
	}
	return v[:mid], v[mid:]
}

// SplitAtChecked is `v.split_at_checked(mid)`.
func SplitAtChecked[T any](v []T, mid int) Option[Tuple2[[]T, []T]] {
	if mid < 0 || mid > len(v) {
		return None[Tuple2[[]T, []T]]()
	}
	return Some(T2(v[:mid], v[mid:]))
}

// Contains is `v.contains(&x)`, comparing with Eq.
func Contains[T any](v []T, x T) bool {
	for i := range v {
		if Eq(v[i], x) {
			return true
		}
	}
	return false
}

// StartsWith is `v.starts_with(needle)`.
func StartsWith[T any](v, needle []T) bool {
	n := len(needle)
	return len(v) >= n && sliceEqual(needle, v[:n])
}

// EndsWith is `v.ends_with(needle)`.
func EndsWith[T any](v, needle []T) bool {
	m, n := len(v), len(needle)
	return m >= n && sliceEqual(needle, v[m-n:])
}

// sliceEqual is `a == b` on two slices: the same length and every element
// equal, compared in order.
func sliceEqual[T any](a, b []T) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if !Eq(a[i], b[i]) {
			return false
		}
	}
	return true
}

// SliceSwap is `v.swap(a, b)`. It panics as indexing does when either is
// out of range.
func SliceSwap[T any](v []T, a, b int) {
	sliceIndexCheck(1, a, len(v))
	sliceIndexCheck(1, b, len(v))
	v[a], v[b] = v[b], v[a]
}

// SliceReverse is `v.reverse()`.
func SliceReverse[T any](v []T) {
	for i, j := 0, len(v)-1; i < j; i, j = i+1, j-1 {
		v[i], v[j] = v[j], v[i]
	}
}

// SliceRotateLeft is `v.rotate_left(mid)`: the first mid elements move to
// the end. It panics when mid is past the end.
func SliceRotateLeft[T any](v []T, mid int) {
	if !(mid >= 0 && mid <= len(v)) {
		PanicAt(1, "assertion failed: mid <= self.len()")
	}
	sliceRotate(v, mid)
}

// SliceRotateRight is `v.rotate_right(k)`: the last k elements move to the
// front. It panics when k is past the end.
func SliceRotateRight[T any](v []T, k int) {
	if !(k >= 0 && k <= len(v)) {
		PanicAt(1, "assertion failed: k <= self.len()")
	}
	sliceRotate(v, len(v)-k)
}

// sliceRotate moves v[:mid] after v[mid:]. Every rotation leaves the same
// arrangement, so this need not follow ptr_rotate's steps.
func sliceRotate[T any](v []T, mid int) {
	SliceReverse(v[:mid])
	SliceReverse(v[mid:])
	SliceReverse(v)
}

// Fill is `v.fill(value)`: every element but the last becomes a Clone of
// value, and the last value itself.
func Fill[T any](v []T, value T) {
	if len(v) == 0 {
		return
	}
	plain := sliceElementsArePlain(reflect.TypeFor[T]())
	for i := range v[:len(v)-1] {
		if plain {
			v[i] = value
		} else {
			v[i] = Clone(value)
		}
	}
	v[len(v)-1] = value
}

// FillWith is `v.fill_with(f)`: each element, in order, becomes f().
func FillWith[T any](v []T, f func() T) {
	for i := range v {
		v[i] = f()
	}
}

// CopyFromSlice is `dst.copy_from_slice(src)`. It panics as Rust does when
// the lengths differ.
func CopyFromSlice[T any](dst, src []T) {
	if len(dst) != len(src) {
		PanicAt(1, "copy_from_slice: source slice length ("+strconv.Itoa(len(src))+
			") does not match destination slice length ("+strconv.Itoa(len(dst))+")")
	}
	copy(dst, src)
}

// Chunks is `v.chunks(size)`: consecutive sub-slices of size elements, the
// last one shorter when size does not divide the length.
func Chunks[T any](v []T, size int) [][]T {
	if size == 0 {
		PanicAt(1, "chunk size must be non-zero")
	}
	chunks := make([][]T, 0, (len(v)+size-1)/size)
	for len(v) > 0 {
		n := min(size, len(v))
		chunks = append(chunks, v[:n:n])
		v = v[n:]
	}
	return chunks
}

// ChunksExact is `v.chunks_exact(size)`: the sub-slices of exactly size
// elements, and the remainder (`.remainder()`) that is left over.
func ChunksExact[T any](v []T, size int) ([][]T, []T) {
	if size == 0 {
		PanicAt(1, "chunk size must be non-zero")
	}
	full := len(v) - len(v)%size
	chunks := make([][]T, 0, full/size)
	for i := 0; i < full; i += size {
		chunks = append(chunks, v[i:i+size:i+size])
	}
	return chunks, v[full:]
}

// Rchunks is `v.rchunks(size)`: chunks taken from the end, the last one
// (the start of v) shorter when size does not divide the length.
func Rchunks[T any](v []T, size int) [][]T {
	if size == 0 {
		PanicAt(1, "chunk size must be non-zero")
	}
	chunks := make([][]T, 0, (len(v)+size-1)/size)
	for len(v) > 0 {
		n := min(size, len(v))
		chunks = append(chunks, v[len(v)-n:len(v):len(v)])
		v = v[:len(v)-n]
	}
	return chunks
}

// Windows is `v.windows(size)`: every run of size consecutive elements, in
// order. There are none when v is shorter than size.
func Windows[T any](v []T, size int) [][]T {
	if size == 0 {
		PanicAt(1, "window size must be non-zero")
	}
	if len(v) < size {
		return [][]T{}
	}
	windows := make([][]T, 0, len(v)-size+1)
	for i := 0; i+size <= len(v); i++ {
		windows = append(windows, v[i:i+size:i+size])
	}
	return windows
}

// ChunkBy is `v.chunk_by(pred)`: the runs of consecutive elements for which
// pred(previous, next) holds. pred is called on each neighbouring pair once,
// in order.
func ChunkBy[T any](v []T, pred func(a, b T) bool) [][]T {
	chunks := [][]T{}
	for len(v) > 0 {
		n := 1
		for n < len(v) && pred(v[n-1], v[n]) {
			n++
		}
		chunks = append(chunks, v[:n:n])
		v = v[n:]
	}
	return chunks
}

// Concat is `slices.concat()` (`[V]::concat` over slices): every element in
// order, each a Clone.
func Concat[T any](slices [][]T) []T {
	total := 0
	for _, s := range slices {
		total += len(s)
	}
	out := make([]T, 0, total)
	for _, s := range slices {
		out = append(out, s...)
	}
	sliceCloneElements(out)
	return out
}

// Join is `slices.join(sep)` (`[V]::join`) for a separator that is one
// element (`join(&x)`) or a slice of them (`join(&[x, y][..])`): the
// slices in order with the separator between each two, each element a
// Clone.
func Join[T any](slices [][]T, sep ...T) []T {
	if len(slices) == 0 {
		return []T{}
	}
	total := len(sep) * (len(slices) - 1)
	for _, s := range slices {
		total += len(s)
	}
	out := make([]T, 0, total)
	for i, s := range slices {
		if i > 0 {
			out = append(out, sep...)
		}
		out = append(out, s...)
	}
	sliceCloneElements(out)
	return out
}

// Repeat is `v.repeat(n)`: v n times over. It panics with "capacity
// overflow" when the length does not fit a usize.
func Repeat[T any](v []T, n int) []T {
	if n < 0 {
		PanicAt(1, "capacity overflow")
	}
	if n > 0 && len(v) > UsizeMax/n {
		PanicAt(1, "capacity overflow")
	}
	out := make([]T, 0, len(v)*n)
	for range n {
		out = append(out, v...)
	}
	return out
}
