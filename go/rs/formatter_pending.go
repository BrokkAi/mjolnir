package rs

// This file fixes the Formatter's interface so that packages written against
// it compile. The bodies are placeholders until the formatter is written
// (fmt_*.go); then this file is deleted.

// Alignment is Rust's fmt::Alignment.
type Alignment uint8

const (
	// Alignment_Left is `<`.
	Alignment_Left Alignment = iota
	// Alignment_Right is `>`.
	Alignment_Right
	// Alignment_Center is `^`.
	Alignment_Center
)

// Formatter is Rust's fmt::Formatter: where a FmtDisplay or FmtDebug method
// writes, and where it reads the flags of the `{}` it is filling in. Methods
// keep Rust's names.
type Formatter struct {
	pending struct{}
}

// WriteStr is Formatter::write_str.
func (f *Formatter) WriteStr(s string) error { panic(Todo("core::fmt::Formatter")) }

// WriteChar is Formatter::write_char.
func (f *Formatter) WriteChar(c rune) error { panic(Todo("core::fmt::Formatter")) }

// WriteFmt is `write!(f, "...", args)` inside a formatting method.
func (f *Formatter) WriteFmt(format string, args ...any) error {
	panic(Todo("core::fmt::Formatter"))
}

// Pad is Formatter::pad: write s honouring width, fill, alignment and, as a
// string precision, truncation.
func (f *Formatter) Pad(s string) error { panic(Todo("core::fmt::Formatter")) }

// PadIntegral is Formatter::pad_integral.
func (f *Formatter) PadIntegral(isNonnegative bool, prefix string, digits string) error {
	panic(Todo("core::fmt::Formatter"))
}

// Alternate is Formatter::alternate: the `#` flag.
func (f *Formatter) Alternate() bool { panic(Todo("core::fmt::Formatter")) }

// Width is Formatter::width.
func (f *Formatter) Width() Option[int] { panic(Todo("core::fmt::Formatter")) }

// Precision is Formatter::precision.
func (f *Formatter) Precision() Option[int] { panic(Todo("core::fmt::Formatter")) }

// Fill is Formatter::fill.
func (f *Formatter) Fill() rune { panic(Todo("core::fmt::Formatter")) }

// Align is Formatter::align.
func (f *Formatter) Align() Option[Alignment] { panic(Todo("core::fmt::Formatter")) }

// SignPlus is Formatter::sign_plus.
func (f *Formatter) SignPlus() bool { panic(Todo("core::fmt::Formatter")) }

// SignMinus is Formatter::sign_minus.
func (f *Formatter) SignMinus() bool { panic(Todo("core::fmt::Formatter")) }

// SignAwareZeroPad is Formatter::sign_aware_zero_pad.
func (f *Formatter) SignAwareZeroPad() bool { panic(Todo("core::fmt::Formatter")) }

// DebugStruct is Formatter::debug_struct.
func (f *Formatter) DebugStruct(name string) *DebugStruct { panic(Todo("core::fmt::builders")) }

// DebugTuple is Formatter::debug_tuple.
func (f *Formatter) DebugTuple(name string) *DebugTuple { panic(Todo("core::fmt::builders")) }

// DebugList is Formatter::debug_list.
func (f *Formatter) DebugList() *DebugList { panic(Todo("core::fmt::builders")) }

// DebugSet is Formatter::debug_set.
func (f *Formatter) DebugSet() *DebugSet { panic(Todo("core::fmt::builders")) }

// DebugMap is Formatter::debug_map.
func (f *Formatter) DebugMap() *DebugMap { panic(Todo("core::fmt::builders")) }

// DebugStruct is Rust's fmt::DebugStruct builder.
type DebugStruct struct{ pending struct{} }

// Field is DebugStruct::field.
func (b *DebugStruct) Field(name string, value any) *DebugStruct {
	panic(Todo("core::fmt::builders"))
}

// FinishNonExhaustive is DebugStruct::finish_non_exhaustive.
func (b *DebugStruct) FinishNonExhaustive() error { panic(Todo("core::fmt::builders")) }

// Finish is DebugStruct::finish.
func (b *DebugStruct) Finish() error { panic(Todo("core::fmt::builders")) }

// DebugTuple is Rust's fmt::DebugTuple builder.
type DebugTuple struct{ pending struct{} }

// Field is DebugTuple::field.
func (b *DebugTuple) Field(value any) *DebugTuple { panic(Todo("core::fmt::builders")) }

// FinishNonExhaustive is DebugTuple::finish_non_exhaustive.
func (b *DebugTuple) FinishNonExhaustive() error { panic(Todo("core::fmt::builders")) }

// Finish is DebugTuple::finish.
func (b *DebugTuple) Finish() error { panic(Todo("core::fmt::builders")) }

// DebugList is Rust's fmt::DebugList builder.
type DebugList struct{ pending struct{} }

// Entry is DebugList::entry.
func (b *DebugList) Entry(value any) *DebugList { panic(Todo("core::fmt::builders")) }

// Entries is DebugList::entries over a slice, array or collection.
func (b *DebugList) Entries(values any) *DebugList { panic(Todo("core::fmt::builders")) }

// FinishNonExhaustive is DebugList::finish_non_exhaustive.
func (b *DebugList) FinishNonExhaustive() error { panic(Todo("core::fmt::builders")) }

// Finish is DebugList::finish.
func (b *DebugList) Finish() error { panic(Todo("core::fmt::builders")) }

// DebugSet is Rust's fmt::DebugSet builder.
type DebugSet struct{ pending struct{} }

// Entry is DebugSet::entry.
func (b *DebugSet) Entry(value any) *DebugSet { panic(Todo("core::fmt::builders")) }

// Entries is DebugSet::entries over a slice, array or collection.
func (b *DebugSet) Entries(values any) *DebugSet { panic(Todo("core::fmt::builders")) }

// Finish is DebugSet::finish.
func (b *DebugSet) Finish() error { panic(Todo("core::fmt::builders")) }

// DebugMap is Rust's fmt::DebugMap builder.
type DebugMap struct{ pending struct{} }

// Entry is DebugMap::entry.
func (b *DebugMap) Entry(key, value any) *DebugMap { panic(Todo("core::fmt::builders")) }

// Key is DebugMap::key.
func (b *DebugMap) Key(key any) *DebugMap { panic(Todo("core::fmt::builders")) }

// Value is DebugMap::value.
func (b *DebugMap) Value(value any) *DebugMap { panic(Todo("core::fmt::builders")) }

// Finish is DebugMap::finish.
func (b *DebugMap) Finish() error { panic(Todo("core::fmt::builders")) }

// DisplayFmt writes value to f as `{}` would with f's own flags: it is
// `fmt::Display::fmt(&value, f)`, the way a Display impl delegates to a field.
func DisplayFmt(value any, f *Formatter) error { panic(Todo("core::fmt")) }

// DebugFmt writes value to f as `{:?}` would with f's own flags: it is
// `fmt::Debug::fmt(&value, f)`.
func DebugFmt(value any, f *Formatter) error { panic(Todo("core::fmt")) }
