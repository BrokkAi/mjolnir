// Port of core 1.99.1 src/num/uint_macros.rs and src/num/int_macros.rs, for
// u128 and i128

package rs

import (
	"encoding/binary"
	"math"
	"math/bits"
)

// U128 is Rust's u128. The zero value is 0, and two values compare with ==.
//
// Operators are methods named after Rust's operator traits: `a + b` is
// a.Add(b), `a << n` is a.Shl(n), `a < b` is a.Lt(b). As in Rust, the
// operators panic on overflow when DebugAssertions is on and wrap when it is
// off; division by zero always panics.
type U128 struct {
	hi, lo uint64
}

// I128 is Rust's i128, in two's complement. The zero value is 0, and two
// values compare with ==.
type I128 struct {
	hi, lo uint64
}

var (
	// U128Min is `u128::MIN`.
	U128Min = U128{}
	// U128Max is `u128::MAX`.
	U128Max = U128{hi: math.MaxUint64, lo: math.MaxUint64}
	// I128Min is `i128::MIN`.
	I128Min = I128{hi: 1 << 63}
	// I128Max is `i128::MAX`.
	I128Max = I128{hi: math.MaxInt64, lo: math.MaxUint64}
)

// ---------------------------------------------------------------------------
// Construction and conversion

// U128FromParts builds a u128 from its high and low 64 bits. It is how a
// literal that does not fit in 64 bits is written.
func U128FromParts(hi, lo uint64) U128 { return U128{hi: hi, lo: lo} }

// I128FromParts builds an i128 from the high and low 64 bits of its two's
// complement form.
func I128FromParts(hi, lo uint64) I128 { return I128{hi: hi, lo: lo} }

// Hi is the high 64 bits.
func (x U128) Hi() uint64 { return x.hi }

// Lo is the low 64 bits.
func (x U128) Lo() uint64 { return x.lo }

// Hi is the high 64 bits of the two's complement form.
func (x I128) Hi() uint64 { return x.hi }

// Lo is the low 64 bits of the two's complement form.
func (x I128) Lo() uint64 { return x.lo }

// U128FromU64 is `u128::from(v)` for a u64, and how a literal is written:
// `250_u128` is rs.U128FromU64(250).
func U128FromU64(v uint64) U128 { return U128{lo: v} }

// U128FromU32 is `u128::from(v)` for a u32.
func U128FromU32(v uint32) U128 { return U128{lo: uint64(v)} }

// U128FromU16 is `u128::from(v)` for a u16.
func U128FromU16(v uint16) U128 { return U128{lo: uint64(v)} }

// U128FromU8 is `u128::from(v)` for a u8.
func U128FromU8(v uint8) U128 { return U128{lo: uint64(v)} }

// U128FromBool is `u128::from(b)`.
func U128FromBool(b bool) U128 { return U128{lo: FromBool[uint64](b)} }

// I128FromI64 is `i128::from(v)` for an i64, and how a literal is written.
func I128FromI64(v int64) I128 { return I128{hi: uint64(v >> 63), lo: uint64(v)} }

// I128FromI32 is `i128::from(v)` for an i32.
func I128FromI32(v int32) I128 { return I128FromI64(int64(v)) }

// I128FromI16 is `i128::from(v)` for an i16.
func I128FromI16(v int16) I128 { return I128FromI64(int64(v)) }

// I128FromI8 is `i128::from(v)` for an i8.
func I128FromI8(v int8) I128 { return I128FromI64(int64(v)) }

// I128FromU64 is `i128::from(v)` for a u64.
func I128FromU64(v uint64) I128 { return I128{lo: v} }

// I128FromU32 is `i128::from(v)` for a u32.
func I128FromU32(v uint32) I128 { return I128{lo: uint64(v)} }

// I128FromU16 is `i128::from(v)` for a u16.
func I128FromU16(v uint16) I128 { return I128{lo: uint64(v)} }

// I128FromU8 is `i128::from(v)` for a u8.
func I128FromU8(v uint8) I128 { return I128{lo: uint64(v)} }

// I128FromBool is `i128::from(b)`.
func I128FromBool(b bool) I128 { return I128{lo: FromBool[uint64](b)} }

// CastToU128 is `v as u128` for any integer: a signed value is
// sign-extended, so a negative one becomes a value near u128::MAX. An int is
// read as signed, which is right for a usize too.
func CastToU128[T anyInt](v T) U128 {
	if intIsSigned[T]() && v < 0 {
		return U128{hi: math.MaxUint64, lo: uint64(int64(v))}
	}
	return U128{lo: uint64(v)}
}

// CastToI128 is `v as i128` for any integer.
func CastToI128[T anyInt](v T) I128 {
	return I128(CastToU128(v))
}

// CastU128To is `x as T` for an integer type T: the low bits of x.
// rs.CastU128To[uint8](x) is `x as u8`; rs.CastU128To[int](x) is `x as
// usize` or `x as isize`.
func CastU128To[T anyInt](x U128) T { return T(x.lo) }

// CastI128To is `x as T` for an integer type T: the low bits of x.
func CastI128To[T anyInt](x I128) T { return T(x.lo) }

// CastU128ToI128 is `x as i128`: the same bits.
func CastU128ToI128(x U128) I128 { return I128(x) }

// CastI128ToU128 is `x as u128`: the same bits.
func CastI128ToU128(x I128) U128 { return U128(x) }

// wideTop is the top 64 bits of x, with everything below them folded into
// the lowest bit, and how far they were shifted down. Converting top to a
// float rounds as converting all of x would (nearest, ties to even).
func wideTop(x U128) (top uint64, shift int) {
	if x.hi == 0 {
		return x.lo, 0
	}
	shift = 64 - bits.LeadingZeros64(x.hi)
	top = x.hi<<(64-shift) | x.lo>>shift
	if x.lo<<(64-shift) != 0 {
		top |= 1
	}
	return top, shift
}

// CastU128ToF64 is `x as f64`: the nearest f64, ties to even.
func CastU128ToF64(x U128) float64 {
	top, shift := wideTop(x)
	return math.Ldexp(float64(top), shift)
}

// CastU128ToF32 is `x as f32`: the nearest f32, ties to even, and infinity
// for the values at the top of the range that round past f32::MAX.
func CastU128ToF32(x U128) float32 {
	top, shift := wideTop(x)
	return float32(math.Ldexp(float64(float32(top)), shift))
}

// CastI128ToF64 is `x as f64`.
func CastI128ToF64(x I128) float64 {
	if x.IsNegative() {
		return -CastU128ToF64(x.UnsignedAbs())
	}
	return CastU128ToF64(U128(x))
}

// CastI128ToF32 is `x as f32`.
func CastI128ToF32(x I128) float32 {
	if x.IsNegative() {
		return -CastU128ToF32(x.UnsignedAbs())
	}
	return CastU128ToF32(U128(x))
}

