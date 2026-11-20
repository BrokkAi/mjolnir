package pulldowncmark

import "github.com/BrokkAi/mjolnir/go/rs"

// Range is Rust's `core::ops::Range<usize>`: the half-open interval
// `Start..End` of byte offsets into the Markdown source. OffsetIter pairs
// each event with one, and LinkDef and BrokenLink carry one as their span.
//
// It lives here because go/rs has no type for `Range` yet; its two fields
// have the names and order a shared `rs.Range[int]` would have, so a Go
// struct conversion turns one into the other.
type Range struct {
	Start int `rs:"start"`
	End   int `rs:"end"`
}

// Len is `ExactSizeIterator::len` of a `Range<usize>`: the number of offsets
// in the range, which is zero when Start is not below End.
func (r Range) Len() int {
	if r.Start < r.End {
		return r.End - r.Start
	}
	return 0
}

// IsEmpty is `Range::is_empty`.
func (r Range) IsEmpty() bool {
	return !(r.Start < r.End)
}

// FmtDebug is `impl Debug for Range<Idx>`: both ends with their own Debug,
// which sees the formatter's flags, around "..", as in `5..30`.
func (r Range) FmtDebug(f *rs.Formatter) error {
	if err := rs.DebugFmt(r.Start, f); err != nil {
		return err
	}
	if err := f.WriteStr(".."); err != nil {
		return err
	}
	return rs.DebugFmt(r.End, f)
}
