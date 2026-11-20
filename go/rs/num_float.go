// Port of core 1.99.1 src/num/f64.rs and src/num/f32.rs, std 1.99.1
// src/num/f64.rs and src/num/f32.rs, and compiler-builtins src/float/pow.rs

package rs

import (
	"math"
	"strconv"
	"strings"
)

// Float methods.
//
// A method on an f64 is rs.F64<Method> and one on an f32 is rs.F32<Method>:
// `a.max(b)` is rs.F64Max(a, b). Go's math package has no float32 functions,
// and several of its float64 ones differ from Rust's, so every method the
// port uses has a function here and product code needs no rule about which
// math function is safe.
//
// Exactly as Rust, bit for bit (checked against the oracle):
//
//	Max, Min         ignore a NaN operand (math.Max and Go's max return NaN)
//	Clamp            panics when min > max or either is NaN; a NaN value stays NaN
//	Signum           1.0 for +0.0, -1.0 for -0.0, NaN for NaN
//	Fract            x - trunc(x) (math.Modf differs for infinities and -0.0)
//	RemEuclid, DivEuclid
//	TotalCmp         IEEE totalOrder, as an rs.Ordering
//	Powi             repeated squaring, as LLVM's powi (not math.Pow)
//	Round            half away from zero (math.Round)
//	RoundTiesEven, Floor, Ceil, Trunc, Abs, Sqrt, MulAdd, Copysign, Recip,
//	ToDegrees, ToRadians, ToBits, FromBits, IsNan, IsInfinite, IsFinite,
//	IsSignNegative, IsSignPositive
//
// Within the last place of Rust: Exp, Exp2, ExpM1, Ln, Ln1p, Log, Log2,
// Log10, Powf, Cbrt, Hypot, Sin, Cos, Tan, Asin, Acos, Atan, Atan2, Sinh,
// Cosh, Tanh. Rust calls the C library for these and documents their
// precision as varying by platform and version; the functions here use Go's
// math package (in float64, rounded once more for the F32 forms). Results
// that are exact in the C library are exact here: F64Log10 of a power of ten
// is the integer (math.Log10(1000) is 2.9999999999999996), powers with
// integer exponents, logarithms of powers of two.
//
// Operators need no helper: `+ - * /` round the same way, and `%` is
// math.Mod. One thing does differ: Go may fuse `x*y + z` into a single
// rounding on arm64 (and on amd64 built with GOAMD64=v3), and Rust never
// does. Write `float64(x*y) + z` in ported arithmetic whose last bit is
// observable; the explicit conversion forbids the fusion.
//
// Constants: `f64::MAX` is math.MaxFloat64 and `f64::MIN` is
// -math.MaxFloat64 (likewise MaxFloat32); `std::f64::consts::PI` is math.Pi,
// and an untyped Go constant converted to float32 is Rust's f32 constant.

const (
	// F64Epsilon is `f64::EPSILON`.
	F64Epsilon = 2.2204460492503131e-16
	// F32Epsilon is `f32::EPSILON`.
	F32Epsilon float32 = 1.19209290e-07
	// F64MinPositive is `f64::MIN_POSITIVE`, the smallest positive normal
	// value (math.SmallestNonzeroFloat64 is the smallest subnormal).
	F64MinPositive = 2.2250738585072014e-308
	// F32MinPositive is `f32::MIN_POSITIVE`.
	F32MinPositive float32 = 1.17549435e-38
)

var (
	// F64Nan is `f64::NAN`. Its bits are Rust's (0x7ff8000000000000);
	// math.NaN() has a different payload, which `to_bits` would show.
	F64Nan = math.Float64frombits(0x7ff8000000000000)
	// F64Infinity is `f64::INFINITY`.
	F64Infinity = math.Inf(1)
	// F64NegInfinity is `f64::NEG_INFINITY`.
	F64NegInfinity = math.Inf(-1)
	// F32Nan is `f32::NAN` (0x7fc00000).
	F32Nan = math.Float32frombits(0x7fc00000)
	// F32Infinity is `f32::INFINITY`.
	F32Infinity = float32(math.Inf(1))
	// F32NegInfinity is `f32::NEG_INFINITY`.
	F32NegInfinity = float32(math.Inf(-1))
)

type float interface {
	~float32 | ~float64
}