// floatToWide is the integer part of a finite x >= 0 below 2^128.
func floatToWide(x float64) U128 {
	if x < 1<<64 {
		return U128{lo: uint64(x)}
	}
	fraction, exponent := math.Frexp(x)
	mantissa := uint64(math.Ldexp(fraction, 64))
	return wideShr(U128{hi: mantissa}, uint32(128-exponent))
}

// CastF64ToU128 is `x as u128` for an f64: NaN and negative values are 0,
// values of 2^128 and more are u128::MAX, and anything else is truncated.
func CastF64ToU128(x float64) U128 {
	if x != x || x <= 0 {
		return U128{}
	}
	if x >= math.Ldexp(1, 128) {
		return U128Max
	}
	return floatToWide(x)
}

// CastF64ToI128 is `x as i128` for an f64.
func CastF64ToI128(x float64) I128 {
	if x != x {
		return I128{}
	}
	limit := math.Ldexp(1, 127)
	if x >= limit {
		return I128Max
	}
	if x <= -limit {
		return I128Min
	}
	if x < 0 {
		return I128(floatToWide(-x)).WrappingNeg()
	}
	return I128(floatToWide(x))
}

// CastF32ToU128 is `x as u128` for an f32.
func CastF32ToU128(x float32) U128 { return CastF64ToU128(float64(x)) }

// CastF32ToI128 is `x as i128` for an f32.
func CastF32ToI128(x float32) I128 { return CastF64ToI128(float64(x)) }

// U128TryFrom is `u128::try_from(v)` for an integer: an error for a
// negative value.
func U128TryFrom[S anyInt](v S) (U128, error) {
	if intIsSigned[S]() && v < 0 {
		return U128{}, TryFromIntError{kind: IntErrorKind_NegOverflow}
	}
	return U128{lo: uint64(v)}, nil
}

// U128TryFromI128 is `u128::try_from(v)` for an i128.
func U128TryFromI128(v I128) (U128, error) {
	if v.IsNegative() {
		return U128{}, TryFromIntError{kind: IntErrorKind_NegOverflow}
	}
	return U128(v), nil
}

// I128TryFromU128 is `i128::try_from(v)` for a u128.
func I128TryFromU128(v U128) (I128, error) {
	if v.hi>>63 != 0 {
		return I128{}, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return I128(v), nil
}

// TryFromU128 is `T::try_from(v)` for a u128 v and a fixed-width integer
// type T: rs.TryFromU128[uint64](v).
func TryFromU128[T FixedInt](v U128) (T, error) {
	if v.hi != 0 || v.lo > uint64(intMax[T]()) {
		return 0, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return T(v.lo), nil
}

// TryFromI128 is `T::try_from(v)` for an i128 v and a fixed-width integer
// type T.
func TryFromI128[T FixedInt](v I128) (T, error) { return tryFromI128[T](v) }

func tryFromI128[T anyInt](v I128) (T, error) {
	if v.IsNegative() {
		if !intIsSigned[T]() || v.hi != math.MaxUint64 || int64(v.lo) >= 0 || int64(v.lo) < int64(intMin[T]()) {
			return 0, TryFromIntError{kind: IntErrorKind_NegOverflow}
		}
		return T(int64(v.lo)), nil
	}
	if v.hi != 0 || v.lo > uint64(intMax[T]()) {
		return 0, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return T(v.lo), nil
}

// UsizeTryFromU128 is `usize::try_from(v)` for a u128: an error above
// UsizeMax.
func UsizeTryFromU128(v U128) (int, error) {
	if v.hi != 0 || !usizeFits(v.lo) {
		return 0, TryFromIntError{kind: IntErrorKind_PosOverflow}
	}
	return int(v.lo), nil
}

// UsizeTryFromI128 is `usize::try_from(v)` for an i128.
func UsizeTryFromI128(v I128) (int, error) {
	if v.IsNegative() {
		return 0, TryFromIntError{kind: IntErrorKind_NegOverflow}
	}
	return UsizeTryFromU128(U128(v))
}

// IsizeTryFromU128 is `isize::try_from(v)` for a u128.
func IsizeTryFromU128(v U128) (int, error) { return UsizeTryFromU128(v) }

// IsizeTryFromI128 is `isize::try_from(v)` for an i128.
func IsizeTryFromI128(v I128) (int, error) { return tryFromI128[int](v) }

// ---------------------------------------------------------------------------
// The unsigned arithmetic everything else is built on.

func wideAdd(a, b U128) (U128, bool) {
	lo, carry := bits.Add64(a.lo, b.lo, 0)
	hi, carry := bits.Add64(a.hi, b.hi, carry)
	return U128{hi: hi, lo: lo}, carry != 0
}

func wideSub(a, b U128) (U128, bool) {
	lo, borrow := bits.Sub64(a.lo, b.lo, 0)
	hi, borrow := bits.Sub64(a.hi, b.hi, borrow)
	return U128{hi: hi, lo: lo}, borrow != 0
}

// wideMul is the low 128 bits of a*b and whether the product needed more.
func wideMul(a, b U128) (U128, bool) {
	hi, lo := bits.Mul64(a.lo, b.lo)
	crossHi1, cross1 := bits.Mul64(a.hi, b.lo)
	crossHi2, cross2 := bits.Mul64(a.lo, b.hi)
	hi, carry1 := bits.Add64(hi, cross1, 0)
	hi, carry2 := bits.Add64(hi, cross2, 0)
	overflow := (a.hi != 0 && b.hi != 0) || crossHi1 != 0 || crossHi2 != 0 || carry1 != 0 || carry2 != 0
	return U128{hi: hi, lo: lo}, overflow
}

func wideCmp(a, b U128) int {
	switch {
	case a.hi < b.hi:
		return -1
	case a.hi > b.hi:
		return 1
	case a.lo < b.lo:
		return -1
	case a.lo > b.lo:
		return 1
	default:
		return 0
	}
}

func wideShl(a U128, n uint32) U128 {
	switch {
	case n == 0:
		return a
	case n < 64:
		return U128{hi: a.hi<<n | a.lo>>(64-n), lo: a.lo << n}
	case n < 128:
		return U128{hi: a.lo << (n - 64)}
	default:
		return U128{}
	}
}

func wideShr(a U128, n uint32) U128 {
	switch {
	case n == 0:
		return a
	case n < 64:
		return U128{hi: a.hi >> n, lo: a.lo>>n | a.hi<<(64-n)}
	case n < 128:
		return U128{lo: a.hi >> (n - 64)}
	default:
		return U128{}
	}
}

// wideDivRem64 divides by a 64-bit divisor that is not zero.
func wideDivRem64(a U128, b uint64) (U128, uint64) {
	if a.hi < b {
		lo, remainder := bits.Div64(a.hi, a.lo, b)
		return U128{lo: lo}, remainder
	}
	hi, remainder := bits.Div64(0, a.hi, b)
	lo, remainder := bits.Div64(remainder, a.lo, b)
	return U128{hi: hi, lo: lo}, remainder
}

// wideDivRem divides by a divisor that is not zero.
func wideDivRem(a, b U128) (quotient, remainder U128) {
	if b.hi == 0 {
		q, r := wideDivRem64(a, b.lo)
		return q, U128{lo: r}
	}
	// The divisor is 2^64 or more, so the quotient fits in 64 bits. Divide
	// the top of the dividend by the top of the normalized divisor for a
	// trial quotient that is the true one or one above it, then correct.
	n := uint32(bits.LeadingZeros64(b.hi))
	normalized := wideShl(b, n)
	halved := wideShr(a, 1)
	trial, _ := bits.Div64(halved.hi, halved.lo, normalized.hi)
	trial >>= 63 - n
	if trial != 0 {
		trial--
	}
	product, _ := wideMul(b, U128{lo: trial})
	remainder, _ = wideSub(a, product)
	if wideCmp(remainder, b) >= 0 {
		trial++
		remainder, _ = wideSub(remainder, b)
	}
	return U128{lo: trial}, remainder
}

func wideNeg(a U128) U128 {
	negated, _ := wideSub(U128{}, a)
	return negated
}

// widePow follows `overflowing_pow`, with multiply as the type's
// overflowing multiplication.
func widePow[T any](base T, one T, exp uint32, multiply func(a, b T) (T, bool)) (T, bool) {
	if exp == 0 {
		return one, false
	}
	acc := one
	overflow := false
	for {
		if exp&1 == 1 {
			var step bool
			acc, step = multiply(acc, base)
			overflow = overflow || step
			if exp == 1 {
				return acc, overflow
			}
		}
		exp /= 2
		var step bool
		base, step = multiply(base, base)
		overflow = overflow || step
	}
}

// wideCheckedPow follows `checked_pow`.
func wideCheckedPow[T any](base T, one T, exp uint32, multiply func(a, b T) (T, bool)) (T, bool) {
	if exp == 0 {
		return one, true
	}
	acc := one
	for {
		if exp&1 == 1 {
			var overflow bool
			acc, overflow = multiply(acc, base)
			if overflow {
				return acc, false
			}
			if exp == 1 {
				return acc, true
			}
		}
		exp /= 2
		var overflow bool
		base, overflow = multiply(base, base)
		if overflow {
			return acc, false
		}
	}
}

// ---------------------------------------------------------------------------
// U128: operators

// Add is `x + y`.
func (x U128) Add(y U128) U128 {
	sum, overflow := wideAdd(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicAddOverflow)
	}
	return sum
}

// Sub is `x - y`.
func (x U128) Sub(y U128) U128 {
	difference, overflow := wideSub(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicSubOverflow)
	}
	return difference
}

// Mul is `x * y`.
func (x U128) Mul(y U128) U128 {
	product, overflow := wideMul(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicMulOverflow)
	}
	return product
}

