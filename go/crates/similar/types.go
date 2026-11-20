// Port of similar 2.7.0 src/types.rs

package similar

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// An enum representing a diffing algorithm.
type Algorithm uint8

const (
	// Picks the myers algorithm from [`crate::algorithms::myers`]
	Algorithm_Myers Algorithm = iota
	// Picks the patience algorithm from [`crate::algorithms::patience`]
	Algorithm_Patience
	// Picks the LCS algorithm from [`crate::algorithms::lcs`]
	Algorithm_Lcs
)

var _ = rs.RegisterEnum[Algorithm]("Algorithm",
	rs.Const(Algorithm_Myers, "Myers"),
	rs.Const(Algorithm_Patience, "Patience"),
	rs.Const(Algorithm_Lcs, "Lcs"),
)

// Returns the default algorithm ([`Algorithm::Myers`]).
func AlgorithmDefault() Algorithm {
	return Algorithm_Myers
}

// The tag of a change.
type ChangeTag uint8

const (
	// The change indicates equality (not a change)
	ChangeTag_Equal ChangeTag = iota
	// The change indicates deleted text.
	ChangeTag_Delete
	// The change indicates inserted text.
	ChangeTag_Insert
)

var _ = rs.RegisterEnum[ChangeTag]("ChangeTag",
	rs.Const(ChangeTag_Equal, "Equal"),
	rs.Const(ChangeTag_Delete, "Delete"),
	rs.Const(ChangeTag_Insert, "Insert"),
)

// String is the `Display` output: the sign a unified diff puts before a line.
func (self ChangeTag) String() string {
	switch self {
	case ChangeTag_Equal:
		return " "
	case ChangeTag_Delete:
		return "-"
	case ChangeTag_Insert:
		return "+"
	default:
		panic(rs.Unreachable(self))
	}
}

// FmtDisplay is `impl fmt::Display for ChangeTag`. The Rust impl writes the
// sign with `write!`, so width, fill and alignment of the caller's `{}` do
// not apply.
func (self ChangeTag) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// Represents the expanded [`DiffOp`] change.
//
// This type is returned from [`DiffOp::iter_changes`] and
// [`TextDiff::iter_changes`](crate::text::TextDiff::iter_changes).
//
// It exists so that it's more convenient to work with textual differences as
// the underlying [`DiffOp`] encodes a group of changes.
//
// This type has additional methods that are only available for types
// implementing [`DiffableStr`](crate::text::DiffableStr).
//
// In Go those additional methods (AsStr, ToStringLossy, MissingNewline and
// the Display form) are declared for every T, because a Go method cannot be
// limited to one instantiation. They are for `Change[string]`; on a change
// over another type they panic, where Rust does not compile.
type Change[T any] struct {
	tag      ChangeTag      `rs:"tag"`
	oldIndex rs.Option[int] `rs:"old_index"`
	newIndex rs.Option[int] `rs:"new_index"`
	value    T              `rs:"value"`
}

var _ = rs.RegisterStruct[Change[string]]("Change")

// These methods are available for all change types.

// Returns the change tag.
func (self Change[T]) Tag() ChangeTag {
	return self.tag
}

// Returns the old index if available.
func (self Change[T]) OldIndex() rs.Option[int] {
	return self.oldIndex
}

// Returns the new index if available.
func (self Change[T]) NewIndex() rs.Option[int] {
	return self.newIndex
}

// Returns the underlying changed value.
//
// Depending on the type of the underlying [`crate::text::DiffableStr`]
// this value is more or less useful.  If you always want to have a utf-8
// string it's best to use the [`Change::as_str`] and
// [`Change::to_string_lossy`] methods.
func (self Change[T]) Value() T {
	return self.value
}

// Returns the underlying changed value as reference.
func (self *Change[T]) ValueRef() *T {
	return &self.value
}

// Returns the underlying changed value as mutable reference.
func (self *Change[T]) ValueMut() *T {
	return &self.value
}

// Utility enum to capture a diff operation.
//
// This is used by [`Capture`](crate::algorithms::Capture).
//
// A DiffOp is `Copy` in Rust. Here a value is a pointer to one of the variant
// structs, and this package never changes a variant struct after building it,
// so two DiffOp values may share one. Code that wants a changed op builds a
// new one.
type DiffOp interface {
	isDiffOp()

	// Returns the tag of the operation.
	Tag() DiffTag

	// Returns the old range.
	OldRange() Range

	// Returns the new range.
	NewRange() Range

	// Transform the op into a tuple of diff tag and ranges.
	//
	// This is useful when operating on slices.  The returned format is
	// `(tag, i1..i2, j1..j2)`:
	//
	// * `Replace`: `a[i1..i2]` should be replaced by `b[j1..j2]`
	// * `Delete`: `a[i1..i2]` should be deleted (`j1 == j2` in this case).
	// * `Insert`: `b[j1..j2]` should be inserted at `a[i1..i2]` (`i1 == i2` in this case).
	// * `Equal`: `a[i1..i2]` is equal to `b[j1..j2]`.
	AsTagTuple() (DiffTag, Range, Range)

	// Apply this operation to a diff hook.
	ApplyToHook(d AlgorithmsDiffHook) error
}

