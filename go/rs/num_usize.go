// Port of core 1.99.1 src/num/uint_macros.rs and src/num/int_macros.rs, for
// usize and isize

package rs

import (
	"math/bits"
	"strconv"
)

// usize and isize.
//
// Both are Go's int in this port, because Go lengths and indexes are ints.
// The integer methods therefore come twice for int: rs.Usize<Method> reads
// its operands as a usize and rs.Isize<Method> as an isize. The generic
// functions of num_int.go do not accept an int at all.
//
// A usize is an int in [0, UsizeMax]. UsizeMax is the largest int, which is
// half of Rust's usize::MAX: no length, index or count reaches the difference.
// Within that range the Usize functions give Rust's results exactly: checked
// functions are None, and saturating functions stop, at zero and at UsizeMax.
//
// A negative int is not a usize. It is what Go computes where Rust's
// subtraction went below zero, which panics in a Rust build with debug
// assertions ("attempt to subtract with overflow"). The checked and
// saturating Usize functions panic on a negative operand when
// DebugAssertions is on, so the tests find the subtraction that should have
// been UsizeSaturatingSub or UsizeCheckedSub; when it is off they read the
// operand as the value above UsizeMax that Rust's release build would hold.
//
// The wrapping functions and the bit functions work on the 64-bit pattern
// and never panic: a result that Rust holds as 2^63 or more is a negative
// int. That includes a u64 of 2^63 or more cast with `as usize`, such as a
// hash; reduce it (`hash % n`) in uint64 before converting.
//
// A usize and an isize order the same way and have the same bits, so the
// functions that only compare or only look at bits take an int as it is and
// have no Usize or Isize form: Clamp, Min, Max, CountOnes, LeadingZeros,
// TrailingZeros, RotateLeft, SwapBytes and the like.

// IsizeMax is `isize::MAX`.
const IsizeMax = int(^uint(0) >> 1)

// IsizeMin is `isize::MIN`.
const IsizeMin = -IsizeMax - 1

// usizeOf reads an int as the usize it stands for. skip counts the frames
// between the code that passed the operand and the caller of usizeOf.
func usizeOf[T ~int](skip int, v T) uint64 {
	if v < 0 && DebugAssertions {
		PanicAt(skip+1, "negative value "+strconv.FormatInt(int64(v), 10)+
			" used as a usize: an earlier subtraction went below zero, where Rust panics with `"+
			panicSubOverflow+"`")
	}
	return uint64(v)
}

// usizeFits reports whether v is a usize of this port.
func usizeFits(v uint64) bool {
	return v <= uint64(UsizeMax)
}

func usizeOption[T ~int](v uint64, ok bool) Option[T] {
	if ok && usizeFits(v) {
		return Some(T(v))
	}
	return Option[T]{}
}

// usizeSaturate is v, or UsizeMax where v does not fit.
func usizeSaturate[T ~int](v uint64, ok bool) T {
	if ok && usizeFits(v) {
		return T(v)
	}
	return T(UsizeMax)
}

func usizeAdd(a, b uint64) (uint64, bool) {
	sum, carry := bits.Add64(a, b, 0)
	return sum, carry == 0
}

func usizeMul(a, b uint64) (uint64, bool) {
	hi, lo := bits.Mul64(a, b)
	return lo, hi == 0
}

func usizePow(base uint64, exp uint32) (uint64, bool) {
	power, ok := checkedPow(base, exp)
	return power, ok
}

// ---------------------------------------------------------------------------
// usize: checked

// UsizeCheckedAdd is `a.checked_add(b)` on usize.
func UsizeCheckedAdd[T ~int](a, b T) Option[T] {
	return usizeOption[T](usizeAdd(usizeOf(1, a), usizeOf(1, b)))
}

// UsizeCheckedSub is `a.checked_sub(b)` on usize: None when b is greater
// than a.
func UsizeCheckedSub[T ~int](a, b T) Option[T] {
	x, y := usizeOf(1, a), usizeOf(1, b)
	return usizeOption[T](x-y, x >= y)
}