// Div is `x / y`. It panics when y is zero.
func (x U128) Div(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicDivByZero)
	}
	quotient, _ := wideDivRem(x, y)
	return quotient
}

// Rem is `x % y`. It panics when y is zero.
func (x U128) Rem(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicRemByZero)
	}
	_, remainder := wideDivRem(x, y)
	return remainder
}

// Shl is `x << n`. A count of 128 or more panics when DebugAssertions is on
// and is taken modulo 128 otherwise.
func (x U128) Shl(n uint32) U128 {
	if n >= 128 && DebugAssertions {
		PanicAt(1, panicShlOverflow)
	}
	return wideShl(x, n&127)
}

// Shr is `x >> n`.
func (x U128) Shr(n uint32) U128 {
	if n >= 128 && DebugAssertions {
		PanicAt(1, panicShrOverflow)
	}
	return wideShr(x, n&127)
}

// BitAnd is `x & y`.
func (x U128) BitAnd(y U128) U128 { return U128{hi: x.hi & y.hi, lo: x.lo & y.lo} }

// BitOr is `x | y`.
func (x U128) BitOr(y U128) U128 { return U128{hi: x.hi | y.hi, lo: x.lo | y.lo} }

// BitXor is `x ^ y`.
func (x U128) BitXor(y U128) U128 { return U128{hi: x.hi ^ y.hi, lo: x.lo ^ y.lo} }

// Not is `!x`.
func (x U128) Not() U128 { return U128{hi: ^x.hi, lo: ^x.lo} }

// Compare is `x.cmp(&y)`: -1, 0 or 1.
func (x U128) Compare(y U128) int { return wideCmp(x, y) }

// Equal is `x == y`.
func (x U128) Equal(y U128) bool { return x == y }

// Lt is `x < y`.
func (x U128) Lt(y U128) bool { return wideCmp(x, y) < 0 }

// Le is `x <= y`.
func (x U128) Le(y U128) bool { return wideCmp(x, y) <= 0 }

// Gt is `x > y`.
func (x U128) Gt(y U128) bool { return wideCmp(x, y) > 0 }

// Ge is `x >= y`.
func (x U128) Ge(y U128) bool { return wideCmp(x, y) >= 0 }

// Min is `x.min(y)`.
func (x U128) Min(y U128) U128 {
	if wideCmp(y, x) < 0 {
		return y
	}
	return x
}

// Max is `x.max(y)`.
func (x U128) Max(y U128) U128 {
	if wideCmp(y, x) < 0 {
		return x
	}
	return y
}

// Clamp is `x.clamp(min, max)`. It panics when min is greater than max.
func (x U128) Clamp(min, max U128) U128 {
	if wideCmp(min, max) > 0 {
		PanicAt(1, "min > max. min = "+min.String()+", max = "+max.String())
	}
	if wideCmp(x, min) < 0 {
		return min
	}
	if wideCmp(x, max) > 0 {
		return max
	}
	return x
}

// ---------------------------------------------------------------------------
// U128: methods

// CheckedAdd is `x.checked_add(y)`.
func (x U128) CheckedAdd(y U128) Option[U128] {
	sum, overflow := wideAdd(x, y)
	return optionOf(sum, !overflow)
}

// CheckedSub is `x.checked_sub(y)`.
func (x U128) CheckedSub(y U128) Option[U128] {
	difference, overflow := wideSub(x, y)
	return optionOf(difference, !overflow)
}

// CheckedMul is `x.checked_mul(y)`.
func (x U128) CheckedMul(y U128) Option[U128] {
	product, overflow := wideMul(x, y)
	return optionOf(product, !overflow)
}

// CheckedDiv is `x.checked_div(y)`: None when y is zero.
func (x U128) CheckedDiv(y U128) Option[U128] {
	if y == (U128{}) {
		return Option[U128]{}
	}
	quotient, _ := wideDivRem(x, y)
	return Some(quotient)
}

// CheckedRem is `x.checked_rem(y)`: None when y is zero.
func (x U128) CheckedRem(y U128) Option[U128] {
	if y == (U128{}) {
		return Option[U128]{}
	}
	_, remainder := wideDivRem(x, y)
	return Some(remainder)
}

