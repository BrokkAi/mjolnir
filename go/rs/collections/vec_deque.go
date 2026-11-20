// Port of alloc 1.99.1 src/collections/vec_deque/mod.rs (with drain.rs,
// iter.rs, into_iter.rs, macros.rs, spec_extend.rs and spec_from_iter.rs, the
// growth policy of src/raw_vec/mod.rs, and binary_search_by of core's
// src/slice/mod.rs)

package collections

import (
	"iter"
	"reflect"
	"slices"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// VecDeque is Rust's VecDeque<T>: a double-ended queue implemented with a
// growable ring buffer.
//
// The zero value is an empty deque. It is changed through a pointer, and a
// plain assignment shares the buffer instead of copying it (the translation
// of a Rust move); `deque.clone()` is rs.Clone(d) (or d.Clone()).
//
// Elements are stored inline in the buffer, as in Rust and as in a Go slice.
// Get, GetMut, Front, Back, Index and the iterators return pointers into the
// buffer, so an element is changed in place through them. Such a pointer is
// valid until the next call that adds, removes or moves elements (any push,
// pop, insert, remove, drain, rotation, reservation and so on); after that
// it may point at a different element or at a buffer the deque no longer
// uses. A deque must not be changed while it is being iterated.
//
// The port follows Rust's buffer management step by step, so Capacity and
// the split AsSlices reports are the ones Rust gives after the same calls.
// Two things cannot be reproduced: Rust picks the first allocation by the
// size of T as Rust lays it out (8 elements for a one-byte T, else 4, or 1
// above 1 KiB), which here is Go's size; and `extend` and `collect` in Rust
// reserve by the iterator's size hint, which a Go sequence does not have, so
// Extend and VecDequeFromIter grow as a Rust iterator of unknown length
// does. Neither changes the contents of the deque.
//
// A method that takes a Rust index range takes its two bounds, as described
// at Bound: `deque.drain(..)` is
// d.Drain(collections.Unbounded[int](), collections.Unbounded[int]()).
type VecDeque[T any] struct {
	// `self[0]`, if it exists, is `buf[head]`.
	// `head < buf.capacity()`, unless `buf.capacity() == 0` when `head == 0`.
	head int
	// the number of initialized elements, starting from the one at `head`
	// and potentially wrapping around. if `len == 0`, the exact value of
	// `head` is unimportant.
	length int
	// buf is Rust's RawVec: its length is the capacity of the deque. Slots
	// outside the live range hold zero values, so that the collector does
	// not see moved-out elements as live.
	buf []T
}

// vecDequeElementSize is `size_of::<T>()`, as Go lays T out.
func vecDequeElementSize[T any]() uintptr {
	var zero T
	return unsafe.Sizeof(zero)
}

// vecDequeMinNonZeroCap is min_non_zero_cap in raw_vec/mod.rs. Tiny Vecs are
// dumb. Skip to:
//   - 8 if the element size is 1, because any heap allocator is likely to
//     round up a request of less than 8 bytes to at least 8 bytes.
//   - 4 if elements are moderate-sized (<= 1 KiB).
//   - 1 otherwise, to avoid wasting too much space for very short Vecs.
func vecDequeMinNonZeroCap(size uintptr) int {
	switch {
	case size == 1:
		return 8
	case size <= 1024:
		return 4
	default:
		return 1
	}
}

// capacity is VecDeque::capacity for the methods that change the deque. A
// deque of zero-sized elements has the capacity usize::MAX and no
// allocation; here its buffer is a slice of that length, which takes no
// memory either, made the first time it is needed.
func (d *VecDeque[T]) capacity() int {
	if d.buf == nil && vecDequeElementSize[T]() == 0 {
		d.buf = make([]T, rs.UsizeMax)
	}
	return len(d.buf)
}

// setCapacity is the reallocation at the end of RawVec's finish_grow and
// shrink: the buffer gets the given capacity and keeps the slots that fit.
func (d *VecDeque[T]) setCapacity(capacity int) {
	if capacity == 0 {
		d.buf = nil
		return
	}
	buf := make([]T, capacity)
	copy(buf, d.buf)
	d.buf = buf
}

// vecDequeCapacityOverflow reports whether a buffer of the given capacity is
// more than Rust allocates (isize::MAX bytes).
func vecDequeCapacityOverflow[T any](capacity int) bool {
	size := vecDequeElementSize[T]()
	return capacity < 0 || (size != 0 && uint(capacity) > uint(rs.UsizeMax)/uint(size))
}

// growAmortized is RawVecInner::grow_amortized. It returns false for the
// capacity overflow Rust panics on.
func (d *VecDeque[T]) growAmortized(length, additional int) bool {
	size := vecDequeElementSize[T]()
	if size == 0 {
		// Since we return a capacity of `usize::MAX` when `elem_size` is
		// 0, getting to here necessarily means the `RawVec` is overfull.
		return false
	}
	// Nothing we can really do about these checks, sadly.
	requiredCap := length + additional
	if requiredCap < length {
		return false
	}
	// This guarantees exponential growth.
	capacity := max(len(d.buf)*2, requiredCap)
	capacity = max(vecDequeMinNonZeroCap(size), capacity)
	if vecDequeCapacityOverflow[T](capacity) {
		return false
	}
	d.setCapacity(capacity)
	return true
}

// growExact is RawVecInner::grow_exact.
func (d *VecDeque[T]) growExact(length, additional int) bool {
	if vecDequeElementSize[T]() == 0 {
		return false
	}
	capacity := length + additional
	if capacity < length || vecDequeCapacityOverflow[T](capacity) {
		return false
	}
	d.setCapacity(capacity)
	return true
}

// bufferRead moves the element out of a slot of the buffer.
func (d *VecDeque[T]) bufferRead(off int) T {
	value := d.buf[off]
	var zero T
	d.buf[off] = zero
	return value
}

// bufferWrite writes an element into a slot of the buffer.
func (d *VecDeque[T]) bufferWrite(off int, value T) *T {
	d.buf[off] = value
	return &d.buf[off]
}

// clearSlots zeroes count slots starting at the logical index start of a
// deque whose head was at head. Rust has nothing to do there: the elements
// were moved out. Go clears the slots so that they do not keep the values
// alive.
func (d *VecDeque[T]) clearSlots(head, start, count int) {
	if count == 0 {
		return
	}
	first := vecDequeWrapIndex(uint(head)+uint(start), len(d.buf))
	headRoom := len(d.buf) - first
	if count <= headRoom {
		clear(d.buf[first : first+count])
		return
	}
	clear(d.buf[first:])
	clear(d.buf[:count-headRoom])
}

// isFull returns `true` if the buffer is at full capacity.
func (d *VecDeque[T]) isFull() bool {
	return d.length == d.capacity()
}

// vecDequeWrapIndex is wrap_index: the index into the underlying buffer for
// a given logical element index, which is less than twice the capacity.
func vecDequeWrapIndex(logicalIndex uint, capacity int) int {
	if logicalIndex >= uint(capacity) {
		return int(logicalIndex - uint(capacity))
	}
	return int(logicalIndex)
}

// wrapAdd returns the index in the underlying buffer for a given logical
// element index + addend.
func (d VecDeque[T]) wrapAdd(idx, addend int) int {
	return vecDequeWrapIndex(uint(idx)+uint(addend), len(d.buf))
}

func (d VecDeque[T]) toWrappedIndex(idx int) int {
	return d.wrapAdd(d.head, idx)
}

// wrapSub returns the index in the underlying buffer for a given logical
// element index - subtrahend.
func (d VecDeque[T]) wrapSub(idx, subtrahend int) int {
	return vecDequeWrapIndex(uint(idx)-uint(subtrahend)+uint(len(d.buf)), len(d.buf))
}

// copy copies a contiguous block of memory len long from src to dst. The
// blocks may overlap.
func (d *VecDeque[T]) copy(src, dst, length int) {
	copy(d.buf[dst:dst+length], d.buf[src:src+length])
}

// wrapCopy copies a potentially wrapping block of memory len long from src
// to dest. (abs(dst - src) + len) must be no larger than capacity() (There
// must be at most one continuous overlapping region between src and dest).
func (d *VecDeque[T]) wrapCopy(src, dst, length int) {
	// If T is a ZST, don't do any copying.
	if vecDequeElementSize[T]() == 0 || src == dst || length == 0 {
		return
	}

	dstAfterSrc := d.wrapSub(dst, src) < length

	srcPreWrapLen := len(d.buf) - src
	dstPreWrapLen := len(d.buf) - dst
	srcWraps := srcPreWrapLen < length
	dstWraps := dstPreWrapLen < length

	switch {
	case !srcWraps && !dstWraps:
		// src doesn't wrap, dst doesn't wrap
		//
		//        S . . .
		// 1 [_ _ A A B B C C _]
		// 2 [_ _ A A A A B B _]
		//            D . . .
		//
		d.copy(src, dst, length)
	case !dstAfterSrc && !srcWraps && dstWraps:
		// dst before src, src doesn't wrap, dst wraps
		//
		//    S . . .
		// 1 [A A B B _ _ _ C C]
		// 2 [A A B B _ _ _ A A]
		// 3 [B B B B _ _ _ A A]
		//    . .           D .
		//
		d.copy(src, dst, dstPreWrapLen)
		d.copy(src+dstPreWrapLen, 0, length-dstPreWrapLen)
	case dstAfterSrc && !srcWraps && dstWraps:
		// src before dst, src doesn't wrap, dst wraps
		//
		//              S . . .
		// 1 [C C _ _ _ A A B B]
		// 2 [B B _ _ _ A A B B]
		// 3 [B B _ _ _ A A A A]
		//    . .           D .
		//
		d.copy(src+dstPreWrapLen, 0, length-dstPreWrapLen)
		d.copy(src, dst, dstPreWrapLen)
	case !dstAfterSrc && srcWraps && !dstWraps:
		// dst before src, src wraps, dst doesn't wrap
		//
		//    . .           S .
		// 1 [C C _ _ _ A A B B]
		// 2 [C C _ _ _ B B B B]
		// 3 [C C _ _ _ B B C C]
		//              D . . .
		//
		d.copy(src, dst, srcPreWrapLen)
		d.copy(0, dst+srcPreWrapLen, length-srcPreWrapLen)
	case dstAfterSrc && srcWraps && !dstWraps:
		// src before dst, src wraps, dst doesn't wrap
		//
		//    . .           S .
		// 1 [A A B B _ _ _ C C]
		// 2 [A A A A _ _ _ C C]
		// 3 [C C A A _ _ _ C C]
		//    D . . .
		//
		d.copy(0, dst+srcPreWrapLen, length-srcPreWrapLen)
		d.copy(src, dst, srcPreWrapLen)
	case !dstAfterSrc && srcWraps && dstWraps:
		// dst before src, src wraps, dst wraps
		//
		//    . . .         S .
		// 1 [A B C D _ E F G H]
		// 2 [A B C D _ E G H H]
		// 3 [A B C D _ E G H A]
		// 4 [B C C D _ E G H A]
		//    . .         D . .
		//
		delta := dstPreWrapLen - srcPreWrapLen
		d.copy(src, dst, srcPreWrapLen)
		d.copy(0, dst+srcPreWrapLen, delta)
		d.copy(delta, 0, length-dstPreWrapLen)
	default:
		// src before dst, src wraps, dst wraps
		//
		//    . .         S . .
		// 1 [A B C D _ E F G H]
		// 2 [A A B D _ E F G H]
		// 3 [H A B D _ E F G H]
		// 4 [H A B D _ E F F G]
		//    . . .         D .
		//
		delta := srcPreWrapLen - dstPreWrapLen
		d.copy(0, delta, length-srcPreWrapLen)
		d.copy(len(d.buf)-delta, 0, delta)
		d.copy(src, dst, dstPreWrapLen)
	}
}

// copySlice copies all values from src to dst, wrapping around if needed.
// Assumes capacity is sufficient.
func (d *VecDeque[T]) copySlice(dst int, src []T) {
	headRoom := len(d.buf) - dst
	if len(src) <= headRoom {
		copy(d.buf[dst:], src)
	} else {
		copy(d.buf[dst:], src[:headRoom])
		copy(d.buf, src[headRoom:])
	}
}

// handleCapacityIncrease frobs the head and tail sections around to handle
// the fact that we just reallocated.
func (d *VecDeque[T]) handleCapacityIncrease(oldCapacity int) {
	newCapacity := d.capacity()

	// Move the shortest contiguous section of the ring buffer
	//
	// H := head
	// L := last element (`self.to_physical_idx(self.len - 1)`)
	//
	//    H             L
	//   [o o o o o o o o ]
	//    H             L
	// A [o o o o o o o o . . . . . . . . ]
	//        L H
	//   [o o o o o o o o ]
	//          H             L
	// B [. . . o o o o o o o o . . . . . ]
	//              L H
	//   [o o o o o o o o ]
	//              L                 H
	// C [o o o o o o . . . . . . . . o o ]

	// can't use is_contiguous() because the capacity is already updated.
	if d.head <= oldCapacity-d.length {
		// A
		// Nop
		return
	}
	headLen := oldCapacity - d.head
	tailLen := d.length - headLen
	if headLen > tailLen && newCapacity-oldCapacity >= tailLen {
		// B
		d.copy(0, oldCapacity, tailLen)
		clear(d.buf[:tailLen])
	} else {
		// C
		newHead := newCapacity - headLen
		// can't use copy_nonoverlapping here, because if e.g. head_len = 2
		// and new_capacity = old_capacity + 1, then the heads overlap.
		d.copy(d.head, newHead, headLen)
		clear(d.buf[d.head:min(oldCapacity, newHead)])
		d.head = newHead
	}
}

// VecDequeNew is VecDeque::new: an empty deque.
func VecDequeNew[T any]() VecDeque[T] {
	var empty VecDeque[T]
	return empty
}

// VecDequeDefault is VecDeque::default: an empty deque.
func VecDequeDefault[T any]() VecDeque[T] {
	var empty VecDeque[T]
	return empty
}

// VecDequeWithCapacity is VecDeque::with_capacity: an empty deque with space
// for at least capacity elements.
func VecDequeWithCapacity[T any](capacity int) VecDeque[T] {
	if vecDequeCapacityOverflow[T](capacity) {
		rs.PanicAt(1, "capacity overflow")
	}
	deque := VecDequeNew[T]()
	if vecDequeElementSize[T]() != 0 {
		deque.setCapacity(capacity)
	}
	return deque
}

// VecDequeFrom is VecDeque::from(vec) and VecDeque::from([a, b, ..]): a
// deque holding the elements in order. The elements are copied into a buffer
// of their number; Rust reuses the buffer of a Vec, and with it whatever
// spare capacity the Vec had.
func VecDequeFrom[T any](elements ...T) VecDeque[T] {
	deque := VecDequeWithCapacity[T](len(elements))
	if vecDequeElementSize[T]() != 0 {
		copy(deque.buf, elements)
	} else if len(elements) != 0 {
		deque.capacity()
	}
	deque.head = 0
	deque.length = len(elements)
	return deque
}

// VecDequeFromIter is `iter.collect::<VecDeque<T>>()` (FromIterator). Rust
// collects into a Vec and takes its buffer; the capacity here is the one a
// Vec collected from an iterator of unknown length has.
func VecDequeFromIter[T any](seq iter.Seq[T]) VecDeque[T] {
	elements := slices.Collect(seq)
	deque := VecDequeNew[T]()
	if len(elements) == 0 {
		return deque
	}
	size := vecDequeElementSize[T]()
	if size == 0 {
		deque.capacity()
		deque.length = len(elements)
		return deque
	}
	capacity := vecDequeMinNonZeroCap(size)
	for capacity < len(elements) {
		capacity *= 2
	}
	deque.setCapacity(capacity)
	copy(deque.buf, elements)
	deque.length = len(elements)
	return deque
}

// VecFromVecDeque is Vec::from(deque) (`let v: Vec<T> = deque.into()`): the
// elements in order as a slice. The deque is not used again; the slice has
// the deque's buffer when the elements already start at its beginning.
func VecFromVecDeque[T any](deque VecDeque[T]) []T {
	contiguous := deque.MakeContiguous()
	if deque.head != 0 {
		copy(deque.buf, contiguous)
		clear(deque.buf[deque.length:min(len(deque.buf), deque.head+deque.length)])
	}
	if vecDequeElementSize[T]() == 0 {
		return make([]T, deque.length)
	}
	return deque.buf[:deque.length]
}

// Get is VecDeque::get: a pointer to the element at index, where index 0 is
// the front of the queue, and whether there is one.
func (d VecDeque[T]) Get(index int) (*T, bool) {
	if index >= 0 && index < d.length {
		return &d.buf[d.toWrappedIndex(index)], true
	}
	return nil, false
}

// GetMut is VecDeque::get_mut. It is the same pointer Get returns.
func (d *VecDeque[T]) GetMut(index int) (*T, bool) {
	if index >= 0 && index < d.length {
		return &d.buf[d.toWrappedIndex(index)], true
	}
	return nil, false
}

// Swap is VecDeque::swap: it swaps the elements at indices i and j. It
// panics if either index is out of bounds.
func (d *VecDeque[T]) Swap(i, j int) {
	if !(i >= 0 && i < d.length) {
		rs.PanicAt(1, "assertion failed: i < self.len()")
	}
	if !(j >= 0 && j < d.length) {
		rs.PanicAt(1, "assertion failed: j < self.len()")
	}
	d.swap(i, j)
}

func (d *VecDeque[T]) swap(i, j int) {
	ri := d.toWrappedIndex(i)
	rj := d.toWrappedIndex(j)
	d.buf[ri], d.buf[rj] = d.buf[rj], d.buf[ri]
}

// Capacity is VecDeque::capacity: the number of elements the deque can hold
// without reallocating.
func (d VecDeque[T]) Capacity() int {
	if vecDequeElementSize[T]() == 0 {
		return rs.UsizeMax
	}
	return len(d.buf)
}

// ReserveExact is VecDeque::reserve_exact: it reserves the minimum capacity
// for at least additional more elements.
func (d *VecDeque[T]) ReserveExact(additional int) {
	newCap := d.length + additional
	if additional < 0 || newCap < d.length {
		rs.PanicAt(1, "capacity overflow")
	}
	oldCap := d.capacity()
	if newCap > oldCap {
		if !d.growExact(d.length, additional) {
			rs.PanicAt(1, "capacity overflow")
		}
		d.handleCapacityIncrease(oldCap)
	}
}

// Reserve is VecDeque::reserve: it reserves capacity for at least additional
// more elements, and may reserve more to avoid frequent reallocations.
func (d *VecDeque[T]) Reserve(additional int) {
	if !d.reserve(additional) {
		rs.PanicAt(1, "capacity overflow")
	}
}

func (d *VecDeque[T]) reserve(additional int) bool {
	newCap := d.length + additional
	if additional < 0 || newCap < d.length {
		return false
	}
	oldCap := d.capacity()
	if newCap > oldCap {
		// we don't need to reserve_exact(), as the size doesn't have
		// to be a power of 2.
		if !d.growAmortized(d.length, additional) {
			return false
		}
		d.handleCapacityIncrease(oldCap)
	}
	return true
}

// ShrinkToFit is VecDeque::shrink_to_fit: it shrinks the capacity as much as
// possible.
func (d *VecDeque[T]) ShrinkToFit() {
	d.ShrinkTo(0)
}

// ShrinkTo is VecDeque::shrink_to: it shrinks the capacity with a lower
// bound. The capacity will remain at least as large as both the length and
// the supplied value.
func (d *VecDeque[T]) ShrinkTo(minCapacity int) {
	targetCap := max(minCapacity, d.length)

	// never shrink ZSTs
	if vecDequeElementSize[T]() == 0 || d.capacity() <= targetCap {
		return
	}

	// There are three cases of interest:
	//   All elements are out of desired bounds
	//   Elements are contiguous, and tail is out of desired bounds
	//   Elements are discontiguous
	//
	// At all other times, element positions are unaffected.

	// `head` and `len` are at most `isize::MAX` and `target_cap < self.capacity()`, so nothing can
	// overflow.
	tailOutside := targetCap+1 <= d.head+d.length && d.head+d.length <= d.capacity()

	switch {
	case d.length == 0:
		d.head = 0
	case d.head >= targetCap && tailOutside:
		// Head and tail are both out of bounds, so copy all of them to the front.
		//
		//  H := head
		//  L := last element
		//                    H           L
		//   [. . . . . . . . o o o o o o o . ]
		//    H           L
		//   [o o o o o o o . ]
		// nonoverlapping because `self.head >= target_cap >= self.len`.
		d.copy(d.head, 0, d.length)
		d.head = 0
	case d.head < targetCap && tailOutside:
		// Head is in bounds, tail is out of bounds.
		// Copy the overflowing part to the beginning of the
		// buffer. This won't overlap because `target_cap >= self.len`.
		//
		//  H := head
		//  L := last element
		//          H           L
		//   [. . . o o o o o o o . . . . . . ]
		//      L   H
		//   [o o . o o o o o ]
		length := d.head + d.length - targetCap
		d.copy(targetCap, 0, length)
	case !d.isContiguous():
		// The head slice is at least partially out of bounds, tail is in bounds.
		// Copy the head backwards so it lines up with the target capacity.
		// This won't overlap because `target_cap >= self.len`.
		//
		//  H := head
		//  L := last element
		//            L                   H
		//   [o o o o o . . . . . . . . . o o ]
		//            L   H
		//   [o o o o o . o o ]
		headLen := d.capacity() - d.head
		// head_len is at least one, so new_head will be < target_cap
		newHead := targetCap - headLen
		// can't use `copy_nonoverlapping()` here because the new and old
		// regions for the head might overlap.
		d.copy(d.head, newHead, headLen)
		d.head = newHead
	}

	d.setCapacity(targetCap)
}

// Truncate is VecDeque::truncate: it shortens the deque, keeping the first
// len elements and dropping the rest. If len is greater or equal to the
// deque's current length, this has no effect.
func (d *VecDeque[T]) Truncate(length int) {
	if length >= d.length {
		return
	}
	front, back := d.AsMutSlices()
	if length > len(front) {
		begin := length - len(front)
		clear(back[begin:])
	} else {
		clear(back)
		clear(front[length:])
	}
	d.length = length
}

// RetainBack is VecDeque::retain_back: it shortens the deque, keeping the
// last len elements and dropping the rest. If len is greater or equal to the
// deque's current length, this has no effect.
func (d *VecDeque[T]) RetainBack(length int) {
	if length >= d.length {
		// No action is taken
		return
	}
	front, back := d.AsMutSlices()
	if length > len(back) {
		// The 'back' slice remains unchanged.
		// front.len() + back.len() == self.len, so 'end' is non-negative
		// and end < front.len()
		end := len(front) - (length - len(back))
		clear(front[:end])
		d.head += end
		d.length = length
	} else {
		// 'end' is non-negative by the condition above
		end := len(back) - length
		clear(front)
		clear(back[:end])
		d.head = d.toWrappedIndex(d.length - length)
		d.length = length
	}
}

// Iter is VecDeque::iter (`for x in &deque`): a pointer to each element,
// front to back.
func (d VecDeque[T]) Iter() iter.Seq[*T] {
	return func(yield func(*T) bool) {
		a, b := d.AsSlices()
		vecDequeForward(a, b, yield)
	}
}

// IterRev is `deque.iter().rev()`: a pointer to each element, back to front.
func (d VecDeque[T]) IterRev() iter.Seq[*T] {
	return func(yield func(*T) bool) {
		a, b := d.AsSlices()
		vecDequeBackward(a, b, yield)
	}
}

// IterMut is VecDeque::iter_mut. It is the same sequence as Iter.
func (d *VecDeque[T]) IterMut() iter.Seq[*T] {
	return VecDeque[T].Iter(*d)
}

// IterMutRev is `deque.iter_mut().rev()`.
func (d *VecDeque[T]) IterMutRev() iter.Seq[*T] {
	return VecDeque[T].IterRev(*d)
}

func vecDequeForward[T any](a, b []T, yield func(*T) bool) {
	for index := range a {
		if !yield(&a[index]) {
			return
		}
	}
	for index := range b {
		if !yield(&b[index]) {
			return
		}
	}
}

func vecDequeBackward[T any](a, b []T, yield func(*T) bool) {
	for index := len(b) - 1; index >= 0; index-- {
		if !yield(&b[index]) {
			return
		}
	}
	for index := len(a) - 1; index >= 0; index-- {
		if !yield(&a[index]) {
			return
		}
	}
}

// AsSlices is VecDeque::as_slices: a pair of slices which contain, in
// order, the contents of the deque. The slices are views of the buffer. If
// MakeContiguous was called just before, all the elements are in the first
// slice and the second is empty.
func (d VecDeque[T]) AsSlices() ([]T, []T) {
	aStart, aEnd, bEnd := d.sliceRanges(0, d.length)
	return d.buf[aStart:aEnd:aEnd], d.buf[0:bEnd:bEnd]
}

// AsMutSlices is VecDeque::as_mut_slices. They are the same slices AsSlices
// returns.
func (d *VecDeque[T]) AsMutSlices() ([]T, []T) {
	return VecDeque[T].AsSlices(*d)
}

// Len is VecDeque::len: the number of elements in the deque.
func (d VecDeque[T]) Len() int {
	return d.length
}

// IsEmpty is VecDeque::is_empty.
func (d VecDeque[T]) IsEmpty() bool {
	return d.length == 0
}

// sliceRanges, given the logical range start..end (which slice::range has
// checked), returns the two ranges into the physical buffer that correspond
// to it: aStart..aEnd, and 0..bEnd.
func (d VecDeque[T]) sliceRanges(start, end int) (aStart, aEnd, bEnd int) {
	length := end - start
	if length == 0 {
		return 0, 0, 0
	}
	// `slice::range` guarantees that `start <= end <= len`.
	// because `len != 0`, we know that `start < end`, so `start < len`
	// and the indexing is valid.
	wrappedStart := d.toWrappedIndex(start)

	// this subtraction can never overflow because `wrapped_start` is
	// at most `self.capacity()` (and if `self.capacity != 0`, then `wrapped_start` is strictly less
	// than `self.capacity`).
	headLen := len(d.buf) - wrappedStart

	if headLen >= length {
		// we know that `len + wrapped_start <= self.capacity <= usize::MAX`, so this addition can't overflow
		return wrappedStart, wrappedStart + length, 0
	}
	// can't overflow because of the if condition
	tailLen := length - headLen
	return wrappedStart, len(d.buf), tailLen
}

// Range is VecDeque::range: a pointer to each element of the given range of
// indexes, front to back. It panics, when called, if the range has its start
// after its end or ends beyond the length of the deque.
func (d VecDeque[T]) Range(lo, hi Bound[int]) iter.Seq[*T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(*T) bool) {
		aStart, aEnd, bEnd := d.sliceRanges(start, end)
		vecDequeForward(d.buf[aStart:aEnd], d.buf[0:bEnd], yield)
	}
}

