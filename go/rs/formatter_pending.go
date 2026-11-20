package rs

// Formatter is Rust's fmt::Formatter: where a FmtDisplay or FmtDebug method
// writes, and where it reads the flags of the `{}` it is filling in. Its
// methods keep Rust's names: WriteStr, WriteFmt (for `write!(f, ..)`), Pad,
// PadIntegral, Alternate, Width, Precision, Fill, Align, SignPlus,
// DebugStruct, DebugTuple, DebugList, DebugSet, DebugMap.
type Formatter struct {
	pending struct{}
}