// CheckedNeg is `x.checked_neg()`: None for everything but zero.
func (x U128) CheckedNeg() Option[U128] {
	return optionOf(U128{}, x == (U128{}))
}

// CheckedPow is `x.checked_pow(exp)`.
func (x U128) CheckedPow(exp uint32) Option[U128] {
	power, ok := wideCheckedPow(x, U128{lo: 1}, exp, wideMul)
	if !ok {
		return Option[U128]{}
	}
	return Some(power)
}

// CheckedShl is `x.checked_shl(n)`: None when n is 128 or more.
func (x U128) CheckedShl(n uint32) Option[U128] {
	if n >= 128 {
		return Option[U128]{}
	}
	return Some(wideShl(x, n))
}

// CheckedShr is `x.checked_shr(n)`: None when n is 128 or more.
func (x U128) CheckedShr(n uint32) Option[U128] {
	if n >= 128 {
		return Option[U128]{}
	}
	return Some(wideShr(x, n))
}

// CheckedNextPowerOfTwo is `x.checked_next_power_of_two()`.
func (x U128) CheckedNextPowerOfTwo() Option[U128] {
	sum, overflow := wideAdd(x.oneLessThanNextPowerOfTwo(), U128{lo: 1})
	return optionOf(sum, !overflow)
}

// SaturatingAdd is `x.saturating_add(y)`.
func (x U128) SaturatingAdd(y U128) U128 {
	sum, overflow := wideAdd(x, y)
	if overflow {
		return U128Max
	}
	return sum
}

// SaturatingSub is `x.saturating_sub(y)`.
func (x U128) SaturatingSub(y U128) U128 {
	difference, overflow := wideSub(x, y)
	if overflow {
		return U128{}
	}
	return difference
}

// SaturatingMul is `x.saturating_mul(y)`.
func (x U128) SaturatingMul(y U128) U128 {
	product, overflow := wideMul(x, y)
	if overflow {
		return U128Max
	}
	return product
}

// SaturatingPow is `x.saturating_pow(exp)`.
func (x U128) SaturatingPow(exp uint32) U128 {
	power, ok := wideCheckedPow(x, U128{lo: 1}, exp, wideMul)
	if !ok {
		return U128Max
	}
	return power
}

// WrappingAdd is `x.wrapping_add(y)`.
func (x U128) WrappingAdd(y U128) U128 {
	sum, _ := wideAdd(x, y)
	return sum
}

// WrappingSub is `x.wrapping_sub(y)`.
func (x U128) WrappingSub(y U128) U128 {
	difference, _ := wideSub(x, y)
	return difference
}

// WrappingMul is `x.wrapping_mul(y)`.
func (x U128) WrappingMul(y U128) U128 {
	product, _ := wideMul(x, y)
	return product
}

// WrappingNeg is `x.wrapping_neg()`.
func (x U128) WrappingNeg() U128 { return wideNeg(x) }

// WrappingPow is `x.wrapping_pow(exp)`.
func (x U128) WrappingPow(exp uint32) U128 {
	power, _ := widePow(x, U128{lo: 1}, exp, wideMul)
	return power
}

// WrappingShl is `x.wrapping_shl(n)`.
func (x U128) WrappingShl(n uint32) U128 { return wideShl(x, n&127) }

// WrappingShr is `x.wrapping_shr(n)`.
func (x U128) WrappingShr(n uint32) U128 { return wideShr(x, n&127) }

// OverflowingAdd is `x.overflowing_add(y)`.
func (x U128) OverflowingAdd(y U128) (U128, bool) { return wideAdd(x, y) }

// OverflowingSub is `x.overflowing_sub(y)`.
func (x U128) OverflowingSub(y U128) (U128, bool) { return wideSub(x, y) }

// OverflowingMul is `x.overflowing_mul(y)`.
func (x U128) OverflowingMul(y U128) (U128, bool) { return wideMul(x, y) }

// OverflowingPow is `x.overflowing_pow(exp)`.
func (x U128) OverflowingPow(exp uint32) (U128, bool) {
	return widePow(x, U128{lo: 1}, exp, wideMul)
}

// Pow is `x.pow(exp)`. Overflow panics when DebugAssertions is on and wraps
// otherwise.
func (x U128) Pow(exp uint32) U128 {
	if DebugAssertions {
		power, ok := wideCheckedPow(x, U128{lo: 1}, exp, wideMul)
		if !ok {
			PanicAt(1, panicPowOverflow)
		}
		return power
	}
	power, _ := widePow(x, U128{lo: 1}, exp, wideMul)
	return power
}

// AbsDiff is `x.abs_diff(y)`.
func (x U128) AbsDiff(y U128) U128 {
	if wideCmp(x, y) < 0 {
		difference, _ := wideSub(y, x)
		return difference
	}
	difference, _ := wideSub(x, y)
	return difference
}

// DivEuclid is `x.div_euclid(y)`, which is `x / y`.
func (x U128) DivEuclid(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicDivByZero)
	}
	quotient, _ := wideDivRem(x, y)
	return quotient
}

// RemEuclid is `x.rem_euclid(y)`, which is `x % y`.
func (x U128) RemEuclid(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicRemByZero)
	}
	_, remainder := wideDivRem(x, y)
	return remainder
}

// DivCeil is `x.div_ceil(y)`: the quotient rounded up. It panics when y is
// zero.
func (x U128) DivCeil(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicDivByZero)
	}
	quotient, remainder := wideDivRem(x, y)
	if remainder != (U128{}) {
		quotient, _ = wideAdd(quotient, U128{lo: 1})
	}
	return quotient
}

// NextMultipleOf is `x.next_multiple_of(y)`. It panics when y is zero;
// overflow panics when DebugAssertions is on and wraps otherwise.
func (x U128) NextMultipleOf(y U128) U128 {
	if y == (U128{}) {
		PanicAt(1, panicRemByZero)
	}
	_, remainder := wideDivRem(x, y)
	if remainder == (U128{}) {
		return x
	}
	step, _ := wideSub(y, remainder)
	sum, overflow := wideAdd(x, step)
	if overflow && DebugAssertions {
		PanicAt(1, panicAddOverflow)
	}
	return sum
}

// IsMultipleOf is `x.is_multiple_of(y)`.
func (x U128) IsMultipleOf(y U128) bool {
	if y == (U128{}) {
		return x == (U128{})
	}
	_, remainder := wideDivRem(x, y)
	return remainder == (U128{})
}

// CountOnes is `x.count_ones()`.
func (x U128) CountOnes() uint32 {
	return uint32(bits.OnesCount64(x.hi) + bits.OnesCount64(x.lo))
}

// CountZeros is `x.count_zeros()`.
func (x U128) CountZeros() uint32 { return 128 - x.CountOnes() }

// LeadingZeros is `x.leading_zeros()`.
func (x U128) LeadingZeros() uint32 {
	if x.hi != 0 {
		return uint32(bits.LeadingZeros64(x.hi))
	}
	return 64 + uint32(bits.LeadingZeros64(x.lo))
}