// RangeRev is `deque.range(..).rev()`.
func (d VecDeque[T]) RangeRev(lo, hi Bound[int]) iter.Seq[*T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return func(yield func(*T) bool) {
		aStart, aEnd, bEnd := d.sliceRanges(start, end)
		vecDequeBackward(d.buf[aStart:aEnd], d.buf[0:bEnd], yield)
	}
}

// RangeMut is VecDeque::range_mut. It is the same sequence as Range.
func (d *VecDeque[T]) RangeMut(lo, hi Bound[int]) iter.Seq[*T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	deque := *d
	return func(yield func(*T) bool) {
		aStart, aEnd, bEnd := deque.sliceRanges(start, end)
		vecDequeForward(deque.buf[aStart:aEnd], deque.buf[0:bEnd], yield)
	}
}

// RangeMutRev is `deque.range_mut(..).rev()`.
func (d *VecDeque[T]) RangeMutRev(lo, hi Bound[int]) iter.Seq[*T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	deque := *d
	return func(yield func(*T) bool) {
		aStart, aEnd, bEnd := deque.sliceRanges(start, end)
		vecDequeBackward(deque.buf[aStart:aEnd], deque.buf[0:bEnd], yield)
	}
}

// Drain is VecDeque::drain: it removes the given range of indexes from the
// deque and returns the removed elements, front to back. It panics if the
// range has its start after its end or ends beyond the length of the deque.
//
// In Rust the elements leave the deque as the iterator is consumed and the
// rest of the range when it is dropped, and the deque cannot be touched in
// between. Here the whole range is removed by the time Drain returns, which
// no caller can tell apart.
func (d *VecDeque[T]) Drain(lo, hi Bound[int]) iter.Seq[T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	return slices.Values(d.drain(start, end-start))
}

