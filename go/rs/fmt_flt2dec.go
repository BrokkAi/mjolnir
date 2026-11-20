// Port of core 1.99.1 src/num/imp/flt2dec/mod.rs, flt2dec/decoder.rs and
// flt2dec/estimator.rs

package rs

import "math/bits"

// Floating-point number to decimal conversion routines. The digit generation
// is in fmt_flt2dec_grisu.go and fmt_flt2dec_dragon.go; the Rust module's
// long description of the problem and of the two algorithms is not repeated
// here.

// fmtDecoded is flt2dec::Decoded: a decoded unsigned finite value, such that:
//
//   - The original value equals to `mant * 2^exp`.
//
//   - Any number from `(mant - minus) * 2^exp` to `(mant + plus) * 2^exp` will
//     round to the original value. The range is inclusive only when
//     `inclusive` is `true`.
type fmtDecoded struct {
	// The scaled mantissa.
	mant uint64
	// The lower error range.
	minus uint64
	// The upper error range.
	plus uint64
	// The shared exponent in base 2.
	exp int16
	// True when the error range is inclusive.
	//
	// In IEEE 754, this is true when the original mantissa was even.
	inclusive bool
}

// fmtFullDecoded is flt2dec::FullDecoded: a decoded unsigned value.
type fmtFullDecoded uint8

const (
	// Not-a-number.
	fmtFullDecodedNan fmtFullDecoded = iota
	// Infinities, either positive or negative.
	fmtFullDecodedInfinite
	// Zero, either positive or negative.
	fmtFullDecodedZero
	// Finite numbers with further decoded fields.
	fmtFullDecodedFinite
)

// fmtFloat is a float the formatter prints: its bits and the layout of its
// type (flt2dec::DecodableFloat).
type fmtFloat struct {
	bits uint64
	// totalBits is 32 or 64, sigBits the width of the significand field.
	totalBits uint
	sigBits   uint
}

func fmtFloatOfF64(bits uint64) fmtFloat { return fmtFloat{bits: bits, totalBits: 64, sigBits: 52} }
func fmtFloatOfF32(bits uint32) fmtFloat {
	return fmtFloat{bits: uint64(bits), totalBits: 32, sigBits: 23}
}

// fmtDecode is flt2dec::decode: it returns a sign (true when negative) and
// the decoded value of a floating point number.
func fmtDecode(v fmtFloat) (negative bool, kind fmtFullDecoded, decoded fmtDecoded) {
	expBits := v.totalBits - 1 - v.sigBits
	sigMask := uint64(1)<<v.sigBits - 1
	expField := (v.bits >> v.sigBits) & (uint64(1)<<expBits - 1)
	sigField := v.bits & sigMask
	negative = v.bits>>(v.totalBits-1) != 0

	// Float::integer_decode
	var mant uint64
	if expField == 0 {
		mant = sigField << 1
	} else {
		mant = sigField | uint64(1)<<v.sigBits
	}
	// Exponent bias + mantissa shift
	expBias := int(1)<<(expBits-1) - 1
	exp := int16(int(expField) - (expBias + int(v.sigBits)))

	even := mant&1 == 0
	switch {
	case expField == uint64(1)<<expBits-1 && sigField != 0:
		return negative, fmtFullDecodedNan, fmtDecoded{}
	case expField == uint64(1)<<expBits-1:
		return negative, fmtFullDecodedInfinite, fmtDecoded{}
	case expField == 0 && sigField == 0:
		return negative, fmtFullDecodedZero, fmtDecoded{}
	case expField == 0:
		// neighbors: (mant - 2, exp) -- (mant, exp) -- (mant + 2, exp)
		// Float::integer_decode always preserves the exponent,
		// so the mantissa is scaled for subnormals.
		return negative, fmtFullDecodedFinite, fmtDecoded{mant: mant, minus: 1, plus: 1, exp: exp, inclusive: even}
	default:
		// The mantissa of the minimum positive normalized value.
		minnormMant := uint64(1) << v.sigBits
		if mant == minnormMant {
			// neighbors: (maxmant, exp - 1) -- (minnormmant, exp) -- (minnormmant + 1, exp)
			// where maxmant = minnormmant * 2 - 1
			return negative, fmtFullDecodedFinite, fmtDecoded{mant: mant << 2, minus: 1, plus: 2, exp: exp - 2, inclusive: even}
		}
		// neighbors: (mant - 1, exp) -- (mant, exp) -- (mant + 1, exp)
		return negative, fmtFullDecodedFinite, fmtDecoded{mant: mant << 1, minus: 1, plus: 1, exp: exp - 1, inclusive: even}
	}
}