// UsizeCheckedMul is `a.checked_mul(b)` on usize.
func UsizeCheckedMul[T ~int](a, b T) Option[T] {
	return usizeOption[T](usizeMul(usizeOf(1, a), usizeOf(1, b)))
}

// UsizeCheckedDiv is `a.checked_div(b)` on usize: None when b is zero.
func UsizeCheckedDiv[T ~int](a, b T) Option[T] {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		return Option[T]{}
	}
	return usizeOption[T](x/y, true)
}

// UsizeCheckedRem is `a.checked_rem(b)` on usize: None when b is zero.
func UsizeCheckedRem[T ~int](a, b T) Option[T] {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		return Option[T]{}
	}
	return usizeOption[T](x%y, true)
}

// UsizeCheckedDivEuclid is `a.checked_div_euclid(b)` on usize.
func UsizeCheckedDivEuclid[T ~int](a, b T) Option[T] {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		return Option[T]{}
	}
	return usizeOption[T](x/y, true)
}

// UsizeCheckedRemEuclid is `a.checked_rem_euclid(b)` on usize.
func UsizeCheckedRemEuclid[T ~int](a, b T) Option[T] {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		return Option[T]{}
	}
	return usizeOption[T](x%y, true)
}

// UsizeCheckedNeg is `a.checked_neg()` on usize: None for everything but
// zero.
func UsizeCheckedNeg[T ~int](a T) Option[T] {
	return usizeOption[T](0, usizeOf(1, a) == 0)
}

// UsizeCheckedPow is `base.checked_pow(exp)` on usize.
func UsizeCheckedPow[T ~int](base T, exp uint32) Option[T] {
	return usizeOption[T](usizePow(usizeOf(1, base), exp))
}

// UsizeCheckedShl is `a.checked_shl(rhs)` on usize: None when rhs is 64 or
// more. It is a bit operation: a result with the top bit set is a negative
// int.
func UsizeCheckedShl[T ~int](a T, rhs uint32) Option[T] {
	if rhs < 64 {
		return Some(T(uint64(a) << rhs))
	}
	return Option[T]{}
}

// UsizeCheckedShr is `a.checked_shr(rhs)` on usize.
func UsizeCheckedShr[T ~int](a T, rhs uint32) Option[T] {
	if rhs < 64 {
		return Some(T(uint64(a) >> rhs))
	}
	return Option[T]{}
}

// UsizeCheckedAddSigned is `a.checked_add_signed(rhs)`: a usize plus an
// isize.
func UsizeCheckedAddSigned[T ~int](a, rhs T) Option[T] {
	x := usizeOf(1, a)
	if rhs < 0 {
		y := -uint64(rhs)
		return usizeOption[T](x-y, x >= y)
	}
	return usizeOption[T](usizeAdd(x, uint64(rhs)))
}

// UsizeCheckedSubSigned is `a.checked_sub_signed(rhs)`: a usize minus an
// isize.
func UsizeCheckedSubSigned[T ~int](a, rhs T) Option[T] {
	x := usizeOf(1, a)
	if rhs < 0 {
		return usizeOption[T](usizeAdd(x, -uint64(rhs)))
	}
	y := uint64(rhs)
	return usizeOption[T](x-y, x >= y)
}

// UsizeCheckedNextPowerOfTwo is `a.checked_next_power_of_two()` on usize.
func UsizeCheckedNextPowerOfTwo[T ~int](a T) Option[T] {
	return usizeOption[T](usizeAdd(oneLessThanNextPowerOfTwo(usizeOf(1, a)), 1))
}

// UsizeCheckedNextMultipleOf is `a.checked_next_multiple_of(b)` on usize.
func UsizeCheckedNextMultipleOf[T ~int](a, b T) Option[T] {
	return usizeOption[T](checkedNextMultipleOf(usizeOf(1, a), usizeOf(1, b)))
}

// UsizeCheckedIlog2 is `a.checked_ilog2()` on usize: None for zero.
func UsizeCheckedIlog2[T ~int](a T) Option[uint32] {
	return CheckedIlog2(usizeOf(1, a))
}

