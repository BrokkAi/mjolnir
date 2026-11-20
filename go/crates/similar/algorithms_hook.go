// Port of similar 2.7.0 src/algorithms/hook.rs

package similar

// A trait for reacting to an edit script from the "old" version to
// the "new" version.
//
// Rust's `type Error` is Go's error. A type that relies on one of the
// trait's default methods forwards to the matching
// AlgorithmsDiffHookDefault function.
type AlgorithmsDiffHook interface {
	// Called when lines with indices `old_index` (in the old version) and
	// `new_index` (in the new version) start an section equal in both
	// versions, of length `len`.
	Equal(oldIndex int, newIndex int, len_ int) error

	// Called when a section of length `old_len`, starting at `old_index`,
	// needs to be deleted from the old version.
	Delete(oldIndex int, oldLen int, newIndex int) error

	// Called when a section of the new version, of length `new_len`
	// and starting at `new_index`, needs to be inserted at position `old_index'.
	Insert(oldIndex int, newIndex int, newLen int) error

	// Called when a section of the old version, starting at index
	// `old_index` and of length `old_len`, needs to be replaced with a
	// section of length `new_len`, starting at `new_index`, of the new
	// version.
	//
	// The default implementations invokes `delete` and `insert`.
	//
	// You can use the [`Replace`](crate::algorithms::Replace) hook to
	// automatically generate these.
	Replace(oldIndex int, oldLen int, newIndex int, newLen int) error

	// Always called at the end of the algorithm.
	Finish() error
}

// AlgorithmsDiffHookDefaultEqual is the default body of `DiffHook::equal`.
func AlgorithmsDiffHookDefaultEqual(self AlgorithmsDiffHook, oldIndex int, newIndex int, len_ int) error {
	_ = oldIndex
	_ = newIndex
	_ = len_
	return nil
}

// AlgorithmsDiffHookDefaultDelete is the default body of `DiffHook::delete`.
func AlgorithmsDiffHookDefaultDelete(self AlgorithmsDiffHook, oldIndex int, oldLen int, newIndex int) error {
	_ = oldIndex
	_ = oldLen
	_ = newIndex
	return nil
}

// AlgorithmsDiffHookDefaultInsert is the default body of `DiffHook::insert`.
func AlgorithmsDiffHookDefaultInsert(self AlgorithmsDiffHook, oldIndex int, newIndex int, newLen int) error {
	_ = oldIndex
	_ = newIndex
	_ = newLen
	return nil
}

// AlgorithmsDiffHookDefaultReplace is the default body of
// `DiffHook::replace`: a delete followed by an insert.
func AlgorithmsDiffHookDefaultReplace(self AlgorithmsDiffHook, oldIndex int, oldLen int, newIndex int, newLen int) error {
	if err := self.Delete(oldIndex, oldLen, newIndex); err != nil {
		return err
	}
	return self.Insert(oldIndex, newIndex, newLen)
}

// AlgorithmsDiffHookDefaultFinish is the default body of `DiffHook::finish`.
func AlgorithmsDiffHookDefaultFinish(self AlgorithmsDiffHook) error {
	return nil
}

// `impl DiffHook for &mut D` has no counterpart: a Go hook is already used
// through a pointer.

// Wrapper [`DiffHook`] that prevents calls to [`DiffHook::finish`].
//
// This hook is useful in situations where diff hooks are composed but you
// want to prevent that the finish hook method is called.
type AlgorithmsNoFinishHook[D AlgorithmsDiffHook] struct {
	F0 D
}

// Wraps another hook.
func AlgorithmsNoFinishHookNew[D AlgorithmsDiffHook](d D) *AlgorithmsNoFinishHook[D] {
	return &AlgorithmsNoFinishHook[D]{F0: d}
}

// Extracts the inner hook.
func (self *AlgorithmsNoFinishHook[D]) IntoInner() D {
	return self.F0
}

func (self *AlgorithmsNoFinishHook[D]) Equal(oldIndex int, newIndex int, len_ int) error {
	return self.F0.Equal(oldIndex, newIndex, len_)
}

func (self *AlgorithmsNoFinishHook[D]) Delete(oldIndex int, oldLen int, newIndex int) error {
	return self.F0.Delete(oldIndex, oldLen, newIndex)
}

func (self *AlgorithmsNoFinishHook[D]) Insert(oldIndex int, newIndex int, newLen int) error {
	return self.F0.Insert(oldIndex, newIndex, newLen)
}

func (self *AlgorithmsNoFinishHook[D]) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	return self.F0.Replace(oldIndex, oldLen, newIndex, newLen)
}

func (self *AlgorithmsNoFinishHook[D]) Finish() error {
	return nil
}