// fmtEstimateScalingFactor is estimator::estimate_scaling_factor: it finds
// `k_0` such that `10^(k_0-1) < mant * 2^exp <= 10^(k_0+1)`.
//
// This is used to approximate `k = ceil(log_10 (mant * 2^exp))`;
// the true `k` is either `k_0` or `k_0+1`.
func fmtEstimateScalingFactor(mant uint64, exp int16) int16 {
	// 2^(nbits-1) < mant <= 2^nbits if mant > 0
	nbits := 64 - int64(bits.LeadingZeros64(mant-1))
	// 1292913986 = floor(2^32 * log_10 2)
	// therefore this always underestimates (or is exact), but not much.
	return int16(((nbits + int64(exp)) * 1292913986) >> 32)
}

// The minimum size of buffer necessary for the shortest mode.
//
// It is a bit non-trivial to derive, but this is one plus the maximal number of
// significant decimal digits from formatting algorithms with the shortest result.
// The exact formula is `ceil(# bits in mantissa * log_10 2 + 1)`.
const fmtMaxSigDigits = 17

// fmtRoundUp is flt2dec::round_up: when `d` contains decimal digits, increase
// the last digit and propagate carry. It returns a next digit when it causes
// the length to change.
func fmtRoundUp(d []byte) (byte, bool) {
	i := len(d) - 1
	for i >= 0 && d[i] == '9' {
		i--
	}
	switch {
	case i >= 0:
		// d[i+1..n] is all nines
		d[i]++
		for j := i + 1; j < len(d); j++ {
			d[j] = '0'
		}
		return 0, false
	case len(d) == 0:
		// an empty buffer rounds up (a bit strange but reasonable)
		return '1', true
	default:
		// 999..999 rounds to 1000..000 with an increased exponent
		d[0] = '1'
		for j := 1; j < len(d); j++ {
			d[j] = '0'
		}
		return '0', true
	}
}

// fmtDigitsToDecStr is flt2dec::digits_to_dec_str: it formats given decimal
// digits `0.<...buf...> * 10^exp` into the decimal form with at least given
// number of fractional digits.
//
// `frac_digits` can be less than the number of actual fractional digits in `buf`;
// it will be ignored and full digits will be printed. It is only used to print
// additional zeroes after rendered digits. Thus `frac_digits` of 0 means that
// it will only print given digits and nothing else.
func fmtDigitsToDecStr(buf []byte, exp int16, fracDigits int) []fmtNumPart {
	// if there is the restriction on the last digit position, `buf` is assumed to be
	// left-padded with the virtual zeroes. the number of virtual zeroes, `nzeroes`,
	// equals to `max(0, exp + frac_digits - buf.len())`, so that the position of
	// the last digit `exp - buf.len() - nzeroes` is no more than `-frac_digits`:
	//
	//                       |<-virtual->|
	//       |<---- buf ---->|  zeroes   |     exp
	//    0. 1 2 3 4 5 6 7 8 9 _ _ _ _ _ _ x 10
	//    |                  |           |
	// 10^exp    10^(exp-buf.len())   10^(exp-buf.len()-nzeroes)
	//
	// `nzeroes` is individually calculated for each case in order to avoid overflow.

	if exp <= 0 {
		// the decimal point is before rendered digits: [0.][000...000][1234][____]
		minusExp := -int(exp)
		parts := []fmtNumPart{
			fmtNumPartOfCopy("0."),
			fmtNumPartOfZero(minusExp),
			fmtNumPartOfCopy(string(buf)),
		}
		if fracDigits > len(buf) && fracDigits-len(buf) > minusExp {
			parts = append(parts, fmtNumPartOfZero((fracDigits-len(buf))-minusExp))
		}
		return parts
	}
	e := int(exp)
	if e < len(buf) {
		// the decimal point is inside rendered digits: [12][.][34][____]
		parts := []fmtNumPart{
			fmtNumPartOfCopy(string(buf[:e])),
			fmtNumPartOfCopy("."),
			fmtNumPartOfCopy(string(buf[e:])),
		}
		if fracDigits > len(buf)-e {
			parts = append(parts, fmtNumPartOfZero(fracDigits-(len(buf)-e)))
		}
		return parts
	}
	// the decimal point is after rendered digits: [1234][____0000] or [1234][__][.][__].
	parts := []fmtNumPart{
		fmtNumPartOfCopy(string(buf)),
		fmtNumPartOfZero(e - len(buf)),
	}
	if fracDigits > 0 {
		parts = append(parts, fmtNumPartOfCopy("."), fmtNumPartOfZero(fracDigits))
	}
	return parts
}