// TrailingZeros is `x.trailing_zeros()`.
func (x U128) TrailingZeros() uint32 {
	if x.lo != 0 {
		return uint32(bits.TrailingZeros64(x.lo))
	}
	return 64 + uint32(bits.TrailingZeros64(x.hi))
}

// IsPowerOfTwo is `x.is_power_of_two()`.
func (x U128) IsPowerOfTwo() bool { return x.CountOnes() == 1 }

func (x U128) oneLessThanNextPowerOfTwo() U128 {
	if x.hi == 0 && x.lo <= 1 {
		return U128{}
	}
	p, _ := wideSub(x, U128{lo: 1})
	return wideShr(U128Max, p.LeadingZeros())
}

// NextPowerOfTwo is `x.next_power_of_two()`. Where the next power of two
// does not fit it panics when DebugAssertions is on and is zero otherwise.
func (x U128) NextPowerOfTwo() U128 {
	sum, overflow := wideAdd(x.oneLessThanNextPowerOfTwo(), U128{lo: 1})
	if overflow && DebugAssertions {
		PanicAt(1, panicAddOverflow)
	}
	return sum
}

// Ilog2 is `x.ilog2()`. It panics for zero.
func (x U128) Ilog2() uint32 {
	if x == (U128{}) {
		PanicAt(1, panicIlogArgument)
	}
	return 127 - x.LeadingZeros()
}

// SwapBytes is `x.swap_bytes()`.
func (x U128) SwapBytes() U128 {
	return U128{hi: bits.ReverseBytes64(x.lo), lo: bits.ReverseBytes64(x.hi)}
}

// ToLeBytes is `x.to_le_bytes()`.
func (x U128) ToLeBytes() [16]byte {
	var bytes [16]byte
	binary.LittleEndian.PutUint64(bytes[:8], x.lo)
	binary.LittleEndian.PutUint64(bytes[8:], x.hi)
	return bytes
}

// ToBeBytes is `x.to_be_bytes()`.
func (x U128) ToBeBytes() [16]byte {
	var bytes [16]byte
	binary.BigEndian.PutUint64(bytes[:8], x.hi)
	binary.BigEndian.PutUint64(bytes[8:], x.lo)
	return bytes
}

// ToNeBytes is `x.to_ne_bytes()`.
func (x U128) ToNeBytes() [16]byte {
	if nativeLittleEndian {
		return x.ToLeBytes()
	}
	return x.ToBeBytes()
}

// nativeLittleEndian reports the byte order of the machine.
var nativeLittleEndian = binary.NativeEndian.Uint16([]byte{1, 0}) == 1

// U128FromLeBytes is `u128::from_le_bytes(bytes)`.
func U128FromLeBytes(bytes [16]byte) U128 {
	return U128{hi: binary.LittleEndian.Uint64(bytes[8:]), lo: binary.LittleEndian.Uint64(bytes[:8])}
}

// U128FromBeBytes is `u128::from_be_bytes(bytes)`.
func U128FromBeBytes(bytes [16]byte) U128 {
	return U128{hi: binary.BigEndian.Uint64(bytes[:8]), lo: binary.BigEndian.Uint64(bytes[8:])}
}

// U128FromNeBytes is `u128::from_ne_bytes(bytes)`.
func U128FromNeBytes(bytes [16]byte) U128 {
	if nativeLittleEndian {
		return U128FromLeBytes(bytes)
	}
	return U128FromBeBytes(bytes)
}

// Text is x written in the given base, 2 to 36, with lower-case letters and
// no prefix.
func (x U128) Text(base int) string {
	if base < 2 || base > 36 {
		PanicAt(1, "rs.U128.Text: base must lie in the range [2, 36]")
	}
	if x == (U128{}) {
		return "0"
	}
	const digits = "0123456789abcdefghijklmnopqrstuvwxyz"
	var buffer [128]byte
	position := len(buffer)
	for x != (U128{}) {
		var digit uint64
		x, digit = wideDivRem64(x, uint64(base))
		position--
		buffer[position] = digits[digit]
	}
	return string(buffer[position:])
}

// String is x in decimal, its Display form.
func (x U128) String() string { return x.Text(10) }

// FmtDisplay is `impl Display for u128`.
func (x U128) FmtDisplay(f *Formatter) error { return f.PadIntegral(true, "", x.Text(10)) }

// FmtDebug is `impl Debug for u128`.
func (x U128) FmtDebug(f *Formatter) error { return f.PadIntegral(true, "", x.Text(10)) }

// FmtLowerHex is `impl LowerHex for u128`: `{:x}`.
func (x U128) FmtLowerHex(f *Formatter) error { return f.PadIntegral(true, "0x", x.Text(16)) }

// FmtUpperHex is `impl UpperHex for u128`: `{:X}`.
func (x U128) FmtUpperHex(f *Formatter) error {
	return f.PadIntegral(true, "0x", upperHex(x.Text(16)))
}

// FmtOctal is `impl Octal for u128`: `{:o}`.
func (x U128) FmtOctal(f *Formatter) error { return f.PadIntegral(true, "0o", x.Text(8)) }

// FmtBinary is `impl Binary for u128`: `{:b}`.
func (x U128) FmtBinary(f *Formatter) error { return f.PadIntegral(true, "0b", x.Text(2)) }

func upperHex(text string) string {
	upper := []byte(text)
	for index, c := range upper {
		if c >= 'a' && c <= 'f' {
			upper[index] = c - 'a' + 'A'
		}
	}
	return string(upper)
}

// U128FromStrRadix is `u128::from_str_radix(src, radix)`. It panics when
// radix is not in 2..=36.
func U128FromStrRadix(src string, radix uint32) (U128, error) {
	checkRadix(1, radix)
	return u128FromStrRadix(src, radix)
}

// U128FromStr is `u128::from_str(src)`, which is what
// `src.parse::<u128>()` calls.
func U128FromStr(src string) (U128, error) { return u128FromStrRadix(src, 10) }

func u128FromStrRadix(src string, radix uint32) (U128, error) {
	if len(src) == 0 {
		return U128{}, parseIntError(IntErrorKind_Empty)
	}
	digits := src
	switch {
	case src == "+" || src == "-":
		return U128{}, parseIntError(IntErrorKind_InvalidDigit)
	case src[0] == '+':
		digits = src[1:]
	}
	var result U128
	base := U128{lo: uint64(radix)}
	for index := 0; index < len(digits); index++ {
		mul, mulOverflow := wideMul(result, base)
		x, ok := toDigit(digits[index], radix)
		if !ok {
			return U128{}, parseIntError(IntErrorKind_InvalidDigit)
		}
		if mulOverflow {
			return U128{}, parseIntError(IntErrorKind_PosOverflow)
		}
		sum, overflow := wideAdd(mul, U128{lo: uint64(x)})
		if overflow {
			return U128{}, parseIntError(IntErrorKind_PosOverflow)
		}
		result = sum
	}
	return result, nil
}

