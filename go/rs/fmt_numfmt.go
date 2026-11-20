// Port of core 1.99.1 src/num/imp/fmt.rs

package rs

// Shared utilities used by both float and integer formatting.

type fmtNumPartKind uint8

const (
	// Given number of zero digits.
	fmtNumPartZero fmtNumPartKind = iota
	// A literal number up to 5 digits.
	fmtNumPartNum
	// A verbatim copy of given bytes.
	fmtNumPartCopy
)

// fmtNumPart is numfmt::Part: one piece of a formatted number.
type fmtNumPart struct {
	kind fmtNumPartKind
	zero int
	num  uint16
	copy string
}

func fmtNumPartOfZero(nzeroes int) fmtNumPart { return fmtNumPart{kind: fmtNumPartZero, zero: nzeroes} }
func fmtNumPartOfNum(v uint16) fmtNumPart     { return fmtNumPart{kind: fmtNumPartNum, num: v} }
func fmtNumPartOfCopy(buf string) fmtNumPart  { return fmtNumPart{kind: fmtNumPartCopy, copy: buf} }

// len returns the exact byte length of given part.
func (p *fmtNumPart) len() int {
	switch p.kind {
	case fmtNumPartZero:
		return p.zero
	case fmtNumPartNum:
		// v.checked_ilog10().unwrap_or_default() as usize + 1
		switch {
		case p.num < 10:
			return 1
		case p.num < 100:
			return 2
		case p.num < 1000:
			return 3
		case p.num < 10000:
			return 4
		default:
			return 5
		}
	default:
		return len(p.copy)
	}
}

// fmtNumFormatted is numfmt::Formatted: a formatted result containing one or
// more parts.
type fmtNumFormatted struct {
	// A string representing a sign, either `""`, `"-"` or `"+"`.
	sign string
	// Formatted parts to be rendered after a sign and optional zero padding.
	parts []fmtNumPart
}

// len returns the byte length of combined formatted result.
func (f *fmtNumFormatted) len() int {
	length := len(f.sign)
	for i := range f.parts {
		length += f.parts[i].len()
	}
	return length
}