// fmtDigitsToExpStr is flt2dec::digits_to_exp_str: it formats the given
// decimal digits `0.<...buf...> * 10^exp` into the exponential form with at
// least the given number of fractional digits. When `upper` is `true`, the
// exponent will be prefixed by `E`; otherwise that's `e`.
//
// `frac_digits` can be less than the number of actual fractional digits in `buf`;
// it will be ignored and full digits will be printed. It is only used to print
// additional zeroes after rendered digits. Thus, `frac_digits == 0` means that
// it will only print the given digits and nothing else.
//
// For example, `buf = b"123", exp = 3, frac_digits = 4` yields the parts for
// `1.2300e2`.
func fmtDigitsToExpStr(buf []byte, exp int16, fracDigits int, upper bool) []fmtNumPart {
	parts := make([]fmtNumPart, 0, 6)

	parts = append(parts, fmtNumPartOfCopy(string(buf[:1])))

	// The first generated digit becomes the integral digit, anything after that is already part of
	// the fractional portion we can emit verbatim.
	actualFracDigits := len(buf) - 1
	if actualFracDigits > 0 || fracDigits > 0 {
		// Emit a decimal point either when we already have fractional digits or when the requested
		// precision needs trailing zeroes after the radix point.
		parts = append(parts, fmtNumPartOfCopy("."))
		if actualFracDigits > 0 {
			parts = append(parts, fmtNumPartOfCopy(string(buf[1:])))
		}
		if fracDigits > actualFracDigits {
			// format_exact exhausted the meaningful digits, so extend the fractional part with
			// zeroes up to the requested precision.
			parts = append(parts, fmtNumPartOfZero(fracDigits-actualFracDigits))
		}
	}

	// 0.1234 x 10^exp = 1.234 x 10^(exp-1)
	e := int32(exp) - 1 // avoid underflow when exp is i16::MIN
	if e < 0 {
		if upper {
			parts = append(parts, fmtNumPartOfCopy("E-"))
		} else {
			parts = append(parts, fmtNumPartOfCopy("e-"))
		}
		parts = append(parts, fmtNumPartOfNum(uint16(-e)))
	} else {
		if upper {
			parts = append(parts, fmtNumPartOfCopy("E"))
		} else {
			parts = append(parts, fmtNumPartOfCopy("e"))
		}
		parts = append(parts, fmtNumPartOfNum(uint16(e)))
	}
	return parts
}

// fmtSign is flt2dec::Sign: sign formatting options.
type fmtSign uint8

const (
	// Prints `-` for any negative value.
	fmtSignMinus fmtSign = iota // -inf -1 -0  0  1  inf nan
	// Prints `-` for any negative value, or `+` otherwise.
	fmtSignMinusPlus // -inf -1 -0 +0 +1 +inf nan
)

// fmtDetermineSign is flt2dec::determine_sign: it returns the string
// corresponding to the sign to be formatted. It can be either `""`, `"+"` or
// `"-"`.
func fmtDetermineSign(sign fmtSign, kind fmtFullDecoded, negative bool) string {
	switch {
	case kind == fmtFullDecodedNan:
		return ""
	case sign == fmtSignMinus:
		if negative {
			return "-"
		}
		return ""
	default:
		if negative {
			return "-"
		}
		return "+"
	}
}

