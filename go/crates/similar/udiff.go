// Port of similar 2.7.0 src/udiff.rs

// This module provides unified diff functionality.
//
// It is available for as long as the `text` feature is enabled which
// is enabled by default:
//
//	textDiff := similar.TextDiffFromLines(oldText, newText)
//	fmt.Print(textDiff.
//		UnifiedDiff().
//		ContextRadius(10).
//		Header("old_file", "new_file"))
//
// # Unicode vs Bytes
//
// The [`UnifiedDiff`] type supports both unicode and byte diffs for all
// types compatible with [`DiffableStr`].  You can pick between the two
// versions by using the [`Display`](std::fmt::Display) implementation or
// [`UnifiedDiff`] or [`UnifiedDiff::to_writer`].
//
// The former uses [`DiffableStr::to_string_lossy`], the latter uses
// [`DiffableStr::as_bytes`] for each line.

package similar

import (
	"io"
	"iter"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type udiffMissingNewlineHint bool

// String is the `Display` output.
func (self udiffMissingNewlineHint) String() string {
	if self {
		return "\n\\ No newline at end of file"
	}
	return ""
}

type udiffUnifiedDiffHunkRange struct {
	F0 int
	F1 int
}

func (self udiffUnifiedDiffHunkRange) start() int {
	return self.F0
}

func (self udiffUnifiedDiffHunkRange) end() int {
	return self.F1
}

// String is the `Display` output.
func (self udiffUnifiedDiffHunkRange) String() string {
	beginning := self.start() + 1
	len_ := usizeSaturatingSub(self.end(), self.start())
	if len_ == 1 {
		return strconv.Itoa(beginning)
	} else {
		if len_ == 0 {
			// empty ranges begin at line just before the range
			beginning -= 1
		}
		return strconv.Itoa(beginning) + "," + strconv.Itoa(len_)
	}
}

// Unified diff hunk header formatter.
type UdiffUnifiedHunkHeader struct {
	oldRange udiffUnifiedDiffHunkRange
	newRange udiffUnifiedDiffHunkRange
}

// Creates a hunk header from a (non empty) slice of diff ops.
func UdiffUnifiedHunkHeaderNew(ops []DiffOp) *UdiffUnifiedHunkHeader {
	first := ops[0]
	last := ops[len(ops)-1]
	oldStart := first.OldRange().Start
	newStart := first.NewRange().Start
	oldEnd := last.OldRange().End
	newEnd := last.NewRange().End
	return &UdiffUnifiedHunkHeader{
		oldRange: udiffUnifiedDiffHunkRange{F0: oldStart, F1: oldEnd},
		newRange: udiffUnifiedDiffHunkRange{F0: newStart, F1: newEnd},
	}
}

// String is the `Display` output: `@@ -1,3 +1,4 @@`.
func (self *UdiffUnifiedHunkHeader) String() string {
	return "@@ -" + self.oldRange.String() + " +" + self.newRange.String() + " @@"
}

// FmtDisplay is `impl fmt::Display for UnifiedHunkHeader`. The Rust impl
// writes with `write!`, so width, fill and alignment of the caller's `{}` do
// not apply.
func (self *UdiffUnifiedHunkHeader) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// Unified diff formatter.
//
//	textDiff := similar.TextDiffFromLines(oldText, newText)
//	fmt.Print(textDiff.
//		UnifiedDiff().
//		ContextRadius(10).
//		Header("old_file", "new_file"))
//
// ## Unicode vs Bytes
//
// The [`UnifiedDiff`] type supports both unicode and byte diffs for all
// types compatible with [`DiffableStr`].  You can pick between the two
// versions by using [`UnifiedDiff.to_string`] or [`UnifiedDiff.to_writer`].
// The former uses [`DiffableStr::to_string_lossy`], the latter uses
// [`DiffableStr::as_bytes`] for each line.
type UdiffUnifiedDiff struct {
	diff               *TextDiff
	contextRadius      int
	missingNewlineHint bool
	header             rs.Option[rs.Tuple2[string, string]]
}

// Creates a formatter from a text diff object.
func UdiffUnifiedDiffFromTextDiff(diff *TextDiff) *UdiffUnifiedDiff {
	return &UdiffUnifiedDiff{
		diff:               diff,
		contextRadius:      3,
		missingNewlineHint: true,
		header:             rs.None[rs.Tuple2[string, string]](),
	}
}

// Changes the context radius.
//
// The context radius is the number of lines between changes that should
// be emitted.  This defaults to `3`.
func (self *UdiffUnifiedDiff) ContextRadius(n int) *UdiffUnifiedDiff {
	self.contextRadius = n
	return self
}

// Sets a header to the diff.
//
// `a` and `b` are the file names that are added to the top of the unified
// file format.  The names are accepted verbatim which lets you encode
// a timestamp into it when separated by a tab (`\t`).  For more information,
// see [the unified diff format specification](https://pubs.opengroup.org/onlinepubs/9699919799/utilities/diff.html#tag_20_34_10_07).
func (self *UdiffUnifiedDiff) Header(a string, b string) *UdiffUnifiedDiff {
	self.header = rs.Some(rs.T2(a, b))
	return self
}

// Controls the missing newline hint.
//
// By default a special `\ No newline at end of file` marker is added to
// the output when a file is not terminated with a final newline.  This can
// be disabled with this flag.
func (self *UdiffUnifiedDiff) MissingNewlineHint(yes bool) *UdiffUnifiedDiff {
	self.missingNewlineHint = yes
	return self
}

// Iterates over all hunks as configured.
//
// As in Rust, the ops are grouped when IterHunks is called, and each hunk is
// built as the sequence is ranged over.
func (self *UdiffUnifiedDiff) IterHunks() iter.Seq[*UdiffUnifiedDiffHunk] {
	diff := self.diff
	missingNewlineHint := self.missingNewlineHint
	groups := self.diff.GroupedOps(self.contextRadius)
	return func(yield func(*UdiffUnifiedDiffHunk) bool) {
		for _, ops := range groups {
			if len(ops) == 0 {
				continue
			}
			if !yield(UdiffUnifiedDiffHunkNew(ops, diff, missingNewlineHint)) {
				return
			}
		}
	}
}

// Write the unified diff as bytes to the output stream.
func (self *UdiffUnifiedDiff) ToWriter(w io.Writer) error {
	header := self.header
	for hunk := range self.IterHunks() {
		if names, ok := header.Take().Get(); ok {
			oldFile, newFile := names.F0, names.F1
			if _, err := io.WriteString(w, "--- "+oldFile+"\n"); err != nil {
				return err
			}
			if _, err := io.WriteString(w, "+++ "+newFile+"\n"); err != nil {
				return err
			}
		}
		if _, err := io.WriteString(w, hunk.String()); err != nil {
			return err
		}
	}
	return nil
}

// Unified diff hunk formatter.
//
// The `Display` this renders out a single unified diff's hunk.
type UdiffUnifiedDiffHunk struct {
	diff               *TextDiff
	ops                []DiffOp
	missingNewlineHint bool
}

// Creates a new hunk for some operations.
func UdiffUnifiedDiffHunkNew(ops []DiffOp, diff *TextDiff, missingNewlineHint bool) *UdiffUnifiedDiffHunk {
	return &UdiffUnifiedDiffHunk{
		diff:               diff,
		ops:                ops,
		missingNewlineHint: missingNewlineHint,
	}
}

// Returns the header for the hunk.
func (self *UdiffUnifiedDiffHunk) Header() *UdiffUnifiedHunkHeader {
	return UdiffUnifiedHunkHeaderNew(self.ops)
}

// Returns all operations in the hunk.
func (self *UdiffUnifiedDiffHunk) Ops() []DiffOp {
	return self.ops
}

// Returns the value of the `missing_newline_hint` flag.
func (self *UdiffUnifiedDiffHunk) MissingNewlineHint() bool {
	return self.missingNewlineHint
}

// Iterates over all changes in a hunk.
func (self *UdiffUnifiedDiffHunk) IterChanges() *IterAllChangesIter[string] {
	return iterAllChangesIterNew(self.diff.OldSlices(), self.diff.NewSlices(), self.Ops())
}

// Write the hunk as bytes to the output stream.
//
// A Go string is its bytes, so this writes what String returns.
func (self *UdiffUnifiedDiffHunk) ToWriter(w io.Writer) error {
	idx := 0
	for change := range self.IterChanges().All() {
		if idx == 0 {
			if _, err := io.WriteString(w, self.Header().String()+"\n"); err != nil {
				return err
			}
		}
		if _, err := io.WriteString(w, change.Tag().String()); err != nil {
			return err
		}
		if _, err := w.Write(DiffableStrAsBytes(change.Value())); err != nil {
			return err
		}
		if !self.diff.NewlineTerminated() {
			if _, err := io.WriteString(w, "\n"); err != nil {
				return err
			}
		}
		if self.diff.NewlineTerminated() && change.MissingNewline() {
			if _, err := io.WriteString(w, udiffMissingNewlineHint(self.missingNewlineHint).String()+"\n"); err != nil {
				return err
			}
		}
		idx++
	}
	return nil
}

// String is the `Display` output: the hunk as unified diff text.
func (self *UdiffUnifiedDiffHunk) String() string {
	var f strings.Builder
	idx := 0
	for change := range self.IterChanges().All() {
		if idx == 0 {
			f.WriteString(self.Header().String())
			f.WriteString("\n")
		}
		f.WriteString(change.Tag().String())
		f.WriteString(change.ToStringLossy())
		if !self.diff.NewlineTerminated() {
			f.WriteString("\n")
		}
		if self.diff.NewlineTerminated() && change.MissingNewline() {
			f.WriteString(udiffMissingNewlineHint(self.missingNewlineHint).String())
			f.WriteString("\n")
		}
		idx++
	}
	return f.String()
}

// FmtDisplay is `impl fmt::Display for UnifiedDiffHunk`. The Rust impl writes
// with `write!`, so width, fill and alignment of the caller's `{}` do not
// apply.
func (self *UdiffUnifiedDiffHunk) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// String is the `Display` output: the whole unified diff. It is what
// `to_string()` returns.
func (self *UdiffUnifiedDiff) String() string {
	var f strings.Builder
	header := self.header
	for hunk := range self.IterHunks() {
		if names, ok := header.Take().Get(); ok {
			oldFile, newFile := names.F0, names.F1
			f.WriteString("--- " + oldFile + "\n")
			f.WriteString("+++ " + newFile + "\n")
		}
		f.WriteString(hunk.String())
	}
	return f.String()
}

// FmtDisplay is `impl fmt::Display for UnifiedDiff`. The Rust impl writes
// with `write!`, so width, fill and alignment of the caller's `{}` do not
// apply.
func (self *UdiffUnifiedDiff) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