// A segment is equal (see [`DiffHook::equal`])
type DiffOp_Equal struct {
	// The starting index in the old sequence.
	OldIndex int `rs:"old_index"`
	// The starting index in the new sequence.
	NewIndex int `rs:"new_index"`
	// The length of the segment.
	Len int `rs:"len"`
}

// A segment was deleted (see [`DiffHook::delete`])
type DiffOp_Delete struct {
	// The starting index in the old sequence.
	OldIndex int `rs:"old_index"`
	// The length of the old segment.
	OldLen int `rs:"old_len"`
	// The starting index in the new sequence.
	NewIndex int `rs:"new_index"`
}

// A segment was inserted (see [`DiffHook::insert`])
type DiffOp_Insert struct {
	// The starting index in the old sequence.
	OldIndex int `rs:"old_index"`
	// The starting index in the new sequence.
	NewIndex int `rs:"new_index"`
	// The length of the new segment.
	NewLen int `rs:"new_len"`
}

// A segment was replaced (see [`DiffHook::replace`])
type DiffOp_Replace struct {
	// The starting index in the old sequence.
	OldIndex int `rs:"old_index"`
	// The length of the old segment.
	OldLen int `rs:"old_len"`
	// The starting index in the new sequence.
	NewIndex int `rs:"new_index"`
	// The length of the new segment.
	NewLen int `rs:"new_len"`
}

func (*DiffOp_Equal) isDiffOp()   {}
func (*DiffOp_Delete) isDiffOp()  {}
func (*DiffOp_Insert) isDiffOp()  {}
func (*DiffOp_Replace) isDiffOp() {}

var _ = rs.RegisterEnum[DiffOp]("DiffOp",
	rs.Variant[DiffOp_Equal]("Equal", rs.ShapeStruct),
	rs.Variant[DiffOp_Delete]("Delete", rs.ShapeStruct),
	rs.Variant[DiffOp_Insert]("Insert", rs.ShapeStruct),
	rs.Variant[DiffOp_Replace]("Replace", rs.ShapeStruct),
)

// The tag of a diff operation.
type DiffTag uint8

const (
	// The diff op encodes an equal segment.
	DiffTag_Equal DiffTag = iota
	// The diff op encodes a deleted segment.
	DiffTag_Delete
	// The diff op encodes an inserted segment.
	DiffTag_Insert
	// The diff op encodes a replaced segment.
	DiffTag_Replace
)

var _ = rs.RegisterEnum[DiffTag]("DiffTag",
	rs.Const(DiffTag_Equal, "Equal"),
	rs.Const(DiffTag_Delete, "Delete"),
	rs.Const(DiffTag_Insert, "Insert"),
	rs.Const(DiffTag_Replace, "Replace"),
)

// The methods of `impl DiffOp`. Each variant forwards to the function that
// holds the Rust method's `match`.

func (self *DiffOp_Equal) Tag() DiffTag   { return diffOpTag(self) }
func (self *DiffOp_Delete) Tag() DiffTag  { return diffOpTag(self) }
func (self *DiffOp_Insert) Tag() DiffTag  { return diffOpTag(self) }
func (self *DiffOp_Replace) Tag() DiffTag { return diffOpTag(self) }

func (self *DiffOp_Equal) OldRange() Range   { return diffOpOldRange(self) }
func (self *DiffOp_Delete) OldRange() Range  { return diffOpOldRange(self) }
func (self *DiffOp_Insert) OldRange() Range  { return diffOpOldRange(self) }
func (self *DiffOp_Replace) OldRange() Range { return diffOpOldRange(self) }

func (self *DiffOp_Equal) NewRange() Range   { return diffOpNewRange(self) }
func (self *DiffOp_Delete) NewRange() Range  { return diffOpNewRange(self) }
func (self *DiffOp_Insert) NewRange() Range  { return diffOpNewRange(self) }
func (self *DiffOp_Replace) NewRange() Range { return diffOpNewRange(self) }

func (self *DiffOp_Equal) AsTagTuple() (DiffTag, Range, Range)   { return diffOpAsTagTuple(self) }
func (self *DiffOp_Delete) AsTagTuple() (DiffTag, Range, Range)  { return diffOpAsTagTuple(self) }
func (self *DiffOp_Insert) AsTagTuple() (DiffTag, Range, Range)  { return diffOpAsTagTuple(self) }
func (self *DiffOp_Replace) AsTagTuple() (DiffTag, Range, Range) { return diffOpAsTagTuple(self) }