// fmtToShortestStr is flt2dec::to_shortest_str: it formats the given floating
// point number into the decimal form with at least given number of fractional
// digits.
//
// `frac_digits` can be less than the number of actual fractional digits in `v`;
// it will be ignored and full digits will be printed. It is only used to print
// additional zeroes after rendered digits. Thus `frac_digits` of 0 means that
// it will only print given digits and nothing else.
func fmtToShortestStr(v fmtFloat, sign fmtSign, fracDigits int) fmtNumFormatted {
	negative, fullDecoded, decoded := fmtDecode(v)
	signText := fmtDetermineSign(sign, fullDecoded, negative)
	switch fullDecoded {
	case fmtFullDecodedNan:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("NaN")}}
	case fmtFullDecodedInfinite:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("inf")}}
	case fmtFullDecodedZero:
		if fracDigits > 0 {
			// [0.][0000]
			return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0."), fmtNumPartOfZero(fracDigits)}}
		}
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0")}}
	default:
		var scratch [fmtMaxSigDigits]byte
		buf, exp := fmtGrisuFormatShortest(&decoded, scratch[:])
		return fmtNumFormatted{sign: signText, parts: fmtDigitsToDecStr(buf, exp, fracDigits)}
	}
}

// fmtToShortestExpStr is flt2dec::to_shortest_exp_str: it formats the given
// floating point number into the decimal form or the exponential form,
// depending on the resulting exponent. `upper` is used to determine the case
// of the exponent prefix (`e` or `E`).
//
// The `dec_bounds` is a tuple `(lo, hi)` such that the number is formatted
// as decimal only when `10^lo <= V < 10^hi`. Note that this is the *apparent* `V`
// instead of the actual `v`! Thus any printed exponent in the exponential form
// cannot be in this range, avoiding any confusion.
func fmtToShortestExpStr(v fmtFloat, sign fmtSign, decBoundsLo, decBoundsHi int16, upper bool) fmtNumFormatted {
	negative, fullDecoded, decoded := fmtDecode(v)
	signText := fmtDetermineSign(sign, fullDecoded, negative)
	switch fullDecoded {
	case fmtFullDecodedNan:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("NaN")}}
	case fmtFullDecodedInfinite:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("inf")}}
	case fmtFullDecodedZero:
		var part fmtNumPart
		if decBoundsLo <= 0 && 0 < decBoundsHi {
			part = fmtNumPartOfCopy("0")
		} else if upper {
			part = fmtNumPartOfCopy("0E0")
		} else {
			part = fmtNumPartOfCopy("0e0")
		}
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{part}}
	default:
		var scratch [fmtMaxSigDigits]byte
		buf, exp := fmtGrisuFormatShortest(&decoded, scratch[:])
		visExp := int32(exp) - 1
		var parts []fmtNumPart
		if int32(decBoundsLo) <= visExp && visExp < int32(decBoundsHi) {
			parts = fmtDigitsToDecStr(buf, exp, 0)
		} else {
			parts = fmtDigitsToExpStr(buf, exp, 0, upper)
		}
		return fmtNumFormatted{sign: signText, parts: parts}
	}
}

// fmtEstimateMaxBufLen is flt2dec::estimate_max_buf_len: it returns a rather
// crude approximation (upper bound) for the maximum buffer size calculated
// from the given decoded exponent.
//
// The exact limit is:
//
// - when `exp < 0`, the maximum length is `ceil(log_10 (5^-exp * (2^64 - 1)))`.
// - when `exp >= 0`, the maximum length is `ceil(log_10 (2^exp * (2^64 - 1)))`.
//
// `ceil(log_10 (x^exp * (2^64 - 1)))` is less than `ceil(log_10 (2^64 - 1)) +
// ceil(exp * log_10 x)`, which is in turn less than `20 + (1 + exp * log_10 x)`.
// We use the facts that `log_10 2 < 5/16` and `log_10 5 < 12/16`, which is
// enough for our purposes.
//
// Why do we need this? `format_exact` functions will fill the entire buffer
// unless limited by the last digit restriction, but it is possible that
// the number of digits requested is ridiculously large (say, 30,000 digits).
// The vast majority of buffer will be filled with zeroes, so we don't want to
// allocate all the buffer beforehand. Consequently, for any given arguments,
// 826 bytes of buffer should be sufficient for `f64`. Compare this with
// the actual number for the worst case: 770 bytes (when `exp = -1074`).
func fmtEstimateMaxBufLen(exp int16) int {
	factor := int32(5)
	if exp < 0 {
		factor = -12
	}
	return 21 + int(uint32(factor*int32(exp))>>4)
}

