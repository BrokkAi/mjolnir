// Port of alloc 1.99.1 src/vec/mod.rs (the methods of Vec<T> that change
// its length)

package rs

import (
	"reflect"
	"strconv"
)

// Vec methods.
//
// A Vec is a Go slice. A method that changes the length takes a pointer to
// the slice: `v.retain(|x| ..)` is rs.Retain(&v, func(x *T) bool { .. }),
// `v.remove(i)` is rs.Remove(&v, i). Removed elements are zeroed in the
// backing array, so nothing keeps them alive, as Rust drops them.
//
// Where Rust panics, these panic with Rust's message.

// sliceZeroTail zeroes s[from:cap], the elements a shortened slice no longer
// holds.
func sliceZeroTail[T any](s []T, from int) {
	clear(s[from:])
}

// Push is `v.push(value)`.
func Push[T any](v *[]T, value T) {
	*v = append(*v, value)
}

// Pop is `v.pop()`: the last element, removed, or None.
func Pop[T any](v *[]T) Option[T] {
	s := *v
	if len(s) == 0 {
		return None[T]()
	}
	last := s[len(s)-1]
	sliceZeroTail(s, len(s)-1)
	*v = s[:len(s)-1]
	return Some(last)
}

// PopIf is `v.pop_if(predicate)`: the last element, removed, when predicate
// holds for it.
func PopIf[T any](v *[]T, predicate func(*T) bool) Option[T] {
	s := *v
	if len(s) == 0 || !predicate(&s[len(s)-1]) {
		return None[T]()
	}
	return Pop(v)
}

// Truncate is `v.truncate(n)`: it keeps the first n elements, and does
// nothing when there are not more than n.
func Truncate[T any](v *[]T, n int) {
	s := *v
	if n < len(s) {
		sliceZeroTail(s, n)
		*v = s[:n]
	}
}

// Insert is `v.insert(index, element)`. It panics when index is past the
// end.
func Insert[T any](v *[]T, index int, element T) {
	s := *v
	if index < 0 || index > len(s) {
		PanicAt(1, "insertion index (is "+strconv.Itoa(index)+") should be <= len (is "+strconv.Itoa(len(s))+")")
	}
	var zero T
	s = append(s, zero)
	copy(s[index+1:], s[index:])
	s[index] = element
	*v = s
}

// Remove is `v.remove(index)`: the element at index, removed, the ones
// after it moving down. It panics when index is out of range.
func Remove[T any](v *[]T, index int) T {
	s := *v
	if index < 0 || index >= len(s) {
		PanicAt(1, "removal index (is "+strconv.Itoa(index)+") should be < len (is "+strconv.Itoa(len(s))+")")
	}
	removed := s[index]
	copy(s[index:], s[index+1:])
	sliceZeroTail(s, len(s)-1)
	*v = s[:len(s)-1]
	return removed
}

// SwapRemove is `v.swap_remove(index)`: the element at index, removed, the
// last element taking its place. It panics when index is out of range.
func SwapRemove[T any](v *[]T, index int) T {
	s := *v
	if index < 0 || index >= len(s) {
		PanicAt(1, "swap_remove index (is "+strconv.Itoa(index)+") should be < len (is "+strconv.Itoa(len(s))+")")
	}
	removed := s[index]
	s[index] = s[len(s)-1]
	sliceZeroTail(s, len(s)-1)
	*v = s[:len(s)-1]
	return removed
}

// SplitOff is `v.split_off(at)`: the elements from at on, removed and
// returned in a new slice. It panics when at is past the end.
func SplitOff[T any](v *[]T, at int) []T {
	s := *v
	if at < 0 || at > len(s) {
		PanicAt(1, "`at` split index (is "+strconv.Itoa(at)+") should be <= len (is "+strconv.Itoa(len(s))+")")
	}
	other := make([]T, len(s)-at)
	copy(other, s[at:])
	sliceZeroTail(s, at)
	*v = s[:at]
	return other
}

// Drain is `v.drain(start..end).collect()`: the elements in the range,
// removed and returned in order. Other ranges: `..` is (0, len(v)), `a..`
// is (a, len(v)), `..=b` is (0, b+1). It panics as Rust does when the range
// does not fit.
func Drain[T any](v *[]T, start, end int) []T {
	s := *v
	sliceRange(1, start, end, len(s))
	drained := make([]T, end-start)
	copy(drained, s[start:end])
	n := copy(s[start:], s[end:])
	sliceZeroTail(s, start+n)
	*v = s[:start+n]
	return drained
}

