// Port of core 1.99.1 src/fmt/num.rs

package rs

import (
	"math/big"
	"math/bits"
	"strconv"
)

// Integer formatting. Rust implements each trait once for every integer
// type; here one function per trait takes the value widened to 64 bits (or
// to 128 bits as two halves), with what distinguishes the types passed along:
// the sign, and for the radix forms the two's-complement bit pattern of the
// type's own width.

// fmtUsizeBits is usize::BITS for a port in which usize is int.
const fmtUsizeBits = bits.UintSize

// fmtRadix is the radix_integer! impls (Binary, Octal, LowerHex, UpperHex):
// value is the integer's bits, zero-extended from its own width.
func fmtRadix(value uint64, base int, upper bool, f *Formatter) error {
	digits := strconv.FormatUint(value, base)
	var prefix string
	switch base {
	case 2:
		prefix = "0b"
	case 8:
		prefix = "0o"
	default:
		prefix = "0x"
		if upper {
			digits = fmtUpperHexDigits(digits)
		}
	}
	return f.PadIntegral(true, prefix, digits)
}

// fmtUpperHexDigits turns the digits a-f into A-F.
func fmtUpperHexDigits(digits string) string {
	buf := []byte(digits)
	for i, c := range buf {
		if c >= 'a' && c <= 'f' {
			buf[i] = c - 'a' + 'A'
		}
	}
	return string(buf)
}

// fmtDisplayInt is `impl Display` for the integers up to 64 bits: abs is the
// magnitude.
func fmtDisplayInt(abs uint64, isNonnegative bool, f *Formatter) error {
	var buf [20]byte
	return f.PadIntegral(isNonnegative, "", string(strconv.AppendUint(buf[:0], abs, 10)))
}

// fmtDebugInt is impl_Debug!: an integer prints as Display does unless the
// formatter asks for hexadecimal (`{:x?}`, `{:X?}`). bits is the value's
// two's-complement pattern in its own width, abs its magnitude.
func fmtDebugInt(bits uint64, abs uint64, isNonnegative bool, f *Formatter) error {
	if f.debugLowerHex() {
		return fmtRadix(bits, 16, false, f)
	} else if f.debugUpperHex() {
		return fmtRadix(bits, 16, true, f)
	}
	return fmtDisplayInt(abs, isNonnegative, f)
}

// fmtExpU64 is impl_Exp!'s exp_u64: LowerExp and UpperExp for the integers
// up to 64 bits.
func fmtExpU64(f *Formatter, n uint64, isNonnegative bool, letterE byte) error {
	// Print the integer as a coefficient in range (-10, 10).
	exp := 0
	if n != 0 {
		exp = fmtIlog10U64(n)
	}

	// Precisison is counted as the number of digits in the fraction.
	coefPrec := exp
	// Keep the digits as an integer (paired with its coef_prec count).
	coef := n

	// A Formatter may set the precision to a fixed number of decimals.
	morePrec := 0
	if fmtPrec, ok := f.options.getPrecision(); !ok {
		// Omit any and all trailing zeroes.
		for coefPrec != 0 && coef%10 == 0 {
			coef /= 10
			coefPrec--
		}
	} else if int(fmtPrec) >= coefPrec {
		// Count the number of additional zeroes needed.
		morePrec = int(fmtPrec) - coefPrec
	} else {
		// Count the number of digits to drop.
		lessPrec := coefPrec - int(fmtPrec)
		// Scale down the coefficient/precision pair. For example,
		// coef 123456 gets coef_prec 5 (to make 1.23456). To format
		// the number with 2 decimals, i.e., fmt_prec 2, coef should
		// be scaled by 10⁵⁻²=1000 to get coef 123 with coef_prec 2.
		scale := uint64(1)
		for i := 0; i < lessPrec; i++ {
			scale *= 10
		}
		floor := coef / scale
		// Round half to even conform documentation.
		over := coef % scale
		half := scale / 2
		var roundUp uint64
		if over < half {
			roundUp = 0
		} else if over > half {
			roundUp = 1
		} else {
			roundUp = floor & 1 // round odd up to even
		}
		// Adding one to a scale down of at least 10 won't overflow.
		coef = floor + roundUp
		coefPrec = int(fmtPrec)

		// The round_up may have caused the coefficient to reach 10
		// (which is not permitted). For example, anything in range
		// [9.95, 10) becomes 10.0 when adjusted to precision 1.
		if roundUp != 0 && coef != 0 && fmtIlog10U64(coef) > coefPrec {
			coef /= 10 // drop one trailing zero
			exp++      // one power of ten higher
		}
	}

	// Encode the coefficient in buf[..coef_len].
	var buf [24]byte
	var leadDec uint64
	coefLen := 1
	if coefPrec == 0 && morePrec == 0 {
		leadDec = coef // single digit; no fraction
	} else {
		buf[1] = '.'
		// Consume the least-significant decimals from a working copy.
		remain := coef
		for i := 2 + coefPrec - 1; i >= 2; i-- {
			buf[i] = '0' + byte(remain%10)
			remain /= 10
		}
		leadDec = remain
		coefLen = 2 + coefPrec
	}
	buf[0] = '0' + byte(leadDec)

	// Encode the scale factor in buf[coef_len..text_len].
	buf[coefLen] = letterE
	textLen := coefLen + 1
	if exp < 10 {
		buf[textLen] = '0' + byte(exp)
		textLen++
	} else {
		buf[textLen] = '0' + byte(exp/10)
		buf[textLen+1] = '0' + byte(exp%10)
		textLen += 2
	}
	text := string(buf[:textLen])

	return fmtExpFinish(f, text, coefLen, morePrec, isNonnegative)
}

