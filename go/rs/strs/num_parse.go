// Port of core 1.99.1 src/num/mod.rs (from_str_radix and FromStr for the
// integer types) and src/num/error.rs

package strs

import (
	"math"
	"math/bits"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// `s.parse::<T>()` has one function for each integer type, named after the
// Rust type: ParseU8 .. ParseU64, ParseUsize, ParseI8 .. ParseI64,
// ParseIsize. (One generic Parse[T] would not do: usize and isize are both
// Go's int and do not parse alike.) `T::from_str_radix(s, radix)` is
// U8FromStrRadix and so on.
//
// What Rust accepts is narrower than strconv: an optional `+`, or `-` for a
// signed type, then digits and nothing else. No whitespace, no `_`, no `0x`
// prefix; `-0` is an error for an unsigned type.

// IntErrorKind is num::IntErrorKind: why parsing an integer failed.
type IntErrorKind uint8

const (
	// IntErrorKind_Empty: the value being parsed is empty.
	IntErrorKind_Empty IntErrorKind = iota
	// IntErrorKind_InvalidDigit: the string contains an invalid digit in its
	// context: a char that is not a digit of the radix, or a `+` or `-` on
	// its own or misplaced.
	IntErrorKind_InvalidDigit
	// IntErrorKind_PosOverflow: the integer is too large to store in the
	// target integer type.
	IntErrorKind_PosOverflow
	// IntErrorKind_NegOverflow: the integer is too small to store in the
	// target integer type.
	IntErrorKind_NegOverflow
	// IntErrorKind_Zero: the value was zero, for a non-zero type. No parser
	// in this package returns it.
	IntErrorKind_Zero
)

var _ = rs.RegisterEnum[IntErrorKind]("IntErrorKind",
	rs.Const(IntErrorKind_Empty, "Empty"),
	rs.Const(IntErrorKind_InvalidDigit, "InvalidDigit"),
	rs.Const(IntErrorKind_PosOverflow, "PosOverflow"),
	rs.Const(IntErrorKind_NegOverflow, "NegOverflow"),
	rs.Const(IntErrorKind_Zero, "Zero"),
)

func (k IntErrorKind) name() string {
	switch k {
	case IntErrorKind_Empty:
		return "Empty"
	case IntErrorKind_InvalidDigit:
		return "InvalidDigit"
	case IntErrorKind_PosOverflow:
		return "PosOverflow"
	case IntErrorKind_NegOverflow:
		return "NegOverflow"
	default:
		return "Zero"
	}
}

// ParseIntError is num::ParseIntError: the error of the integer parsers.
type ParseIntError struct {
	kind IntErrorKind
}

// Kind is ParseIntError::kind: the detailed cause of the failure.
func (e *ParseIntError) Kind() IntErrorKind { return e.kind }

// String is the error's Display text.
func (e *ParseIntError) String() string {
	switch e.kind {
	case IntErrorKind_Empty:
		return "cannot parse integer from empty string"
	case IntErrorKind_InvalidDigit:
		return "invalid digit found in string"
	case IntErrorKind_PosOverflow:
		return "number too large to fit in target type"
	case IntErrorKind_NegOverflow:
		return "number too small to fit in target type"
	default:
		return "number would be zero for non-zero type"
	}
}

// Error makes a ParseIntError a Go error.
func (e *ParseIntError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseIntError`.
func (e *ParseIntError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`: `ParseIntError { kind: InvalidDigit }`.
func (e *ParseIntError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ParseIntError").Field("kind", e.kind).Finish()
}

func (e *ParseIntError) debugString() string {
	return "ParseIntError { kind: " + e.kind.name() + " }"
}

// checkRadix is the radix check of from_ascii_bytes_radix_impl. skip is the
// number of frames between it and the caller's caller, for the panic's
// location.
func checkRadix(radix uint32, skip int) {
	if 2 > radix || radix > 36 {
		rs.PanicAt(skip+1, "from_ascii_bytes_radix: radix must lie in the range `[2, 36]` - found "+
			strconv.FormatUint(uint64(radix), 10))
	}
}

// byteToDigit is `(*c as char).to_digit(radix)` for a radix already checked.
func byteToDigit(c byte, radix uint32) (uint32, bool) {
	var value uint32
	if c > '9' && radix > 10 {
		value = ((uint32(c) - 'A') &^ 0b0010_0000) + 10
	} else {
		value = uint32(c) - '0'
	}
	return value, value < radix
}

// splitSign is the head of from_ascii_bytes_radix_impl: the sign and the
// digits of src.
func splitSign(src string, isSignedTy bool) (isPositive bool, digits string, kind IntErrorKind, ok bool) {
	if src == "" {
		return false, "", IntErrorKind_Empty, false
	}
	switch {
	case src == "+" || src == "-":
		return false, "", IntErrorKind_InvalidDigit, false
	case src[0] == '+':
		return true, src[1:], 0, true
	case src[0] == '-' && isSignedTy:
		return false, src[1:], 0, true
	default:
		return true, src, 0, true
	}
}

// parseMagnitude is the digit loop of from_ascii_bytes_radix_impl, run on
// the magnitude: limit is the largest magnitude the type holds with this
// sign, and overflow the kind reported beyond it.
//
// Rust has two loops, one unchecked for strings too short to overflow; they
// agree wherever the unchecked one runs. The order of the checks is Rust's:
// a char that is not a digit is reported before the overflow of the digits
// that came before it, and after the overflow of an earlier digit.
func parseMagnitude(digits string, radix uint32, limit uint64, overflow IntErrorKind) (uint64, IntErrorKind, bool) {
	var result uint64
	for i := 0; i < len(digits); i++ {
		hi, mul := bits.Mul64(result, uint64(radix))
		mulOk := hi == 0 && mul <= limit
		x, isDigit := byteToDigit(digits[i], radix)
		if !isDigit {
			return 0, IntErrorKind_InvalidDigit, false
		}
		if !mulOk {
			return 0, overflow, false
		}
		sum, carry := bits.Add64(mul, uint64(x), 0)
		if carry != 0 || sum > limit {
			return 0, overflow, false
		}
		result = sum
	}
	return result, 0, true
}

type unsignedInteger interface {
	~uint8 | ~uint16 | ~uint32 | ~uint64
}

type signedInteger interface {
	~int8 | ~int16 | ~int32 | ~int64 | ~int
}

// parseUnsigned is from_ascii_bytes_radix_impl for an unsigned type of the
// given width.
func parseUnsigned[T unsignedInteger](src string, radix uint32, width int) (T, error) {
	_, digits, kind, ok := splitSign(src, false)
	if !ok {
		return 0, &ParseIntError{kind: kind}
	}
	limit := uint64(math.MaxUint64) >> (64 - width)
	magnitude, kind, ok := parseMagnitude(digits, radix, limit, IntErrorKind_PosOverflow)
	if !ok {
		return 0, &ParseIntError{kind: kind}
	}
	return T(magnitude), nil
}

// parseSigned is from_ascii_bytes_radix_impl for a signed type of the given
// width.
func parseSigned[T signedInteger](src string, radix uint32, width int) (T, error) {
	isPositive, digits, kind, ok := splitSign(src, true)
	if !ok {
		return 0, &ParseIntError{kind: kind}
	}
	if isPositive {
		limit := uint64(1)<<(width-1) - 1
		magnitude, kind, ok := parseMagnitude(digits, radix, limit, IntErrorKind_PosOverflow)
		if !ok {
			return 0, &ParseIntError{kind: kind}
		}
		return T(magnitude), nil
	}
	limit := uint64(1) << (width - 1)
	magnitude, kind, ok := parseMagnitude(digits, radix, limit, IntErrorKind_NegOverflow)
	if !ok {
		return 0, &ParseIntError{kind: kind}
	}
	return T(-int64(magnitude)), nil
}

// U8FromStrRadix is u8::from_str_radix. It panics when radix is not between
// 2 and 36.
func U8FromStrRadix(src string, radix uint32) (uint8, error) {
	checkRadix(radix, 1)
	return parseUnsigned[uint8](src, radix, 8)
}

// U16FromStrRadix is u16::from_str_radix.
func U16FromStrRadix(src string, radix uint32) (uint16, error) {
	checkRadix(radix, 1)
	return parseUnsigned[uint16](src, radix, 16)
}

// U32FromStrRadix is u32::from_str_radix.
func U32FromStrRadix(src string, radix uint32) (uint32, error) {
	checkRadix(radix, 1)
	return parseUnsigned[uint32](src, radix, 32)
}

// U64FromStrRadix is u64::from_str_radix.
func U64FromStrRadix(src string, radix uint32) (uint64, error) {
	checkRadix(radix, 1)
	return parseUnsigned[uint64](src, radix, 64)
}

// UsizeFromStrRadix is usize::from_str_radix. A usize is a Go int in this
// port, so a value above the largest int, which Rust would accept, is
// reported as too large.
func UsizeFromStrRadix(src string, radix uint32) (int, error) {
	checkRadix(radix, 1)
	return parseUsize(src, radix)
}

func parseUsize(src string, radix uint32) (int, error) {
	value, err := parseUnsigned[uint64](src, radix, bits.UintSize)
	if err != nil {
		return 0, err
	}
	if value > math.MaxInt {
		return 0, &ParseIntError{kind: IntErrorKind_PosOverflow}
	}
	return int(value), nil
}

// I8FromStrRadix is i8::from_str_radix.
func I8FromStrRadix(src string, radix uint32) (int8, error) {
	checkRadix(radix, 1)
	return parseSigned[int8](src, radix, 8)
}

// I16FromStrRadix is i16::from_str_radix.
func I16FromStrRadix(src string, radix uint32) (int16, error) {
	checkRadix(radix, 1)
	return parseSigned[int16](src, radix, 16)
}

// I32FromStrRadix is i32::from_str_radix.
func I32FromStrRadix(src string, radix uint32) (int32, error) {
	checkRadix(radix, 1)
	return parseSigned[int32](src, radix, 32)
}

// I64FromStrRadix is i64::from_str_radix.
func I64FromStrRadix(src string, radix uint32) (int64, error) {
	checkRadix(radix, 1)
	return parseSigned[int64](src, radix, 64)
}

// IsizeFromStrRadix is isize::from_str_radix.
func IsizeFromStrRadix(src string, radix uint32) (int, error) {
	checkRadix(radix, 1)
	return parseSigned[int](src, radix, bits.UintSize)
}

// ParseU8 is `s.parse::<u8>()` and u8::from_str.
func ParseU8(s string) (uint8, error) { return parseUnsigned[uint8](s, 10, 8) }

// ParseU16 is `s.parse::<u16>()` and u16::from_str.
func ParseU16(s string) (uint16, error) { return parseUnsigned[uint16](s, 10, 16) }

// ParseU32 is `s.parse::<u32>()` and u32::from_str.
func ParseU32(s string) (uint32, error) { return parseUnsigned[uint32](s, 10, 32) }

// ParseU64 is `s.parse::<u64>()` and u64::from_str.
func ParseU64(s string) (uint64, error) { return parseUnsigned[uint64](s, 10, 64) }

// ParseUsize is `s.parse::<usize>()` and usize::from_str. A usize is a Go
// int in this port, so a value above the largest int, which Rust would
// accept, is reported as too large.
func ParseUsize(s string) (int, error) { return parseUsize(s, 10) }

// ParseI8 is `s.parse::<i8>()` and i8::from_str.
func ParseI8(s string) (int8, error) { return parseSigned[int8](s, 10, 8) }

// ParseI16 is `s.parse::<i16>()` and i16::from_str.
func ParseI16(s string) (int16, error) { return parseSigned[int16](s, 10, 16) }

// ParseI32 is `s.parse::<i32>()` and i32::from_str.
func ParseI32(s string) (int32, error) { return parseSigned[int32](s, 10, 32) }

// ParseI64 is `s.parse::<i64>()` and i64::from_str.
func ParseI64(s string) (int64, error) { return parseSigned[int64](s, 10, 64) }

// ParseIsize is `s.parse::<isize>()` and isize::from_str.
func ParseIsize(s string) (int, error) { return parseSigned[int](s, 10, bits.UintSize) }

// parseMagnitude128 is parseMagnitude for a 128-bit type: the magnitude and
// the limit are (high, low) pairs of 64-bit halves.
func parseMagnitude128(digits string, radix uint32, limitHi, limitLo uint64, overflow IntErrorKind) (hi, lo uint64, kind IntErrorKind, ok bool) {
	for i := 0; i < len(digits); i++ {
		// result * radix, as a 192-bit product whose top word must be zero.
		carry, mulLo := bits.Mul64(lo, uint64(radix))
		top, mulHi := bits.Mul64(hi, uint64(radix))
		mulHi, carryOut := bits.Add64(mulHi, carry, 0)
		top += carryOut
		mulOk := top == 0 && (mulHi < limitHi || (mulHi == limitHi && mulLo <= limitLo))
		x, isDigit := byteToDigit(digits[i], radix)
		if !isDigit {
			return 0, 0, IntErrorKind_InvalidDigit, false
		}
		if !mulOk {
			return 0, 0, overflow, false
		}
		sumLo, carryLo := bits.Add64(mulLo, uint64(x), 0)
		sumHi, carryHi := bits.Add64(mulHi, 0, carryLo)
		if carryHi != 0 || sumHi > limitHi || (sumHi == limitHi && sumLo > limitLo) {
			return 0, 0, overflow, false
		}
		hi, lo = sumHi, sumLo
	}
	return hi, lo, 0, true
}

func parseU128(src string, radix uint32) (hi, lo uint64, err error) {
	_, digits, kind, ok := splitSign(src, false)
	if !ok {
		return 0, 0, &ParseIntError{kind: kind}
	}
	hi, lo, kind, ok = parseMagnitude128(digits, radix, math.MaxUint64, math.MaxUint64, IntErrorKind_PosOverflow)
	if !ok {
		return 0, 0, &ParseIntError{kind: kind}
	}
	return hi, lo, nil
}

func parseI128(src string, radix uint32) (hi int64, lo uint64, err error) {
	isPositive, digits, kind, ok := splitSign(src, true)
	if !ok {
		return 0, 0, &ParseIntError{kind: kind}
	}
	if isPositive {
		magHi, magLo, kind, ok := parseMagnitude128(digits, radix, math.MaxInt64, math.MaxUint64, IntErrorKind_PosOverflow)
		if !ok {
			return 0, 0, &ParseIntError{kind: kind}
		}
		return int64(magHi), magLo, nil
	}
	magHi, magLo, kind, ok := parseMagnitude128(digits, radix, 1<<63, 0, IntErrorKind_NegOverflow)
	if !ok {
		return 0, 0, &ParseIntError{kind: kind}
	}
	// Two's complement negation of the 128-bit magnitude.
	negLo, borrow := bits.Sub64(0, magLo, 0)
	negHi, _ := bits.Sub64(0, magHi, borrow)
	return int64(negHi), negLo, nil
}

// ParseU128Parts is `s.parse::<u128>()` and u128::from_str, with the value
// returned as its high and low 64-bit halves. (This port's 128-bit integer
// types, rs.U128 and rs.I128, did not exist when the package was written;
// wrap these once they do.)
func ParseU128Parts(s string) (hi, lo uint64, err error) { return parseU128(s, 10) }

// U128FromStrRadixParts is u128::from_str_radix, with the value returned as
// its high and low 64-bit halves.
func U128FromStrRadixParts(src string, radix uint32) (hi, lo uint64, err error) {
	checkRadix(radix, 1)
	return parseU128(src, radix)
}

// ParseI128Parts is `s.parse::<i128>()` and i128::from_str, with the value
// returned as the high (signed) and low (unsigned) 64-bit halves of its
// two's complement form.
func ParseI128Parts(s string) (hi int64, lo uint64, err error) { return parseI128(s, 10) }

// I128FromStrRadixParts is i128::from_str_radix, with the value returned as
// the high and low 64-bit halves of its two's complement form.
func I128FromStrRadixParts(src string, radix uint32) (hi int64, lo uint64, err error) {
	checkRadix(radix, 1)
	return parseI128(src, radix)
}