// DrainRev is `deque.drain(..).rev()`: as Drain, with the removed elements
// back to front.
func (d *VecDeque[T]) DrainRev(lo, hi Bound[int]) iter.Seq[T] {
	start, end, failure := sliceRange(d.length, lo, hi)
	if failure != "" {
		rs.PanicAt(1, failure)
	}
	drained := d.drain(start, end-start)
	return func(yield func(T) bool) {
		for index := len(drained) - 1; index >= 0; index-- {
			if !yield(drained[index]) {
				return
			}
		}
	}
}

// drain takes drainLen elements starting at the logical index drainStart out
// of the deque and closes the gap as the destructor of Rust's Drain does
// (DropGuard in drain.rs).
func (d *VecDeque[T]) drain(drainStart, drainLen int) []T {
	// The deque's elements are parted into three segments:
	// * 0  -> drain_start
	// * drain_start -> drain_start+drain_len
	// * drain_start+drain_len -> self.len
	drained := make([]T, drainLen)
	if drainLen != 0 {
		aStart, aEnd, bEnd := d.sliceRanges(drainStart, drainStart+drainLen)
		copied := copy(drained, d.buf[aStart:aEnd])
		copy(drained[copied:], d.buf[0:bEnd])
	}

	origLen := d.length
	origHead := d.head
	headLen := drainStart                      // #elements in front of the drain
	tailLen := origLen - drainStart - drainLen // #elements behind the drain
	newLen := headLen + tailLen

	if vecDequeElementSize[T]() == 0 {
		// no need to copy around any memory if T is a ZST
		d.length = newLen
		return drained
	}

	// Next, we will fill the hole left by the drain with as few writes as possible.
	// The code below handles the following control flow and reduces the amount of
	// branches under the assumption that `head_len == 0 || tail_len == 0`, i.e.
	// draining at the front or at the back of the dequeue is especially common.
	//
	// H = "head index" = `deque.head`
	// h = elements in front of the drain
	// d = elements in the drain
	// t = elements behind the drain
	//
	// Note that the buffer may wrap at any point and the wrapping is handled by
	// `wrap_copy` and `to_physical_idx`.
	//
	// Case 1: if `head_len == 0 && tail_len == 0`
	// Everything was drained, reset the head index back to 0.
	//             H
	// [ . . . . . d d d d . . . . . ]
	//   H
	// [ . . . . . . . . . . . . . . ]
	//
	// Case 2: else if `tail_len == 0`
	// Don't move data or the head index.
	//         H
	// [ . . . h h h h d d d d . . . ]
	//         H
	// [ . . . h h h h . . . . . . . ]
	//
	// Case 3: else if `head_len == 0`
	// Don't move data, but move the head index.
	//         H
	// [ . . . d d d d t t t t . . . ]
	//                 H
	// [ . . . . . . . t t t t . . . ]
	//
	// Case 4: else if `tail_len <= head_len`
	// Move data, but not the head index.
	//       H
	// [ . . h h h h d d d d t t . . ]
	//       H
	// [ . . h h h h t t . . . . . . ]
	//
	// Case 5: else
	// Move data and the head index.
	//       H
	// [ . . h h d d d d t t t t . . ]
	//               H
	// [ . . . . . . h h t t t t . . ]

	// When draining at the front (`.drain(..n)`) or at the back (`.drain(n..)`),
	// we don't need to copy any data. The number of elements copied would be 0.
	if headLen != 0 && tailLen != 0 {
		// Pick whether to move the head or the tail here.
		if headLen < tailLen {
			d.wrapCopy(d.head, d.toWrappedIndex(drainLen), headLen)
		} else {
			d.wrapCopy(d.toWrappedIndex(headLen+drainLen), d.toWrappedIndex(headLen), tailLen)
		}
	}

	if headLen < tailLen {
		// The elements now start drain_len slots further on.
		d.clearSlots(origHead, 0, drainLen)
	} else {
		d.clearSlots(origHead, newLen, drainLen)
	}

	if newLen == 0 {
		// Special case: If the entire deque was drained, reset the head back to 0,
		// like `.clear()` does.
		d.head = 0
	} else if headLen < tailLen {
		// If we moved the head above, then we need to adjust the head index here.
		d.head = d.toWrappedIndex(drainLen)
	}
	d.length = newLen
	return drained
}