// floatMax is `intrinsics::maximum_number_nsz`: a NaN operand is ignored,
// and of two zeros of different sign the second is returned.
func floatMax[F float](x, y F) F {
	if x != x || y >= x {
		return y
	}
	// Either y < x or y is a NaN.
	return x
}

// floatMin is `intrinsics::minimum_number_nsz`.
func floatMin[F float](x, y F) F {
	if x != x || y <= x {
		return y
	}
	// Either y > x or y is a NaN.
	return x
}

// floatDebug is `format!("{:?}", x)` for a float: the shortest digits that
// read back as x, with a fraction always shown, and an exponent below 1e-4
// and from 1e16 up.
func floatDebug(x float64, bitSize int, exponential bool) string {
	switch {
	case x != x:
		return "NaN"
	case math.IsInf(x, 1):
		return "inf"
	case math.IsInf(x, -1):
		return "-inf"
	}
	if exponential {
		text := strconv.FormatFloat(x, 'e', -1, bitSize)
		mantissa, exponent, _ := strings.Cut(text, "e")
		sign := ""
		if strings.HasPrefix(exponent, "-") {
			sign = "-"
		}
		exponent = strings.TrimLeft(exponent, "+-0")
		if exponent == "" {
			exponent = "0"
		}
		return mantissa + "e" + sign + exponent
	}
	text := strconv.FormatFloat(x, 'f', -1, bitSize)
	if !strings.Contains(text, ".") {
		text += ".0"
	}
	return text
}

func f64Debug(x float64) string {
	abs := math.Abs(x)
	return floatDebug(x, 64, (abs != 0 && abs < 1e-4) || abs >= 1e+16)
}

func f32Debug(x float32) string {
	abs := float32(math.Abs(float64(x)))
	return floatDebug(float64(x), 32, (abs != 0 && abs < float32(1e-4)) || abs >= float32(1e+16))
}

// ---------------------------------------------------------------------------
// f64

// F64Max is `a.max(b)`: the larger, ignoring a NaN operand. NaN only when
// both are NaN.
func F64Max(a, b float64) float64 { return floatMax(a, b) }

// F64Min is `a.min(b)`: the smaller, ignoring a NaN operand.
func F64Min(a, b float64) float64 { return floatMin(a, b) }

// F64Clamp is `x.clamp(min, max)`. It panics when min > max or either bound
// is NaN. A NaN x is returned as it is, and -0.0 is not raised to a 0.0
// bound.
func F64Clamp(x, min, max float64) float64 {
	if !(min <= max) {
		PanicAt(1, "min > max, or either was NaN. min = "+f64Debug(min)+", max = "+f64Debug(max))
	}
	if x < min {
		x = min
	}
	if x > max {
		x = max
	}
	return x
}

// F64Signum is `x.signum()`: 1.0 for a positive value, +0.0 and infinity,
// -1.0 for a negative value, -0.0 and negative infinity, NaN for NaN.
func F64Signum(x float64) float64 {
	if x != x {
		return F64Nan
	}
	return math.Copysign(1, x)
}

// F64Fract is `x.fract()`: x - x.trunc().
func F64Fract(x float64) float64 { return x - math.Trunc(x) }

// F64RemEuclid is `x.rem_euclid(rhs)`: the least non-negative remainder.
func F64RemEuclid(x, rhs float64) float64 {
	r := math.Mod(x, rhs)
	if r < 0 {
		return r + math.Abs(rhs)
	}
	return r
}

// F64DivEuclid is `x.div_euclid(rhs)`.
func F64DivEuclid(x, rhs float64) float64 {
	q := math.Trunc(x / rhs)
	if math.Mod(x, rhs) < 0 {
		if rhs > 0 {
			return q - 1
		}
		return q + 1
	}
	return q
}

// F64TotalCmp is `a.total_cmp(&b)`: IEEE 754's total order, where -0.0 is
// below +0.0 and NaNs sort outside the infinities by sign.
func F64TotalCmp(a, b float64) Ordering {
	left := int64(math.Float64bits(a))
	right := int64(math.Float64bits(b))
	left ^= int64(uint64(left>>63) >> 1)
	right ^= int64(uint64(right>>63) >> 1)
	switch {
	case left < right:
		return Ordering_Less
	case left > right:
		return Ordering_Greater
	default:
		return Ordering_Equal
	}
}