// UsizeCheckedIlog10 is `a.checked_ilog10()` on usize.
func UsizeCheckedIlog10[T ~int](a T) Option[uint32] {
	return CheckedIlog10(usizeOf(1, a))
}

// UsizeCheckedIlog is `a.checked_ilog(base)` on usize.
func UsizeCheckedIlog[T ~int](a, base T) Option[uint32] {
	return CheckedIlog(usizeOf(1, a), usizeOf(1, base))
}

// ---------------------------------------------------------------------------
// usize: saturating

// UsizeSaturatingAdd is `a.saturating_add(b)` on usize.
func UsizeSaturatingAdd[T ~int](a, b T) T {
	return usizeSaturate[T](usizeAdd(usizeOf(1, a), usizeOf(1, b)))
}

// UsizeSaturatingSub is `a.saturating_sub(b)` on usize: zero when b is
// greater than a.
func UsizeSaturatingSub[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if x < y {
		return 0
	}
	return usizeSaturate[T](x-y, true)
}

// UsizeSaturatingMul is `a.saturating_mul(b)` on usize.
func UsizeSaturatingMul[T ~int](a, b T) T {
	return usizeSaturate[T](usizeMul(usizeOf(1, a), usizeOf(1, b)))
}

// UsizeSaturatingPow is `base.saturating_pow(exp)` on usize.
func UsizeSaturatingPow[T ~int](base T, exp uint32) T {
	return usizeSaturate[T](usizePow(usizeOf(1, base), exp))
}

// UsizeSaturatingDiv is `a.saturating_div(b)` on usize. It panics when b is
// zero.
func UsizeSaturatingDiv[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicDivByZero)
	}
	return usizeSaturate[T](x/y, true)
}

// UsizeSaturatingAddSigned is `a.saturating_add_signed(rhs)`: a usize plus
// an isize, stopping at zero and at UsizeMax.
func UsizeSaturatingAddSigned[T ~int](a, rhs T) T {
	x := usizeOf(1, a)
	if rhs < 0 {
		y := -uint64(rhs)
		if x < y {
			return 0
		}
		return usizeSaturate[T](x-y, true)
	}
	return usizeSaturate[T](usizeAdd(x, uint64(rhs)))
}

// UsizeSaturatingSubSigned is `a.saturating_sub_signed(rhs)`: a usize minus
// an isize.
func UsizeSaturatingSubSigned[T ~int](a, rhs T) T {
	x := usizeOf(1, a)
	if rhs < 0 {
		return usizeSaturate[T](usizeAdd(x, -uint64(rhs)))
	}
	y := uint64(rhs)
	if x < y {
		return 0
	}
	return usizeSaturate[T](x-y, true)
}

// ---------------------------------------------------------------------------
// usize: wrapping and overflowing. These keep the 64-bit pattern.

// UsizeWrappingAdd is `a.wrapping_add(b)` on usize.
func UsizeWrappingAdd[T ~int](a, b T) T { return a + b }

// UsizeWrappingSub is `a.wrapping_sub(b)` on usize. Below zero the result is
// a negative int, the pattern of Rust's value near usize::MAX.
func UsizeWrappingSub[T ~int](a, b T) T { return a - b }

// UsizeWrappingMul is `a.wrapping_mul(b)` on usize.
func UsizeWrappingMul[T ~int](a, b T) T { return a * b }

// UsizeWrappingNeg is `a.wrapping_neg()` on usize.
func UsizeWrappingNeg[T ~int](a T) T { return -a }

// UsizeWrappingPow is `base.wrapping_pow(exp)` on usize.
func UsizeWrappingPow[T ~int](base T, exp uint32) T {
	power, _ := overflowingPow(uint64(base), exp)
	return T(power)
}

// UsizeWrappingShl is `a.wrapping_shl(rhs)` on usize.
func UsizeWrappingShl[T ~int](a T, rhs uint32) T {
	return T(uint64(a) << (rhs & 63))
}

