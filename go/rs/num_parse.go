// Port of core 1.99.1 src/num/mod.rs (from_str_radix) and src/num/error.rs
// (ParseIntError)

package rs

import "strconv"

// ParseIntError is core::num::ParseIntError: the error of parsing an integer
// with `from_str_radix` or `str::parse`.
type ParseIntError struct {
	kind IntErrorKind
}

// Kind is ParseIntError::kind.
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
	case IntErrorKind_Zero:
		return "number would be zero for non-zero type"
	case IntErrorKind_NotAPowerOfTwo:
		return "number is not a power of two"
	default:
		panic(Unreachable(uint8(e.kind)))
	}
}

// Error makes ParseIntError a Go error.
func (e *ParseIntError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseIntError`.
func (e *ParseIntError) FmtDisplay(f *Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug: `ParseIntError { kind: InvalidDigit }`.
func (e *ParseIntError) FmtDebug(f *Formatter) error {
	return f.DebugStruct("ParseIntError").Field("kind", e.kind).Finish()
}

// Equal is the derived PartialEq.
func (e *ParseIntError) Equal(other *ParseIntError) bool { return e.kind == other.kind }

// Clone is the derived Clone.
func (e *ParseIntError) Clone() *ParseIntError { return &ParseIntError{kind: e.kind} }

func parseIntError(kind IntErrorKind) error {
	return &ParseIntError{kind: kind}
}

// toDigit is `char::to_digit(radix)` for one byte of the input: the value
// of an ASCII digit or letter when it is a digit of the radix.
func toDigit(c byte, radix uint32) (uint32, bool) {
	var digit uint32
	switch {
	case c >= '0' && c <= '9':
		digit = uint32(c - '0')
	case c >= 'a' && c <= 'z':
		digit = uint32(c-'a') + 10
	case c >= 'A' && c <= 'Z':
		digit = uint32(c-'A') + 10
	default:
		return 0, false
	}
	return digit, digit < radix
}

// checkRadix panics as `from_ascii_bytes_radix_panic` does.
func checkRadix(skip int, radix uint32) {
	if radix < 2 || radix > 36 {
		PanicAt(skip+1, "from_ascii_bytes_radix: radix must lie in the range `[2, 36]` - found "+
			strconv.FormatUint(uint64(radix), 10))
	}
}

// fromStrRadix is `from_ascii_bytes_radix_impl`. The error for a digit that
// is not valid is found before the overflow of the multiplication that
// precedes it, as in Rust.
func fromStrRadix[T anyInt](skip int, src string, radix uint32) (T, error) {
	checkRadix(skip+1, radix)

	if len(src) == 0 {
		return 0, parseIntError(IntErrorKind_Empty)
	}

	isSignedTy := intIsSigned[T]()

	isPositive, digits := true, src
	switch {
	case src == "+" || src == "-":
		return 0, parseIntError(IntErrorKind_InvalidDigit)
	case src[0] == '+':
		digits = src[1:]
	case src[0] == '-' && isSignedTy:
		isPositive, digits = false, src[1:]
	}

	var result T
	base := T(radix)
	for index := 0; index < len(digits); index++ {
		mul, mulOverflow := overflowingMul(result, base)
		x, ok := toDigit(digits[index], radix)
		if !ok {
			return 0, parseIntError(IntErrorKind_InvalidDigit)
		}
		if isPositive {
			if mulOverflow {
				return 0, parseIntError(IntErrorKind_PosOverflow)
			}
			sum, overflow := overflowingAdd(mul, T(x))
			if overflow {
				return 0, parseIntError(IntErrorKind_PosOverflow)
			}
			result = sum
		} else {
			if mulOverflow {
				return 0, parseIntError(IntErrorKind_NegOverflow)
			}
			difference, overflow := overflowingSub(mul, T(x))
			if overflow {
				return 0, parseIntError(IntErrorKind_NegOverflow)
			}
			result = difference
		}
	}
	return result, nil
}

// U8FromStrRadix is `u8::from_str_radix(src, radix)`. It panics when radix
// is not in 2..=36.
func U8FromStrRadix(src string, radix uint32) (uint8, error) {
	return fromStrRadix[uint8](1, src, radix)
}

// U16FromStrRadix is `u16::from_str_radix(src, radix)`.
func U16FromStrRadix(src string, radix uint32) (uint16, error) {
	return fromStrRadix[uint16](1, src, radix)
}

// U32FromStrRadix is `u32::from_str_radix(src, radix)`.
func U32FromStrRadix(src string, radix uint32) (uint32, error) {
	return fromStrRadix[uint32](1, src, radix)
}

// U64FromStrRadix is `u64::from_str_radix(src, radix)`.
func U64FromStrRadix(src string, radix uint32) (uint64, error) {
	return fromStrRadix[uint64](1, src, radix)
}

// I8FromStrRadix is `i8::from_str_radix(src, radix)`.
func I8FromStrRadix(src string, radix uint32) (int8, error) {
	return fromStrRadix[int8](1, src, radix)
}

// I16FromStrRadix is `i16::from_str_radix(src, radix)`.
func I16FromStrRadix(src string, radix uint32) (int16, error) {
	return fromStrRadix[int16](1, src, radix)
}

// I32FromStrRadix is `i32::from_str_radix(src, radix)`.
func I32FromStrRadix(src string, radix uint32) (int32, error) {
	return fromStrRadix[int32](1, src, radix)
}

// I64FromStrRadix is `i64::from_str_radix(src, radix)`.
func I64FromStrRadix(src string, radix uint32) (int64, error) {
	return fromStrRadix[int64](1, src, radix)
}

// IsizeFromStrRadix is `isize::from_str_radix(src, radix)`.
func IsizeFromStrRadix(src string, radix uint32) (int, error) {
	return fromStrRadix[int](1, src, radix)
}

// UsizeFromStrRadix is `usize::from_str_radix(src, radix)`. A number above
// UsizeMax is too large, as one above usize::MAX is in Rust.
func UsizeFromStrRadix(src string, radix uint32) (int, error) {
	value, err := fromStrRadix[uint64](1, src, radix)
	if err != nil {
		return 0, err
	}
	if !usizeFits(value) {
		return 0, parseIntError(IntErrorKind_PosOverflow)
	}
	return int(value), nil
}

// U8FromStr is `u8::from_str(src)`, which is what `src.parse::<u8>()` calls.
func U8FromStr(src string) (uint8, error) { return fromStrRadix[uint8](1, src, 10) }

// U16FromStr is `u16::from_str(src)`.
func U16FromStr(src string) (uint16, error) { return fromStrRadix[uint16](1, src, 10) }

// U32FromStr is `u32::from_str(src)`.
func U32FromStr(src string) (uint32, error) { return fromStrRadix[uint32](1, src, 10) }

// U64FromStr is `u64::from_str(src)`.
func U64FromStr(src string) (uint64, error) { return fromStrRadix[uint64](1, src, 10) }

// I8FromStr is `i8::from_str(src)`.
func I8FromStr(src string) (int8, error) { return fromStrRadix[int8](1, src, 10) }

// I16FromStr is `i16::from_str(src)`.
func I16FromStr(src string) (int16, error) { return fromStrRadix[int16](1, src, 10) }

// I32FromStr is `i32::from_str(src)`.
func I32FromStr(src string) (int32, error) { return fromStrRadix[int32](1, src, 10) }

// I64FromStr is `i64::from_str(src)`.
func I64FromStr(src string) (int64, error) { return fromStrRadix[int64](1, src, 10) }

// IsizeFromStr is `isize::from_str(src)`.
func IsizeFromStr(src string) (int, error) { return fromStrRadix[int](1, src, 10) }

// UsizeFromStr is `usize::from_str(src)`.
func UsizeFromStr(src string) (int, error) { return UsizeFromStrRadix(src, 10) }