// F64Powi is `x.powi(n)`: x raised to an integer power by repeated
// squaring, which is what LLVM's powi computes. It is not math.Pow, whose
// result can differ in the last place.
func F64Powi(x float64, n int32) float64 {
	a := x
	pow := uint32(n)
	if n < 0 {
		pow = -pow
	}
	mul := 1.0
	for {
		if pow&1 != 0 {
			mul *= a
		}
		pow >>= 1
		if pow == 0 {
			break
		}
		a *= a
	}
	if n < 0 {
		return 1 / mul
	}
	return mul
}

// F64IsNan is `x.is_nan()`.
func F64IsNan(x float64) bool { return x != x }

// F64IsInfinite is `x.is_infinite()`.
func F64IsInfinite(x float64) bool { return math.IsInf(x, 0) }

// F64IsFinite is `x.is_finite()`: neither infinite nor NaN.
func F64IsFinite(x float64) bool { return math.Abs(x) < math.Inf(1) }

// F64IsSignNegative is `x.is_sign_negative()`, true for -0.0 and for a NaN
// with its sign bit set.
func F64IsSignNegative(x float64) bool { return math.Signbit(x) }

// F64IsSignPositive is `x.is_sign_positive()`.
func F64IsSignPositive(x float64) bool { return !math.Signbit(x) }

// F64Copysign is `x.copysign(sign)`.
func F64Copysign(x, sign float64) float64 { return math.Copysign(x, sign) }

// F64Recip is `x.recip()`.
func F64Recip(x float64) float64 { return 1 / x }

// F64ToDegrees is `x.to_degrees()`.
func F64ToDegrees(x float64) float64 {
	const pisIn180 float64 = 180.0 / math.Pi
	return x * pisIn180
}

// F64ToRadians is `x.to_radians()`.
func F64ToRadians(x float64) float64 {
	const radsPerDeg float64 = math.Pi / 180.0
	return x * radsPerDeg
}

// F64ToBits is `x.to_bits()`.
func F64ToBits(x float64) uint64 { return math.Float64bits(x) }

// F64FromBits is `f64::from_bits(v)`.
func F64FromBits(v uint64) float64 { return math.Float64frombits(v) }

// F64Round is `x.round()`: to the nearest integer, halfway cases away from
// zero.
func F64Round(x float64) float64 { return math.Round(x) }

// F64RoundTiesEven is `x.round_ties_even()`.
func F64RoundTiesEven(x float64) float64 { return math.RoundToEven(x) }

// F64Floor is `x.floor()`.
func F64Floor(x float64) float64 { return math.Floor(x) }

// F64Ceil is `x.ceil()`.
func F64Ceil(x float64) float64 { return math.Ceil(x) }

// F64Trunc is `x.trunc()`.
func F64Trunc(x float64) float64 { return math.Trunc(x) }

// F64Abs is `x.abs()`.
func F64Abs(x float64) float64 { return math.Abs(x) }

// F64Sqrt is `x.sqrt()`.
func F64Sqrt(x float64) float64 { return math.Sqrt(x) }

// F64MulAdd is `x.mul_add(a, b)`: x*a + b with one rounding.
func F64MulAdd(x, a, b float64) float64 { return math.FMA(x, a, b) }

// F64Cbrt is `x.cbrt()`.
func F64Cbrt(x float64) float64 { return math.Cbrt(x) }

// F64Exp is `x.exp()`.
func F64Exp(x float64) float64 { return math.Exp(x) }

// F64Exp2 is `x.exp2()`.
func F64Exp2(x float64) float64 { return math.Exp2(x) }

// F64ExpM1 is `x.exp_m1()`.
func F64ExpM1(x float64) float64 { return math.Expm1(x) }

// F64Ln is `x.ln()`.
func F64Ln(x float64) float64 { return math.Log(x) }

// F64Ln1p is `x.ln_1p()`.
func F64Ln1p(x float64) float64 { return math.Log1p(x) }

// F64Log is `x.log(base)`: x.ln() / base.ln().
func F64Log(x, base float64) float64 { return math.Log(x) / math.Log(base) }

// F64Log2 is `x.log2()`. The exponent is split off around 1, not around
// 1/2 as math.Log2 does, so that values near 1 keep their digits; a power of
// two gives the integer exactly.
func F64Log2(x float64) float64 {
	if !(x > 0) || math.IsInf(x, 1) {
		return math.Log2(x)
	}
	fraction, exponent := math.Frexp(x)
	if fraction < math.Sqrt2/2 {
		fraction *= 2
		exponent--
	}
	return float64(exponent) + float64(math.Log(fraction)*(1/math.Ln2))
}

