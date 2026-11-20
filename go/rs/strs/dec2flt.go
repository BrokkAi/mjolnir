// Port of core 1.99.1 src/num/imp/dec2flt (mod.rs, parse.rs, common.rs,
// decimal.rs, lemire.rs, slow.rs, decimal_seq.rs), the float constants of
// src/num/imp/traits.rs, and src/num/float_parse.rs

package strs

import (
	"math"
	"math/bits"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// This is Rust's own decimal-to-float conversion rather than a call to
// strconv.ParseFloat. Both round correctly, so they agree on any number a
// person would write; they differ in what they accept (Go takes hex floats,
// underscores and "infinity" spelled with other letters), and in how they
// cut off absurd exponents, which changes the result for inputs of several
// thousand digits. The port takes the same path as Rust for every input: the
// exact fast path, then Eisel-Lemire, then the big-decimal fallback.
//
// # Notation
//
// This file uses the same notation as the Lemire paper:
//
//   - `m`: binary mantissa; always nonnegative
//   - `p`: binary exponent; a signed integer
//   - `w`: decimal significand; always nonnegative
//   - `q`: decimal exponent; a signed integer
//
// This gives `m * 2^p` for the binary floating-point number, with `w * 10^q`
// as the decimal equivalent.

// ParseFloatError is num::ParseFloatError: the error of ParseF32 and
// ParseF64.
type ParseFloatError struct {
	kind floatErrorKind
}

type floatErrorKind uint8

const (
	floatErrorKind_Empty floatErrorKind = iota
	floatErrorKind_Invalid
)

var _ = rs.RegisterEnum[floatErrorKind]("FloatErrorKind",
	rs.Const(floatErrorKind_Empty, "Empty"),
	rs.Const(floatErrorKind_Invalid, "Invalid"),
)

// String is the error's Display text.
func (e *ParseFloatError) String() string {
	switch e.kind {
	case floatErrorKind_Empty:
		return "cannot parse float from empty string"
	default:
		return "invalid float literal"
	}
}

// Error makes a ParseFloatError a Go error.
func (e *ParseFloatError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseFloatError`.
func (e *ParseFloatError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived `Debug`: `ParseFloatError { kind: Invalid }`.
func (e *ParseFloatError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ParseFloatError").Field("kind", e.kind).Finish()
}

func (e *ParseFloatError) debugString() string {
	if e.kind == floatErrorKind_Empty {
		return "ParseFloatError { kind: Empty }"
	}
	return "ParseFloatError { kind: Invalid }"
}

// ParseF64 is `s.parse::<f64>()` and f64::from_str: an optional sign, then
// `inf`, `infinity` or `nan` in any letter case, or decimal digits with an
// optional point and an optional exponent. Nothing else: no whitespace, no
// `_`, no hex. The result is the float nearest to the decimal number, and an
// infinity when it is out of range; NaN has the bits Rust's has.
func ParseF64(s string) (float64, error) {
	word, kind, ok := dec2flt(s, &dec2fltF64)
	if !ok {
		return 0, &ParseFloatError{kind: kind}
	}
	return math.Float64frombits(word), nil
}

// ParseF32 is `s.parse::<f32>()` and f32::from_str. The decimal number is
// rounded once, to the nearest f32; parsing an f64 and narrowing it can
// round twice and differ.
func ParseF32(s string) (float32, error) {
	word, kind, ok := dec2flt(s, &dec2fltF32)
	if !ok {
		return 0, &ParseFloatError{kind: kind}
	}
	return math.Float32frombits(uint32(word)), nil
}

// dec2fltFormat holds, for one float type, the constants of the traits Float
// and Lemire.
type dec2fltFormat struct {
	// bits is BITS: the width of the type.
	bits uint
	// sigTotalBits is SIG_TOTAL_BITS: the significand digits including the
	// implicit one.
	sigTotalBits uint
	// sigBits is SIG_BITS: the significand digits that are stored.
	sigBits uint
	// infinitePower is INFINITE_POWER: the saturated (maximum) biased
	// exponent, that of infinity and NaN.
	infinitePower int32
	// expMin is EXP_MIN: the minimum exponent of a normal number.
	expMin int32
	// Round-to-even only happens for negative values of q when
	// q >= minExponentRoundToEven and q <= maxExponentRoundToEven.
	minExponentRoundToEven int32
	maxExponentRoundToEven int32
	// largestPowerOfTen is LARGEST_POWER_OF_TEN: the largest exponent value
	// before overflow, `(w + 1) * 10^q >= 2^max_exp`.
	largestPowerOfTen int32
	// smallestPowerOfTen is SMALLEST_POWER_OF_TEN: the smallest decimal
	// exponent for a non-zero value.
	smallestPowerOfTen int32
	// maxExponentFastPath is MAX_EXPONENT_FAST_PATH: the maximum exponent for
	// a fast path case, or `⌊(SIG_BITS+1)/log2(5)⌋`.
	maxExponentFastPath int64
	// minExponentFastPath is MIN_EXPONENT_FAST_PATH.
	minExponentFastPath int64
	// maxExponentDisguisedFastPath is MAX_EXPONENT_DISGUISED_FAST_PATH.
	maxExponentDisguisedFastPath int64
	// maxMantissaFastPath is MAX_MANTISSA_FAST_PATH: `1 << SIG_TOTAL_BITS`.
	maxMantissaFastPath uint64
	// nan is the bit pattern of NAN.
	nan uint64
}

var dec2fltF32 = dec2fltFormat{
	bits:                         32,
	sigTotalBits:                 24,
	sigBits:                      23,
	infinitePower:                0xFF,
	expMin:                       -126,
	minExponentRoundToEven:       -17,
	maxExponentRoundToEven:       10,
	largestPowerOfTen:            38,
	smallestPowerOfTen:           -65,
	maxExponentFastPath:          10,
	minExponentFastPath:          -10,
	maxExponentDisguisedFastPath: 17,
	maxMantissaFastPath:          1 << 24,
	nan:                          0x7fc00000,
}

var dec2fltF64 = dec2fltFormat{
	bits:                         64,
	sigTotalBits:                 53,
	sigBits:                      52,
	infinitePower:                0x7FF,
	expMin:                       -1022,
	minExponentRoundToEven:       -4,
	maxExponentRoundToEven:       23,
	largestPowerOfTen:            308,
	smallestPowerOfTen:           -342,
	maxExponentFastPath:          22,
	minExponentFastPath:          -22,
	maxExponentDisguisedFastPath: 37,
	maxMantissaFastPath:          1 << 53,
	nan:                          0x7ff8000000000000,
}

// infinity is the bit pattern of INFINITY.
func (f *dec2fltFormat) infinity() uint64 {
	return uint64(f.infinitePower) << f.sigBits
}

// negate is unary minus on a bit pattern.
func (f *dec2fltFormat) negate(word uint64) uint64 {
	return word ^ (1 << (f.bits - 1))
}

// dec2fltPow10FastPathF32 is `<f32 as Lemire>::pow10_fast_path`'s table.
var dec2fltPow10FastPathF32 = [16]float32{1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 0, 0, 0, 0, 0}

// dec2fltPow10FastPathF64 is `<f64 as Lemire>::pow10_fast_path`'s table.
var dec2fltPow10FastPathF64 = [32]float64{
	1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
	1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 0, 0, 0, 0, 0, 0, 0, 0, 0,
}

// dec2fltBiasedFp is common::BiasedFp: a custom 64-bit floating point type,
// representing `m * 2^p`. p is biased, so it can be directly shifted into the
// exponent bits.
type dec2fltBiasedFp struct {
	// The significant digits.
	m uint64
	// The biased, binary exponent.
	pBiased int32
}

// biasedFpToFloat is biased_fp_to_float: it converts a BiasedFp to the bits
// of the closest machine float.
func (f *dec2fltFormat) biasedFpToFloat(x dec2fltBiasedFp) uint64 {
	word := x.m
	word |= uint64(x.pBiased) << f.sigBits
	return word
}

// dec2flt is dec2flt::dec2flt: it converts a decimal string into the bits of
// a floating point number.
func dec2flt(s string, f *dec2fltFormat) (uint64, floatErrorKind, bool) {
	if len(s) == 0 {
		return 0, floatErrorKind_Empty, false
	}
	c := s[0]
	negative := c == '-'
	if c == '-' || c == '+' {
		s = s[1:]
	}
	if len(s) == 0 {
		return 0, floatErrorKind_Invalid, false
	}

	num, ok := dec2fltParseNumber(s)
	if !ok {
		if value, ok := dec2fltParseInfNan(s, negative, f); ok {
			return value, 0, true
		}
		return 0, floatErrorKind_Invalid, false
	}
	num.negative = negative
	if value, ok := num.tryFastPath(f); ok {
		return value, 0, true
	}

	// If significant digits were truncated, then we can have rounding error
	// only if `mantissa + 1` produces a different result. We also avoid
	// redundantly using the Eisel-Lemire algorithm if it was unable to
	// correctly round on the first pass.
	fp := dec2fltComputeFloat(num.exponent, num.mantissa, f)
	if num.manyDigits && fp.pBiased >= 0 && fp != dec2fltComputeFloat(num.exponent, num.mantissa+1, f) {
		fp.pBiased = -1
	}
	// Unable to correctly round the float using the Eisel-Lemire algorithm.
	// Fallback to a slower, but always correct algorithm.
	if fp.pBiased < 0 {
		fp = dec2fltParseLongMantissa(s, f)
	}

	float := f.biasedFpToFloat(fp)
	if num.negative {
		float = f.negate(float)
	}
	return float, 0, true
}

// --- decimal.rs ---

var dec2fltIntPow10 = [16]uint64{
	1,
	10,
	100,
	1000,
	10000,
	100000,
	1000000,
	10000000,
	100000000,
	1000000000,
	10000000000,
	100000000000,
	1000000000000,
	10000000000000,
	100000000000000,
	1000000000000000,
}

// dec2fltDecimal is decimal::Decimal: a floating point number with up to 64
// bits of mantissa and an `i64` exponent.
type dec2fltDecimal struct {
	exponent   int64
	mantissa   uint64
	negative   bool
	manyDigits bool
}

// canUseFastPath is Decimal::can_use_fast_path: it detects if the float can
// be accurately reconstructed from native floats.
func (d *dec2fltDecimal) canUseFastPath(f *dec2fltFormat) bool {
	return f.minExponentFastPath <= d.exponent &&
		d.exponent <= f.maxExponentDisguisedFastPath &&
		d.mantissa <= f.maxMantissaFastPath &&
		!d.manyDigits
}

// tryFastPath is Decimal::try_fast_path: it tries turning the decimal into an
// exact float representation, using machine-sized integers and floats.
//
// This only works if both the mantissa and the exponent can be exactly
// represented as a machine float, since IEE-754 guarantees no rounding will
// occur.
//
// There is an exception: disguised fast-path cases, where we can shift
// powers-of-10 from the exponent to the significant digits.
//
// The arithmetic is done in the target type, one operation, so that it is
// rounded once, to the right number of bits.
func (d *dec2fltDecimal) tryFastPath(f *dec2fltFormat) (uint64, bool) {
	if !d.canUseFastPath(f) {
		return 0, false
	}

	mantissa := d.mantissa
	exponent := d.exponent
	if d.exponent > f.maxExponentFastPath {
		// disguised fast path
		shift := d.exponent - f.maxExponentFastPath
		hi, lo := bits.Mul64(d.mantissa, dec2fltIntPow10[shift])
		if hi != 0 {
			return 0, false
		}
		mantissa = lo
		if mantissa > f.maxMantissaFastPath {
			return 0, false
		}
		exponent = f.maxExponentFastPath
	}

	var word uint64
	if f.bits == 32 {
		value := float32(mantissa)
		if exponent < 0 {
			value = float32(value / dec2fltPow10FastPathF32[(-exponent)&15])
		} else {
			value = float32(value * dec2fltPow10FastPathF32[exponent&15])
		}
		word = uint64(math.Float32bits(value))
	} else {
		value := float64(mantissa)
		if exponent < 0 {
			value = float64(value / dec2fltPow10FastPathF64[(-exponent)&31])
		} else {
			value = float64(value * dec2fltPow10FastPathF64[exponent&31])
		}
		word = math.Float64bits(value)
	}
	if d.negative {
		word = f.negate(word)
	}
	return word, true
}

// --- parse.rs ---

const dec2fltMin19DigitInt uint64 = 100_0000_0000_0000_0000

// dec2fltTryParseDigits is try_parse_digits: it parses digits until a
// non-digit character is found. It may overflow, which is handled later.
// (Rust takes eight digits at a time when it can; the wrapped result is the
// same.)
func dec2fltTryParseDigits(s string, x uint64) (string, uint64) {
	for len(s) > 0 {
		digit := s[0] - '0'
		if digit >= 10 {
			break
		}
		x = x*10 + uint64(digit)
		s = s[1:]
	}
	return s, x
}

// dec2fltTryParse19Digits is try_parse_19digits: it parses up to 19 digits
// (the max that can be stored in a 64-bit integer).
func dec2fltTryParse19Digits(s string, x uint64) (string, uint64) {
	for x < dec2fltMin19DigitInt {
		if len(s) == 0 {
			break
		}
		digit := s[0] - '0'
		if digit >= 10 {
			break
		}
		x = (x * 10) + uint64(digit) // no overflows here
		s = s[1:]
	}
	return s, x
}

// dec2fltParseScientific is parse_scientific: it parses the scientific
// notation component of a float.
func dec2fltParseScientific(s string) (string, int64, bool) {
	exponent := int64(0)
	negative := false

	if len(s) > 0 {
		c := s[0]
		negative = c == '-'
		if c == '-' || c == '+' {
			s = s[1:]
		}
	}

	if len(s) > 0 && s[0] >= '0' && s[0] <= '9' {
		for len(s) > 0 {
			digit := s[0] - '0'
			if digit >= 10 {
				break
			}
			// no overflows here, saturate well before overflow
			if exponent < 0x10000 {
				exponent = 10*exponent + int64(digit)
			}
			s = s[1:]
		}
		if negative {
			return s, -exponent, true
		}
		return s, exponent, true
	}
	return s, 0, false
}

// dec2fltParsePartialNumber is parse_partial_number: it parses a partial,
// non-special floating point number.
//
// This creates a representation of the float as the significant digits and
// the decimal exponent, and returns how many bytes it read.
func dec2fltParsePartialNumber(s string) (dec2fltDecimal, int, bool) {
	// parse initial digits before dot
	mantissa := uint64(0)
	start := s
	s, mantissa = dec2fltTryParseDigits(s, mantissa)
	nDigits := len(start) - len(s)

	// handle dot with the following digits
	nAfterDot := 0
	exponent := int64(0)
	intEnd := s

	if len(s) > 0 && s[0] == '.' {
		s = s[1:]
		before := s
		s, mantissa = dec2fltTryParseDigits(s, mantissa)
		nAfterDot = len(before) - len(s)
		exponent = -int64(nAfterDot)
	}

	nDigits += nAfterDot
	if nDigits == 0 {
		return dec2fltDecimal{}, 0, false
	}

	// handle scientific format
	expNumber := int64(0)
	if len(s) > 0 && (s[0] == 'e' || s[0] == 'E') {
		s = s[1:]
		// If None, we have no trailing digits after exponent, or an invalid float.
		var ok bool
		s, expNumber, ok = dec2fltParseScientific(s)
		if !ok {
			return dec2fltDecimal{}, 0, false
		}
		exponent += expNumber
	}

	length := len(start) - len(s)

	// handle uncommon case with many digits
	if nDigits <= 19 {
		return dec2fltDecimal{exponent: exponent, mantissa: mantissa, negative: false, manyDigits: false}, length, true
	}

	nDigits -= 19
	manyDigits := false
	p := start
	for len(p) > 0 {
		c := p[0]
		if c == '.' || c == '0' {
			// c.saturating_sub(b'0' - 1): 1 for a zero, 0 for the point.
			if c == '0' {
				nDigits--
			}
			p = p[1:]
		} else {
			break
		}
	}
	if nDigits > 0 {
		// at this point we have more than 19 significant digits, let's try again
		manyDigits = true
		mantissa = 0
		s := start
		s, mantissa = dec2fltTryParse19Digits(s, mantissa)
		if mantissa >= dec2fltMin19DigitInt {
			// big int
			exponent = int64(len(s) - len(intEnd))
		} else {
			s = s[1:]
			before := s
			s, mantissa = dec2fltTryParse19Digits(s, mantissa)
			exponent = -int64(len(before) - len(s))
		}
		// add back the explicit part
		exponent += expNumber
	}

	return dec2fltDecimal{exponent: exponent, mantissa: mantissa, negative: false, manyDigits: manyDigits}, length, true
}

// dec2fltParseNumber is parse_number: it tries to parse a non-special
// floating point number that takes the whole of s.
func dec2fltParseNumber(s string) (dec2fltDecimal, bool) {
	if float, rest, ok := dec2fltParsePartialNumber(s); ok {
		if rest == len(s) {
			return float, true
		}
	}
	return dec2fltDecimal{}, false
}

// dec2fltParseInfNan is parse_inf_nan: it tries to parse a special,
// non-finite float.
func dec2fltParseInfNan(s string, negative bool, f *dec2fltFormat) (uint64, bool) {
	// Since a valid string has at most the length 8, we can load
	// all relevant characters into a u64 and work from there.
	var register uint64
	var length int

	// All valid strings are either of length 8 or 3.
	if len(s) == 8 {
		register = dec2fltReadU64(s)
		length = 8
	} else if len(s) == 3 {
		a := uint64(s[0])
		b := uint64(s[1])
		c := uint64(s[2])
		register = (c << 16) | (b << 8) | a
		length = 3
	} else {
		return 0, false
	}

	// Clear out the bits which turn ASCII uppercase characters into
	// lowercase characters. The resulting string is all uppercase.
	// What happens to other characters is irrelevant.
	register &= 0xDFDFDFDFDFDFDFDF

	// u64 values corresponding to relevant cases
	const inf3 uint64 = 0x464E49           // "INF"
	const inf8 uint64 = 0x5954494E49464E49 // "INFINITY"
	const nan uint64 = 0x4E414E            // "NAN"

	// Match register value to constant to parse string.
	// Also match on the string length to catch edge cases
	// like "inf\0\0\0\0\0".
	var float uint64
	switch {
	case register == inf3 && length == 3:
		float = f.infinity()
	case register == inf8 && length == 8:
		float = f.infinity()
	case register == nan && length == 3:
		float = f.nan
	default:
		return 0, false
	}

	if negative {
		return f.negate(float), true
	}
	return float, true
}

// --- common.rs ---

// dec2fltReadU64 is ByteSlice::read_u64: it reads 8 bytes as a 64-bit integer
// in little-endian order.
func dec2fltReadU64(s string) uint64 {
	return uint64(s[0]) | uint64(s[1])<<8 | uint64(s[2])<<16 | uint64(s[3])<<24 |
		uint64(s[4])<<32 | uint64(s[5])<<40 | uint64(s[6])<<48 | uint64(s[7])<<56
}

// --- lemire.rs ---

// dec2fltComputeFloat is compute_float: it computes w * 10^q using an
// extended-precision float representation.
//
// Fast conversion of a the significant digits and decimal exponent
// a float to an extended representation with a binary float. This
// algorithm will accurately parse the vast majority of cases,
// and uses a 128-bit representation (with a fallback 192-bit
// representation).
//
// This algorithm scales the exponent by the decimal exponent
// using pre-computed powers-of-5, and calculates if the
// representation can be unambiguously rounded to the nearest
// machine float. Near-halfway cases are not handled here,
// and are represented by a negative, biased binary exponent.
//
// The algorithm is described in detail in "Daniel Lemire, Number Parsing
// at a Gigabyte per Second" in section 5, "Fast Algorithm", and
// section 6, "Exact Numbers And Ties", available online:
// <https://arxiv.org/abs/2101.11408.pdf>.
func dec2fltComputeFloat(q int64, w uint64, f *dec2fltFormat) dec2fltBiasedFp {
	fpZero := dec2fltBiasedFp{m: 0, pBiased: 0}
	fpInf := dec2fltBiasedFp{m: 0, pBiased: f.infinitePower}
	fpError := dec2fltBiasedFp{m: 0, pBiased: -1}

	// Short-circuit if the value can only be a literal 0 or infinity.
	if w == 0 || q < int64(f.smallestPowerOfTen) {
		return fpZero
	} else if q > int64(f.largestPowerOfTen) {
		return fpInf
	}
	// Normalize our significant digits, so the most-significant bit is set.
	lz := bits.LeadingZeros64(w)
	w <<= lz
	lo, hi := dec2fltComputeProductApprox(q, w, int(f.sigBits)+3)
	if lo == 0xFFFF_FFFF_FFFF_FFFF {
		// If we have failed to approximate w x 5^-q with our 128-bit value.
		// Since the addition of 1 could lead to an overflow which could then
		// round up over the half-way point, this can lead to improper rounding
		// of a float.
		//
		// However, this can only occur if q ∈ [-27, 55]. The upper bound of q
		// is 55 because 5^55 < 2^128, however, this can only happen if 5^q > 2^64,
		// since otherwise the product can be represented in 64-bits, producing
		// an exact result. For negative exponents, rounding-to-even can
		// only occur if 5^-q < 2^64.
		//
		// For detailed explanations of rounding for negative exponents, see
		// <https://arxiv.org/pdf/2101.11408.pdf#section.9.1>. For detailed
		// explanations of rounding for positive exponents, see
		// <https://arxiv.org/pdf/2101.11408.pdf#section.8>.
		insideSafeExponent := (q >= -27) && (q <= 55)
		if !insideSafeExponent {
			return fpError
		}
	}
	upperbit := int32(hi >> 63)
	mantissa := hi >> uint(upperbit+64-int32(f.sigBits)-3)
	power2 := dec2fltPower(int32(q)) + upperbit - int32(lz) - f.expMin + 1
	if power2 <= 0 {
		if -power2+1 >= 64 {
			// Have more than 64 bits below the minimum exponent, must be 0.
			return fpZero
		}
		// Have a subnormal value.
		mantissa >>= uint(-power2 + 1)
		mantissa += mantissa & 1
		mantissa >>= 1
		power2 = 0
		if mantissa >= (uint64(1) << f.sigBits) {
			power2 = 1
		}
		return dec2fltBiasedFp{m: mantissa, pBiased: power2}
	}
	// Need to handle rounding ties. Normally, we need to round up,
	// but if we fall right in between and we have an even basis, we
	// need to round down.
	//
	// This will only occur if:
	//  1. The lower 64 bits of the 128-bit representation is 0.
	//      IE, 5^q fits in single 64-bit word.
	//  2. The least-significant bit prior to truncated mantissa is odd.
	//  3. All the bits truncated when shifting to mantissa bits + 1 are 0.
	//
	// Or, we may fall between two floats: we are exactly halfway.
	if lo <= 1 &&
		q >= int64(f.minExponentRoundToEven) &&
		q <= int64(f.maxExponentRoundToEven) &&
		mantissa&0b11 == 0b01 &&
		(mantissa<<uint(upperbit+64-int32(f.sigBits)-3)) == hi {
		// Zero the lowest bit, so we don't round up.
		mantissa &^= 1
	}
	// Round-to-even, then shift the significant digits into place.
	mantissa += mantissa & 1
	mantissa >>= 1
	if mantissa >= (uint64(2) << f.sigBits) {
		// Rounding up overflowed, so the carry bit is set. Set the
		// mantissa to 1 (only the implicit, hidden bit is set) and
		// increase the exponent.
		mantissa = uint64(1) << f.sigBits
		power2++
	}
	// Zero out the hidden bit.
	mantissa &^= uint64(1) << f.sigBits
	if power2 >= f.infinitePower {
		// Exponent is above largest normal value, must be infinite.
		return fpInf
	}
	return dec2fltBiasedFp{m: mantissa, pBiased: power2}
}

// dec2fltPower is lemire::power: it calculates a base 2 exponent from a
// decimal exponent. This uses a pre-computed integer approximation for
// log2(10), where 217706 / 2^16 is accurate for the entire range of
// non-finite decimal exponents.
func dec2fltPower(q int32) int32 {
	return ((q * (152_170 + 65536)) >> 16) + 63
}

// dec2fltFullMultiplication is full_multiplication: the low and the high
// word of a * b.
func dec2fltFullMultiplication(a, b uint64) (uint64, uint64) {
	hi, lo := bits.Mul64(a, b)
	return lo, hi
}

// dec2fltComputeProductApprox is compute_product_approx. This will compute or
// rather approximate w * 5**q and return a pair of 64-bit words approximating
// the result, with the "high" part corresponding to the most significant bits
// and the low part corresponding to the least significant bits.
func dec2fltComputeProductApprox(q int64, w uint64, precision int) (uint64, uint64) {
	mask := uint64(0xFFFF_FFFF_FFFF_FFFF)
	if precision < 64 {
		mask = 0xFFFF_FFFF_FFFF_FFFF >> uint(precision)
	}

	// 5^q < 2^64, then the multiplication always provides an exact value.
	// That means whenever we need to round ties to even, we always have
	// an exact value.
	index := q - dec2fltSmallestPowerOfFive
	lo5, hi5 := dec2fltPowerOfFive128[index][0], dec2fltPowerOfFive128[index][1]
	// Only need one multiplication as long as there is 1 zero but
	// in the explicit mantissa bits, +1 for the hidden bit, +1 to
	// determine the rounding direction, +1 for if the computed
	// product has a leading zero.
	firstLo, firstHi := dec2fltFullMultiplication(w, lo5)
	if firstHi&mask == mask {
		// Need to do a second multiplication to get better precision
		// for the lower product. This will always be exact
		// where q is < 55, since 5^55 < 2^128. If this wraps,
		// then we need to round up the hi product.
		_, secondHi := dec2fltFullMultiplication(w, hi5)
		firstLo += secondHi
		if secondHi > firstLo {
			firstHi++
		}
	}
	return firstLo, firstHi
}

// --- slow.rs ---

// dec2fltParseLongMantissa is parse_long_mantissa: it parses the significant
// digits and biased, binary exponent of a float.
//
// This is a fallback algorithm that uses a big-integer representation
// of the float, and therefore is considerably slower than faster
// approximations. However, it will always determine how to round
// the significant digits to the nearest machine float, allowing
// use to handle near half-way cases.
//
// Near half-way cases are halfway between two consecutive machine floats.
// For example, the float `16777217.0` has a bitwise representation of
// `100000000000000000000000 1`. Rounding to a single-precision float,
// the trailing `1` is truncated. Using round-nearest, tie-even, any
// value above `16777217.0` must be rounded up to `16777218.0`, while
// any value before or equal to `16777217.0` must be rounded down
// to `16777216.0`. These near-halfway conversions therefore may require
// a large number of digits to unambiguously determine how to round.
//
// The algorithms described here are based on "Processing Long Numbers Quickly",
// available here: <https://arxiv.org/pdf/2101.11408.pdf#section.11>.
func dec2fltParseLongMantissa(s string, f *dec2fltFormat) dec2fltBiasedFp {
	const maxShift = 60
	const numPowers = 19
	powers := [19]uint8{0, 3, 6, 9, 13, 16, 19, 23, 26, 29, 33, 36, 39, 43, 46, 49, 53, 56, 59}

	getShift := func(n int) int {
		if n < numPowers {
			return int(powers[n])
		}
		return maxShift
	}

	fpZero := dec2fltBiasedFp{m: 0, pBiased: 0}
	fpInf := dec2fltBiasedFp{m: 0, pBiased: f.infinitePower}

	d := dec2fltParseDecimalSeq(s)

	// Short-circuit if the value can only be a literal 0 or infinity.
	if d.numDigits == 0 || d.decimalPoint < -324 {
		return fpZero
	} else if d.decimalPoint >= 310 {
		return fpInf
	}
	exp2 := int32(0)
	// Shift right toward (1/2 ... 1].
	for d.decimalPoint > 0 {
		n := int(d.decimalPoint)
		shift := getShift(n)
		d.rightShift(shift)
		if d.decimalPoint < -dec2fltDecimalPointRange {
			return fpZero
		}
		exp2 += int32(shift)
	}
	// Shift left toward (1/2 ... 1].
	for d.decimalPoint <= 0 {
		var shift int
		if d.decimalPoint == 0 {
			digit := d.digits[0]
			if digit >= 5 {
				break
			}
			if digit == 0 || digit == 1 {
				shift = 2
			} else {
				shift = 1
			}
		} else {
			shift = getShift(int(-d.decimalPoint))
		}
		d.leftShift(shift)
		if d.decimalPoint > dec2fltDecimalPointRange {
			return fpInf
		}
		exp2 -= int32(shift)
	}
	// We are now in the range [1/2 ... 1] but the binary format uses [1 ... 2].
	exp2--
	for f.expMin > exp2 {
		n := int(f.expMin - exp2)
		if n > maxShift {
			n = maxShift
		}
		d.rightShift(n)
		exp2 += int32(n)
	}
	if (exp2 - f.expMin + 1) >= f.infinitePower {
		return fpInf
	}
	// Shift the decimal to the hidden bit, and then round the value
	// to get the high mantissa+1 bits.
	d.leftShift(int(f.sigBits) + 1)
	mantissa := d.round()
	if mantissa >= (uint64(1) << (f.sigBits + 1)) {
		// Rounding up overflowed to the carry bit, need to
		// shift back to the hidden bit.
		d.rightShift(1)
		exp2++
		mantissa = d.round()
		if (exp2 - f.expMin + 1) >= f.infinitePower {
			return fpInf
		}
	}
	power2 := exp2 - f.expMin + 1
	if mantissa < (uint64(1) << f.sigBits) {
		power2--
	}
	// Zero out all the bits above the explicit mantissa bits.
	mantissa &= (uint64(1) << f.sigBits) - 1
	return dec2fltBiasedFp{m: mantissa, pBiased: power2}
}

// --- decimal_seq.rs ---
//
// Arbitrary-precision decimal type used by fallback algorithms.
//
// This is only used if the fast-path (native floats) and
// the Eisel-Lemire algorithm are unable to unambiguously
// determine the float.
//
// The technique used is "Simple Decimal Conversion", developed
// by Nigel Tao and Ken Thompson. A detailed description of the
// algorithm can be found in "ParseNumberF64 by Simple Decimal Conversion",
// available online: <https://nigeltao.github.io/blog/2020/parse-number-f64-simple.html>.

const (
	// dec2fltMaxDigits is DecimalSeq::MAX_DIGITS: the maximum number of
	// digits required to unambiguously round up to a 64-bit float. For an
	// IEEE 754 binary64 float, this required 767 digits. So we store the max
	// digits + 1.
	dec2fltMaxDigits = 768
	// dec2fltMaxDigitsWithoutOverflow is
	// DecimalSeq::MAX_DIGITS_WITHOUT_OVERFLOW: the max decimal digits that
	// can be exactly represented in a 64-bit integer.
	dec2fltMaxDigitsWithoutOverflow = 19
	// dec2fltDecimalPointRange is DecimalSeq::DECIMAL_POINT_RANGE.
	dec2fltDecimalPointRange = 2047
)

// dec2fltDecimalSeq is decimal_seq::DecimalSeq: a decimal floating-point
// number, represented as a sequence of decimal digits.
type dec2fltDecimalSeq struct {
	// The number of significant digits in the decimal.
	numDigits int
	// The offset of the decimal point in the significant digits.
	decimalPoint int32
	// If the number of significant digits stored in the decimal is truncated.
	truncated bool
	// Buffer of the raw digits, in the range [0, 9].
	digits [dec2fltMaxDigits]uint8
}

// tryAddDigit is DecimalSeq::try_add_digit: it appends a digit to the buffer
// if it fits. (The digit counter grows even when nothing is stored.)
func (d *dec2fltDecimalSeq) tryAddDigit(digit uint8) {
	if d.numDigits < dec2fltMaxDigits {
		d.digits[d.numDigits] = digit
	}
	d.numDigits++
}

// trim is DecimalSeq::trim: it trims trailing zeros from the buffer.
func (d *dec2fltDecimalSeq) trim() {
	for d.numDigits != 0 && d.digits[d.numDigits-1] == 0 {
		d.numDigits--
	}
}

// round is DecimalSeq::round.
func (d *dec2fltDecimalSeq) round() uint64 {
	if d.numDigits == 0 || d.decimalPoint < 0 {
		return 0
	} else if d.decimalPoint >= dec2fltMaxDigitsWithoutOverflow {
		return 0xFFFF_FFFF_FFFF_FFFF
	}

	dp := int(d.decimalPoint)
	n := uint64(0)

	for i := 0; i < dp; i++ {
		n *= 10
		if i < d.numDigits {
			n += uint64(d.digits[i])
		}
	}

	roundUp := false

	if dp < d.numDigits {
		roundUp = d.digits[dp] >= 5
		if d.digits[dp] == 5 && dp+1 == d.numDigits {
			roundUp = d.truncated || ((dp != 0) && (1&d.digits[dp-1] != 0))
		}
	}

	if roundUp {
		n++
	}
	return n
}

// leftShift is DecimalSeq::left_shift: it computes decimal * 2^shift.
func (d *dec2fltDecimalSeq) leftShift(shift int) {
	if d.numDigits == 0 {
		return
	}
	numNewDigits := dec2fltNumberOfDigitsDecimalLeftShift(d, shift)
	readIndex := d.numDigits
	writeIndex := d.numDigits + numNewDigits
	n := uint64(0)

	for readIndex != 0 {
		readIndex--
		writeIndex--
		n += uint64(d.digits[readIndex]) << uint(shift)
		quotient := n / 10
		remainder := n - (10 * quotient)
		if writeIndex < dec2fltMaxDigits {
			d.digits[writeIndex] = uint8(remainder)
		} else if remainder > 0 {
			d.truncated = true
		}
		n = quotient
	}

	for n > 0 {
		writeIndex--
		quotient := n / 10
		remainder := n - (10 * quotient)
		if writeIndex < dec2fltMaxDigits {
			d.digits[writeIndex] = uint8(remainder)
		} else if remainder > 0 {
			d.truncated = true
		}
		n = quotient
	}

	d.numDigits += numNewDigits

	if d.numDigits > dec2fltMaxDigits {
		d.numDigits = dec2fltMaxDigits
	}

	d.decimalPoint += int32(numNewDigits)
	d.trim()
}

// rightShift is DecimalSeq::right_shift: it computes decimal * 2^-shift.
func (d *dec2fltDecimalSeq) rightShift(shift int) {
	readIndex := 0
	writeIndex := 0
	n := uint64(0)
	for (n >> uint(shift)) == 0 {
		if readIndex < d.numDigits {
			n = (10 * n) + uint64(d.digits[readIndex])
			readIndex++
		} else if n == 0 {
			return
		} else {
			for (n >> uint(shift)) == 0 {
				n *= 10
				readIndex++
			}
			break
		}
	}
	d.decimalPoint -= int32(readIndex) - 1
	if d.decimalPoint < -dec2fltDecimalPointRange {
		// `self = Self::Default()`, but without the overhead of clearing `digits`.
		d.numDigits = 0
		d.decimalPoint = 0
		d.truncated = false
		return
	}
	mask := (uint64(1) << uint(shift)) - 1
	for readIndex < d.numDigits {
		newDigit := uint8(n >> uint(shift))
		n = (10 * (n & mask)) + uint64(d.digits[readIndex])
		readIndex++
		d.digits[writeIndex] = newDigit
		writeIndex++
	}
	for n > 0 {
		newDigit := uint8(n >> uint(shift))
		n = 10 * (n & mask)
		if writeIndex < dec2fltMaxDigits {
			d.digits[writeIndex] = newDigit
			writeIndex++
		} else if newDigit > 0 {
			d.truncated = true
		}
	}
	d.numDigits = writeIndex
	d.trim()
}

// dec2fltParseDecimalSeq is parse_decimal_seq: it parses a big integer
// representation of the float as a decimal.
func dec2fltParseDecimalSeq(s string) *dec2fltDecimalSeq {
	d := &dec2fltDecimalSeq{}
	start := s

	for len(s) > 0 && s[0] == '0' {
		s = s[1:]
	}

	parseDigits := func(s string) string {
		for len(s) > 0 {
			c := s[0] - '0'
			if c >= 10 {
				break
			}
			d.tryAddDigit(c)
			s = s[1:]
		}
		return s
	}

	s = parseDigits(s)

	if len(s) > 0 && s[0] == '.' {
		s = s[1:]
		first := s
		// Skip leading zeros.
		if d.numDigits == 0 {
			for len(s) > 0 && s[0] == '0' {
				s = s[1:]
			}
		}
		// (Rust first copies eight digits at a time while they fit the
		// buffer, which stores the same digits.)
		s = parseDigits(s)
		d.decimalPoint = int32(len(s)) - int32(len(first))
	}

	if d.numDigits != 0 {
		// Ignore the trailing zeros if there are any
		nTrailingZeros := 0
		consumed := start[:len(start)-len(s)]
		for i := len(consumed) - 1; i >= 0; i-- {
			c := consumed[i]
			if c == '0' {
				nTrailingZeros++
			} else if c != '.' {
				break
			}
		}
		d.decimalPoint += int32(nTrailingZeros)
		d.numDigits -= nTrailingZeros
		d.decimalPoint += int32(d.numDigits)
		if d.numDigits > dec2fltMaxDigits {
			d.truncated = true
			d.numDigits = dec2fltMaxDigits
		}
	}

	if len(s) > 0 {
		ch := s[0]
		if ch == 'e' || ch == 'E' {
			s = s[1:]
			negExp := false
			if len(s) > 0 {
				ch := s[0]
				negExp = ch == '-'
				if ch == '-' || ch == '+' {
					s = s[1:]
				}
			}
			expNum := int32(0)

			for len(s) > 0 {
				digit := s[0] - '0'
				if digit >= 10 {
					break
				}
				if expNum < 0x10000 {
					expNum = 10*expNum + int32(digit)
				}
				s = s[1:]
			}

			if negExp {
				d.decimalPoint += -expNum
			} else {
				d.decimalPoint += expNum
			}
		}
	}

	for i := d.numDigits; i < dec2fltMaxDigitsWithoutOverflow; i++ {
		d.digits[i] = 0
	}

	return d
}

// dec2fltNumberOfDigitsDecimalLeftShift is
// number_of_digits_decimal_left_shift.
func dec2fltNumberOfDigitsDecimalLeftShift(d *dec2fltDecimalSeq, shift int) int {
	shift &= 63
	xA := dec2fltLeftShiftTable[shift]
	xB := dec2fltLeftShiftTable[shift+1]
	numNewDigits := int(xA >> 11)
	pow5A := int(0x7FF & xA)
	pow5B := int(0x7FF & xB)
	pow5 := dec2fltLeftShiftPow5[pow5A:]

	for i := 0; i < len(pow5) && i < pow5B-pow5A; i++ {
		p5 := pow5[i]
		if i >= d.numDigits {
			return numNewDigits - 1
		} else if d.digits[i] == p5 {
			continue
		} else if d.digits[i] < p5 {
			return numNewDigits - 1
		} else {
			return numNewDigits
		}
	}

	return numNewDigits
}