// UsizeWrappingShr is `a.wrapping_shr(rhs)` on usize.
func UsizeWrappingShr[T ~int](a T, rhs uint32) T {
	return T(uint64(a) >> (rhs & 63))
}

// UsizeWrappingAddSigned is `a.wrapping_add_signed(rhs)` on usize.
func UsizeWrappingAddSigned[T ~int](a, rhs T) T { return a + rhs }

// UsizeOverflowingAdd is `a.overflowing_add(b)` on usize. It reports
// overflow where the sum is above UsizeMax.
func UsizeOverflowingAdd[T ~int](a, b T) (T, bool) {
	sum, ok := usizeAdd(usizeOf(1, a), usizeOf(1, b))
	return a + b, !ok || !usizeFits(sum)
}

// UsizeOverflowingSub is `a.overflowing_sub(b)` on usize.
func UsizeOverflowingSub[T ~int](a, b T) (T, bool) {
	x, y := usizeOf(1, a), usizeOf(1, b)
	return a - b, x < y || !usizeFits(x-y)
}

// UsizeOverflowingMul is `a.overflowing_mul(b)` on usize.
func UsizeOverflowingMul[T ~int](a, b T) (T, bool) {
	product, ok := usizeMul(usizeOf(1, a), usizeOf(1, b))
	return a * b, !ok || !usizeFits(product)
}

// ---------------------------------------------------------------------------
// usize: the rest

// usizeChecked is a usize result that Rust computes with plain arithmetic:
// where it does not fit, a panic when debug assertions are on and the
// wrapped pattern otherwise.
func usizeChecked[T ~int](skip int, v uint64, ok bool, message string) T {
	if (!ok || !usizeFits(v)) && DebugAssertions {
		PanicAt(skip+1, message)
	}
	return T(v)
}

// UsizeAbsDiff is `a.abs_diff(b)` on usize.
func UsizeAbsDiff[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if x < y {
		return usizeSaturate[T](y-x, true)
	}
	return usizeSaturate[T](x-y, true)
}

// UsizePow is `base.pow(exp)` on usize. Overflow panics when
// DebugAssertions is on and wraps otherwise.
func UsizePow[T ~int](base T, exp uint32) T {
	x := usizeOf(1, base)
	power, ok := usizePow(x, exp)
	if !ok {
		power, _ = overflowingPow(x, exp)
	}
	return usizeChecked[T](1, power, ok, panicPowOverflow)
}

// UsizeIsqrt is `a.isqrt()` on usize.
func UsizeIsqrt[T ~int](a T) T {
	return T(isqrt(usizeOf(1, a)))
}

// UsizeIlog2 is `a.ilog2()` on usize. It panics for zero.
func UsizeIlog2[T ~int](a T) uint32 {
	x := usizeOf(1, a)
	if x == 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog2(x)
}

// UsizeIlog10 is `a.ilog10()` on usize. It panics for zero.
func UsizeIlog10[T ~int](a T) uint32 {
	x := usizeOf(1, a)
	if x == 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog10(x)
}

// UsizeIlog is `a.ilog(base)` on usize. It panics when a is zero or base is
// less than 2.
func UsizeIlog[T ~int](a, base T) uint32 {
	x, y := usizeOf(1, a), usizeOf(1, base)
	if y < 2 {
		PanicAt(1, panicIlogBase)
	}
	if x == 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog(x, y)
}

// UsizeDivEuclid is `a.div_euclid(b)` on usize, which is `a / b`. It panics
// when b is zero.
func UsizeDivEuclid[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicDivByZero)
	}
	return T(x / y)
}

// UsizeRemEuclid is `a.rem_euclid(b)` on usize, which is `a % b`. It panics
// when b is zero.
func UsizeRemEuclid[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicRemByZero)
	}
	return T(x % y)
}

// UsizeDivFloor is `a.div_floor(b)` on usize, which is `a / b`.
func UsizeDivFloor[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicDivByZero)
	}
	return T(x / y)
}