// Clear is VecDeque::clear: it removes every element.
func (d *VecDeque[T]) Clear() {
	d.Truncate(0)
	// Not strictly necessary, but leaves things in a more consistent/predictable state.
	d.head = 0
}

// Contains is VecDeque::contains: whether the deque holds an element equal
// (by rs.Eq) to x.
func (d VecDeque[T]) Contains(x T) bool {
	a, b := d.AsSlices()
	return vecDequeSliceContains(a, x) || vecDequeSliceContains(b, x)
}

func vecDequeSliceContains[T any](s []T, x T) bool {
	for index := range s {
		if rs.Eq(s[index], x) {
			return true
		}
	}
	return false
}

// Front is VecDeque::front: a pointer to the front element, and whether the
// deque has one.
func (d VecDeque[T]) Front() (*T, bool) {
	return d.Get(0)
}

// FrontMut is VecDeque::front_mut. It is the same pointer Front returns.
func (d *VecDeque[T]) FrontMut() (*T, bool) {
	return d.GetMut(0)
}

// Back is VecDeque::back: a pointer to the back element, and whether the
// deque has one.
func (d VecDeque[T]) Back() (*T, bool) {
	return d.Get(d.length - 1)
}

// BackMut is VecDeque::back_mut. It is the same pointer Back returns.
func (d *VecDeque[T]) BackMut() (*T, bool) {
	return d.GetMut(d.length - 1)
}

