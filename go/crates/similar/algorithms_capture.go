// Port of similar 2.7.0 src/algorithms/capture.rs

package similar

// A [`DiffHook`] that captures all diff operations.
type AlgorithmsCapture []DiffOp

// Creates a new capture hook.
func AlgorithmsCaptureNew() *AlgorithmsCapture {
	return &AlgorithmsCapture{}
}

// Converts the capture hook into a vector of ops.
func (self *AlgorithmsCapture) IntoOps() []DiffOp {
	return []DiffOp(*self)
}

// Isolate change clusters by eliminating ranges with no changes.
//
// This is equivalent to calling [`group_diff_ops`] on [`Capture::into_ops`].
func (self *AlgorithmsCapture) IntoGroupedOps(n int) [][]DiffOp {
	return GroupDiffOps(self.IntoOps(), n)
}

// Accesses the captured operations.
func (self *AlgorithmsCapture) Ops() []DiffOp {
	return []DiffOp(*self)
}

func (self *AlgorithmsCapture) Equal(oldIndex int, newIndex int, len_ int) error {
	*self = append(*self, &DiffOp_Equal{
		OldIndex: oldIndex,
		NewIndex: newIndex,
		Len:      len_,
	})
	return nil
}

func (self *AlgorithmsCapture) Delete(oldIndex int, oldLen int, newIndex int) error {
	*self = append(*self, &DiffOp_Delete{
		OldIndex: oldIndex,
		OldLen:   oldLen,
		NewIndex: newIndex,
	})
	return nil
}

func (self *AlgorithmsCapture) Insert(oldIndex int, newIndex int, newLen int) error {
	*self = append(*self, &DiffOp_Insert{
		OldIndex: oldIndex,
		NewIndex: newIndex,
		NewLen:   newLen,
	})
	return nil
}

func (self *AlgorithmsCapture) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	*self = append(*self, &DiffOp_Replace{
		OldIndex: oldIndex,
		OldLen:   oldLen,
		NewIndex: newIndex,
		NewLen:   newLen,
	})
	return nil
}

func (self *AlgorithmsCapture) Finish() error {
	return AlgorithmsDiffHookDefaultFinish(self)
}