// UsizeDivCeil is `a.div_ceil(b)` on usize: the quotient rounded up. It
// panics when b is zero.
func UsizeDivCeil[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicDivByZero)
	}
	d := x / y
	if x%y > 0 {
		d++
	}
	return usizeSaturate[T](d, true)
}

// UsizeNextMultipleOf is `a.next_multiple_of(b)` on usize. It panics when b
// is zero; overflow panics when DebugAssertions is on and wraps otherwise.
func UsizeNextMultipleOf[T ~int](a, b T) T {
	x, y := usizeOf(1, a), usizeOf(1, b)
	if y == 0 {
		PanicAt(1, panicRemByZero)
	}
	r := x % y
	if r == 0 {
		return T(x)
	}
	sum, ok := usizeAdd(x, y-r)
	return usizeChecked[T](1, sum, ok, panicAddOverflow)
}

// UsizeIsMultipleOf is `a.is_multiple_of(b)` on usize.
func UsizeIsMultipleOf[T ~int](a, b T) bool {
	return IsMultipleOf(usizeOf(1, a), usizeOf(1, b))
}

// UsizeIsPowerOfTwo is `a.is_power_of_two()` on usize.
func UsizeIsPowerOfTwo[T ~int](a T) bool {
	return bits.OnesCount64(uint64(a)) == 1
}

// UsizeNextPowerOfTwo is `a.next_power_of_two()` on usize. Where the next
// power of two is above UsizeMax it panics when DebugAssertions is on.
func UsizeNextPowerOfTwo[T ~int](a T) T {
	sum, ok := usizeAdd(oneLessThanNextPowerOfTwo(usizeOf(1, a)), 1)
	return usizeChecked[T](1, sum, ok, panicAddOverflow)
}

// ---------------------------------------------------------------------------
// isize: an int as the signed integer Go says it is.

// IsizeCheckedAdd is `a.checked_add(b)` on isize.
func IsizeCheckedAdd[T ~int](a, b T) Option[T] {
	sum, overflow := overflowingAdd(a, b)
	return optionOf(sum, !overflow)
}

// IsizeCheckedSub is `a.checked_sub(b)` on isize.
func IsizeCheckedSub[T ~int](a, b T) Option[T] {
	difference, overflow := overflowingSub(a, b)
	return optionOf(difference, !overflow)
}

// IsizeCheckedMul is `a.checked_mul(b)` on isize.
func IsizeCheckedMul[T ~int](a, b T) Option[T] {
	product, overflow := overflowingMul(a, b)
	return optionOf(product, !overflow)
}

// IsizeCheckedDiv is `a.checked_div(b)` on isize.
func IsizeCheckedDiv[T ~int](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(a / b)
}

// IsizeCheckedRem is `a.checked_rem(b)` on isize.
func IsizeCheckedRem[T ~int](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(a % b)
}

// IsizeCheckedDivEuclid is `a.checked_div_euclid(b)` on isize.
func IsizeCheckedDivEuclid[T ~int](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(divEuclid(a, b))
}

// IsizeCheckedRemEuclid is `a.checked_rem_euclid(b)` on isize.
func IsizeCheckedRemEuclid[T ~int](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(remEuclid(a, b))
}

// IsizeCheckedNeg is `a.checked_neg()` on isize: None for IsizeMin.
func IsizeCheckedNeg[T ~int](a T) Option[T] {
	negated, overflow := overflowingNeg(a)
	return optionOf(negated, !overflow)
}

// IsizeCheckedAbs is `a.checked_abs()` on isize: None for IsizeMin.
func IsizeCheckedAbs[T ~int](a T) Option[T] {
	if a < 0 {
		return IsizeCheckedNeg(a)
	}
	return Some(a)
}

// IsizeCheckedPow is `base.checked_pow(exp)` on isize.
func IsizeCheckedPow[T ~int](base T, exp uint32) Option[T] {
	return optionOf(checkedPow(base, exp))
}

// IsizeCheckedShl is `a.checked_shl(rhs)` on isize.
func IsizeCheckedShl[T ~int](a T, rhs uint32) Option[T] {
	if rhs < 64 {
		return Some(a << rhs)
	}
	return Option[T]{}
}

