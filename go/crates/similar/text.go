// Port of similar 2.7.0 src/text/mod.rs

// Text diffing utilities.

package similar

import (
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

type textDeadline interface {
	isTextDeadline()
}

type textDeadline_Absolute struct {
	F0 rstime.Instant
}

type textDeadline_Relative struct {
	F0 time.Duration
}

func (*textDeadline_Absolute) isTextDeadline() {}
func (*textDeadline_Relative) isTextDeadline() {}

var _ = rs.RegisterEnum[textDeadline]("Deadline",
	rs.Variant[textDeadline_Absolute]("Absolute", rs.ShapeTuple),
	rs.Variant[textDeadline_Relative]("Relative", rs.ShapeTuple),
)

func textDeadlineIntoInstant(self textDeadline) rs.Option[rstime.Instant] {
	switch self := self.(type) {
	case *textDeadline_Absolute:
		return rs.Some(self.F0)
	case *textDeadline_Relative:
		return deadlineSupportDurationToDeadline(self.F0)
	default:
		panic(rs.Unreachable(self))
	}
}

// A builder type config for more complex uses of [`TextDiff`].
//
// Requires the `text` feature.
//
// The zero value is `TextDiffConfig::default()`. The setters return the
// config they were called on, so calls chain as they do in Rust:
//
//	similar.TextDiffConfigure().Timeout(d).DiffLines(old, new)
type TextDiffConfig struct {
	algorithm         Algorithm               `rs:"algorithm"`
	newlineTerminated rs.Option[bool]         `rs:"newline_terminated"`
	deadline          rs.Option[textDeadline] `rs:"deadline"`
}

var _ = rs.RegisterStruct[TextDiffConfig]("TextDiffConfig")

// Changes the algorithm.
//
// The default algorithm is [`Algorithm::Myers`].
func (self *TextDiffConfig) Algorithm(alg Algorithm) *TextDiffConfig {
	self.algorithm = alg
	return self
}

// Sets a deadline for the diff operation.
//
// By default a diff will take as long as it takes.  For certain diff
// algorithms like Myer's and Patience a maximum running time can be
// defined after which the algorithm gives up and approximates.
func (self *TextDiffConfig) Deadline(deadline rstime.Instant) *TextDiffConfig {
	self.deadline = rs.Some[textDeadline](&textDeadline_Absolute{F0: deadline})
	return self
}

// Sets a timeout for thediff operation.
//
// This is like [`deadline`](Self::deadline) but accepts a duration.
func (self *TextDiffConfig) Timeout(timeout time.Duration) *TextDiffConfig {
	self.deadline = rs.Some[textDeadline](&textDeadline_Relative{F0: timeout})
	return self
}

// Changes the newline termination flag.
//
// The default is automatic based on input.  This flag controls the
// behavior of [`TextDiff::iter_changes`] and unified diff generation
// with regards to newlines.  When the flag is set to `false` (which
// is the default) then newlines are added.  Otherwise the newlines
// from the source sequences are reused.
func (self *TextDiffConfig) NewlineTerminated(yes bool) *TextDiffConfig {
	self.newlineTerminated = rs.Some(yes)
	return self
}

// Creates a diff of lines.
//
// This splits the text `old` and `new` into lines preserving newlines
// in the input.  Line diffs are very common and because of that enjoy
// special handling in similar.  When a line diff is created with this
// method the `newline_terminated` flag is flipped to `true` and will
// influence the behavior of unified diff generation.
//
//	diff := similar.TextDiffConfigure().DiffLines("a\nb\nc", "a\nb\nC")
//	// The changes are:
//	//    (ChangeTag_Equal, "a\n"),
//	//    (ChangeTag_Equal, "b\n"),
//	//    (ChangeTag_Delete, "c"),
//	//    (ChangeTag_Insert, "C"),
func (self *TextDiffConfig) DiffLines(old string, new_ string) *TextDiff {
	return self.diff(
		DiffableStrTokenizeLines(old),
		DiffableStrTokenizeLines(new_),
		true,
	)
}

// Creates a diff of words.
//
// This splits the text into words and whitespace.
//
// Note on word diffs: because the text differ will tokenize the strings
// into small segments it can be inconvenient to work with the results
// depending on the use case.  You might also want to combine word level
// diffs with the [`TextDiffRemapper`](crate::utils::TextDiffRemapper)
// which lets you remap the diffs back to the original input strings.
//
//	diff := similar.TextDiffConfigure().DiffWords("foo bar baz", "foo BAR baz")
//	// The changes are:
//	//    (ChangeTag_Equal, "foo"),
//	//    (ChangeTag_Equal, " "),
//	//    (ChangeTag_Delete, "bar"),
//	//    (ChangeTag_Insert, "BAR"),
//	//    (ChangeTag_Equal, " "),
//	//    (ChangeTag_Equal, "baz"),
func (self *TextDiffConfig) DiffWords(old string, new_ string) *TextDiff {
	return self.diff(
		DiffableStrTokenizeWords(old),
		DiffableStrTokenizeWords(new_),
		false,
	)
}

// Creates a diff of characters.
//
// Note on character diffs: because the text differ will tokenize the strings
// into small segments it can be inconvenient to work with the results
// depending on the use case.  You might also want to combine word level
// diffs with the [`TextDiffRemapper`](crate::utils::TextDiffRemapper)
// which lets you remap the diffs back to the original input strings.
//
//	diff := similar.TextDiffConfigure().DiffChars("abcdef", "abcDDf")
//	// The changes are:
//	//    (ChangeTag_Equal, "a"),
//	//    (ChangeTag_Equal, "b"),
//	//    (ChangeTag_Equal, "c"),
//	//    (ChangeTag_Delete, "d"),
//	//    (ChangeTag_Delete, "e"),
//	//    (ChangeTag_Insert, "D"),
//	//    (ChangeTag_Insert, "D"),
//	//    (ChangeTag_Equal, "f"),
func (self *TextDiffConfig) DiffChars(old string, new_ string) *TextDiff {
	return self.diff(
		DiffableStrTokenizeChars(old),
		DiffableStrTokenizeChars(new_),
		false,
	)
}

// Creates a diff of arbitrary slices.
//
//	old := []string{"foo", "bar", "baz"}
//	new := []string{"foo", "BAR", "baz"}
//	diff := similar.TextDiffConfigure().DiffSlices(old, new)
//	// The changes are:
//	//    (ChangeTag_Equal, "foo"),
//	//    (ChangeTag_Delete, "bar"),
//	//    (ChangeTag_Insert, "BAR"),
//	//    (ChangeTag_Equal, "baz"),
//
// The diff keeps the two slices; it does not copy them.
func (self *TextDiffConfig) DiffSlices(old []string, new_ []string) *TextDiff {
	return self.diff(old, new_, false)
}

func (self *TextDiffConfig) diff(old []string, new_ []string, newlineTerminated bool) *TextDiff {
	deadline := rs.OptionAndThen(self.deadline, textDeadlineIntoInstant)
	var ops []DiffOp
	if len(old) > 100 || len(new_) > 100 {
		ih := AlgorithmsIdentifyDistinctNew[uint32](old, Range{Start: 0, End: len(old)}, new_, Range{Start: 0, End: len(new_)})
		ops = CaptureDiffDeadline(
			self.algorithm,
			ih.OldLookup(),
			ih.OldRange(),
			ih.NewLookup(),
			ih.NewRange(),
			deadline,
		)
	} else {
		ops = CaptureDiffDeadline(
			self.algorithm,
			old,
			Range{Start: 0, End: len(old)},
			new_,
			Range{Start: 0, End: len(new_)},
			deadline,
		)
	}
	return &TextDiff{
		old:               old,
		new:               new_,
		ops:               ops,
		newlineTerminated: self.newlineTerminated.UnwrapOr(newlineTerminated),
		algorithm:         self.algorithm,
	}
}

// Captures diff op codes for textual diffs.
//
// The exact diff behavior is depending on the underlying [`DiffableStr`].
// For instance diffs on bytes and strings are slightly different.  You can
// create a text diff from constructors such as [`TextDiff::from_lines`] or
// the [`TextDiffConfig`] created by [`TextDiff::configure`].
//
// Requires the `text` feature.
type TextDiff struct {
	old               []string
	new               []string
	ops               []DiffOp
	newlineTerminated bool
	algorithm         Algorithm
}

// Configures a text differ before diffing.
func TextDiffConfigure() *TextDiffConfig {
	return &TextDiffConfig{
		algorithm:         AlgorithmDefault(),
		newlineTerminated: rs.None[bool](),
		deadline:          rs.None[textDeadline](),
	}
}

// Creates a diff of lines.
//
// For more information see [`TextDiffConfig::diff_lines`].
func TextDiffFromLines(old string, new_ string) *TextDiff {
	return TextDiffConfigure().DiffLines(old, new_)
}

// Creates a diff of words.
//
// For more information see [`TextDiffConfig::diff_words`].
func TextDiffFromWords(old string, new_ string) *TextDiff {
	return TextDiffConfigure().DiffWords(old, new_)
}

// Creates a diff of chars.
//
// For more information see [`TextDiffConfig::diff_chars`].
func TextDiffFromChars(old string, new_ string) *TextDiff {
	return TextDiffConfigure().DiffChars(old, new_)
}

// Creates a diff of arbitrary slices.
//
// For more information see [`TextDiffConfig::diff_slices`].
func TextDiffFromSlices(old []string, new_ []string) *TextDiff {
	return TextDiffConfigure().DiffSlices(old, new_)
}

// The name of the algorithm that created the diff.
func (self *TextDiff) Algorithm() Algorithm {
	return self.algorithm
}

// Returns `true` if items in the slice are newline terminated.
//
// This flag is used by the unified diff writer to determine if extra
// newlines have to be added.
func (self *TextDiff) NewlineTerminated() bool {
	return self.newlineTerminated
}

// Returns all old slices.
func (self *TextDiff) OldSlices() []string {
	return self.old
}

// Returns all new slices.
func (self *TextDiff) NewSlices() []string {
	return self.new
}

// Return a measure of the sequences' similarity in the range `0..=1`.
//
// A ratio of `1.0` means the two sequences are a complete match, a
// ratio of `0.0` would indicate completely distinct sequences.
//
//	diff := similar.TextDiffFromChars("abcd", "bcde")
//	// diff.Ratio() is 0.75
func (self *TextDiff) Ratio() float32 {
	return GetDiffRatio(self.Ops(), len(self.old), len(self.new))
}

// Iterates over the changes the op expands to.
//
// This method is a convenient way to automatically resolve the different
// ways in which a change could be encoded (insert/delete vs replace), look
// up the value from the appropriate slice and also handle correct index
// handling.
func (self *TextDiff) IterChanges(op DiffOp) *IterChangesIter[string] {
	return DiffOpIterChanges(op, self.OldSlices(), self.NewSlices())
}

// Returns the captured diff ops.
func (self *TextDiff) Ops() []DiffOp {
	return self.ops
}

// Isolate change clusters by eliminating ranges with no changes.
//
// This is equivalent to calling [`group_diff_ops`] on [`TextDiff::ops`].
func (self *TextDiff) GroupedOps(n int) [][]DiffOp {
	return GroupDiffOps(slices.Clone(self.Ops()), n)
}

// Flattens out the diff into all changes.
//
// This is a shortcut for combining [`TextDiff::ops`] with
// [`TextDiff::iter_changes`].
func (self *TextDiff) IterAllChanges() *IterAllChangesIter[string] {
	return iterAllChangesIterNew(self.old, self.new, self.Ops())
}

// Utility to return a unified diff formatter.
func (self *TextDiff) UnifiedDiff() *UdiffUnifiedDiff {
	return UdiffUnifiedDiffFromTextDiff(self)
}