// F64Log10 is `x.log10()`. It follows the C library's method (fdlibm
// e_log10.c) rather than math.Log10, so the logarithm of a power of ten is
// the integer exactly, as code that takes `.log10().floor()` relies on.
func F64Log10(x float64) float64 {
	const (
		two54    = 1.80143985094819840000e+16 // 0x43500000, 0x00000000
		ivln10   = 4.34294481903251816668e-01 // 0x3FDBCB7B, 0x1526E50E
		log102hi = 3.01029995663611771306e-01 // 0x3FD34413, 0x509F6000
		log102lo = 3.69423907715893078616e-13 // 0x3D59FEF3, 0x11F12B36
	)
	bits := math.Float64bits(x)
	hx := int32(bits >> 32)
	lx := uint32(bits)

	k := int32(0)
	if hx < 0x00100000 { // x < 2**-1022
		if (uint32(hx)&0x7fffffff)|lx == 0 {
			return math.Inf(-1) // log(+-0)=-inf
		}
		if hx < 0 {
			return F64Nan // log(-#) = NaN
		}
		k -= 54
		x *= two54 // subnormal number, scale up x
		hx = int32(math.Float64bits(x) >> 32)
	}
	if hx >= 0x7ff00000 {
		return x + x
	}
	k += (hx >> 20) - 1023
	i := int32((uint32(k) & 0x80000000) >> 31)
	hx = (hx & 0x000fffff) | ((0x3ff - i) << 20)
	y := float64(k + i)
	x = math.Float64frombits(uint64(uint32(hx))<<32 | (math.Float64bits(x) & 0xffffffff))
	z := float64(y*log102lo) + float64(ivln10*math.Log(x))
	return z + float64(y*log102hi)
}

// F64Powf is `x.powf(n)`.
func F64Powf(x, n float64) float64 { return math.Pow(x, n) }

// F64Hypot is `x.hypot(other)`.
func F64Hypot(x, other float64) float64 { return math.Hypot(x, other) }

// F64Sin is `x.sin()`.
func F64Sin(x float64) float64 { return math.Sin(x) }

// F64Cos is `x.cos()`.
func F64Cos(x float64) float64 { return math.Cos(x) }

// F64Tan is `x.tan()`.
func F64Tan(x float64) float64 { return math.Tan(x) }

// F64Asin is `x.asin()`. This form keeps the digits that math.Asin loses
// near 1 and -1.
func F64Asin(x float64) float64 {
	return math.Atan2(x, math.Sqrt(float64((1-x)*(1+x))))
}

// F64Acos is `x.acos()`. math.Acos subtracts the arcsine from pi/2, which
// loses most of the result's digits near 1; this form keeps them.
func F64Acos(x float64) float64 {
	return 2 * math.Atan2(math.Sqrt(1-x), math.Sqrt(1+x))
}

// F64Atan is `x.atan()`.
func F64Atan(x float64) float64 { return math.Atan(x) }

// F64Atan2 is `y.atan2(x)`.
func F64Atan2(y, x float64) float64 { return math.Atan2(y, x) }

// F64Sinh is `x.sinh()`.
func F64Sinh(x float64) float64 { return math.Sinh(x) }

// F64Cosh is `x.cosh()`.
func F64Cosh(x float64) float64 { return math.Cosh(x) }

// F64Tanh is `x.tanh()`.
func F64Tanh(x float64) float64 { return math.Tanh(x) }

// ---------------------------------------------------------------------------
// f32

// F32Max is `a.max(b)`: the larger, ignoring a NaN operand.
func F32Max(a, b float32) float32 { return floatMax(a, b) }

// F32Min is `a.min(b)`: the smaller, ignoring a NaN operand.
func F32Min(a, b float32) float32 { return floatMin(a, b) }

// F32Clamp is `x.clamp(min, max)`. It panics when min > max or either bound
// is NaN.
func F32Clamp(x, min, max float32) float32 {
	if !(min <= max) {
		PanicAt(1, "min > max, or either was NaN. min = "+f32Debug(min)+", max = "+f32Debug(max))
	}
	if x < min {
		x = min
	}
	if x > max {
		x = max
	}
	return x
}

// F32Signum is `x.signum()`.
func F32Signum(x float32) float32 {
	if x != x {
		return F32Nan
	}
	return float32(math.Copysign(1, float64(x)))
}

