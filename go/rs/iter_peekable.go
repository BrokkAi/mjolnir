// Port of core 1.99.1 src/iter/adapters/peekable.rs and src/slice/iter.rs
// (the iterator over a slice)

package rs

import "iter"

// Iterator is Rust's Iterator trait as the port's iterator types implement
// it: Next returns the next item, or None when the iterator is exhausted.
type Iterator[T any] interface {
	Next() Option[T]
}

// Collect is `it.collect::<Vec<_>>()`: every item the iterator yields, in
// order.
func Collect[T any](it Iterator[T]) []T {
	out := []T{}
	for {
		item, ok := it.Next().Get()
		if !ok {
			return out
		}
		out = append(out, item)
	}
}

// IterSeq turns an Iterator into a Go sequence, for `for item := range
// rs.IterSeq(it)`, which is `for item in it`. Breaking out of the loop
// leaves the remaining items in the iterator.
func IterSeq[T any](it Iterator[T]) iter.Seq[T] {
	return func(yield func(T) bool) {
		for {
			item, ok := it.Next().Get()
			if !ok || !yield(item) {
				return
			}
		}
	}
}

// SliceIter is core::slice::Iter (and vec::IntoIter): the iterator over a
// slice's elements, from either end. `v.iter()` and `v.into_iter()` are
// rs.SliceIterOf(v).
type SliceIter[T any] struct {
	rest []T
}

// SliceIterOf is `v.iter()`.
func SliceIterOf[T any](v []T) *SliceIter[T] {
	return &SliceIter[T]{rest: v}
}

// Next is Iterator::next.
func (it *SliceIter[T]) Next() Option[T] {
	if len(it.rest) == 0 {
		return None[T]()
	}
	item := it.rest[0]
	it.rest = it.rest[1:]
	return Some(item)
}

// NextBack is DoubleEndedIterator::next_back.
func (it *SliceIter[T]) NextBack() Option[T] {
	if len(it.rest) == 0 {
		return None[T]()
	}
	item := it.rest[len(it.rest)-1]
	it.rest = it.rest[:len(it.rest)-1]
	return Some(item)
}

// Len is ExactSizeIterator::len: how many items are left.
func (it *SliceIter[T]) Len() int { return len(it.rest) }

// AsSlice is Iter::as_slice: the items that are left.
func (it *SliceIter[T]) AsSlice() []T { return it.rest }

// Peekable is core::iter::Peekable: an iterator that can look at its next
// item without taking it. `v.iter().peekable()` is rs.PeekableOf(v), and
// `it.peekable()` for another iterator rs.PeekableFrom(it).
type Peekable[T any] struct {
	iter Iterator[T]
	// Remember a peeked value, even if it was None.
	peeked Option[Option[T]]
}

// PeekableOf is `v.iter().peekable()`.
func PeekableOf[T any](v []T) *Peekable[T] {
	return &Peekable[T]{iter: SliceIterOf(v)}
}

// PeekableFrom is `it.peekable()`.
func PeekableFrom[T any](it Iterator[T]) *Peekable[T] {
	return &Peekable[T]{iter: it}
}

// Next is Iterator::next.
func (p *Peekable[T]) Next() Option[T] {
	if peeked, ok := p.peeked.Take().Get(); ok {
		return peeked
	}
	return p.iter.Next()
}

// Peek is Peekable::peek: the next item, without advancing the iterator.
func (p *Peekable[T]) Peek() Option[T] {
	if p.peeked.IsNone() {
		p.peeked = Some(p.iter.Next())
	}
	peeked, _ := p.peeked.Get()
	return peeked
}

// PeekMut is Peekable::peek_mut: a pointer to the next item, which a write
// through changes, and whether there is one.
func (p *Peekable[T]) PeekMut() (*T, bool) {
	p.Peek()
	inner := p.peeked.AsRef()
	if value := inner.AsRef(); value != nil {
		return value, true
	}
	return nil, false
}

// NextIf is Peekable::next_if: the next item when f holds for it.
func (p *Peekable[T]) NextIf(f func(T) bool) Option[T] {
	var next Option[T]
	if peeked, ok := p.peeked.Take().Get(); ok {
		next = peeked
	} else {
		next = p.iter.Next()
	}
	if matched, ok := next.Get(); ok && f(matched) {
		return Some(matched)
	}
	// Since we called `self.next()`, we consumed `self.peeked`.
	p.peeked = Some(next)
	return None[T]()
}

// NextIfEq is Peekable::next_if_eq: the next item when it equals (Eq)
// expected.
func (p *Peekable[T]) NextIfEq(expected T) Option[T] {
	return p.NextIf(func(next T) bool { return Eq(next, expected) })
}

// Count is Iterator::count: how many items are left, consuming them.
func (p *Peekable[T]) Count() int {
	n := 0
	for p.Next().IsSome() {
		n++
	}
	return n
}