// fmtExpFinish is the end of impl_Exp!'s function: the rendered text goes out
// through pad_integral, or in parts when zeroes have to be added to reach the
// precision.
func fmtExpFinish(f *Formatter, text string, coefLen int, morePrec int, isNonnegative bool) error {
	if morePrec == 0 {
		return f.PadIntegral(isNonnegative, "", text)
	}
	parts := []fmtNumPart{
		fmtNumPartOfCopy(text[:coefLen]),
		fmtNumPartOfZero(morePrec),
		fmtNumPartOfCopy(text[coefLen:]),
	}
	var sign string
	if !isNonnegative {
		sign = "-"
	} else if f.SignPlus() {
		sign = "+"
	} else {
		sign = ""
	}
	return f.padFormattedParts(&fmtNumFormatted{sign: sign, parts: parts})
}

// fmtIlog10U64 is u64::ilog10 for a value that is not zero.
func fmtIlog10U64(n uint64) int {
	log := 0
	for n >= 10 {
		n /= 10
		log++
	}
	return log
}

// 128-bit integers. rs.U128 and rs.I128 present themselves to the formatter
// through fmtInt128; the arithmetic that 64 bits cannot hold is done with
// math/big, since these forms are rare and exactness is all that matters.

// fmtInt128 is how the 128-bit integer types of this package present
// themselves to the formatter. A type that implements it prints as Rust
// prints u128 or i128 under every trait (Display; Debug, with `{:x?}`; the
// radix forms; LowerExp and UpperExp), ahead of any FmtDisplay or FmtDebug
// method of its own:
//
//	func (x U128) fmtInt128() (hi, lo uint64, signed bool) { return x.hi, x.lo, false }
//	func (x I128) fmtInt128() (hi, lo uint64, signed bool) { return x.hi, x.lo, true }
type fmtInt128 interface {
	// fmtInt128 returns the value's two's-complement bits as two halves and
	// whether the type is signed.
	fmtInt128() (hi, lo uint64, signed bool)
}

// fmtInt128Abs returns the magnitude of a 128-bit integer and whether the
// value is not negative.
func fmtInt128Abs(hi, lo uint64, signed bool) (absHi, absLo uint64, isNonnegative bool) {
	if !signed || hi>>63 == 0 {
		return hi, lo, true
	}
	// unsigned_abs: negate the two's-complement pattern.
	absLo = ^lo + 1
	absHi = ^hi
	if absLo == 0 {
		absHi++
	}
	return absHi, absLo, false
}

func fmtBigFromHalves(hi, lo uint64) *big.Int {
	value := new(big.Int).SetUint64(hi)
	value.Lsh(value, 64)
	return value.Or(value, new(big.Int).SetUint64(lo))
}

// fmtDisplayInt128 is `impl Display for u128` and `for i128`.
func fmtDisplayInt128(hi, lo uint64, signed bool, f *Formatter) error {
	absHi, absLo, isNonnegative := fmtInt128Abs(hi, lo, signed)
	if absHi == 0 {
		return fmtDisplayInt(absLo, isNonnegative, f)
	}
	return f.PadIntegral(isNonnegative, "", fmtBigFromHalves(absHi, absLo).Text(10))
}