func (self *DiffOp_Equal) ApplyToHook(d AlgorithmsDiffHook) error {
	return diffOpApplyToHook(self, d)
}
func (self *DiffOp_Delete) ApplyToHook(d AlgorithmsDiffHook) error {
	return diffOpApplyToHook(self, d)
}
func (self *DiffOp_Insert) ApplyToHook(d AlgorithmsDiffHook) error {
	return diffOpApplyToHook(self, d)
}
func (self *DiffOp_Replace) ApplyToHook(d AlgorithmsDiffHook) error {
	return diffOpApplyToHook(self, d)
}

// Returns the tag of the operation.
func diffOpTag(self DiffOp) DiffTag {
	tag, _, _ := diffOpAsTagTuple(self)
	return tag
}

// Returns the old range.
func diffOpOldRange(self DiffOp) Range {
	_, old, _ := diffOpAsTagTuple(self)
	return old
}

// Returns the new range.
func diffOpNewRange(self DiffOp) Range {
	_, _, new_ := diffOpAsTagTuple(self)
	return new_
}

// Transform the op into a tuple of diff tag and ranges.
func diffOpAsTagTuple(self DiffOp) (DiffTag, Range, Range) {
	switch self := self.(type) {
	case *DiffOp_Equal:
		return DiffTag_Equal,
			Range{Start: self.OldIndex, End: self.OldIndex + self.Len},
			Range{Start: self.NewIndex, End: self.NewIndex + self.Len}
	case *DiffOp_Delete:
		return DiffTag_Delete,
			Range{Start: self.OldIndex, End: self.OldIndex + self.OldLen},
			Range{Start: self.NewIndex, End: self.NewIndex}
	case *DiffOp_Insert:
		return DiffTag_Insert,
			Range{Start: self.OldIndex, End: self.OldIndex},
			Range{Start: self.NewIndex, End: self.NewIndex + self.NewLen}
	case *DiffOp_Replace:
		return DiffTag_Replace,
			Range{Start: self.OldIndex, End: self.OldIndex + self.OldLen},
			Range{Start: self.NewIndex, End: self.NewIndex + self.NewLen}
	default:
		panic(rs.Unreachable(self))
	}
}

// Apply this operation to a diff hook.
func diffOpApplyToHook(self DiffOp, d AlgorithmsDiffHook) error {
	switch self := self.(type) {
	case *DiffOp_Equal:
		return d.Equal(self.OldIndex, self.NewIndex, self.Len)
	case *DiffOp_Delete:
		return d.Delete(self.OldIndex, self.OldLen, self.NewIndex)
	case *DiffOp_Insert:
		return d.Insert(self.OldIndex, self.NewIndex, self.NewLen)
	case *DiffOp_Replace:
		return d.Replace(self.OldIndex, self.OldLen, self.NewIndex, self.NewLen)
	default:
		panic(rs.Unreachable(self))
	}
}

// Iterates over all changes encoded in the diff op against old and new
// sequences.
//
// `old` and `new` are two indexable objects like the types you pass to
// the diffing algorithm functions.
//
// It is `DiffOp::iter_changes`, a function because a Go method cannot
// introduce the element type.
func DiffOpIterChanges[T any](self DiffOp, old []T, new_ []T) *IterChangesIter[T] {
	return iterChangesIterNew(old, new_, self)
}

// Given a diffop yields the changes it encodes against the given slices.
//
// This is similar to [`DiffOp::iter_changes`] but instead of yielding the
// individual changes it yields consequitive changed slices.
//
// This will only ever yield a single tuple or two tuples in case a
// [`DiffOp::Replace`] operation is passed.
//
// It is `DiffOp::iter_slices`, a function because a Go method cannot
// introduce the element type. The Rust iterator holds at most two items, so
// they are returned as a slice.
func DiffOpIterSlices[T any](self DiffOp, old []T, new_ []T) []rs.Tuple2[ChangeTag, []T] {
	switch self := self.(type) {
	case *DiffOp_Equal:
		return []rs.Tuple2[ChangeTag, []T]{
			rs.T2(ChangeTag_Equal, old[self.OldIndex:self.OldIndex+self.Len]),
		}
	case *DiffOp_Insert:
		return []rs.Tuple2[ChangeTag, []T]{
			rs.T2(ChangeTag_Insert, new_[self.NewIndex:self.NewIndex+self.NewLen]),
		}
	case *DiffOp_Delete:
		return []rs.Tuple2[ChangeTag, []T]{
			rs.T2(ChangeTag_Delete, old[self.OldIndex:self.OldIndex+self.OldLen]),
		}
	case *DiffOp_Replace:
		return []rs.Tuple2[ChangeTag, []T]{
			rs.T2(ChangeTag_Delete, old[self.OldIndex:self.OldIndex+self.OldLen]),
			rs.T2(ChangeTag_Insert, new_[self.NewIndex:self.NewIndex+self.NewLen]),
		}
	default:
		panic(rs.Unreachable(self))
	}
}