// F32Fract is `x.fract()`: x - x.trunc().
func F32Fract(x float32) float32 { return x - F32Trunc(x) }

// F32RemEuclid is `x.rem_euclid(rhs)`.
func F32RemEuclid(x, rhs float32) float32 {
	r := f32Rem(x, rhs)
	if r < 0 {
		return r + F32Abs(rhs)
	}
	return r
}

// F32DivEuclid is `x.div_euclid(rhs)`.
func F32DivEuclid(x, rhs float32) float32 {
	q := F32Trunc(x / rhs)
	if f32Rem(x, rhs) < 0 {
		if rhs > 0 {
			return q - 1
		}
		return q + 1
	}
	return q
}

// f32Rem is `x % y` for f32. The remainder of two f32 values is an f32, so
// taking it in float64 loses nothing.
func f32Rem(x, y float32) float32 {
	return float32(math.Mod(float64(x), float64(y)))
}

// F32Rem is `x % y` for f32, which Go's operators do not offer.
func F32Rem(x, y float32) float32 { return f32Rem(x, y) }

// F32TotalCmp is `a.total_cmp(&b)`.
func F32TotalCmp(a, b float32) Ordering {
	left := int32(math.Float32bits(a))
	right := int32(math.Float32bits(b))
	left ^= int32(uint32(left>>31) >> 1)
	right ^= int32(uint32(right>>31) >> 1)
	switch {
	case left < right:
		return Ordering_Less
	case left > right:
		return Ordering_Greater
	default:
		return Ordering_Equal
	}
}

// F32Powi is `x.powi(n)`, by repeated squaring in f32.
func F32Powi(x float32, n int32) float32 {
	a := x
	pow := uint32(n)
	if n < 0 {
		pow = -pow
	}
	mul := float32(1)
	for {
		if pow&1 != 0 {
			mul = float32(mul * a)
		}
		pow >>= 1
		if pow == 0 {
			break
		}
		a = float32(a * a)
	}
	if n < 0 {
		return 1 / mul
	}
	return mul
}

// F32IsNan is `x.is_nan()`.
func F32IsNan(x float32) bool { return x != x }

// F32IsInfinite is `x.is_infinite()`.
func F32IsInfinite(x float32) bool { return math.IsInf(float64(x), 0) }

// F32IsFinite is `x.is_finite()`.
func F32IsFinite(x float32) bool { return math.Abs(float64(x)) < math.Inf(1) }

// F32IsSignNegative is `x.is_sign_negative()`.
func F32IsSignNegative(x float32) bool { return math.Float32bits(x)>>31 != 0 }

// F32IsSignPositive is `x.is_sign_positive()`.
func F32IsSignPositive(x float32) bool { return math.Float32bits(x)>>31 == 0 }

// F32Copysign is `x.copysign(sign)`.
func F32Copysign(x, sign float32) float32 {
	const signBit = 1 << 31
	return math.Float32frombits(math.Float32bits(x)&^signBit | math.Float32bits(sign)&signBit)
}

// F32Recip is `x.recip()`.
func F32Recip(x float32) float32 { return 1 / x }

// F32ToDegrees is `x.to_degrees()`.
func F32ToDegrees(x float32) float32 {
	// Use a literal to avoid double rounding, consts::PI is already rounded,
	// and dividing would round again.
	const pisIn180 float32 = 57.2957795130823208767981548141051703
	return x * pisIn180
}

// F32ToRadians is `x.to_radians()`.
func F32ToRadians(x float32) float32 {
	// Rust divides the f32 constant PI by 180 in f32.
	radsPerDeg := float32(math.Pi) / 180.0
	return x * radsPerDeg
}

// F32ToBits is `x.to_bits()`.
func F32ToBits(x float32) uint32 { return math.Float32bits(x) }

// F32FromBits is `f32::from_bits(v)`.
func F32FromBits(v uint32) float32 { return math.Float32frombits(v) }

// F32Round is `x.round()`: halfway cases away from zero.
func F32Round(x float32) float32 { return float32(math.Round(float64(x))) }

// F32RoundTiesEven is `x.round_ties_even()`.
func F32RoundTiesEven(x float32) float32 { return float32(math.RoundToEven(float64(x))) }

// F32Floor is `x.floor()`.
func F32Floor(x float32) float32 { return float32(math.Floor(float64(x))) }

// F32Ceil is `x.ceil()`.
func F32Ceil(x float32) float32 { return float32(math.Ceil(float64(x))) }