// PopFront is VecDeque::pop_front: it removes the first element and returns
// it, or None if the deque is empty.
func (d *VecDeque[T]) PopFront() rs.Option[T] {
	if d.length == 0 {
		return rs.None[T]()
	}
	oldHead := d.head
	d.head = d.toWrappedIndex(1)
	d.length--
	return rs.Some(d.bufferRead(oldHead))
}

// PopBack is VecDeque::pop_back: it removes the last element and returns
// it, or None if the deque is empty.
func (d *VecDeque[T]) PopBack() rs.Option[T] {
	if d.length == 0 {
		return rs.None[T]()
	}
	d.length--
	return rs.Some(d.bufferRead(d.toWrappedIndex(d.length)))
}

// PopFrontIf is VecDeque::pop_front_if: it removes and returns the first
// element if the predicate returns true, or None if the predicate returns
// false or the deque is empty (the predicate will not be called in that
// case).
func (d *VecDeque[T]) PopFrontIf(predicate func(*T) bool) rs.Option[T] {
	first, ok := d.FrontMut()
	if !ok {
		return rs.None[T]()
	}
	if predicate(first) {
		return d.PopFront()
	}
	return rs.None[T]()
}

// PopBackIf is VecDeque::pop_back_if: it removes and returns the last
// element if the predicate returns true, or None if the predicate returns
// false or the deque is empty (the predicate will not be called in that
// case).
func (d *VecDeque[T]) PopBackIf(predicate func(*T) bool) rs.Option[T] {
	last, ok := d.BackMut()
	if !ok {
		return rs.None[T]()
	}
	if predicate(last) {
		return d.PopBack()
	}
	return rs.None[T]()
}

// PushFront is VecDeque::push_front: it prepends an element to the deque.
func (d *VecDeque[T]) PushFront(value T) {
	d.pushFrontMut(value)
}

// PushFrontMut is VecDeque::push_front_mut: it prepends an element to the
// deque, returning a pointer to it.
func (d *VecDeque[T]) PushFrontMut(value T) *T {
	return d.pushFrontMut(value)
}

func (d *VecDeque[T]) pushFrontMut(value T) *T {
	if d.isFull() {
		d.grow()
	}
	d.head = d.wrapSub(d.head, 1)
	d.length++
	return d.bufferWrite(d.head, value)
}

// PushBack is VecDeque::push_back: it appends an element to the back of the
// deque.
func (d *VecDeque[T]) PushBack(value T) {
	d.pushBackMut(value)
}

// PushBackMut is VecDeque::push_back_mut: it appends an element to the back
// of the deque, returning a pointer to it.
func (d *VecDeque[T]) PushBackMut(value T) *T {
	return d.pushBackMut(value)
}

func (d *VecDeque[T]) pushBackMut(value T) *T {
	if d.isFull() {
		d.grow()
	}
	length := d.length
	d.length++
	return d.bufferWrite(d.toWrappedIndex(length), value)
}

func (d VecDeque[T]) isContiguous() bool {
	// Do the calculation like this to avoid overflowing if len + head > usize::MAX
	return d.head <= len(d.buf)-d.length
}

// SwapRemoveFront is VecDeque::swap_remove_front: it removes an element from
// anywhere in the deque and returns it, replacing it with the first element.
// This does not preserve ordering, but is O(1). It returns None if index is
// out of bounds.
func (d *VecDeque[T]) SwapRemoveFront(index int) rs.Option[T] {
	length := d.length
	if index >= 0 && index < length && index != 0 {
		d.swap(index, 0)
	} else if index < 0 || index >= length {
		return rs.None[T]()
	}
	return d.PopFront()
}

// SwapRemoveBack is VecDeque::swap_remove_back: it removes an element from
// anywhere in the deque and returns it, replacing it with the last element.
// This does not preserve ordering, but is O(1). It returns None if index is
// out of bounds.
func (d *VecDeque[T]) SwapRemoveBack(index int) rs.Option[T] {
	length := d.length
	if length > 0 && index >= 0 && index < length-1 {
		d.swap(index, length-1)
	} else if index < 0 || index >= length {
		return rs.None[T]()
	}
	return d.PopBack()
}

// Insert is VecDeque::insert: it inserts an element at index within the
// deque, shifting all elements with indices greater than or equal to index
// towards the back. Element at index 0 is the front of the queue. It panics
// if index is strictly greater than the deque's length.
func (d *VecDeque[T]) Insert(index int, value T) {
	if !(index >= 0 && index <= d.length) {
		rs.PanicAt(1, "index out of bounds")
	}
	d.insertMut(index, value)
}

// InsertMut is VecDeque::insert_mut: as Insert, returning a pointer to the
// inserted element.
func (d *VecDeque[T]) InsertMut(index int, value T) *T {
	if !(index >= 0 && index <= d.length) {
		rs.PanicAt(1, "index out of bounds")
	}
	return d.insertMut(index, value)
}