// ---------------------------------------------------------------------------
// I128

// IsNegative is `x.is_negative()`.
func (x I128) IsNegative() bool { return x.hi>>63 != 0 }

// IsPositive is `x.is_positive()`.
func (x I128) IsPositive() bool { return !x.IsNegative() && x != (I128{}) }

func signedAdd(a, b I128) (I128, bool) {
	sum, _ := wideAdd(U128(a), U128(b))
	result := I128(sum)
	return result, a.IsNegative() == b.IsNegative() && result.IsNegative() != a.IsNegative()
}

func signedSub(a, b I128) (I128, bool) {
	difference, _ := wideSub(U128(a), U128(b))
	result := I128(difference)
	return result, a.IsNegative() != b.IsNegative() && result.IsNegative() != a.IsNegative()
}

func signedMul(a, b I128) (I128, bool) {
	negative := a.IsNegative() != b.IsNegative()
	product, overflow := wideMul(a.UnsignedAbs(), b.UnsignedAbs())
	result := I128(product)
	if negative {
		result = I128(wideNeg(product))
		// The magnitude may be 2^127 exactly, which is MIN.
		if product.hi>>63 != 0 && product != U128(I128Min) {
			overflow = true
		}
	} else if product.hi>>63 != 0 {
		overflow = true
	}
	if overflow {
		// The wrapped product is the low bits of the two's complement
		// product, which is the unsigned product of the bit patterns.
		wrapped, _ := wideMul(U128(a), U128(b))
		return I128(wrapped), true
	}
	return result, false
}

// signedDivRem truncates toward zero. The divisor is not zero and the
// division is not MIN / -1.
func signedDivRem(a, b I128) (I128, I128) {
	quotient, remainder := wideDivRem(a.UnsignedAbs(), b.UnsignedAbs())
	q, r := I128(quotient), I128(remainder)
	if a.IsNegative() != b.IsNegative() {
		q = I128(wideNeg(quotient))
	}
	if a.IsNegative() {
		r = I128(wideNeg(remainder))
	}
	return q, r
}

func (x I128) divOverflows(y I128) bool {
	return x == I128Min && y == I128{hi: math.MaxUint64, lo: math.MaxUint64}
}

// Add is `x + y`.
func (x I128) Add(y I128) I128 {
	sum, overflow := signedAdd(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicAddOverflow)
	}
	return sum
}

// Sub is `x - y`.
func (x I128) Sub(y I128) I128 {
	difference, overflow := signedSub(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicSubOverflow)
	}
	return difference
}

// Mul is `x * y`.
func (x I128) Mul(y I128) I128 {
	product, overflow := signedMul(x, y)
	if overflow && DebugAssertions {
		PanicAt(1, panicMulOverflow)
	}
	return product
}

// Div is `x / y`, truncated toward zero. It panics when y is zero and for
// MIN / -1.
func (x I128) Div(y I128) I128 {
	if y == (I128{}) {
		PanicAt(1, panicDivByZero)
	}
	if x.divOverflows(y) {
		PanicAt(1, panicDivOverflow)
	}
	quotient, _ := signedDivRem(x, y)
	return quotient
}

// Rem is `x % y`, with the sign of x. It panics when y is zero and for
// MIN % -1.
func (x I128) Rem(y I128) I128 {
	if y == (I128{}) {
		PanicAt(1, panicRemByZero)
	}
	if x.divOverflows(y) {
		PanicAt(1, panicRemOverflow)
	}
	_, remainder := signedDivRem(x, y)
	return remainder
}

// Neg is `-x`. Negating MIN panics when DebugAssertions is on and is MIN
// otherwise.
func (x I128) Neg() I128 {
	if x == I128Min && DebugAssertions {
		PanicAt(1, panicNegOverflow)
	}
	return I128(wideNeg(U128(x)))
}

// Shl is `x << n`.
func (x I128) Shl(n uint32) I128 {
	if n >= 128 && DebugAssertions {
		PanicAt(1, panicShlOverflow)
	}
	return I128(wideShl(U128(x), n&127))
}

// Shr is `x >> n`: an arithmetic shift, which keeps the sign.
func (x I128) Shr(n uint32) I128 {
	if n >= 128 && DebugAssertions {
		PanicAt(1, panicShrOverflow)
	}
	return x.arithmeticShr(n & 127)
}

func (x I128) arithmeticShr(n uint32) I128 {
	shifted := wideShr(U128(x), n)
	if x.IsNegative() && n > 0 {
		shifted = shifted.BitOr(wideShl(U128Max, 128-n))
	}
	return I128(shifted)
}

// BitAnd is `x & y`.
func (x I128) BitAnd(y I128) I128 { return I128{hi: x.hi & y.hi, lo: x.lo & y.lo} }

// BitOr is `x | y`.
func (x I128) BitOr(y I128) I128 { return I128{hi: x.hi | y.hi, lo: x.lo | y.lo} }

// BitXor is `x ^ y`.
func (x I128) BitXor(y I128) I128 { return I128{hi: x.hi ^ y.hi, lo: x.lo ^ y.lo} }

// Not is `!x`.
func (x I128) Not() I128 { return I128{hi: ^x.hi, lo: ^x.lo} }

// Compare is `x.cmp(&y)`: -1, 0 or 1.
func (x I128) Compare(y I128) int {
	if x.IsNegative() != y.IsNegative() {
		if x.IsNegative() {
			return -1
		}
		return 1
	}
	return wideCmp(U128(x), U128(y))
}

// Equal is `x == y`.
func (x I128) Equal(y I128) bool { return x == y }

// Lt is `x < y`.
func (x I128) Lt(y I128) bool { return x.Compare(y) < 0 }

// Le is `x <= y`.
func (x I128) Le(y I128) bool { return x.Compare(y) <= 0 }

// Gt is `x > y`.
func (x I128) Gt(y I128) bool { return x.Compare(y) > 0 }

// Ge is `x >= y`.
func (x I128) Ge(y I128) bool { return x.Compare(y) >= 0 }

// Min is `x.min(y)`.
func (x I128) Min(y I128) I128 {
	if y.Compare(x) < 0 {
		return y
	}
	return x
}

// Max is `x.max(y)`.
func (x I128) Max(y I128) I128 {
	if y.Compare(x) < 0 {
		return x
	}
	return y
}

// Clamp is `x.clamp(min, max)`. It panics when min is greater than max.
func (x I128) Clamp(min, max I128) I128 {
	if min.Compare(max) > 0 {
		PanicAt(1, "min > max. min = "+min.String()+", max = "+max.String())
	}
	if x.Compare(min) < 0 {
		return min
	}
	if x.Compare(max) > 0 {
		return max
	}
	return x
}

// CheckedAdd is `x.checked_add(y)`.
func (x I128) CheckedAdd(y I128) Option[I128] {
	sum, overflow := signedAdd(x, y)
	return optionOf(sum, !overflow)
}

