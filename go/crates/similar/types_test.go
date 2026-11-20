// Port of similar 2.7.0 src/types.rs

package similar

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// src/types.rs has no `#[test]` functions. These are its documentation
// examples, which Rust runs as tests.

func TestTypes_doc_iter_changes(t *testing.T) {
	old := []string{"foo", "bar", "baz"}
	new_ := []string{"foo", "bar", "blah"}
	ops := CaptureDiffSlices(Algorithm_Myers, old, new_)
	changes := []rs.Tuple2[ChangeTag, string]{}
	for _, x := range ops {
		for x := range DiffOpIterChanges(x, old, new_).All() {
			changes = append(changes, rs.T2(x.Tag(), x.Value()))
		}
	}
	assertSliceEq(t, changes, []rs.Tuple2[ChangeTag, string]{
		rs.T2(ChangeTag_Equal, "foo"),
		rs.T2(ChangeTag_Equal, "bar"),
		rs.T2(ChangeTag_Delete, "baz"),
		rs.T2(ChangeTag_Insert, "blah"),
	})
}

func TestTypes_doc_iter_slices(t *testing.T) {
	old := []string{"foo", "bar", "baz"}
	new_ := []string{"foo", "bar", "blah"}
	ops := CaptureDiffSlices(Algorithm_Myers, old, new_)
	changes := []rs.Tuple2[ChangeTag, []string]{}
	for _, x := range ops {
		changes = append(changes, DiffOpIterSlices(x, old, new_)...)
	}
	expected := []rs.Tuple2[ChangeTag, []string]{
		rs.T2(ChangeTag_Equal, []string{"foo", "bar"}),
		rs.T2(ChangeTag_Delete, []string{"baz"}),
		rs.T2(ChangeTag_Insert, []string{"blah"}),
	}
	equal := slices.EqualFunc(changes, expected, func(a, b rs.Tuple2[ChangeTag, []string]) bool {
		return a.F0 == b.F0 && slices.Equal(a.F1, b.F1)
	})
	if !equal {
		t.Fatalf("assertion `left == right` failed\n  left: %v\n right: %v", changes, expected)
	}
}

// A change over anything but text has no text methods in Rust. Go declares
// them for every element type, and they refuse at run time.
func TestTypes_text_methods_need_text(t *testing.T) {
	changes := DiffOpIterChanges(&DiffOp_Equal{OldIndex: 0, NewIndex: 0, Len: 1}, []int{1}, []int{1}).Collect()
	defer func() {
		message := rs.PanicInfoOf(recover()).Message
		assertEq(t, message, "similar::Change::missing_newline is only available for a change over text (Change[string])")
	}()
	changes[0].MissingNewline()
	t.Fatal("MissingNewline returned for a change over integers")
}