func (d *VecDeque[T]) insertMut(index int, value T) *T {
	if d.isFull() {
		d.grow()
	}

	k := d.length - index
	if k < index {
		// `index + 1` can't overflow, because if index was usize::MAX, then either the
		// assert would've failed, or the deque would've tried to grow past usize::MAX
		// and panicked.
		// see `remove()` for explanation why this wrap_copy() call is safe.
		d.wrapCopy(d.toWrappedIndex(index), d.toWrappedIndex(index+1), k)
		d.length++
		return d.bufferWrite(d.toWrappedIndex(index), value)
	}
	oldHead := d.head
	d.head = d.wrapSub(d.head, 1)
	d.wrapCopy(oldHead, d.head, index)
	d.length++
	return d.bufferWrite(d.toWrappedIndex(index), value)
}

// Remove is VecDeque::remove: it removes and returns the element at index
// from the deque. Whichever end is closer to the removal point will be
// moved to make room, and all the affected elements will be moved to new
// positions. It returns None if index is out of bounds.
func (d *VecDeque[T]) Remove(index int) rs.Option[T] {
	if index < 0 || d.length <= index {
		return rs.None[T]()
	}

	wrappedIdx := d.toWrappedIndex(index)

	elem := d.buf[wrappedIdx]

	k := d.length - index - 1
	// safety: due to the nature of the if-condition, whichever wrap_copy gets called,
	// its length argument will be at most `self.len / 2`, so there can't be more than
	// one overlapping area.
	if k < index {
		d.wrapCopy(d.wrapAdd(wrappedIdx, 1), wrappedIdx, k)
		d.length--
		d.clearSlots(d.head, d.length, 1)
	} else {
		oldHead := d.head
		d.head = d.toWrappedIndex(1)
		d.wrapCopy(oldHead, d.head, index)
		d.length--
		var zero T
		d.buf[oldHead] = zero
	}

	return rs.Some(elem)
}

// SplitOff is VecDeque::split_off: it splits the deque into two at the given
// index. It returns a newly allocated deque with the elements at and after
// at, and leaves the elements before at in d, with its capacity unchanged.
// Element at index 0 is the front of the queue. It panics if at > len.
func (d *VecDeque[T]) SplitOff(at int) VecDeque[T] {
	length := d.length
	if !(at >= 0 && at <= length) {
		rs.PanicAt(1, "`at` out of bounds")
	}

	otherLen := length - at
	other := VecDequeWithCapacity[T](otherLen)

	if vecDequeElementSize[T]() != 0 {
		firstHalf, secondHalf := d.AsSlices()
		firstLen := len(firstHalf)
		if at < firstLen {
			// `at` lies in the first half.
			amountInFirst := firstLen - at
			copy(other.buf, firstHalf[at:])
			// just take all of the second half.
			copy(other.buf[amountInFirst:], secondHalf)
		} else {
			// `at` lies in the second half, need to factor in the elements we skipped
			// in the first half.
			offset := at - firstLen
			copy(other.buf, secondHalf[offset:])
		}
		d.clearSlots(d.head, at, otherLen)
	} else if otherLen != 0 {
		other.capacity()
	}

	// Cleanup where the ends of the buffers are
	d.length = at
	other.length = otherLen

	return other
}

// Append is VecDeque::append: it moves all the elements of other into d,
// leaving other empty.
func (d *VecDeque[T]) Append(other *VecDeque[T]) {
	if vecDequeElementSize[T]() == 0 {
		length := d.length + other.length
		if length < d.length {
			rs.PanicAt(1, "capacity overflow")
		}
		if length != 0 {
			d.capacity()
		}
		d.length = length
		other.length = 0
		other.head = 0
		return
	}

	if !d.reserve(other.length) {
		rs.PanicAt(1, "capacity overflow")
	}
	left, right := other.AsSlices()
	d.copySlice(d.toWrappedIndex(d.length), left)
	// no overflow, because self.capacity() >= old_cap + left.len() >= self.len + left.len()
	d.copySlice(d.toWrappedIndex(d.length+len(left)), right)
	// SAFETY: Update pointers after copying to avoid leaving doppelganger
	// in case of panics.
	d.length += other.length
	// Now that we own its values, forget everything in `other`.
	clear(left)
	clear(right)
	other.length = 0
	other.head = 0
}

// Retain is VecDeque::retain: it retains only the elements for which f
// returns true, visiting each element exactly once in the original order,
// and preserves the order of the retained elements.
func (d *VecDeque[T]) Retain(f func(*T) bool) {
	d.RetainMut(f)
}

// RetainMut is VecDeque::retain_mut: as Retain. f may change the element
// through the pointer.
func (d *VecDeque[T]) RetainMut(f func(*T) bool) {
	length := d.length
	idx := 0
	cur := 0

	// Stage 1: All values are retained.
	for cur < length {
		if !f(d.index(cur)) {
			cur++
			break
		}
		cur++
		idx++
	}
	// Stage 2: Swap retained value into current idx.
	for cur < length {
		if !f(d.index(cur)) {
			cur++
			continue
		}

		d.swap(idx, cur)
		cur++
		idx++
	}
	// Stage 3: Truncate all values after idx.
	if cur != idx {
		d.Truncate(idx)
	}
}

// grow doubles the buffer size.
func (d *VecDeque[T]) grow() {
	// Extend or possibly remove this assertion when valid use-cases for growing the
	// buffer without it being full emerge
	oldCap := d.capacity()
	if !d.growAmortized(oldCap, 1) {
		rs.PanicAt(3, "capacity overflow")
	}
	d.handleCapacityIncrease(oldCap)
}

// ResizeWith is VecDeque::resize_with: it modifies the deque in-place so
// that its length is equal to newLen, either by removing excess elements
// from the back or by appending elements generated by calling generator to
// the back.
func (d *VecDeque[T]) ResizeWith(newLen int, generator func() T) {
	length := d.length

	if newLen > length {
		if !d.extendTrustedLen(newLen-length, generator) {
			rs.PanicAt(1, "capacity overflow")
		}
	} else {
		d.Truncate(newLen)
	}
}

// extendTrustedLen is spec_extend for a TrustedLen iterator: the space for
// the additional elements is reserved in one step, and next is called once
// for each of them.
func (d *VecDeque[T]) extendTrustedLen(additional int, next func() T) bool {
	if !d.reserve(additional) {
		return false
	}
	// write_iter_wrapping: the length follows the elements written, so a
	// panic in next leaves the ones before it in the deque.
	for written := 0; written < additional; written++ {
		value := next()
		d.bufferWrite(d.toWrappedIndex(d.length), value)
		d.length++
	}
	return true
}

