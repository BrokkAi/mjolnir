// Port of similar 2.7.0 src/algorithms/patience.rs

// Patience diff algorithm.
//
// * time: `O(N log N + M log M + (N+M)D)`
// * space: `O(N+M)`
//
// Tends to give more human-readable outputs. See [Bram Cohen's blog
// post](https://bramcohen.livejournal.com/73318.html) describing it.
//
// This is based on the patience implementation of [pijul](https://pijul.org/)
// by Pierre-Étienne Meunier.

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// Patience diff algorithm.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
func AlgorithmsPatienceDiff[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
) error {
	return AlgorithmsPatienceDiffDeadline(d, old, oldRange, new_, newRange, rs.None[rstime.Instant]())
}

// Patience diff algorithm with deadline.
//
// Diff `old`, between indices `old_range` and `new` between indices `new_range`.
//
// This diff is done with an optional deadline that defines the maximal
// execution time permitted before it bails and falls back to an approximation.
func AlgorithmsPatienceDiffDeadline[T comparable](
	d AlgorithmsDiffHook,
	old []T,
	oldRange Range,
	new_ []T,
	newRange Range,
	deadline rs.Option[rstime.Instant],
) error {
	oldIndexes := algorithmsUtilsUnique(old, oldRange)
	newIndexes := algorithmsUtilsUnique(new_, newRange)

	d2 := AlgorithmsReplaceNew(&algorithmsPatiencePatience[T]{
		d:          d,
		old:        old,
		oldCurrent: oldRange.Start,
		oldEnd:     oldRange.End,
		oldIndexes: oldIndexes,
		new:        new_,
		newCurrent: newRange.Start,
		newEnd:     newRange.End,
		newIndexes: newIndexes,
		deadline:   deadline,
	})
	// Two unique items compare as their values do, so the diff over the
	// unique items is a diff over their values.
	oldValues := make([]T, len(oldIndexes))
	for i, item := range oldIndexes {
		oldValues[i] = item.value()
	}
	newValues := make([]T, len(newIndexes))
	for i, item := range newIndexes {
		newValues[i] = item.value()
	}
	if err := AlgorithmsMyersDiffDeadline(
		d2,
		oldValues,
		Range{Start: 0, End: len(oldIndexes)},
		newValues,
		Range{Start: 0, End: len(newIndexes)},
		deadline,
	); err != nil {
		return err
	}
	return nil
}

type algorithmsPatiencePatience[T comparable] struct {
	d          AlgorithmsDiffHook
	old        []T
	oldCurrent int
	oldEnd     int
	oldIndexes []algorithmsUtilsUniqueItem[T]
	new        []T
	newCurrent int
	newEnd     int
	newIndexes []algorithmsUtilsUniqueItem[T]
	deadline   rs.Option[rstime.Instant]
}

func (self *algorithmsPatiencePatience[T]) Equal(old int, new_ int, len_ int) error {
	// for (old, new) in (old..old + len).zip(new..new + len)
	oldEnd, newEnd := old+len_, new_+len_
	for ; old < oldEnd && new_ < newEnd; old, new_ = old+1, new_+1 {
		a0 := self.oldCurrent
		b0 := self.newCurrent
		for self.oldCurrent < algorithmsUtilsIndex(self.oldIndexes, old).originalIndex() &&
			self.newCurrent < algorithmsUtilsIndex(self.newIndexes, new_).originalIndex() &&
			algorithmsUtilsIndex(self.new, self.newCurrent) == algorithmsUtilsIndex(self.old, self.oldCurrent) {
			self.oldCurrent += 1
			self.newCurrent += 1
		}
		if self.oldCurrent > a0 {
			if err := self.d.Equal(a0, b0, self.oldCurrent-a0); err != nil {
				return err
			}
		}
		noFinishD := AlgorithmsNoFinishHookNew(self.d)
		if err := AlgorithmsMyersDiffDeadline(
			noFinishD,
			self.old,
			Range{Start: self.oldCurrent, End: algorithmsUtilsIndex(self.oldIndexes, old).originalIndex()},
			self.new,
			Range{Start: self.newCurrent, End: algorithmsUtilsIndex(self.newIndexes, new_).originalIndex()},
			self.deadline,
		); err != nil {
			return err
		}
		self.oldCurrent = algorithmsUtilsIndex(self.oldIndexes, old).originalIndex()
		self.newCurrent = algorithmsUtilsIndex(self.newIndexes, new_).originalIndex()
	}
	return nil
}

func (self *algorithmsPatiencePatience[T]) Delete(oldIndex int, oldLen int, newIndex int) error {
	return AlgorithmsDiffHookDefaultDelete(self, oldIndex, oldLen, newIndex)
}

func (self *algorithmsPatiencePatience[T]) Insert(oldIndex int, newIndex int, newLen int) error {
	return AlgorithmsDiffHookDefaultInsert(self, oldIndex, newIndex, newLen)
}

func (self *algorithmsPatiencePatience[T]) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	return AlgorithmsDiffHookDefaultReplace(self, oldIndex, oldLen, newIndex, newLen)
}

func (self *algorithmsPatiencePatience[T]) Finish() error {
	return AlgorithmsMyersDiffDeadline(
		self.d,
		self.old,
		Range{Start: self.oldCurrent, End: self.oldEnd},
		self.new,
		Range{Start: self.newCurrent, End: self.newEnd},
		self.deadline,
	)
}