// IsizeCheckedShr is `a.checked_shr(rhs)` on isize: an arithmetic shift.
func IsizeCheckedShr[T ~int](a T, rhs uint32) Option[T] {
	if rhs < 64 {
		return Some(a >> rhs)
	}
	return Option[T]{}
}

// IsizeCheckedAddUnsigned is `a.checked_add_unsigned(rhs)`: an isize plus a
// usize.
func IsizeCheckedAddUnsigned[T ~int](a, rhs T) Option[T] {
	sum, overflow := overflowingAddUnsigned(a, usizeOf(1, rhs))
	return optionOf(sum, !overflow)
}

// IsizeCheckedSubUnsigned is `a.checked_sub_unsigned(rhs)`: an isize minus
// a usize.
func IsizeCheckedSubUnsigned[T ~int](a, rhs T) Option[T] {
	difference, overflow := overflowingSubUnsigned(a, usizeOf(1, rhs))
	return optionOf(difference, !overflow)
}

// IsizeCheckedNextMultipleOf is `a.checked_next_multiple_of(b)` on isize.
func IsizeCheckedNextMultipleOf[T ~int](a, b T) Option[T] {
	return optionOf(checkedNextMultipleOf(a, b))
}

// IsizeCheckedIlog2 is `a.checked_ilog2()` on isize.
func IsizeCheckedIlog2[T ~int](a T) Option[uint32] {
	if a <= 0 {
		return Option[uint32]{}
	}
	return Some(ilog2(a))
}

// IsizeCheckedIlog10 is `a.checked_ilog10()` on isize.
func IsizeCheckedIlog10[T ~int](a T) Option[uint32] {
	if a <= 0 {
		return Option[uint32]{}
	}
	return Some(ilog10(a))
}

// IsizeCheckedIsqrt is `a.checked_isqrt()` on isize.
func IsizeCheckedIsqrt[T ~int](a T) Option[T] {
	if a < 0 {
		return Option[T]{}
	}
	return Some(T(isqrt(uint64(a))))
}

// IsizeSaturatingAdd is `a.saturating_add(b)` on isize.
func IsizeSaturatingAdd[T ~int](a, b T) T { return saturatingAdd(a, b) }

// IsizeSaturatingSub is `a.saturating_sub(b)` on isize.
func IsizeSaturatingSub[T ~int](a, b T) T { return saturatingSub(a, b) }

// IsizeSaturatingMul is `a.saturating_mul(b)` on isize.
func IsizeSaturatingMul[T ~int](a, b T) T { return saturatingMul(a, b) }

// IsizeSaturatingPow is `base.saturating_pow(exp)` on isize.
func IsizeSaturatingPow[T ~int](base T, exp uint32) T { return saturatingPow(base, exp) }

// IsizeSaturatingDiv is `a.saturating_div(b)` on isize. It panics when b is
// zero.
func IsizeSaturatingDiv[T ~int](a, b T) T {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	if divOverflows(a, b) {
		return T(IsizeMax)
	}
	return a / b
}

// IsizeSaturatingNeg is `a.saturating_neg()` on isize.
func IsizeSaturatingNeg[T ~int](a T) T { return saturatingSub(0, a) }

// IsizeSaturatingAbs is `a.saturating_abs()` on isize.
func IsizeSaturatingAbs[T ~int](a T) T {
	if a < 0 {
		return saturatingSub(0, a)
	}
	return a
}

// IsizeSaturatingAddUnsigned is `a.saturating_add_unsigned(rhs)`: an isize
// plus a usize.
func IsizeSaturatingAddUnsigned[T ~int](a, rhs T) T {
	sum, overflow := overflowingAddUnsigned(a, usizeOf(1, rhs))
	if overflow {
		return T(IsizeMax)
	}
	return sum
}

// IsizeSaturatingSubUnsigned is `a.saturating_sub_unsigned(rhs)`: an isize
// minus a usize.
func IsizeSaturatingSubUnsigned[T ~int](a, rhs T) T {
	difference, overflow := overflowingSubUnsigned(a, usizeOf(1, rhs))
	if overflow {
		return T(IsizeMin)
	}
	return difference
}

