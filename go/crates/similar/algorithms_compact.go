// Port of similar 2.7.0 src/algorithms/compact.rs

// Implements basic compacting.  This is based on the compaction logic from
// diffy by Brandon Williams.

package similar

import (
	"slices"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Performs semantic cleanup operations on a diff.
//
// This merges similar ops together but also tries to move hunks up and
// down the diff with the desire to connect as many hunks as possible.
// It still needs to be combined with [`Replace`](crate::algorithms::Replace)
// to get actual replace diff ops out.
type AlgorithmsCompact[T comparable, D AlgorithmsDiffHook] struct {
	d   D
	ops []DiffOp
	old []T
	new []T
}

// Creates a new compact hook wrapping another hook.
func AlgorithmsCompactNew[T comparable, D AlgorithmsDiffHook](d D, old []T, new_ []T) *AlgorithmsCompact[T, D] {
	return &AlgorithmsCompact[T, D]{
		d:   d,
		ops: []DiffOp{},
		old: old,
		new: new_,
	}
}

// Extracts the inner hook.
func (self *AlgorithmsCompact[T, D]) IntoInner() D {
	return self.d
}

// AsRef is `impl AsRef<D> for Compact<..>`.
func (self *AlgorithmsCompact[T, D]) AsRef() D {
	return self.d
}

// AsMut is `impl AsMut<D> for Compact<..>`.
func (self *AlgorithmsCompact[T, D]) AsMut() D {
	return self.d
}

func (self *AlgorithmsCompact[T, D]) Equal(oldIndex int, newIndex int, len_ int) error {
	self.ops = append(self.ops, &DiffOp_Equal{
		OldIndex: oldIndex,
		NewIndex: newIndex,
		Len:      len_,
	})
	return nil
}

func (self *AlgorithmsCompact[T, D]) Delete(oldIndex int, oldLen int, newIndex int) error {
	self.ops = append(self.ops, &DiffOp_Delete{
		OldIndex: oldIndex,
		OldLen:   oldLen,
		NewIndex: newIndex,
	})
	return nil
}

func (self *AlgorithmsCompact[T, D]) Insert(oldIndex int, newIndex int, newLen int) error {
	self.ops = append(self.ops, &DiffOp_Insert{
		OldIndex: oldIndex,
		NewIndex: newIndex,
		NewLen:   newLen,
	})
	return nil
}

func (self *AlgorithmsCompact[T, D]) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	return AlgorithmsDiffHookDefaultReplace(self, oldIndex, oldLen, newIndex, newLen)
}

func (self *AlgorithmsCompact[T, D]) Finish() error {
	algorithmsCompactCleanupDiffOps(self.old, self.new, &self.ops)
	for _, op := range self.ops {
		if err := op.ApplyToHook(self.d); err != nil {
			return err
		}
	}
	return self.d.Finish()
}

// Walks through all edits and shifts them up and then down, trying to see if
// they run into similar edits which can be merged.
func algorithmsCompactCleanupDiffOps[T comparable](old []T, new_ []T, ops *[]DiffOp) {
	// First attempt to compact all Deletions
	pointer := 0
	for pointer < len(*ops) {
		op := (*ops)[pointer]
		if op.Tag() == DiffTag_Delete {
			pointer = algorithmsCompactShiftDiffOpsUp(ops, old, new_, pointer)
			pointer = algorithmsCompactShiftDiffOpsDown(ops, old, new_, pointer)
		}
		pointer += 1
	}

	// Then attempt to compact all Insertions
	pointer = 0
	for pointer < len(*ops) {
		op := (*ops)[pointer]
		if op.Tag() == DiffTag_Insert {
			pointer = algorithmsCompactShiftDiffOpsUp(ops, old, new_, pointer)
			pointer = algorithmsCompactShiftDiffOpsDown(ops, old, new_, pointer)
		}
		pointer += 1
	}
}

// algorithmsCompactTagAt is `ops.get(index).map(|x| x.tag())`.
func algorithmsCompactTagAt(ops []DiffOp, index int) rs.Option[DiffTag] {
	if index < 0 || index >= len(ops) {
		return rs.None[DiffTag]()
	}
	return rs.Some(ops[index].Tag())
}

