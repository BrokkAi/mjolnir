// Port of bytesize 2.7.0 src/parse.rs

package bytesize

import (
	"math"
	"math/bits"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// parseError is the `String` that `impl FromStr for ByteSize` returns as its
// error. Error and String give the text; it prints with `{}` and `{:?}` as a
// Rust String does.
type parseError string

func (self parseError) Error() string { return string(self) }

// String is the `Display` output: the text.
func (self parseError) String() string { return string(self) }

// FmtDisplay is `impl Display for String`.
func (self parseError) FmtDisplay(f *rs.Formatter) error { return f.Pad(string(self)) }

// FmtDebug is `impl Debug for String`.
func (self parseError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(string(self), f) }

// debugString is the `Debug` output: the text quoted and escaped.
func (self parseError) debugString() string { return parseDebugStr(string(self)) }

// ByteSizeFromStr is `impl FromStr for ByteSize`, which `text.parse::<ByteSize>()`
// calls: a whole number of bytes, or a number with an optional fraction
// followed by a unit (`1.5KiB`, `3 MB`, `8P`). The error is the crate's
// String: its Error text says what could not be parsed.
func ByteSizeFromStr(value string) (ByteSize, error) {
	if v, err := strs.U64FromStrRadix(value, 10); err == nil {
		return ByteSize(v), nil
	}
	number := parseTakeWhile(value, func(c rune) bool { return ('0' <= c && c <= '9') || c == '.' })
	v, err := strs.ParseF64(number)
	if err == nil {
		suffix := parseSkipWhile(value[len(number):], strs.IsWhitespace)
		u, unitErr := UnitFromStr(suffix)
		switch {
		// Use exact integer arithmetic when the number has no
		// fractional part. `f64` only has a 53-bit mantissa, so byte
		// counts at or above 2^53 would otherwise be rounded (e.g.
		// "9007199254740993B" parsed to 9007199254740992).
		case unitErr == nil && !strings.Contains(number, "."):
			if n, err := strs.U64FromStrRadix(number, 10); err == nil {
				return ByteSize(parseSaturatingMul(n, u.factor())), nil
			} else {
				return ByteSize(parseCastF64ToU64(u.MulF64(v))), nil
			}
		case unitErr == nil:
			return ByteSize(parseCastF64ToU64(u.MulF64(v))), nil
		default:
			// format!("couldn't parse {suffix:?} into a known SI unit, {error}")
			return 0, parseError("couldn't parse " + parseDebugStr(suffix) + " into a known SI unit, " + unitErr.Error())
		}
	} else {
		// format!("couldn't parse {value:?} into a ByteSize, {error}")
		return 0, parseError("couldn't parse " + parseDebugStr(value) + " into a ByteSize, " + err.Error())
	}
}

// parseTakeWhile is take_while: the longest prefix of s whose chars all
// satisfy predicate.
func parseTakeWhile(s string, predicate func(rune) bool) string {
	offset := 0
	for offset < len(s) {
		ch, size := utf8.DecodeRuneInString(s[offset:])
		if !predicate(ch) {
			break
		}
		offset += size
	}
	return s[:offset]
}

// parseSkipWhile is skip_while: s without its longest prefix whose chars all
// satisfy predicate.
func parseSkipWhile(s string, predicate func(rune) bool) string {
	return s[len(parseTakeWhile(s, predicate)):]
}

// parseSaturatingMul is u64::saturating_mul.
func parseSaturatingMul(a uint64, b uint64) uint64 {
	hi, lo := bits.Mul64(a, b)
	if hi != 0 {
		return math.MaxUint64
	}
	return lo
}

// parseCastF64ToU64 is `x as u64`: the value truncated toward zero, with NaN
// and anything below zero giving 0 and anything at or above 2^64 giving
// u64::MAX.
func parseCastF64ToU64(x float64) uint64 {
	switch {
	case x != x || x <= 0:
		return 0
	case x >= 18446744073709551616.0:
		return math.MaxUint64
	default:
		return uint64(x)
	}
}

// parseDebugStr is `format!("{:?}", s)` for a str: the text in double
// quotes, each char escaped as `impl Debug for str` escapes it. That is
// char::escape_debug except that a single quote is left alone.
func parseDebugStr(s string) string {
	var out strings.Builder
	out.WriteByte('"')
	for _, c := range s {
		if c == '\'' {
			out.WriteRune(c)
		} else {
			out.WriteString(strs.CharEscapeDebug(c))
		}
	}
	out.WriteByte('"')
	return out.String()
}

// Scale unit.
//
//	unit, _ := bytesize.UnitFromStr("GiB")   // bytesize.Unit_GibiByte
//	_, err := bytesize.UnitFromStr("gibibyte") // an error
type Unit uint8

const (
	// Single byte.
	Unit_Byte Unit = iota

	// power of tens

	// Kilobyte (10^3 bytes).
	Unit_KiloByte

	// Megabyte (10^6 bytes)
	Unit_MegaByte

	// Gigabyte (10^9 bytes)
	Unit_GigaByte

	// Terabyte (10^12 bytes)
	Unit_TeraByte

	// Petabyte (10^15 bytes)
	Unit_PetaByte

	// Exabyte (10^18 bytes)
	Unit_ExaByte

	// power of twos

	// Kibibyte (2^10 bytes)
	Unit_KibiByte

	// Mebibyte (2^20 bytes)
	Unit_MebiByte

	// Gibibyte (2^30 bytes)
	Unit_GibiByte

	// Tebibyte (2^40 bytes)
	Unit_TebiByte

	// Pebibyte (2^50 bytes)
	Unit_PebiByte

	// Exbibyte (2^60 bytes)
	Unit_ExbiByte
)

var _ = rs.RegisterEnum[Unit]("Unit",
	rs.Const(Unit_Byte, "Byte"),
	rs.Const(Unit_KiloByte, "KiloByte"),
	rs.Const(Unit_MegaByte, "MegaByte"),
	rs.Const(Unit_GigaByte, "GigaByte"),
	rs.Const(Unit_TeraByte, "TeraByte"),
	rs.Const(Unit_PetaByte, "PetaByte"),
	rs.Const(Unit_ExaByte, "ExaByte"),
	rs.Const(Unit_KibiByte, "KibiByte"),
	rs.Const(Unit_MebiByte, "MebiByte"),
	rs.Const(Unit_GibiByte, "GibiByte"),
	rs.Const(Unit_TebiByte, "TebiByte"),
	rs.Const(Unit_PebiByte, "PebiByte"),
	rs.Const(Unit_ExbiByte, "ExbiByte"),
)

func (self Unit) factor() uint64 {
	switch self {
	case Unit_Byte:
		return 1
	// decimal units
	case Unit_KiloByte:
		return KB
	case Unit_MegaByte:
		return MB
	case Unit_GigaByte:
		return GB
	case Unit_TeraByte:
		return TB
	case Unit_PetaByte:
		return PB
	case Unit_ExaByte:
		return EB
	// binary units
	case Unit_KibiByte:
		return KIB
	case Unit_MebiByte:
		return MIB
	case Unit_GibiByte:
		return GIB
	case Unit_TebiByte:
		return TIB
	case Unit_PebiByte:
		return PIB
	case Unit_ExbiByte:
		return EIB
	default:
		panic(rs.Unreachable(self))
	}
}

// mod impl_ops. Go has no operator overloading, so each pair of impls is a
// method; the operations are commutative.

// Add is `impl Add<u64> for Unit` and `impl Add<Unit> for u64`: the unit's
// factor plus other.
func (self Unit) Add(other uint64) uint64 {
	return self.factor() + other
}

// Mul is `impl Mul<u64> for Unit` and `impl Mul<Unit> for u64`: the unit's
// factor times other.
func (self Unit) Mul(other uint64) uint64 {
	return self.factor() * other
}

// AddF64 is `impl Add<f64> for Unit` and `impl Add<Unit> for f64`.
func (self Unit) AddF64(other float64) float64 {
	return float64(self.factor()) + other
}

// MulF64 is `impl Mul<f64> for Unit` and `impl Mul<Unit> for f64`.
func (self Unit) MulF64(other float64) float64 {
	return float64(self.factor()) * other
}

// UnitFromStr is `impl FromStr for Unit`: a unit symbol in any ASCII letter
// case (`b`, `k`, `kb`, `ki`, `kib`, ... `e`, `eb`, `ei`, `eib`). The error
// is a UnitParseError.
func UnitFromStr(unit string) (Unit, error) {
	switch {
	case strs.EqIgnoreAsciiCase(unit, "b"):
		return Unit_Byte, nil
	case strs.EqIgnoreAsciiCase(unit, "k") || strs.EqIgnoreAsciiCase(unit, "kb"):
		return Unit_KiloByte, nil
	case strs.EqIgnoreAsciiCase(unit, "m") || strs.EqIgnoreAsciiCase(unit, "mb"):
		return Unit_MegaByte, nil
	case strs.EqIgnoreAsciiCase(unit, "g") || strs.EqIgnoreAsciiCase(unit, "gb"):
		return Unit_GigaByte, nil
	case strs.EqIgnoreAsciiCase(unit, "t") || strs.EqIgnoreAsciiCase(unit, "tb"):
		return Unit_TeraByte, nil
	case strs.EqIgnoreAsciiCase(unit, "p") || strs.EqIgnoreAsciiCase(unit, "pb"):
		return Unit_PetaByte, nil
	case strs.EqIgnoreAsciiCase(unit, "e") || strs.EqIgnoreAsciiCase(unit, "eb"):
		return Unit_ExaByte, nil
	case strs.EqIgnoreAsciiCase(unit, "ki") || strs.EqIgnoreAsciiCase(unit, "kib"):
		return Unit_KibiByte, nil
	case strs.EqIgnoreAsciiCase(unit, "mi") || strs.EqIgnoreAsciiCase(unit, "mib"):
		return Unit_MebiByte, nil
	case strs.EqIgnoreAsciiCase(unit, "gi") || strs.EqIgnoreAsciiCase(unit, "gib"):
		return Unit_GibiByte, nil
	case strs.EqIgnoreAsciiCase(unit, "ti") || strs.EqIgnoreAsciiCase(unit, "tib"):
		return Unit_TebiByte, nil
	case strs.EqIgnoreAsciiCase(unit, "pi") || strs.EqIgnoreAsciiCase(unit, "pib"):
		return Unit_PebiByte, nil
	case strs.EqIgnoreAsciiCase(unit, "ei") || strs.EqIgnoreAsciiCase(unit, "eib"):
		return Unit_ExbiByte, nil
	default:
		return 0, UnitParseError(parseToStringTruncate(unit))
	}
}

// Safely truncates
func parseToStringTruncate(unit string) string {
	const maxUnitLen = 3

	if len(unit) > maxUnitLen {
		// TODO(MSRV 1.91): use ceil_char_boundary

		if parseIsCharBoundary(unit, 3) {
			return unit[:3] + "..."
		} else if parseIsCharBoundary(unit, 4) {
			return unit[:4] + "..."
		} else if parseIsCharBoundary(unit, 5) {
			return unit[:5] + "..."
		} else if parseIsCharBoundary(unit, 6) {
			return unit[:6] + "..."
		} else {
			panic(rs.Unreachable("char boundary will be within 4 bytes"))
		}
	} else {
		return unit
	}
}

// parseIsCharBoundary is str::is_char_boundary, with a byte that is not part
// of a valid UTF-8 encoding taken as a char of its own, as Go decodes it.
// For valid UTF-8, which is all a Rust str can hold, the two agree; for any
// other Go string a boundary is still found within 4 bytes, so the
// truncation above never reaches its unreachable arm.
func parseIsCharBoundary(s string, index int) bool {
	offset := 0
	for offset < index && offset < len(s) {
		_, size := utf8.DecodeRuneInString(s[offset:])
		offset += size
	}
	return offset == index
}

// Error returned when parsing a [`Unit`] fails.
type UnitParseError string

var _ = rs.RegisterNewtype[UnitParseError]("UnitParseError")

// Error makes a UnitParseError a Go error: the Display text.
func (self UnitParseError) Error() string {
	return self.String()
}

// String is the `Display` output: `Failed to parse unit "<the unit>"`, the
// unit cut after three bytes (rounded up to a whole char) and followed by
// `...` when it is longer.
func (self UnitParseError) String() string {
	return "Failed to parse unit \"" + string(self) + "\""
}

// FmtDisplay is `impl fmt::Display for UnitParseError`, which writes with
// `write!`: formatting options do not apply.
func (self UnitParseError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// debugString is the derived `Debug` output: `UnitParseError("foo...")`.
func (self UnitParseError) debugString() string {
	return "UnitParseError(" + parseDebugStr(string(self)) + ")"
}