// fmtRadixInt128 is the radix_integer! impls for u128 and i128.
func fmtRadixInt128(hi, lo uint64, base int, upper bool, f *Formatter) error {
	if hi == 0 {
		return fmtRadix(lo, base, upper, f)
	}
	digits := fmtBigFromHalves(hi, lo).Text(base)
	var prefix string
	switch base {
	case 2:
		prefix = "0b"
	case 8:
		prefix = "0o"
	default:
		prefix = "0x"
		if upper {
			digits = fmtUpperHexDigits(digits)
		}
	}
	return f.PadIntegral(true, prefix, digits)
}

// fmtDebugInt128 is impl_Debug! for u128 and i128.
func fmtDebugInt128(hi, lo uint64, signed bool, f *Formatter) error {
	if f.debugLowerHex() {
		return fmtRadixInt128(hi, lo, 16, false, f)
	} else if f.debugUpperHex() {
		return fmtRadixInt128(hi, lo, 16, true, f)
	}
	return fmtDisplayInt128(hi, lo, signed, f)
}

// fmtExpInt128 is impl_Exp!'s exp_u128: LowerExp and UpperExp for u128 and
// i128. It follows fmtExpU64 step for step on big integers.
func fmtExpInt128(f *Formatter, hi, lo uint64, signed bool, letterE byte) error {
	absHi, absLo, isNonnegative := fmtInt128Abs(hi, lo, signed)
	if absHi == 0 {
		return fmtExpU64(f, absLo, isNonnegative, letterE)
	}
	ten := big.NewInt(10)
	n := fmtBigFromHalves(absHi, absLo)

	// Print the integer as a coefficient in range (-10, 10).
	exp := len(n.Text(10)) - 1

	// Precisison is counted as the number of digits in the fraction.
	coefPrec := exp
	// Keep the digits as an integer (paired with its coef_prec count).
	coef := new(big.Int).Set(n)

	// A Formatter may set the precision to a fixed number of decimals.
	morePrec := 0
	if fmtPrec, ok := f.options.getPrecision(); !ok {
		// Omit any and all trailing zeroes.
		quotient, remainder := new(big.Int), new(big.Int)
		for coefPrec != 0 {
			quotient.QuoRem(coef, ten, remainder)
			if remainder.Sign() != 0 {
				break
			}
			coef.Set(quotient)
			coefPrec--
		}
	} else if int(fmtPrec) >= coefPrec {
		// Count the number of additional zeroes needed.
		morePrec = int(fmtPrec) - coefPrec
	} else {
		// Count the number of digits to drop.
		lessPrec := coefPrec - int(fmtPrec)
		scale := new(big.Int).Exp(ten, big.NewInt(int64(lessPrec)), nil)
		floor, over := new(big.Int).QuoRem(coef, scale, new(big.Int))
		// Round half to even conform documentation.
		half := new(big.Int).Rsh(scale, 1)
		roundUp := false
		switch over.Cmp(half) {
		case -1:
			roundUp = false
		case 1:
			roundUp = true
		default:
			roundUp = floor.Bit(0) == 1 // round odd up to even
		}
		coef.Set(floor)
		if roundUp {
			coef.Add(coef, big.NewInt(1))
		}
		coefPrec = int(fmtPrec)

		// The round_up may have caused the coefficient to reach 10
		// (which is not permitted). For example, anything in range
		// [9.95, 10) becomes 10.0 when adjusted to precision 1.
		if roundUp && len(coef.Text(10))-1 > coefPrec {
			coef.Quo(coef, ten) // drop one trailing zero
			exp++               // one power of ten higher
		}
	}

	// Encode the coefficient in buf[..coef_len].
	digits := coef.Text(10)
	var text []byte
	coefLen := 1
	if coefPrec == 0 && morePrec == 0 {
		text = append(text, digits...) // single digit; no fraction
	} else {
		// The coefficient has coef_prec digits after its leading one; a
		// coefficient of zero cannot occur here, the value being 2^64 or more.
		text = append(text, digits[0], '.')
		text = append(text, digits[1:]...)
		coefLen = 2 + coefPrec
	}

	// Encode the scale factor in buf[coef_len..text_len].
	text = append(text, letterE)
	text = strconv.AppendInt(text, int64(exp), 10)

	return fmtExpFinish(f, string(text), coefLen, morePrec, isNonnegative)
}