// MakeContiguous is VecDeque::make_contiguous: it rearranges the internal
// storage of the deque so it is one contiguous slice, which is then
// returned. The slice is a view of the buffer. This method does not
// allocate and does not change the order of the inserted elements.
func (d *VecDeque[T]) MakeContiguous() []T {
	if vecDequeElementSize[T]() == 0 {
		d.head = 0
		if d.length != 0 {
			d.capacity()
		}
	}

	if d.isContiguous() {
		return d.buf[d.head : d.head+d.length : d.head+d.length]
	}

	head, length := d.head, d.length
	capacity := d.capacity()

	free := capacity - length
	headLen := capacity - head
	// tail <= head < capacity
	// head cannot be <= capacity, because we know that VecDeque is non-empty, since it is not
	// contiguous at this point
	tail := length - headLen
	tailLen := tail

	if free >= headLen {
		// there is enough free space to copy the head in one go,
		// this means that we first shift the tail backwards, and then
		// copy the head to the correct position.
		//
		// from: DEFGH....ABC
		// to:   ABCDEFGH....
		d.copy(0, headLen, tailLen)
		// ...DEFGH.ABC
		d.copy(head, 0, headLen)
		// ABCDEFGH....
		clear(d.buf[head:])

		d.head = 0
	} else if free >= tailLen {
		// there is enough free space to copy the tail in one go,
		// this means that we first shift the head forwards, and then
		// copy the tail to the correct position.
		//
		// from: FGH....ABCDE
		// to:   ...ABCDEFGH.
		d.copy(head, tail, headLen)
		// FGHABCDE....
		d.copy(0, tail+headLen, tailLen)
		// ...ABCDEFGH.
		clear(d.buf[:tailLen])
		clear(d.buf[max(head, tail+length):])

		d.head = tail
	} else {
		// `free` is smaller than both `head_len` and `tail_len`.
		// the general algorithm for this first moves the slices
		// right next to each other and then uses `slice::rotate`
		// to rotate them into place:
		//
		// initially:   HIJK..ABCDEFG
		// step 1:      ..HIJKABCDEFG
		// step 2:      ..ABCDEFGHIJK
		//
		// or:
		//
		// initially:   FGHIJK..ABCDE
		// step 1:      FGHIJKABCDE..
		// step 2:      ABCDEFGHIJK..

		// pick the shorter of the 2 slices to reduce the amount
		// of memory that needs to be moved around.
		if headLen > tailLen {
			// tail is shorter, so:
			//  1. copy tail forwards
			//  2. rotate used part of the buffer
			//  3. update head to point to the new beginning (which is just `free`)

			// if there is no free space in the buffer, then the slices are already
			// right next to each other and we don't need to move any memory.
			if free != 0 {
				// because we only move the tail forward as much as there's free space
				// behind it, we don't overwrite any elements of the head slice, and
				// the slices end up right next to each other.
				d.copy(0, free, tailLen)
				clear(d.buf[:free])
			}

			// We just copied the tail right next to the head slice,
			// so all of the elements in the range are initialized
			// because the deque wasn't contiguous, we know that `tail_len < self.len == slice.len()`,
			// so this will never panic.
			vecDequeRotateLeft(d.buf[free:], tailLen)

			// the used part of the buffer now is `free..self.capacity()`, so set
			// `head` to the beginning of that range.
			d.head = free
		} else {
			// head is shorter so:
			//  1. copy head backwards
			//  2. rotate used part of the buffer
			//  3. update head to point to the new beginning (which is the beginning of the buffer)

			// if there is no free space in the buffer, then the slices are already
			// right next to each other and we don't need to move any memory.
			if free != 0 {
				// copy the head slice to lie right behind the tail slice.
				d.copy(d.head, tailLen, headLen)
				clear(d.buf[length:])
			}

			// because we copied the head slice so that both slices lie right
			// next to each other, all the elements in the range are initialized.
			// because the deque wasn't contiguous, we know that `head_len < self.len == slice.len()`
			// so this will never panic.
			vecDequeRotateLeft(d.buf[:length], length-headLen)

			// the used part of the buffer now is `0..self.len`, so set
			// `head` to the beginning of that range.
			d.head = 0
		}
	}

	return d.buf[d.head : d.head+d.length : d.head+d.length]
}

// vecDequeRotateLeft is slice::rotate_left: the first mid elements move to
// the end. (rotate_right(k) is rotate_left(len - k).)
func vecDequeRotateLeft[T any](s []T, mid int) {
	slices.Reverse(s[:mid])
	slices.Reverse(s[mid:])
	slices.Reverse(s)
}

// RotateLeft is VecDeque::rotate_left: it rotates the double-ended queue n
// places to the left. Equivalently, it rotates item n into the first
// position, pops the first n items and pushes them to the end, or rotates
// len() - n places to the right. It panics if n is greater than len().
func (d *VecDeque[T]) RotateLeft(n int) {
	if !(n >= 0 && n <= d.length) {
		rs.PanicAt(1, "assertion failed: n <= self.len()")
	}
	k := d.length - n
	if n <= k {
		d.rotateLeftInner(n)
	} else {
		d.rotateRightInner(k)
	}
}

// RotateRight is VecDeque::rotate_right: it rotates the double-ended queue
// n places to the right. Equivalently, it rotates the first item into
// position n, pops the last n items and pushes them to the front, or
// rotates len() - n places to the left. It panics if n is greater than
// len().
func (d *VecDeque[T]) RotateRight(n int) {
	if !(n >= 0 && n <= d.length) {
		rs.PanicAt(1, "assertion failed: n <= self.len()")
	}
	k := d.length - n
	if n <= k {
		d.rotateRightInner(n)
	} else {
		d.rotateLeftInner(k)
	}
}

// SAFETY: the following two methods require that the rotation amount
// be less than half the length of the deque.
//
// `wrap_copy` requires that `min(x, capacity() - x) + copy_len <= capacity()`,
// but then `min` is never more than half the capacity, regardless of x,
// so it's sound to call here because we're calling with something
// less than half the length, which is never above half the capacity.

func (d *VecDeque[T]) rotateLeftInner(mid int) {
	if mid == 0 {
		return
	}
	d.capacity()
	d.wrapCopy(d.head, d.toWrappedIndex(d.length), mid)
	d.head = d.toWrappedIndex(mid)
	// The slots just before the new head that the elements left and that
	// are not in use again.
	vacated := min(mid, len(d.buf)-d.length)
	d.clearSlots(d.wrapSub(d.head, vacated), 0, vacated)
}

func (d *VecDeque[T]) rotateRightInner(k int) {
	if k == 0 {
		return
	}
	d.capacity()
	d.head = d.wrapSub(d.head, k)
	d.wrapCopy(d.toWrappedIndex(d.length), d.head, k)
	// The slots just after the new end that the elements left and that are
	// not in use again.
	vacated := min(k, len(d.buf)-d.length)
	d.clearSlots(d.head, d.length, vacated)
}

// BinarySearch is VecDeque::binary_search: it binary searches the sorted
// deque for a given element, comparing by rs.Cmp. If the value is found the
// results are the index of the matching element and true (`Ok(index)`); if
// there are multiple matches, then any one of the matches could be
// returned. If the value is not found the results are the index where a
// matching element could be inserted while maintaining sorted order, and
// false (`Err(index)`).
func (d VecDeque[T]) BinarySearch(x T) (int, bool) {
	return d.BinarySearchBy(func(element *T) int { return rs.Cmp(*element, x) })
}

// BinarySearchBy is VecDeque::binary_search_by: it binary searches the
// sorted deque with a comparator function, which returns a negative number,
// zero or a positive number as its argument is Less, Equal or Greater than
// the desired target. The results are as for BinarySearch.
func (d VecDeque[T]) BinarySearchBy(f func(*T) int) (int, bool) {
	front, back := d.AsSlices()
	if len(back) > 0 {
		cmpBack := f(&back[0])
		if cmpBack == 0 {
			return len(front), true
		}
		if cmpBack < 0 {
			index, found := vecDequeSliceBinarySearchBy(back, f)
			return index + len(front), found
		}
	}
	return vecDequeSliceBinarySearchBy(front, f)
}

// vecDequeSliceBinarySearchBy is <[T]>::binary_search_by in core's
// slice/mod.rs. Which of several matches it finds is part of what it does.
func vecDequeSliceBinarySearchBy[T any](s []T, f func(*T) int) (int, bool) {
	size := len(s)
	if size == 0 {
		return 0, false
	}
	base := 0

	// This loop intentionally doesn't have an early exit if the comparison
	// returns Equal. We want the number of loop iterations to depend *only*
	// on the size of the input slice so that the CPU can reliably predict
	// the loop count.
	for size > 1 {
		half := size / 2
		mid := base + half

		cmp := f(&s[mid])
		if cmp <= 0 {
			base = mid
		}

		// This is imprecise in the case where `size` is odd and the
		// comparison returns Greater: the mid element still gets included
		// by `size` even though it's known to be larger than the element
		// being searched for.
		//
		// This is fine though: we gain more performance by keeping the
		// loop iteration count invariant (and thus predictable) than we
		// lose from considering one additional element.
		size -= half
	}

	cmp := f(&s[base])
	if cmp == 0 {
		return base, true
	}
	if cmp < 0 {
		return base + 1, false
	}
	return base, false
}

// VecDequeBinarySearchByKey is VecDeque::binary_search_by_key: it binary
// searches the sorted deque with a key extraction function, comparing keys
// by rs.Cmp. The results are as for BinarySearch. It is a function because
// a Go method cannot introduce the key's type.
func VecDequeBinarySearchByKey[T, B any](d *VecDeque[T], b B, f func(*T) B) (int, bool) {
	return d.BinarySearchBy(func(k *T) int { return rs.Cmp(f(k), b) })
}