// fmtToExactExpStr is flt2dec::to_exact_exp_str: it formats given floating
// point number into the exponential form with exactly given number of
// fractional digits. `upper` is used to determine the case of the exponent
// prefix (`e` or `E`).
//
// The returned format is `[sign][digit][.fraction?][zero padding?][e|E][exponent]`.
func fmtToExactExpStr(v fmtFloat, sign fmtSign, fracDigits int, upper bool) fmtNumFormatted {
	negative, fullDecoded, decoded := fmtDecode(v)
	signText := fmtDetermineSign(sign, fullDecoded, negative)
	switch fullDecoded {
	case fmtFullDecodedNan:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("NaN")}}
	case fmtFullDecodedInfinite:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("inf")}}
	case fmtFullDecodedZero:
		if fracDigits > 0 {
			// [0.][0000][e0]
			exponent := "e0"
			if upper {
				exponent = "E0"
			}
			return fmtNumFormatted{sign: signText, parts: []fmtNumPart{
				fmtNumPartOfCopy("0."), fmtNumPartOfZero(fracDigits), fmtNumPartOfCopy(exponent),
			}}
		}
		zero := "0e0"
		if upper {
			zero = "0E0"
		}
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy(zero)}}
	default:
		maxlen := fmtEstimateMaxBufLen(decoded.exp)
		// Scratch space is only needed for the significant digits that `format_exact` can
		// actually generate. Any remaining requested fractional precision becomes a trailing
		// `Part::Zero`.
		sigDigits := maxlen
		if fracDigits < maxlen {
			sigDigits = fracDigits + 1
		}

		scratch := make([]byte, sigDigits)
		buf, exp := fmtGrisuFormatExact(&decoded, scratch, -32768)
		return fmtNumFormatted{sign: signText, parts: fmtDigitsToExpStr(buf, exp, fracDigits, upper)}
	}
}

// fmtToExactFixedStr is flt2dec::to_exact_fixed_str: it formats given
// floating point number into the decimal form with exactly given number of
// fractional digits.
func fmtToExactFixedStr(v fmtFloat, sign fmtSign, fracDigits int) fmtNumFormatted {
	negative, fullDecoded, decoded := fmtDecode(v)
	signText := fmtDetermineSign(sign, fullDecoded, negative)
	switch fullDecoded {
	case fmtFullDecodedNan:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("NaN")}}
	case fmtFullDecodedInfinite:
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("inf")}}
	case fmtFullDecodedZero:
		if fracDigits > 0 {
			// [0.][0000]
			return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0."), fmtNumPartOfZero(fracDigits)}}
		}
		return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0")}}
	default:
		maxlen := fmtEstimateMaxBufLen(decoded.exp)

		// it *is* possible that `frac_digits` is ridiculously large.
		// `format_exact` will end rendering digits much earlier in this case,
		// because we are strictly limited by `maxlen`.
		limit := int16(-32768)
		if fracDigits < 0x8000 {
			limit = -int16(fracDigits)
		}
		scratch := make([]byte, maxlen)
		buf, exp := fmtGrisuFormatExact(&decoded, scratch, limit)
		if exp <= limit {
			// the restriction couldn't been met, so this should render like zero no matter
			// `exp` was. this does not include the case that the restriction has been met
			// only after the final rounding-up; it's a regular case with `exp = limit + 1`.
			if fracDigits > 0 {
				// [0.][0000]
				return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0."), fmtNumPartOfZero(fracDigits)}}
			}
			return fmtNumFormatted{sign: signText, parts: []fmtNumPart{fmtNumPartOfCopy("0")}}
		}
		return fmtNumFormatted{sign: signText, parts: fmtDigitsToDecStr(buf, exp, fracDigits)}
	}
}