// IsizeWrappingAdd is `a.wrapping_add(b)` on isize.
func IsizeWrappingAdd[T ~int](a, b T) T { return a + b }

// IsizeWrappingSub is `a.wrapping_sub(b)` on isize.
func IsizeWrappingSub[T ~int](a, b T) T { return a - b }

// IsizeWrappingMul is `a.wrapping_mul(b)` on isize.
func IsizeWrappingMul[T ~int](a, b T) T { return a * b }

// IsizeWrappingNeg is `a.wrapping_neg()` on isize.
func IsizeWrappingNeg[T ~int](a T) T { return -a }

// IsizeWrappingAbs is `a.wrapping_abs()` on isize.
func IsizeWrappingAbs[T ~int](a T) T {
	if a < 0 {
		return -a
	}
	return a
}

// IsizeWrappingPow is `base.wrapping_pow(exp)` on isize.
func IsizeWrappingPow[T ~int](base T, exp uint32) T {
	power, _ := overflowingPow(base, exp)
	return power
}

// IsizeWrappingShl is `a.wrapping_shl(rhs)` on isize.
func IsizeWrappingShl[T ~int](a T, rhs uint32) T { return a << (rhs & 63) }

// IsizeWrappingShr is `a.wrapping_shr(rhs)` on isize.
func IsizeWrappingShr[T ~int](a T, rhs uint32) T { return a >> (rhs & 63) }

// IsizeOverflowingAdd is `a.overflowing_add(b)` on isize.
func IsizeOverflowingAdd[T ~int](a, b T) (T, bool) { return overflowingAdd(a, b) }

// IsizeOverflowingSub is `a.overflowing_sub(b)` on isize.
func IsizeOverflowingSub[T ~int](a, b T) (T, bool) { return overflowingSub(a, b) }

// IsizeOverflowingMul is `a.overflowing_mul(b)` on isize.
func IsizeOverflowingMul[T ~int](a, b T) (T, bool) { return overflowingMul(a, b) }

// IsizeOverflowingNeg is `a.overflowing_neg()` on isize.
func IsizeOverflowingNeg[T ~int](a T) (T, bool) { return overflowingNeg(a) }

// IsizeOverflowingPow is `base.overflowing_pow(exp)` on isize.
func IsizeOverflowingPow[T ~int](base T, exp uint32) (T, bool) {
	return overflowingPow(base, exp)
}

// IsizeAbs is `a.abs()` on isize. The absolute value of IsizeMin does not
// fit: that panics when DebugAssertions is on and is IsizeMin otherwise.
func IsizeAbs[T ~int](a T) T {
	if a < 0 {
		if DebugAssertions && a == T(IsizeMin) {
			PanicAt(1, panicNegOverflow)
		}
		return -a
	}
	return a
}

// IsizeUnsignedAbs is `a.unsigned_abs()` on isize, as a usize. The absolute
// value of IsizeMin is one more than UsizeMax and is given as UsizeMax.
func IsizeUnsignedAbs[T ~int](a T) T {
	if a < 0 {
		return usizeSaturate[T](-uint64(a), true)
	}
	return a
}

// IsizeAbsDiff is `a.abs_diff(b)` on isize, as a usize. A difference above
// UsizeMax is given as UsizeMax.
func IsizeAbsDiff[T ~int](a, b T) T {
	if a < b {
		return usizeSaturate[T](uint64(b)-uint64(a), true)
	}
	return usizeSaturate[T](uint64(a)-uint64(b), true)
}

// IsizeSignum is `a.signum()` on isize.
func IsizeSignum[T ~int](a T) T {
	switch {
	case a < 0:
		return -1
	case a > 0:
		return 1
	default:
		return 0
	}
}

// IsizePow is `base.pow(exp)` on isize.
func IsizePow[T ~int](base T, exp uint32) T { return pow(1, base, exp) }

