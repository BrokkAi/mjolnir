// Port of similar 2.7.0 src/algorithms/replace.rs

package similar

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// A [`DiffHook`] that combines deletions and insertions to give blocks
// of maximal length, and replacements when appropriate.
//
// It will replace [`DiffHook::insert`] and [`DiffHook::delete`] events when
// possible with [`DiffHook::replace`] events.  Note that even though the
// text processing in the crate does not use replace events and always resolves
// then back to delete and insert, it's useful to always use the replacer to
// ensure a consistent order of inserts and deletes.  This is why for instance
// the text diffing automatically uses this hook internally.
type AlgorithmsReplace[D AlgorithmsDiffHook] struct {
	d   D
	del rs.Option[rs.Tuple3[int, int, int]]
	ins rs.Option[rs.Tuple3[int, int, int]]
	eq  rs.Option[rs.Tuple3[int, int, int]]
}

// Creates a new replace hook wrapping another hook.
func AlgorithmsReplaceNew[D AlgorithmsDiffHook](d D) *AlgorithmsReplace[D] {
	return &AlgorithmsReplace[D]{
		d:   d,
		del: rs.None[rs.Tuple3[int, int, int]](),
		ins: rs.None[rs.Tuple3[int, int, int]](),
		eq:  rs.None[rs.Tuple3[int, int, int]](),
	}
}

// Extracts the inner hook.
func (self *AlgorithmsReplace[D]) IntoInner() D {
	return self.d
}

func (self *AlgorithmsReplace[D]) flushEq() error {
	if eq, ok := self.eq.Take().Get(); ok {
		eqOldIndex, eqNewIndex, eqLen := eq.F0, eq.F1, eq.F2
		if err := self.d.Equal(eqOldIndex, eqNewIndex, eqLen); err != nil {
			return err
		}
	}
	return nil
}

func (self *AlgorithmsReplace[D]) flushDelIns() error {
	if del, ok := self.del.Take().Get(); ok {
		delOldIndex, delOldLen, delNewIndex := del.F0, del.F1, del.F2
		if ins, ok := self.ins.Take().Get(); ok {
			insNewIndex, insNewLen := ins.F1, ins.F2
			if err := self.d.Replace(delOldIndex, delOldLen, insNewIndex, insNewLen); err != nil {
				return err
			}
		} else {
			if err := self.d.Delete(delOldIndex, delOldLen, delNewIndex); err != nil {
				return err
			}
		}
	} else if ins, ok := self.ins.Take().Get(); ok {
		insOldIndex, insNewIndex, insNewLen := ins.F0, ins.F1, ins.F2
		if err := self.d.Insert(insOldIndex, insNewIndex, insNewLen); err != nil {
			return err
		}
	}
	return nil
}

// AsRef is `impl AsRef<D> for Replace<D>`.
func (self *AlgorithmsReplace[D]) AsRef() D {
	return self.d
}

// AsMut is `impl AsMut<D> for Replace<D>`.
func (self *AlgorithmsReplace[D]) AsMut() D {
	return self.d
}

func (self *AlgorithmsReplace[D]) Equal(oldIndex int, newIndex int, len_ int) error {
	if err := self.flushDelIns(); err != nil {
		return err
	}

	if eq, ok := self.eq.Take().Get(); ok {
		eqOldIndex, eqNewIndex, eqLen := eq.F0, eq.F1, eq.F2
		self.eq = rs.Some(rs.T3(eqOldIndex, eqNewIndex, eqLen+len_))
	} else {
		self.eq = rs.Some(rs.T3(oldIndex, newIndex, len_))
	}

	return nil
}

func (self *AlgorithmsReplace[D]) Delete(oldIndex int, oldLen int, newIndex int) error {
	if err := self.flushEq(); err != nil {
		return err
	}
	if del, ok := self.del.Take().Get(); ok {
		delOldIndex, delOldLen, delNewIndex := del.F0, del.F1, del.F2
		algorithmsReplaceDebugAssertEq(oldIndex, delOldIndex+delOldLen)
		self.del = rs.Some(rs.T3(delOldIndex, delOldLen+oldLen, delNewIndex))
	} else {
		self.del = rs.Some(rs.T3(oldIndex, oldLen, newIndex))
	}
	return nil
}

func (self *AlgorithmsReplace[D]) Insert(oldIndex int, newIndex int, newLen int) error {
	if err := self.flushEq(); err != nil {
		return err
	}
	if ins, ok := self.ins.Take().Get(); ok {
		insOldIndex, insNewIndex, insNewLen := ins.F0, ins.F1, ins.F2
		algorithmsReplaceDebugAssertEq(insNewIndex+insNewLen, newIndex)
		self.ins = rs.Some(rs.T3(insOldIndex, insNewIndex, newLen+insNewLen))
	} else {
		self.ins = rs.Some(rs.T3(oldIndex, newIndex, newLen))
	}

	return nil
}

func (self *AlgorithmsReplace[D]) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	if err := self.flushEq(); err != nil {
		return err
	}
	return self.d.Replace(oldIndex, oldLen, newIndex, newLen)
}

func (self *AlgorithmsReplace[D]) Finish() error {
	if err := self.flushEq(); err != nil {
		return err
	}
	if err := self.flushDelIns(); err != nil {
		return err
	}
	return self.d.Finish()
}

// algorithmsReplaceDebugAssertEq is `debug_assert_eq!(left, right)` on two
// indexes, with the message Rust's macro gives.
func algorithmsReplaceDebugAssertEq(left int, right int) {
	if rs.DebugAssertions && left != right {
		rs.PanicAt(1, "assertion `left == right` failed\n  left: "+strconv.Itoa(left)+"\n right: "+strconv.Itoa(right))
	}
}
