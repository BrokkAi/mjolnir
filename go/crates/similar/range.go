package similar

// Range is Rust's `core::ops::Range<usize>`: the half-open interval
// `Start..End`. The diff ops report their extents with it (`DiffOp.OldRange`,
// `DiffOp.NewRange`), and the algorithms take the part of each sequence to
// compare as one.
//
// It lives here because go/rs has no type for `Range` yet; its two fields
// have the names and order a shared `rs.Range[int]` would have, so a Go
// struct conversion turns one into the other.
type Range struct {
	Start int `rs:"start"`
	End   int `rs:"end"`
}

// Len is `ExactSizeIterator::len` of a `Range<usize>`: the number of indexes
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

// usizeSaturatingSub is `usize::saturating_sub`.
func usizeSaturatingSub(a int, b int) int {
	if a < b {
		return 0
	}
	return a - b
}