// PartitionPoint is VecDeque::partition_point: the index of the partition
// point according to the given predicate (the index of the first element of
// the second partition). The deque is assumed to be partitioned according
// to the given predicate.
func (d VecDeque[T]) PartitionPoint(pred func(*T) bool) int {
	front, back := d.AsSlices()

	if len(back) > 0 && pred(&back[0]) {
		return vecDequeSlicePartitionPoint(back, pred) + len(front)
	}
	return vecDequeSlicePartitionPoint(front, pred)
}

func vecDequeSlicePartitionPoint[T any](s []T, pred func(*T) bool) int {
	index, _ := vecDequeSliceBinarySearchBy(s, func(x *T) int {
		if pred(x) {
			return -1
		}
		return 1
	})
	return index
}

// Resize is VecDeque::resize: it modifies the deque in-place so that its
// length is equal to newLen, either by removing excess elements from the
// back or by appending clones (rs.Clone) of value to the back.
func (d *VecDeque[T]) Resize(newLen int, value T) {
	if !d.resize(newLen, value, rs.Clone[T]) {
		rs.PanicAt(1, "capacity overflow")
	}
}

func (d *VecDeque[T]) resize(newLen int, value T, clone func(T) T) bool {
	if newLen > d.length {
		extra := newLen - d.length
		// repeat_n clones the value for every element but the last, which
		// is the value itself.
		remaining := extra
		return d.extendTrustedLen(extra, func() T {
			remaining--
			if remaining == 0 {
				return value
			}
			return clone(value)
		})
	}
	d.Truncate(newLen)
	return true
}

// Clone is VecDeque::clone: a deque with a clone (rs.Clone) of every
// element.
func (d VecDeque[T]) Clone() VecDeque[T] {
	return d.cloneWith(rs.Clone[T])
}

func (d VecDeque[T]) cloneWith(clone func(T) T) VecDeque[T] {
	deq := VecDequeWithCapacity[T](d.length)
	a, b := d.AsSlices()
	index := 0
	deq.extendTrustedLen(d.length, func() T {
		var element T
		if index < len(a) {
			element = a[index]
		} else {
			element = b[index-len(a)]
		}
		index++
		return clone(element)
	})
	return deq
}

// Equal is VecDeque's PartialEq: the same length and equal elements (by
// rs.Eq) in order.
func (d VecDeque[T]) Equal(other *VecDeque[T]) bool {
	if d.length != other.length {
		return false
	}
	sa, sb := d.AsSlices()
	oa, ob := other.AsSlices()
	if len(sa) == len(oa) {
		return vecDequeSliceEq(sa, oa) && vecDequeSliceEq(sb, ob)
	} else if len(sa) < len(oa) {
		// Always divisible in three sections, for example:
		// self:  [a b c|d e f]
		// other: [0 1 2 3|4 5]
		// front = 3, mid = 1,
		// [a b c] == [0 1 2] && [d] == [3] && [e f] == [4 5]
		front := len(sa)
		mid := len(oa) - front

		oaFront, oaMid := oa[:front], oa[front:]
		sbMid, sbBack := sb[:mid], sb[mid:]
		return vecDequeSliceEq(sa, oaFront) && vecDequeSliceEq(sbMid, oaMid) && vecDequeSliceEq(sbBack, ob)
	}
	front := len(oa)
	mid := len(sa) - front

	saFront, saMid := sa[:front], sa[front:]
	obMid, obBack := ob[:mid], ob[mid:]
	return vecDequeSliceEq(saFront, oa) && vecDequeSliceEq(saMid, obMid) && vecDequeSliceEq(sb, obBack)
}

// EqualSlice is `deque == vec`, `deque == [a, b, ..]` and `deque ==
// &slice[..]` (VecDeque's PartialEq with Vec, arrays and slices): the same
// length and equal elements (by rs.Eq) in order.
func (d VecDeque[T]) EqualSlice(other []T) bool {
	if d.length != len(other) {
		return false
	}
	sa, sb := d.AsSlices()
	oa, ob := other[:len(sa)], other[len(sa):]
	return vecDequeSliceEq(sa, oa) && vecDequeSliceEq(sb, ob)
}

func vecDequeSliceEq[T any](a, b []T) bool {
	if len(a) != len(b) {
		return false
	}
	for index := range a {
		if !rs.Eq(a[index], b[index]) {
			return false
		}
	}
	return true
}

// Compare is VecDeque's Ord: the elements compared in order by rs.Cmp, and
// the shorter deque first when one is a prefix of the other.
func (d VecDeque[T]) Compare(other *VecDeque[T]) int {
	for index := 0; index < d.length && index < other.length; index++ {
		if order := rs.Cmp(*d.index(index), *other.index(index)); order != 0 {
			return order
		}
	}
	switch {
	case d.length < other.length:
		return -1
	case d.length > other.length:
		return 1
	default:
		return 0
	}
}

// Index is `deque[index]`: a pointer to the element at index. It panics as
// Rust does when the index is out of bounds.
func (d VecDeque[T]) Index(index int) *T {
	if !(index >= 0 && index < d.length) {
		rs.PanicAt(1, "out of bounds access")
	}
	return d.index(index)
}

// IndexMut is `deque[index]` on the left of an assignment. It is the same
// pointer Index returns.
func (d *VecDeque[T]) IndexMut(index int) *T {
	if !(index >= 0 && index < d.length) {
		rs.PanicAt(1, "out of bounds access")
	}
	return d.index(index)
}

func (d VecDeque[T]) index(index int) *T {
	return &d.buf[d.toWrappedIndex(index)]
}

// IntoIter is VecDeque::into_iter (`for x in deque`): the elements front to
// back, for a deque that is not used again.
func (d VecDeque[T]) IntoIter() iter.Seq[T] {
	return func(yield func(T) bool) {
		a, b := d.AsSlices()
		vecDequeForward(a, b, func(element *T) bool { return yield(*element) })
	}
}

// IntoIterRev is `deque.into_iter().rev()`.
func (d VecDeque[T]) IntoIterRev() iter.Seq[T] {
	return func(yield func(T) bool) {
		a, b := d.AsSlices()
		vecDequeBackward(a, b, func(element *T) bool { return yield(*element) })
	}
}

// Extend is VecDeque::extend: it appends every element of seq to the back,
// as PushBack does.
func (d *VecDeque[T]) Extend(seq iter.Seq[T]) {
	// This function should be the moral equivalent of:
	//
	// for item in iter {
	//     self.push_back(item);
	// }
	//
	// For an iterator whose size hint has a lower bound of zero, Rust's
	// spec_extend reserves one slot whenever the deque is full, which is
	// what PushBack does.
	for element := range seq {
		if d.isFull() {
			d.grow()
		}
		d.bufferWrite(d.toWrappedIndex(d.length), element)
		d.length++
	}
}

// The methods below describe the deque to rs's formatter, structural clone
// and comparison, and to go/crates/serde (rs.ReflectSeq).

// ReflectLen returns the number of elements.
func (d *VecDeque[T]) ReflectLen() int { return d.length }

// ReflectElemType returns the type T.
func (d *VecDeque[T]) ReflectElemType() reflect.Type { return reflect.TypeFor[T]() }

// ReflectElements calls yield with a pointer to each element, front to back.
func (d *VecDeque[T]) ReflectElements(yield func(element any) bool) {
	a, b := d.AsSlices()
	vecDequeForward(a, b, func(element *T) bool { return yield(element) })
}

// ReflectPush appends element (a T), as PushBack does.
func (d *VecDeque[T]) ReflectPush(element any) {
	d.PushBack(reflectCast[T](element))
}

// ReflectIsSet reports that the collection is a sequence, not a set.
func (d *VecDeque[T]) ReflectIsSet() bool { return false }

// ReflectSorted reports that the order of iteration is the order to write
// the elements in: a deque is a sequence, and its order is its content.
func (d *VecDeque[T]) ReflectSorted() bool { return true }

var _ rs.ReflectSeq = (*VecDeque[int])(nil)