// IsizeIsqrt is `a.isqrt()` on isize. It panics for a negative value.
func IsizeIsqrt[T ~int](a T) T {
	if a < 0 {
		PanicAt(1, "argument of integer square root cannot be negative")
	}
	return T(isqrt(uint64(a)))
}

// IsizeIlog2 is `a.ilog2()` on isize. It panics when a is not positive.
func IsizeIlog2[T ~int](a T) uint32 {
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog2(a)
}

// IsizeIlog10 is `a.ilog10()` on isize. It panics when a is not positive.
func IsizeIlog10[T ~int](a T) uint32 {
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog10(a)
}

// IsizeDivEuclid is `a.div_euclid(b)` on isize.
func IsizeDivEuclid[T ~int](a, b T) T {
	checkDivisor(1, a, b)
	return divEuclid(a, b)
}

// IsizeRemEuclid is `a.rem_euclid(b)` on isize: the remainder that is never
// negative.
func IsizeRemEuclid[T ~int](a, b T) T {
	checkRemDivisor(1, a, b)
	return remEuclid(a, b)
}

// IsizeDivFloor is `a.div_floor(b)` on isize.
func IsizeDivFloor[T ~int](a, b T) T { return divFloor(1, a, b) }

// IsizeDivCeil is `a.div_ceil(b)` on isize.
func IsizeDivCeil[T ~int](a, b T) T { return divCeil(1, a, b) }

// IsizeNextMultipleOf is `a.next_multiple_of(b)` on isize.
func IsizeNextMultipleOf[T ~int](a, b T) T { return nextMultipleOf(1, a, b) }

// IsizeCheckedIlog is `a.checked_ilog(base)` on isize.
func IsizeCheckedIlog[T ~int](a, base T) Option[uint32] {
	if a <= 0 || base <= 1 {
		return Option[uint32]{}
	}
	return Some(ilog(a, base))
}

// IsizeIlog is `a.ilog(base)` on isize. It panics when a is not positive or
// base is less than 2.
func IsizeIlog[T ~int](a, base T) uint32 {
	if base < 2 {
		PanicAt(1, panicIlogBase)
	}
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog(a, base)
}

// IsizeWrappingDiv is `a.wrapping_div(b)` on isize: IsizeMin / -1 is
// IsizeMin. It panics when b is zero.
func IsizeWrappingDiv[T ~int](a, b T) T {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	return a / b
}

// IsizeWrappingRem is `a.wrapping_rem(b)` on isize: IsizeMin % -1 is 0. It
// panics when b is zero.
func IsizeWrappingRem[T ~int](a, b T) T {
	if b == 0 {
		PanicAt(1, panicRemByZero)
	}
	if divOverflows(a, b) {
		return 0
	}
	return a % b
}

// IsizeOverflowingDiv is `a.overflowing_div(b)` on isize. It panics when b
// is zero.
func IsizeOverflowingDiv[T ~int](a, b T) (T, bool) {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	if divOverflows(a, b) {
		return a, true
	}
	return a / b, false
}

// IsizeOverflowingRem is `a.overflowing_rem(b)` on isize. It panics when b
// is zero.
func IsizeOverflowingRem[T ~int](a, b T) (T, bool) {
	if b == 0 {
		PanicAt(1, panicRemByZero)
	}
	if b == -1 {
		return 0, a == T(IsizeMin)
	}
	return a % b, false
}

// IsizeOverflowingShl is `a.overflowing_shl(rhs)` on isize.
func IsizeOverflowingShl[T ~int](a T, rhs uint32) (T, bool) {
	return a << (rhs & 63), rhs >= 64
}

// IsizeOverflowingShr is `a.overflowing_shr(rhs)` on isize.
func IsizeOverflowingShr[T ~int](a T, rhs uint32) (T, bool) {
	return a >> (rhs & 63), rhs >= 64
}

// IsizeOverflowingAbs is `a.overflowing_abs()` on isize.
func IsizeOverflowingAbs[T ~int](a T) (T, bool) {
	if a < 0 {
		return -a, a == T(IsizeMin)
	}
	return a, false
}