// Append is `v.append(&mut other)`: every element of other moves to the end
// of v, and other is left empty.
func Append[T any](v, other *[]T) {
	*v = append(*v, *other...)
	sliceZeroTail(*other, 0)
	*other = (*other)[:0]
}

// Extend is `v.extend(items)` for items that are moved in: Go's append.
func Extend[T any](v *[]T, items []T) {
	*v = append(*v, items...)
}

// ExtendFromSlice is `v.extend_from_slice(other)`: a Clone of each element
// of other, in order.
func ExtendFromSlice[T any](v *[]T, other []T) {
	n := len(*v)
	*v = append(*v, other...)
	sliceCloneElements((*v)[n:])
}

// Resize is `v.resize(n, value)`: it truncates to n, or grows to n with
// Clones of value and value itself last.
func Resize[T any](v *[]T, n int, value T) {
	s := *v
	if n <= len(s) {
		Truncate(v, n)
		return
	}
	extra := n - len(s)
	plain := sliceElementsArePlain(reflect.TypeFor[T]())
	for range extra - 1 {
		if plain {
			s = append(s, value)
		} else {
			s = append(s, Clone(value))
		}
	}
	*v = append(s, value)
}

// ResizeWith is `v.resize_with(n, f)`: it truncates to n, or grows to n
// with the results of f, called once for each new element in order.
func ResizeWith[T any](v *[]T, n int, f func() T) {
	s := *v
	if n <= len(s) {
		Truncate(v, n)
		return
	}
	for len(s) < n {
		s = append(s, f())
	}
	*v = s
}

// Retain is `v.retain(f)`: it keeps the elements for which f holds, in
// order. f sees each element once, in order. Should f panic, the elements
// it has not seen stay in v after the ones kept so far, as in Rust.
func Retain[T any](v *[]T, f func(*T) bool) {
	RetainMut(v, f)
}

// RetainMut is `v.retain_mut(f)`; f may change the elements it keeps.
func RetainMut[T any](v *[]T, f func(*T) bool) {
	s := *v
	originalLen := len(s)

	if originalLen == 0 {
		return
	}

	read := 0
	for {
		if !f(&s[read]) {
			break
		}
		read++
		if read == originalLen {
			return
		}
	}

	read, write := read+1, read
	defer func() {
		// When f panics: the elements not yet looked at follow the kept
		// ones.
		if read < originalLen {
			remaining := originalLen - read
			copy(s[write:], s[read:originalLen])
			sliceZeroTail(s, write+remaining)
			*v = s[:write+remaining]
		}
	}()

	for read < originalLen {
		current := &s[read]
		if !f(current) {
			read++
		} else {
			s[write] = *current
			write++
			read++
		}
	}

	sliceZeroTail(s, write)
	*v = s[:write]
}

// Dedup is `v.dedup()`: it removes each element equal (Eq) to the one kept
// before it.
func Dedup[T any](v *[]T) {
	DedupBy(v, func(a, b *T) bool { return Eq(*a, *b) })
}

// DedupByKey is `v.dedup_by_key(key)`: it removes each element whose key
// equals (Eq) the key of the one kept before it. key is called on the
// element, then on the one kept before it.
func DedupByKey[T, K any](v *[]T, key func(*T) K) {
	DedupBy(v, func(a, b *T) bool { return Eq(key(a), key(b)) })
}

// DedupBy is `v.dedup_by(sameBucket)`: it removes each element for which
// sameBucket(element, keptBefore) holds. The arguments come in the opposite
// order from their order in the slice, as in Rust. Should sameBucket panic,
// the elements it has not seen stay after the ones kept so far.
func DedupBy[T any](v *[]T, sameBucket func(a, b *T) bool) {
	s := *v
	n := len(s)
	if n <= 1 {
		return
	}

	firstDuplicateIdx := 1
	for firstDuplicateIdx != n {
		if sameBucket(&s[firstDuplicateIdx], &s[firstDuplicateIdx-1]) {
			break
		}
		firstDuplicateIdx++
	}
	if firstDuplicateIdx == n {
		return
	}

	read, write := firstDuplicateIdx+1, firstDuplicateIdx
	done := false
	defer func() {
		// This code gets executed when `same_bucket` panics.
		if !done {
			itemsLeft := n - read
			copy(s[write:], s[read:])
			sliceZeroTail(s, write+itemsLeft)
			*v = s[:write+itemsLeft]
		}
	}()

	for read < n {
		if sameBucket(&s[read], &s[write-1]) {
			read++
		} else {
			s[write] = s[read]
			write++
			read++
		}
	}

	done = true
	sliceZeroTail(s, write)
	*v = s[:write]
}