// CheckedSub is `x.checked_sub(y)`.
func (x I128) CheckedSub(y I128) Option[I128] {
	difference, overflow := signedSub(x, y)
	return optionOf(difference, !overflow)
}

// CheckedMul is `x.checked_mul(y)`.
func (x I128) CheckedMul(y I128) Option[I128] {
	product, overflow := signedMul(x, y)
	return optionOf(product, !overflow)
}

// CheckedDiv is `x.checked_div(y)`: None when y is zero, and for MIN / -1.
func (x I128) CheckedDiv(y I128) Option[I128] {
	if y == (I128{}) || x.divOverflows(y) {
		return Option[I128]{}
	}
	quotient, _ := signedDivRem(x, y)
	return Some(quotient)
}

// CheckedRem is `x.checked_rem(y)`: None when y is zero, and for MIN % -1.
func (x I128) CheckedRem(y I128) Option[I128] {
	if y == (I128{}) || x.divOverflows(y) {
		return Option[I128]{}
	}
	_, remainder := signedDivRem(x, y)
	return Some(remainder)
}

// CheckedNeg is `x.checked_neg()`: None for MIN.
func (x I128) CheckedNeg() Option[I128] {
	return optionOf(I128(wideNeg(U128(x))), x != I128Min)
}

// CheckedAbs is `x.checked_abs()`: None for MIN.
func (x I128) CheckedAbs() Option[I128] {
	if x.IsNegative() {
		return x.CheckedNeg()
	}
	return Some(x)
}

// CheckedPow is `x.checked_pow(exp)`.
func (x I128) CheckedPow(exp uint32) Option[I128] {
	power, ok := wideCheckedPow(x, I128{lo: 1}, exp, signedMul)
	if !ok {
		return Option[I128]{}
	}
	return Some(power)
}

// CheckedShl is `x.checked_shl(n)`: None when n is 128 or more.
func (x I128) CheckedShl(n uint32) Option[I128] {
	if n >= 128 {
		return Option[I128]{}
	}
	return Some(I128(wideShl(U128(x), n)))
}

// CheckedShr is `x.checked_shr(n)`: None when n is 128 or more.
func (x I128) CheckedShr(n uint32) Option[I128] {
	if n >= 128 {
		return Option[I128]{}
	}
	return Some(x.arithmeticShr(n))
}

// SaturatingAdd is `x.saturating_add(y)`.
func (x I128) SaturatingAdd(y I128) I128 {
	sum, overflow := signedAdd(x, y)
	if !overflow {
		return sum
	}
	if y.IsNegative() {
		return I128Min
	}
	return I128Max
}

// SaturatingSub is `x.saturating_sub(y)`.
func (x I128) SaturatingSub(y I128) I128 {
	difference, overflow := signedSub(x, y)
	if !overflow {
		return difference
	}
	if y.IsNegative() {
		return I128Max
	}
	return I128Min
}

// SaturatingMul is `x.saturating_mul(y)`.
func (x I128) SaturatingMul(y I128) I128 {
	product, overflow := signedMul(x, y)
	if !overflow {
		return product
	}
	if x.IsNegative() == y.IsNegative() {
		return I128Max
	}
	return I128Min
}

// SaturatingPow is `x.saturating_pow(exp)`.
func (x I128) SaturatingPow(exp uint32) I128 {
	power, ok := wideCheckedPow(x, I128{lo: 1}, exp, signedMul)
	if ok {
		return power
	}
	if x.IsNegative() && exp%2 == 1 {
		return I128Min
	}
	return I128Max
}

// SaturatingNeg is `x.saturating_neg()`.
func (x I128) SaturatingNeg() I128 {
	if x == I128Min {
		return I128Max
	}
	return I128(wideNeg(U128(x)))
}

// SaturatingAbs is `x.saturating_abs()`.
func (x I128) SaturatingAbs() I128 {
	if x.IsNegative() {
		return x.SaturatingNeg()
	}
	return x
}

// WrappingAdd is `x.wrapping_add(y)`.
func (x I128) WrappingAdd(y I128) I128 {
	sum, _ := wideAdd(U128(x), U128(y))
	return I128(sum)
}

// WrappingSub is `x.wrapping_sub(y)`.
func (x I128) WrappingSub(y I128) I128 {
	difference, _ := wideSub(U128(x), U128(y))
	return I128(difference)
}

// WrappingMul is `x.wrapping_mul(y)`.
func (x I128) WrappingMul(y I128) I128 {
	product, _ := wideMul(U128(x), U128(y))
	return I128(product)
}

// WrappingNeg is `x.wrapping_neg()`.
func (x I128) WrappingNeg() I128 { return I128(wideNeg(U128(x))) }

// WrappingAbs is `x.wrapping_abs()`: the absolute value of MIN is MIN.
func (x I128) WrappingAbs() I128 {
	if x.IsNegative() {
		return x.WrappingNeg()
	}
	return x
}

// WrappingPow is `x.wrapping_pow(exp)`.
func (x I128) WrappingPow(exp uint32) I128 {
	power, _ := widePow(x, I128{lo: 1}, exp, signedMul)
	return power
}

// WrappingShl is `x.wrapping_shl(n)`.
func (x I128) WrappingShl(n uint32) I128 { return I128(wideShl(U128(x), n&127)) }

// WrappingShr is `x.wrapping_shr(n)`.
func (x I128) WrappingShr(n uint32) I128 { return x.arithmeticShr(n & 127) }

// OverflowingAdd is `x.overflowing_add(y)`.
func (x I128) OverflowingAdd(y I128) (I128, bool) { return signedAdd(x, y) }

// OverflowingSub is `x.overflowing_sub(y)`.
func (x I128) OverflowingSub(y I128) (I128, bool) { return signedSub(x, y) }

// OverflowingMul is `x.overflowing_mul(y)`.
func (x I128) OverflowingMul(y I128) (I128, bool) { return signedMul(x, y) }

// OverflowingNeg is `x.overflowing_neg()`.
func (x I128) OverflowingNeg() (I128, bool) {
	return I128(wideNeg(U128(x))), x == I128Min
}

// OverflowingPow is `x.overflowing_pow(exp)`.
func (x I128) OverflowingPow(exp uint32) (I128, bool) {
	return widePow(x, I128{lo: 1}, exp, signedMul)
}

// Pow is `x.pow(exp)`. Overflow panics when DebugAssertions is on and wraps
// otherwise.
func (x I128) Pow(exp uint32) I128 {
	if DebugAssertions {
		power, ok := wideCheckedPow(x, I128{lo: 1}, exp, signedMul)
		if !ok {
			PanicAt(1, panicPowOverflow)
		}
		return power
	}
	power, _ := widePow(x, I128{lo: 1}, exp, signedMul)
	return power
}

