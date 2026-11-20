package rs

// What the tests in package rs_test need from inside the package. The oracle
// tests must live there: go/rs/rstest/oracle imports this package.

// fmtTestInt128 is a 128-bit integer for the formatter's tests, which run
// before rs.U128 and rs.I128 exist or change.
type fmtTestInt128 struct {
	hi, lo uint64
	signed bool
}

func (v fmtTestInt128) fmtInt128() (hi, lo uint64, signed bool) { return v.hi, v.lo, v.signed }

// FmtTestInt128 returns a value the formatter prints as the u128 or i128
// with the given two's-complement halves.
func FmtTestInt128(hi, lo uint64, signed bool) any {
	return fmtTestInt128{hi: hi, lo: lo, signed: signed}
}

// FmtTestIsWhitespace is char::is_whitespace as the format-string parser
// uses it.
func FmtTestIsWhitespace(c rune) bool { return fmtCharIsWhitespace(c) }
