// Port of similar 2.7.0 src/iter.rs

// The various iterators this crate provides.
//
// These iterators are not a very stable interface and you really should
// avoid considering them to be concrete types.  A lot of the iterators in
// this crate use `impl Iterator` for this reason but restrictions in the
// language don't allow this to be used in all places on the versions of
// rust this crate wants to compile for.

package similar

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Iterator for [`DiffOp::iter_changes`].
type IterChangesIter[T any] struct {
	old      []T
	new      []T
	oldRange Range
	newRange Range
	oldIndex int
	newIndex int
	oldI     int
	newI     int
	tag      DiffTag
}

func iterChangesIterNew[T any](old []T, new_ []T, op DiffOp) *IterChangesIter[T] {
	tag, oldRange, newRange := op.AsTagTuple()
	oldIndex := oldRange.Start
	newIndex := newRange.Start
	oldI := oldRange.Start
	newI := newRange.Start
	return &IterChangesIter[T]{
		old:      old,
		new:      new_,
		oldRange: oldRange,
		newRange: newRange,
		oldIndex: oldIndex,
		newIndex: newIndex,
		oldI:     oldI,
		newI:     newI,
		tag:      tag,
	}
}

// Next is `Iterator::next`.
func (self *IterChangesIter[T]) Next() rs.Option[Change[T]] {
	switch self.tag {
	case DiffTag_Equal:
		if self.oldI < self.oldRange.End {
			value := algorithmsUtilsIndex(self.old, self.oldI)
			self.oldI += 1
			self.oldIndex += 1
			self.newIndex += 1
			return rs.Some(Change[T]{
				tag:      ChangeTag_Equal,
				oldIndex: rs.Some(self.oldIndex - 1),
				newIndex: rs.Some(self.newIndex - 1),
				value:    value,
			})
		} else {
			return rs.None[Change[T]]()
		}
	case DiffTag_Delete:
		if self.oldI < self.oldRange.End {
			value := algorithmsUtilsIndex(self.old, self.oldI)
			self.oldI += 1
			self.oldIndex += 1
			return rs.Some(Change[T]{
				tag:      ChangeTag_Delete,
				oldIndex: rs.Some(self.oldIndex - 1),
				newIndex: rs.None[int](),
				value:    value,
			})
		} else {
			return rs.None[Change[T]]()
		}
	case DiffTag_Insert:
		if self.newI < self.newRange.End {
			value := algorithmsUtilsIndex(self.new, self.newI)
			self.newI += 1
			self.newIndex += 1
			return rs.Some(Change[T]{
				tag:      ChangeTag_Insert,
				oldIndex: rs.None[int](),
				newIndex: rs.Some(self.newIndex - 1),
				value:    value,
			})
		} else {
			return rs.None[Change[T]]()
		}
	case DiffTag_Replace:
		if self.oldI < self.oldRange.End {
			value := algorithmsUtilsIndex(self.old, self.oldI)
			self.oldI += 1
			self.oldIndex += 1
			return rs.Some(Change[T]{
				tag:      ChangeTag_Delete,
				oldIndex: rs.Some(self.oldIndex - 1),
				newIndex: rs.None[int](),
				value:    value,
			})
		} else if self.newI < self.newRange.End {
			value := algorithmsUtilsIndex(self.new, self.newI)
			self.newI += 1
			self.newIndex += 1
			return rs.Some(Change[T]{
				tag:      ChangeTag_Insert,
				oldIndex: rs.None[int](),
				newIndex: rs.Some(self.newIndex - 1),
				value:    value,
			})
		} else {
			return rs.None[Change[T]]()
		}
	default:
		panic(rs.Unreachable(self.tag))
	}
}

// All returns the rest of the iterator as a sequence to range over. It is
// what a Rust `for change in iter` loop does.
func (self *IterChangesIter[T]) All() iter.Seq[Change[T]] {
	return func(yield func(Change[T]) bool) {
		for {
			change, ok := self.Next().Get()
			if !ok || !yield(change) {
				return
			}
		}
	}
}

// Collect is `Iterator::collect::<Vec<_>>()`: the rest of the iterator.
func (self *IterChangesIter[T]) Collect() []Change[T] {
	rv := []Change[T]{}
	for change := range self.All() {
		rv = append(rv, change)
	}
	return rv
}

// Iterator for [`TextDiff::iter_all_changes`](crate::TextDiff::iter_all_changes).
type IterAllChangesIter[T any] struct {
	old         []T
	new         []T
	ops         []DiffOp
	currentIter rs.Option[*IterChangesIter[T]]
}

func iterAllChangesIterNew[T any](old []T, new_ []T, ops []DiffOp) *IterAllChangesIter[T] {
	return &IterAllChangesIter[T]{
		old:         old,
		new:         new_,
		ops:         ops,
		currentIter: rs.None[*IterChangesIter[T]](),
	}
}

// Next is `Iterator::next`.
func (self *IterAllChangesIter[T]) Next() rs.Option[Change[T]] {
	for {
		if iter_, ok := self.currentIter.Get(); ok {
			if rv := iter_.Next(); rv.IsSome() {
				return rv
			}
			self.currentIter.Take()
		}
		if len(self.ops) > 0 {
			first, rest := self.ops[0], self.ops[1:]
			self.currentIter = rs.Some(iterChangesIterNew(self.old, self.new, first))
			self.ops = rest
		} else {
			return rs.None[Change[T]]()
		}
	}
}

// All returns the rest of the iterator as a sequence to range over. It is
// what a Rust `for change in iter` loop does.
func (self *IterAllChangesIter[T]) All() iter.Seq[Change[T]] {
	return func(yield func(Change[T]) bool) {
		for {
			change, ok := self.Next().Get()
			if !ok || !yield(change) {
				return
			}
		}
	}
}

// Collect is `Iterator::collect::<Vec<_>>()`: the rest of the iterator.
func (self *IterAllChangesIter[T]) Collect() []Change[T] {
	rv := []Change[T]{}
	for change := range self.All() {
		rv = append(rv, change)
	}
	return rv
}