// F32Trunc is `x.trunc()`.
func F32Trunc(x float32) float32 { return float32(math.Trunc(float64(x))) }

// F32Abs is `x.abs()`.
func F32Abs(x float32) float32 {
	return math.Float32frombits(math.Float32bits(x) &^ (1 << 31))
}

// F32Sqrt is `x.sqrt()`. The square root taken in float64 and rounded to
// float32 is the correctly rounded f32 square root.
func F32Sqrt(x float32) float32 { return float32(math.Sqrt(float64(x))) }

// F32MulAdd is `x.mul_add(a, b)`: x*a + b with one rounding.
func F32MulAdd(x, a, b float32) float32 {
	// The product of two f32 values is exact in float64. Their sum with b
	// is rounded to odd in float64, which makes the second rounding, to
	// f32, the same as rounding the exact sum once.
	product := float64(x) * float64(a)
	addend := float64(b)
	sum := product + addend
	if math.IsInf(sum, 0) || sum != sum {
		return float32(sum)
	}
	// TwoSum: the exact error of the addition.
	bVirtual := sum - product
	aVirtual := sum - bVirtual
	err := (product - aVirtual) + (addend - bVirtual)
	if err != 0 {
		bits := math.Float64bits(sum)
		if bits&1 == 0 {
			// Step to the neighbour on the side of the exact sum.
			if (err > 0) == (sum > 0) {
				bits++
			} else {
				bits--
			}
			sum = math.Float64frombits(bits)
		}
	}
	return float32(sum)
}

// F32Cbrt is `x.cbrt()`.
func F32Cbrt(x float32) float32 { return float32(math.Cbrt(float64(x))) }

// F32Exp is `x.exp()`.
func F32Exp(x float32) float32 { return float32(math.Exp(float64(x))) }

// F32Exp2 is `x.exp2()`.
func F32Exp2(x float32) float32 { return float32(math.Exp2(float64(x))) }

// F32ExpM1 is `x.exp_m1()`.
func F32ExpM1(x float32) float32 { return float32(math.Expm1(float64(x))) }

// F32Ln is `x.ln()`.
func F32Ln(x float32) float32 { return float32(math.Log(float64(x))) }

// F32Ln1p is `x.ln_1p()`.
func F32Ln1p(x float32) float32 { return float32(math.Log1p(float64(x))) }

// F32Log is `x.log(base)`: x.ln() / base.ln(), each in f32.
func F32Log(x, base float32) float32 { return F32Ln(x) / F32Ln(base) }

// F32Log2 is `x.log2()`.
func F32Log2(x float32) float32 { return float32(F64Log2(float64(x))) }

// F32Log10 is `x.log10()`.
func F32Log10(x float32) float32 { return float32(F64Log10(float64(x))) }

// F32Powf is `x.powf(n)`.
func F32Powf(x, n float32) float32 { return float32(math.Pow(float64(x), float64(n))) }

// F32Hypot is `x.hypot(other)`.
func F32Hypot(x, other float32) float32 {
	return float32(math.Hypot(float64(x), float64(other)))
}

// F32Sin is `x.sin()`.
func F32Sin(x float32) float32 { return float32(math.Sin(float64(x))) }

// F32Cos is `x.cos()`.
func F32Cos(x float32) float32 { return float32(math.Cos(float64(x))) }

// F32Tan is `x.tan()`.
func F32Tan(x float32) float32 { return float32(math.Tan(float64(x))) }

// F32Asin is `x.asin()`.
func F32Asin(x float32) float32 { return float32(F64Asin(float64(x))) }

// F32Acos is `x.acos()`.
func F32Acos(x float32) float32 { return float32(F64Acos(float64(x))) }

// F32Atan is `x.atan()`.
func F32Atan(x float32) float32 { return float32(math.Atan(float64(x))) }

// F32Atan2 is `y.atan2(x)`.
func F32Atan2(y, x float32) float32 { return float32(math.Atan2(float64(y), float64(x))) }

// F32Sinh is `x.sinh()`.
func F32Sinh(x float32) float32 { return float32(math.Sinh(float64(x))) }

// F32Cosh is `x.cosh()`.
func F32Cosh(x float32) float32 { return float32(math.Cosh(float64(x))) }

// F32Tanh is `x.tanh()`.
func F32Tanh(x float32) float32 { return float32(math.Tanh(float64(x))) }