// Abs is `x.abs()`. The absolute value of MIN panics when DebugAssertions is
// on and is MIN otherwise.
func (x I128) Abs() I128 {
	if x.IsNegative() {
		if x == I128Min && DebugAssertions {
			PanicAt(1, panicNegOverflow)
		}
		return I128(wideNeg(U128(x)))
	}
	return x
}

// UnsignedAbs is `x.unsigned_abs()`.
func (x I128) UnsignedAbs() U128 {
	if x.IsNegative() {
		return wideNeg(U128(x))
	}
	return U128(x)
}

// AbsDiff is `x.abs_diff(y)`.
func (x I128) AbsDiff(y I128) U128 {
	if x.Compare(y) < 0 {
		difference, _ := wideSub(U128(y), U128(x))
		return difference
	}
	difference, _ := wideSub(U128(x), U128(y))
	return difference
}

// Signum is `x.signum()`: -1, 0 or 1.
func (x I128) Signum() I128 {
	switch {
	case x.IsNegative():
		return I128{hi: math.MaxUint64, lo: math.MaxUint64}
	case x == (I128{}):
		return I128{}
	default:
		return I128{lo: 1}
	}
}

// DivEuclid is `x.div_euclid(y)`. It panics when y is zero and for MIN / -1.
func (x I128) DivEuclid(y I128) I128 {
	if y == (I128{}) {
		PanicAt(1, panicDivByZero)
	}
	if x.divOverflows(y) {
		PanicAt(1, panicDivOverflow)
	}
	quotient, remainder := signedDivRem(x, y)
	if remainder.IsNegative() {
		if y.IsNegative() {
			return quotient.WrappingAdd(I128{lo: 1})
		}
		return quotient.WrappingSub(I128{lo: 1})
	}
	return quotient
}

// RemEuclid is `x.rem_euclid(y)`: the remainder that is never negative. It
// panics when y is zero and for MIN % -1.
func (x I128) RemEuclid(y I128) I128 {
	if y == (I128{}) {
		PanicAt(1, panicRemByZero)
	}
	if x.divOverflows(y) {
		PanicAt(1, panicRemOverflow)
	}
	_, remainder := signedDivRem(x, y)
	if remainder.IsNegative() {
		// r.wrapping_add(rhs.wrapping_abs())
		return remainder.WrappingAdd(y.WrappingAbs())
	}
	return remainder
}

// CountOnes is `x.count_ones()`.
func (x I128) CountOnes() uint32 { return U128(x).CountOnes() }

// CountZeros is `x.count_zeros()`.
func (x I128) CountZeros() uint32 { return U128(x).CountZeros() }

// LeadingZeros is `x.leading_zeros()`.
func (x I128) LeadingZeros() uint32 { return U128(x).LeadingZeros() }

// TrailingZeros is `x.trailing_zeros()`.
func (x I128) TrailingZeros() uint32 { return U128(x).TrailingZeros() }

// ToLeBytes is `x.to_le_bytes()`.
func (x I128) ToLeBytes() [16]byte { return U128(x).ToLeBytes() }

// ToBeBytes is `x.to_be_bytes()`.
func (x I128) ToBeBytes() [16]byte { return U128(x).ToBeBytes() }

// ToNeBytes is `x.to_ne_bytes()`.
func (x I128) ToNeBytes() [16]byte { return U128(x).ToNeBytes() }

// I128FromLeBytes is `i128::from_le_bytes(bytes)`.
func I128FromLeBytes(bytes [16]byte) I128 { return I128(U128FromLeBytes(bytes)) }

// I128FromBeBytes is `i128::from_be_bytes(bytes)`.
func I128FromBeBytes(bytes [16]byte) I128 { return I128(U128FromBeBytes(bytes)) }

// I128FromNeBytes is `i128::from_ne_bytes(bytes)`.
func I128FromNeBytes(bytes [16]byte) I128 { return I128(U128FromNeBytes(bytes)) }

// String is x in decimal, its Display form.
func (x I128) String() string {
	if x.IsNegative() {
		return "-" + x.UnsignedAbs().Text(10)
	}
	return U128(x).Text(10)
}

// FmtDisplay is `impl Display for i128`.
func (x I128) FmtDisplay(f *Formatter) error {
	return f.PadIntegral(!x.IsNegative(), "", x.UnsignedAbs().Text(10))
}

// FmtDebug is `impl Debug for i128`.
func (x I128) FmtDebug(f *Formatter) error {
	return f.PadIntegral(!x.IsNegative(), "", x.UnsignedAbs().Text(10))
}

// FmtLowerHex is `impl LowerHex for i128`: the two's complement bits.
func (x I128) FmtLowerHex(f *Formatter) error { return U128(x).FmtLowerHex(f) }

// FmtUpperHex is `impl UpperHex for i128`.
func (x I128) FmtUpperHex(f *Formatter) error { return U128(x).FmtUpperHex(f) }

// FmtOctal is `impl Octal for i128`.
func (x I128) FmtOctal(f *Formatter) error { return U128(x).FmtOctal(f) }

// FmtBinary is `impl Binary for i128`.
func (x I128) FmtBinary(f *Formatter) error { return U128(x).FmtBinary(f) }

// I128FromStrRadix is `i128::from_str_radix(src, radix)`. It panics when
// radix is not in 2..=36.
func I128FromStrRadix(src string, radix uint32) (I128, error) {
	checkRadix(1, radix)
	return i128FromStrRadix(src, radix)
}

// I128FromStr is `i128::from_str(src)`, which is what
// `src.parse::<i128>()` calls.
func I128FromStr(src string) (I128, error) { return i128FromStrRadix(src, 10) }

func i128FromStrRadix(src string, radix uint32) (I128, error) {
	if len(src) == 0 {
		return I128{}, parseIntError(IntErrorKind_Empty)
	}
	isPositive, digits := true, src
	switch {
	case src == "+" || src == "-":
		return I128{}, parseIntError(IntErrorKind_InvalidDigit)
	case src[0] == '+':
		digits = src[1:]
	case src[0] == '-':
		isPositive, digits = false, src[1:]
	}
	var result I128
	base := I128{lo: uint64(radix)}
	for index := 0; index < len(digits); index++ {
		mul, mulOverflow := signedMul(result, base)
		x, ok := toDigit(digits[index], radix)
		if !ok {
			return I128{}, parseIntError(IntErrorKind_InvalidDigit)
		}
		if isPositive {
			if mulOverflow {
				return I128{}, parseIntError(IntErrorKind_PosOverflow)
			}
			sum, overflow := signedAdd(mul, I128{lo: uint64(x)})
			if overflow {
				return I128{}, parseIntError(IntErrorKind_PosOverflow)
			}
			result = sum
		} else {
			if mulOverflow {
				return I128{}, parseIntError(IntErrorKind_NegOverflow)
			}
			difference, overflow := signedSub(mul, I128{lo: uint64(x)})
			if overflow {
				return I128{}, parseIntError(IntErrorKind_NegOverflow)
			}
			result = difference
		}
	}
	return result, nil
}