func diffOpIsEmpty(self DiffOp) bool {
	_, old, new_ := diffOpAsTagTuple(self)
	return algorithmsUtilsIsEmptyRange(old) && algorithmsUtilsIsEmptyRange(new_)
}

// The six functions below change an op in Rust. Here they return the changed
// op and the caller stores it, because a variant struct is never changed once
// another DiffOp value may point at it.

func diffOpShiftLeft(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(adjust, true), rs.T2(0, false))
}

func diffOpShiftRight(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(adjust, false), rs.T2(0, false))
}

func diffOpGrowLeft(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(adjust, true), rs.T2(adjust, false))
}

func diffOpGrowRight(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(0, false), rs.T2(adjust, false))
}

func diffOpShrinkLeft(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(0, false), rs.T2(adjust, true))
}

func diffOpShrinkRight(self DiffOp, adjust int) DiffOp {
	return diffOpAdjust(self, rs.T2(adjust, false), rs.T2(adjust, true))
}

func diffOpAdjust(self DiffOp, adjustOffset rs.Tuple2[int, bool], adjustLen rs.Tuple2[int, bool]) DiffOp {
	modify := func(val int, adj rs.Tuple2[int, bool]) int {
		if adj.F1 {
			return val - adj.F0
		}
		return val + adj.F0
	}

	switch self := self.(type) {
	case *DiffOp_Equal:
		return &DiffOp_Equal{
			OldIndex: modify(self.OldIndex, adjustOffset),
			NewIndex: modify(self.NewIndex, adjustOffset),
			Len:      modify(self.Len, adjustLen),
		}
	case *DiffOp_Delete:
		return &DiffOp_Delete{
			OldIndex: modify(self.OldIndex, adjustOffset),
			OldLen:   modify(self.OldLen, adjustLen),
			NewIndex: modify(self.NewIndex, adjustOffset),
		}
	case *DiffOp_Insert:
		return &DiffOp_Insert{
			OldIndex: modify(self.OldIndex, adjustOffset),
			NewIndex: modify(self.NewIndex, adjustOffset),
			NewLen:   modify(self.NewLen, adjustLen),
		}
	case *DiffOp_Replace:
		return &DiffOp_Replace{
			OldIndex: modify(self.OldIndex, adjustOffset),
			OldLen:   modify(self.OldLen, adjustLen),
			NewIndex: modify(self.NewIndex, adjustOffset),
			NewLen:   modify(self.NewLen, adjustLen),
		}
	default:
		panic(rs.Unreachable(self))
	}
}

// The text interface can produce changes over [`DiffableStr`] implementing
// values.  As those are generic interfaces for different types of strings
// utility methods to make working with standard rust strings more enjoyable.

// changeText returns the value of a text change. Only `str` implements
// `DiffableStr` in this port (the crate's `bytes` feature is off in Mjolnir).
func changeText[T any](self Change[T], method string) string {
	text, ok := any(self.value).(string)
	if !ok {
		rs.PanicAt(2, "similar::Change::"+method+" is only available for a change over text (Change[string])")
	}
	return text
}

// Returns the value as string if it is utf-8.
func (self Change[T]) AsStr() rs.Option[string] {
	return DiffableStrAsStr(changeText(self, "as_str"))
}

// Returns the value (lossy) decoded as utf-8 string.
func (self Change[T]) ToStringLossy() string {
	return DiffableStrToStringLossy(changeText(self, "to_string_lossy"))
}

// Returns `true` if this change does not end in a newline and must be
// followed up by one if line based diffs are used.
//
// The [`std::fmt::Display`] implementation of [`Change`] will automatically
// insert a newline after the value if this is true.
func (self Change[T]) MissingNewline() bool {
	return !DiffableStrEndsWithNewline(changeText(self, "missing_newline"))
}

// String is the `Display` output of a text change: the value, followed by a
// newline when the value does not end in one.
func (self Change[T]) String() string {
	text := changeText(self, "fmt")
	if !DiffableStrEndsWithNewline(text) {
		return DiffableStrToStringLossy(text) + "\n"
	}
	return DiffableStrToStringLossy(text)
}

// FmtDisplay is `impl fmt::Display for Change<&T>`. The Rust impl writes with
// `write!`, so width, fill and alignment of the caller's `{}` do not apply.
func (self Change[T]) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