func algorithmsCompactShiftDiffOpsUp[T comparable](ops *[]DiffOp, old []T, new_ []T, pointer int) int {
	// `pointer.checked_sub(1).and_then(|idx| ops.get(idx))`
	for pointer >= 1 && pointer-1 < len(*ops) {
		prevOp := (*ops)[pointer-1]
		thisOp := (*ops)[pointer]
		thisTag, prevTag := thisOp.Tag(), prevOp.Tag()
		switch {
		// Shift Inserts Upwards
		case thisTag == DiffTag_Insert && prevTag == DiffTag_Equal:
			suffixLen := algorithmsUtilsCommonSuffixLen(old, prevOp.OldRange(), new_, thisOp.NewRange())
			if suffixLen > 0 {
				if algorithmsCompactTagAt(*ops, pointer+1) == rs.Some(DiffTag_Equal) {
					(*ops)[pointer+1] = diffOpGrowLeft((*ops)[pointer+1], suffixLen)
				} else {
					*ops = slices.Insert(*ops, pointer+1, DiffOp(&DiffOp_Equal{
						OldIndex: prevOp.OldRange().End - suffixLen,
						NewIndex: thisOp.NewRange().End - suffixLen,
						Len:      suffixLen,
					}))
				}
				(*ops)[pointer] = diffOpShiftLeft((*ops)[pointer], suffixLen)
				(*ops)[pointer-1] = diffOpShrinkLeft((*ops)[pointer-1], suffixLen)

				if diffOpIsEmpty((*ops)[pointer-1]) {
					*ops = slices.Delete(*ops, pointer-1, pointer)
					pointer -= 1
				}
			} else if diffOpIsEmpty((*ops)[pointer-1]) {
				*ops = slices.Delete(*ops, pointer-1, pointer)
				pointer -= 1
			} else {
				// We can't shift upwards anymore
				return pointer
			}
		// Shift Deletions Upwards
		case thisTag == DiffTag_Delete && prevTag == DiffTag_Equal:
			// check common suffix for the amount we can shift
			suffixLen := algorithmsUtilsCommonSuffixLen(old, prevOp.OldRange(), new_, thisOp.NewRange())
			if suffixLen != 0 {
				if algorithmsCompactTagAt(*ops, pointer+1) == rs.Some(DiffTag_Equal) {
					(*ops)[pointer+1] = diffOpGrowLeft((*ops)[pointer+1], suffixLen)
				} else {
					oldRange := prevOp.OldRange()
					*ops = slices.Insert(*ops, pointer+1, DiffOp(&DiffOp_Equal{
						OldIndex: oldRange.End - suffixLen,
						NewIndex: thisOp.NewRange().End - suffixLen,
						Len:      oldRange.Len() - suffixLen,
					}))
				}
				(*ops)[pointer] = diffOpShiftLeft((*ops)[pointer], suffixLen)
				(*ops)[pointer-1] = diffOpShrinkLeft((*ops)[pointer-1], suffixLen)

				if diffOpIsEmpty((*ops)[pointer-1]) {
					*ops = slices.Delete(*ops, pointer-1, pointer)
					pointer -= 1
				}
			} else if diffOpIsEmpty((*ops)[pointer-1]) {
				*ops = slices.Delete(*ops, pointer-1, pointer)
				pointer -= 1
			} else {
				// We can't shift upwards anymore
				return pointer
			}
		// Swap the Delete and Insert
		case (thisTag == DiffTag_Insert && prevTag == DiffTag_Delete) || (thisTag == DiffTag_Delete && prevTag == DiffTag_Insert):
			(*ops)[pointer-1], (*ops)[pointer] = (*ops)[pointer], (*ops)[pointer-1]
			pointer -= 1
		// Merge the two ranges
		case thisTag == DiffTag_Insert && prevTag == DiffTag_Insert:
			(*ops)[pointer-1] = diffOpGrowRight((*ops)[pointer-1], thisOp.NewRange().Len())
			*ops = slices.Delete(*ops, pointer, pointer+1)
			pointer -= 1
		case thisTag == DiffTag_Delete && prevTag == DiffTag_Delete:
			(*ops)[pointer-1] = diffOpGrowRight((*ops)[pointer-1], thisOp.OldRange().Len())
			*ops = slices.Delete(*ops, pointer, pointer+1)
			pointer -= 1
		default:
			// unreachable!("unexpected tag")
			rs.PanicAt(0, "internal error: entered unreachable code: unexpected tag")
		}
	}
	return pointer
}

func algorithmsCompactShiftDiffOpsDown[T comparable](ops *[]DiffOp, old []T, new_ []T, pointer int) int {
	// `pointer.checked_add(1).and_then(|idx| ops.get(idx))`
	for pointer+1 < len(*ops) {
		nextOp := (*ops)[pointer+1]
		thisOp := (*ops)[pointer]
		thisTag, nextTag := thisOp.Tag(), nextOp.Tag()
		switch {
		// Shift Inserts Downwards
		case thisTag == DiffTag_Insert && nextTag == DiffTag_Equal:
			prefixLen := algorithmsUtilsCommonPrefixLen(old, nextOp.OldRange(), new_, thisOp.NewRange())
			if prefixLen > 0 {
				if algorithmsCompactTagAt(*ops, pointer-1) == rs.Some(DiffTag_Equal) {
					(*ops)[pointer-1] = diffOpGrowRight((*ops)[pointer-1], prefixLen)
				} else {
					*ops = slices.Insert(*ops, pointer, DiffOp(&DiffOp_Equal{
						OldIndex: nextOp.OldRange().Start,
						NewIndex: thisOp.NewRange().Start,
						Len:      prefixLen,
					}))
					pointer += 1
				}
				(*ops)[pointer] = diffOpShiftRight((*ops)[pointer], prefixLen)
				(*ops)[pointer+1] = diffOpShrinkRight((*ops)[pointer+1], prefixLen)

				if diffOpIsEmpty((*ops)[pointer+1]) {
					*ops = slices.Delete(*ops, pointer+1, pointer+2)
				}
			} else if diffOpIsEmpty((*ops)[pointer+1]) {
				*ops = slices.Delete(*ops, pointer+1, pointer+2)
			} else {
				// We can't shift upwards anymore
				return pointer
			}
		// Shift Deletions Downwards
		case thisTag == DiffTag_Delete && nextTag == DiffTag_Equal:
			// check common suffix for the amount we can shift
			prefixLen := algorithmsUtilsCommonPrefixLen(old, nextOp.OldRange(), new_, thisOp.NewRange())
			if prefixLen > 0 {
				if algorithmsCompactTagAt(*ops, pointer-1) == rs.Some(DiffTag_Equal) {
					(*ops)[pointer-1] = diffOpGrowRight((*ops)[pointer-1], prefixLen)
				} else {
					*ops = slices.Insert(*ops, pointer, DiffOp(&DiffOp_Equal{
						OldIndex: nextOp.OldRange().Start,
						NewIndex: thisOp.NewRange().Start,
						Len:      prefixLen,
					}))
					pointer += 1
				}
				(*ops)[pointer] = diffOpShiftRight((*ops)[pointer], prefixLen)
				(*ops)[pointer+1] = diffOpShrinkRight((*ops)[pointer+1], prefixLen)

				if diffOpIsEmpty((*ops)[pointer+1]) {
					*ops = slices.Delete(*ops, pointer+1, pointer+2)
				}
			} else if diffOpIsEmpty((*ops)[pointer+1]) {
				*ops = slices.Delete(*ops, pointer+1, pointer+2)
			} else {
				// We can't shift downwards anymore
				return pointer
			}
		// Swap the Delete and Insert
		case (thisTag == DiffTag_Insert && nextTag == DiffTag_Delete) || (thisTag == DiffTag_Delete && nextTag == DiffTag_Insert):
			(*ops)[pointer], (*ops)[pointer+1] = (*ops)[pointer+1], (*ops)[pointer]
			pointer += 1
		// Merge the two ranges
		case thisTag == DiffTag_Insert && nextTag == DiffTag_Insert:
			(*ops)[pointer] = diffOpGrowRight((*ops)[pointer], nextOp.NewRange().Len())
			*ops = slices.Delete(*ops, pointer+1, pointer+2)
		case thisTag == DiffTag_Delete && nextTag == DiffTag_Delete:
			(*ops)[pointer] = diffOpGrowRight((*ops)[pointer], nextOp.OldRange().Len())
			*ops = slices.Delete(*ops, pointer+1, pointer+2)
		default:
			// unreachable!("unexpected tag")
			rs.PanicAt(0, "internal error: entered unreachable code: unexpected tag")
		}
	}
	return pointer
}
