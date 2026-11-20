// Port of core 1.99.1 src/num/int_macros.rs and src/num/uint_macros.rs

package rs

import (
	"math/bits"
	"strconv"
	"unsafe"
)

// Integer methods.
//
// A method on a fixed-width Rust integer (u8..u64, i8..i64) is the generic
// function of the same name: `a.checked_add(b)` is rs.CheckedAdd(a, b).
//
// Go's int stands for both usize and isize, and the two disagree on what
// happens below zero, so int is deliberately not accepted by the generic
// functions. A method on a usize is rs.Usize<Method> and a method on an isize
// is rs.Isize<Method> (num_usize.go): `len.saturating_sub(1)` is
// rs.UsizeSaturatingSub(len, 1). Passing an int to rs.SaturatingSub does not
// compile, which is the point.
//
// What needs no helper:
//
//   - `as` between integer types is Go's conversion: both truncate and
//     sign-extend in two's complement. (A u64 of 2^63 or more cast to usize is
//     a negative int; see num_usize.go.)
//   - `T::from(x)` between integer types is Go's conversion. From a bool it is
//     rs.FromBool, which is `flag as T` too.
//   - `+`, `-`, `*` wrap in Go. Rust panics on overflow in a build with debug
//     assertions and wraps in a release build, so code that is correct in Rust
//     never observes the difference.
//   - `/` and `%` truncate toward zero in both languages. Dividing by zero
//     panics in both, with different text: Go's is "runtime error: integer
//     divide by zero", Rust's "attempt to divide by zero". Rust also panics on
//     MIN / -1 ("attempt to divide with overflow"), where Go yields MIN. The
//     helpers in this file that divide (DivCeil, DivEuclid, RemEuclid,
//     NextMultipleOf, ...) panic with Rust's text.
//   - `<<` and `>>`: Go yields zero (or the sign) for a count of the type's
//     width or more, where Rust panics in a debug build and masks the count in
//     a release build. Use CheckedShl, WrappingShl or OverflowingShl where the
//     Rust code does.
//   - `min`, `max`: rs.Min and rs.Max, or Go's built-ins.
//
// The helpers that Rust defines through plain arithmetic (Pow, Abs,
// NextPowerOfTwo, NextMultipleOf, DivCeil on signed types) panic on overflow
// with Rust's message when DebugAssertions is on and wrap when it is off, as
// Rust's do.

// SignedInt is the Go types that stand for Rust's i8, i16, i32 and i64.
type SignedInt interface {
	~int8 | ~int16 | ~int32 | ~int64
}

// UnsignedInt is the Go types that stand for Rust's u8, u16, u32 and u64.
// Go's uint and uintptr are accepted as 64-bit (pointer-width) unsigned
// integers; the port itself does not use them.
type UnsignedInt interface {
	~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uint | ~uintptr
}

// FixedInt is every integer type whose Rust type follows from its Go type.
// It leaves out int, which is usize in some places and isize in others: use
// the Usize and Isize functions for it.
type FixedInt interface {
	SignedInt | UnsignedInt
}

// anyInt is FixedInt and int. The unexported helpers below treat an int as
// the signed integer Go says it is, which is isize; usize has its own
// arithmetic in num_usize.go.
type anyInt interface {
	FixedInt | ~int
}

// intBits is T::BITS.
func intBits[T anyInt]() uint32 {
	var zero T
	return uint32(unsafe.Sizeof(zero)) * 8
}

// intIsSigned reports whether T is a signed type.
func intIsSigned[T anyInt]() bool {
	var zero T
	return zero-1 < 0
}

// intMin is T::MIN.
func intMin[T anyInt]() T {
	if intIsSigned[T]() {
		return T(1) << (intBits[T]() - 1)
	}
	return 0
}

// intMax is T::MAX.
func intMax[T anyInt]() T {
	if intIsSigned[T]() {
		return ^(T(1) << (intBits[T]() - 1))
	}
	return ^T(0)
}

// The messages of Rust's arithmetic panics (core/src/panicking.rs and
// core/src/num/imp/overflow_panic.rs).
const (
	panicAddOverflow = "attempt to add with overflow"
	panicSubOverflow = "attempt to subtract with overflow"
	panicMulOverflow = "attempt to multiply with overflow"
	panicDivOverflow = "attempt to divide with overflow"
	panicRemOverflow = "attempt to calculate the remainder with overflow"
	panicNegOverflow = "attempt to negate with overflow"
	panicShlOverflow = "attempt to shift left with overflow"
	panicShrOverflow = "attempt to shift right with overflow"
	panicPowOverflow = "attempt to exponentiate with overflow"
	panicDivByZero   = "attempt to divide by zero"
	panicRemByZero   = "attempt to calculate the remainder with a divisor of zero"
)

func optionOf[T any](value T, ok bool) Option[T] {
	if ok {
		return Some(value)
	}
	return Option[T]{}
}

// ---------------------------------------------------------------------------
// The arithmetic itself, for any integer type. An int is signed here.

func overflowingAdd[T anyInt](a, b T) (T, bool) {
	sum := a + b
	if intIsSigned[T]() {
		return sum, (a < 0) == (b < 0) && (sum < 0) != (a < 0)
	}
	return sum, sum < a
}

func overflowingSub[T anyInt](a, b T) (T, bool) {
	difference := a - b
	if intIsSigned[T]() {
		return difference, (a < 0) != (b < 0) && (difference < 0) != (a < 0)
	}
	return difference, a < b
}

func overflowingMul[T anyInt](a, b T) (T, bool) {
	product := a * b
	if a == 0 || b == 0 {
		return product, false
	}
	if intIsSigned[T]() {
		// MIN * -1 is the one product whose quotient check below would
		// itself overflow.
		minusOne, min := ^T(0), intMin[T]()
		if (a == minusOne && b == min) || (b == minusOne && a == min) {
			return product, true
		}
	}
	return product, product/a != b
}

func overflowingNeg[T anyInt](a T) (T, bool) {
	if intIsSigned[T]() {
		return -a, a == intMin[T]()
	}
	return -a, a != 0
}

// overflowingPow follows the run-time loop of `overflowing_pow`. The
// branches Rust takes for a base or exponent known at compile time multiply
// the same values in the same order.
func overflowingPow[T anyInt](base T, exp uint32) (T, bool) {
	if exp == 0 {
		return 1, false
	}
	acc := T(1)
	overflow := false
	for {
		if exp&1 == 1 {
			var step bool
			acc, step = overflowingMul(acc, base)
			overflow = overflow || step
			// since exp!=0, finally the exp must be 1.
			if exp == 1 {
				return acc, overflow
			}
		}
		exp /= 2
		var step bool
		base, step = overflowingMul(base, base)
		overflow = overflow || step
	}
}

// checkedPow follows `checked_pow`: it stops at the first multiplication
// that overflows, which is not always where overflowingPow first sees one
// (the last squaring is skipped here), though the two agree on whether the
// power fits.
func checkedPow[T anyInt](base T, exp uint32) (T, bool) {
	if exp == 0 {
		return 1, true
	}
	acc := T(1)
	for {
		if exp&1 == 1 {
			var overflow bool
			acc, overflow = overflowingMul(acc, base)
			if overflow {
				return 0, false
			}
			if exp == 1 {
				return acc, true
			}
		}
		exp /= 2
		var overflow bool
		base, overflow = overflowingMul(base, base)
		if overflow {
			return 0, false
		}
	}
}

// divOverflows reports the one signed division that does not fit: MIN / -1.
func divOverflows[T anyInt](a, b T) bool {
	return intIsSigned[T]() && b == ^T(0) && a == intMin[T]()
}

// divEuclid is `div_euclid` without its panics.
func divEuclid[T anyInt](a, b T) T {
	q := a / b
	if a%b < 0 {
		if b > 0 {
			return q - 1
		}
		return q + 1
	}
	return q
}

// remEuclid is `rem_euclid` without its panics.
func remEuclid[T anyInt](a, b T) T {
	r := a % b
	if r < 0 {
		// r.wrapping_add(rhs.wrapping_abs())
		if b < 0 {
			return r - b
		}
		return r + b
	}
	return r
}

// checkDivisor panics as Rust's `/` does. skip counts the frames between the
// caller and the code that wrote the division.
func checkDivisor[T anyInt](skip int, a, b T) {
	if b == 0 {
		PanicAt(skip+1, panicDivByZero)
	}
	if divOverflows(a, b) {
		PanicAt(skip+1, panicDivOverflow)
	}
}

// checkRemDivisor panics as Rust's `%` does.
func checkRemDivisor[T anyInt](skip int, a, b T) {
	if b == 0 {
		PanicAt(skip+1, panicRemByZero)
	}
	if divOverflows(a, b) {
		PanicAt(skip+1, panicRemOverflow)
	}
}

// addChecked is `a + b` as Rust compiles it: a panic on overflow when debug
// assertions are on, wrapping otherwise.
func addChecked[T anyInt](skip int, a, b T) T {
	sum, overflow := overflowingAdd(a, b)
	if overflow && DebugAssertions {
		PanicAt(skip+1, panicAddOverflow)
	}
	return sum
}

func divCeil[T anyInt](skip int, a, b T) T {
	checkDivisor(skip+1, a, b)
	d := a / b
	r := a % b
	if r == 0 {
		return d
	}
	if !intIsSigned[T]() {
		return d + 1
	}
	// When remainder is non-zero we have a.div_ceil(b) == 1 + a.div_floor(b),
	// so we can re-use the algorithm from div_floor, just adding 1.
	correction := 1 + ((a ^ b) >> (intBits[T]() - 1))
	return d + correction
}

func divFloor[T anyInt](skip int, a, b T) T {
	checkDivisor(skip+1, a, b)
	d := a / b
	if !intIsSigned[T]() {
		return d
	}
	r := a % b
	// If the remainder is non-zero, we need to subtract one if the signs of
	// self and rhs differ, as this means we rounded upwards instead of
	// downwards.
	correction := (a ^ b) >> (intBits[T]() - 1)
	if r != 0 {
		return d + correction
	}
	return d
}

func nextMultipleOf[T anyInt](skip int, a, b T) T {
	if intIsSigned[T]() {
		// This would otherwise fail when calculating `r` when self == T::MIN.
		if b == ^T(0) {
			return a
		}
		checkRemDivisor(skip+1, a, b)
		r := a % b
		m := r
		if (r > 0 && b < 0) || (r < 0 && b > 0) {
			m = r + b
		}
		if m == 0 {
			return a
		}
		return addChecked(skip+1, a, b-m)
	}
	checkRemDivisor(skip+1, a, b)
	r := a % b
	if r == 0 {
		return a
	}
	return addChecked(skip+1, a, b-r)
}

func checkedNextMultipleOf[T anyInt](a, b T) (T, bool) {
	if intIsSigned[T]() {
		if b == ^T(0) {
			return a, true
		}
		if b == 0 {
			return 0, false
		}
		r := a % b
		m := r
		if (r > 0 && b < 0) || (r < 0 && b > 0) {
			m = r + b
		}
		if m == 0 {
			return a, true
		}
		sum, overflow := overflowingAdd(a, b-m)
		return sum, !overflow
	}
	if b == 0 {
		return 0, false
	}
	r := a % b
	if r == 0 {
		return a, true
	}
	sum, overflow := overflowingAdd(a, b-r)
	return sum, !overflow
}

// toU64 widens an unsigned value, or the bits of a signed one.
func toU64[T anyInt](v T) uint64 {
	if intIsSigned[T]() {
		return uint64(int64(v))
	}
	return uint64(v)
}

// lowBits is v's bits within T's width, zero-extended.
func lowBits[T anyInt](v T) uint64 {
	width := intBits[T]()
	if width == 64 {
		return toU64(v)
	}
	return toU64(v) & (1<<width - 1)
}

func ilog2[T anyInt](v T) uint32 {
	return uint32(bits.Len64(lowBits(v))) - 1
}

func ilog10[T anyInt](v T) uint32 {
	n := lowBits(v)
	var log uint32
	for n >= 10 {
		n /= 10
		log++
	}
	return log
}

// ilog is `checked_ilog` for a value and base already known to be in range
// (v > 0, base >= 2).
func ilog[T anyInt](v, base T) uint32 {
	n, b := lowBits(v), lowBits(base)
	var log uint32
	for n >= b {
		n /= b
		log++
	}
	return log
}

// isqrt is the integer square root of a non-negative value.
func isqrt(n uint64) uint64 {
	if n == 0 {
		return 0
	}
	// Newton's iteration from an estimate that is not below the root.
	x := uint64(1) << ((bits.Len64(n) + 1) / 2)
	for {
		y := (x + n/x) / 2
		if y >= x {
			return x
		}
		x = y
	}
}

// ---------------------------------------------------------------------------
// Checked arithmetic: None where the result does not fit.

// CheckedAdd is `a.checked_add(b)`.
func CheckedAdd[T FixedInt](a, b T) Option[T] {
	sum, overflow := overflowingAdd(a, b)
	return optionOf(sum, !overflow)
}

// CheckedSub is `a.checked_sub(b)`.
func CheckedSub[T FixedInt](a, b T) Option[T] {
	difference, overflow := overflowingSub(a, b)
	return optionOf(difference, !overflow)
}

// CheckedMul is `a.checked_mul(b)`.
func CheckedMul[T FixedInt](a, b T) Option[T] {
	product, overflow := overflowingMul(a, b)
	return optionOf(product, !overflow)
}

// CheckedDiv is `a.checked_div(b)`: None when b is zero, and for MIN / -1.
func CheckedDiv[T FixedInt](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(a / b)
}

// CheckedRem is `a.checked_rem(b)`: None when b is zero, and for MIN % -1.
func CheckedRem[T FixedInt](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(a % b)
}

// CheckedDivEuclid is `a.checked_div_euclid(b)`.
func CheckedDivEuclid[T FixedInt](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(divEuclid(a, b))
}

// CheckedRemEuclid is `a.checked_rem_euclid(b)`.
func CheckedRemEuclid[T FixedInt](a, b T) Option[T] {
	if b == 0 || divOverflows(a, b) {
		return Option[T]{}
	}
	return Some(remEuclid(a, b))
}

// CheckedNeg is `a.checked_neg()`. For an unsigned type it is None for
// everything but zero.
func CheckedNeg[T FixedInt](a T) Option[T] {
	negated, overflow := overflowingNeg(a)
	return optionOf(negated, !overflow)
}

// CheckedAbs is `a.checked_abs()`: None for MIN.
func CheckedAbs[T SignedInt](a T) Option[T] {
	if a < 0 {
		return CheckedNeg(a)
	}
	return Some(a)
}

// CheckedPow is `base.checked_pow(exp)`.
func CheckedPow[T FixedInt](base T, exp uint32) Option[T] {
	return optionOf(checkedPow(base, exp))
}

// CheckedShl is `a.checked_shl(rhs)`: None when rhs is the width of the type
// or more. Bits shifted out are lost, as in Rust.
func CheckedShl[T FixedInt](a T, rhs uint32) Option[T] {
	if rhs < intBits[T]() {
		return Some(a << rhs)
	}
	return Option[T]{}
}

// CheckedShr is `a.checked_shr(rhs)`.
func CheckedShr[T FixedInt](a T, rhs uint32) Option[T] {
	if rhs < intBits[T]() {
		return Some(a >> rhs)
	}
	return Option[T]{}
}

// CheckedNextPowerOfTwo is `a.checked_next_power_of_two()`.
func CheckedNextPowerOfTwo[T UnsignedInt](a T) Option[T] {
	sum, overflow := overflowingAdd(oneLessThanNextPowerOfTwo(a), 1)
	return optionOf(sum, !overflow)
}

// CheckedNextMultipleOf is `a.checked_next_multiple_of(b)`.
func CheckedNextMultipleOf[T FixedInt](a, b T) Option[T] {
	return optionOf(checkedNextMultipleOf(a, b))
}

// CheckedAddSigned is `a.checked_add_signed(rhs)` on an unsigned integer.
// rhs is the signed integer of a's width, widened.
func CheckedAddSigned[T UnsignedInt](a T, rhs int64) Option[T] {
	sum, overflow := overflowingAddSigned(a, rhs)
	return optionOf(sum, !overflow)
}

// CheckedSubSigned is `a.checked_sub_signed(rhs)` on an unsigned integer.
func CheckedSubSigned[T UnsignedInt](a T, rhs int64) Option[T] {
	difference, overflow := overflowingSubSigned(a, rhs)
	return optionOf(difference, !overflow)
}

// CheckedAddUnsigned is `a.checked_add_unsigned(rhs)` on a signed integer.
// rhs is the unsigned integer of a's width, widened.
func CheckedAddUnsigned[T SignedInt](a T, rhs uint64) Option[T] {
	sum, overflow := overflowingAddUnsigned(a, rhs)
	return optionOf(sum, !overflow)
}

// CheckedSubUnsigned is `a.checked_sub_unsigned(rhs)` on a signed integer.
func CheckedSubUnsigned[T SignedInt](a T, rhs uint64) Option[T] {
	difference, overflow := overflowingSubUnsigned(a, rhs)
	return optionOf(difference, !overflow)
}

// CheckedIlog2 is `a.checked_ilog2()`: None for zero and negative values.
func CheckedIlog2[T FixedInt](a T) Option[uint32] {
	if a <= 0 {
		return Option[uint32]{}
	}
	return Some(ilog2(a))
}

// CheckedIlog10 is `a.checked_ilog10()`.
func CheckedIlog10[T FixedInt](a T) Option[uint32] {
	if a <= 0 {
		return Option[uint32]{}
	}
	return Some(ilog10(a))
}

// CheckedIlog is `a.checked_ilog(base)`: None when a is not positive or base
// is less than 2.
func CheckedIlog[T FixedInt](a, base T) Option[uint32] {
	if a <= 0 || base <= 1 {
		return Option[uint32]{}
	}
	return Some(ilog(a, base))
}

// CheckedIsqrt is `a.checked_isqrt()` on a signed integer: None for a
// negative value.
func CheckedIsqrt[T SignedInt](a T) Option[T] {
	if a < 0 {
		return Option[T]{}
	}
	return Some(T(isqrt(uint64(a))))
}

// ---------------------------------------------------------------------------
// Saturating arithmetic: the nearest bound where the result does not fit.

// SaturatingAdd is `a.saturating_add(b)`.
func SaturatingAdd[T FixedInt](a, b T) T {
	return saturatingAdd(a, b)
}

func saturatingAdd[T anyInt](a, b T) T {
	sum, overflow := overflowingAdd(a, b)
	if !overflow {
		return sum
	}
	if b < 0 {
		return intMin[T]()
	}
	return intMax[T]()
}

// SaturatingSub is `a.saturating_sub(b)`.
func SaturatingSub[T FixedInt](a, b T) T {
	return saturatingSub(a, b)
}

func saturatingSub[T anyInt](a, b T) T {
	difference, overflow := overflowingSub(a, b)
	if !overflow {
		return difference
	}
	if !intIsSigned[T]() || b > 0 {
		return intMin[T]()
	}
	return intMax[T]()
}

// SaturatingMul is `a.saturating_mul(b)`.
func SaturatingMul[T FixedInt](a, b T) T {
	return saturatingMul(a, b)
}

func saturatingMul[T anyInt](a, b T) T {
	product, overflow := overflowingMul(a, b)
	if !overflow {
		return product
	}
	if (a < 0) == (b < 0) {
		return intMax[T]()
	}
	return intMin[T]()
}

// SaturatingPow is `base.saturating_pow(exp)`.
func SaturatingPow[T FixedInt](base T, exp uint32) T {
	return saturatingPow(base, exp)
}

func saturatingPow[T anyInt](base T, exp uint32) T {
	power, ok := checkedPow(base, exp)
	if ok {
		return power
	}
	if base < 0 && exp%2 == 1 {
		return intMin[T]()
	}
	return intMax[T]()
}

// SaturatingDiv is `a.saturating_div(b)`: MIN / -1 is MAX. It panics when b
// is zero.
func SaturatingDiv[T FixedInt](a, b T) T {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	if divOverflows(a, b) {
		return intMax[T]()
	}
	return a / b
}

// SaturatingNeg is `a.saturating_neg()`: the negation of MIN is MAX.
func SaturatingNeg[T SignedInt](a T) T {
	return saturatingSub(0, a)
}

// SaturatingAbs is `a.saturating_abs()`: the absolute value of MIN is MAX.
func SaturatingAbs[T SignedInt](a T) T {
	if a < 0 {
		return saturatingSub(0, a)
	}
	return a
}

// SaturatingAddSigned is `a.saturating_add_signed(rhs)` on an unsigned
// integer. rhs is the signed integer of a's width, widened.
func SaturatingAddSigned[T UnsignedInt](a T, rhs int64) T {
	sum, overflow := overflowingAddSigned(a, rhs)
	if !overflow {
		return sum
	}
	if rhs < 0 {
		return 0
	}
	return intMax[T]()
}

// SaturatingSubSigned is `a.saturating_sub_signed(rhs)` on an unsigned
// integer.
func SaturatingSubSigned[T UnsignedInt](a T, rhs int64) T {
	difference, overflow := overflowingSubSigned(a, rhs)
	if !overflow {
		return difference
	}
	if rhs < 0 {
		return intMax[T]()
	}
	return 0
}

// SaturatingAddUnsigned is `a.saturating_add_unsigned(rhs)` on a signed
// integer.
func SaturatingAddUnsigned[T SignedInt](a T, rhs uint64) T {
	sum, overflow := overflowingAddUnsigned(a, rhs)
	if overflow {
		return intMax[T]()
	}
	return sum
}

// SaturatingSubUnsigned is `a.saturating_sub_unsigned(rhs)` on a signed
// integer.
func SaturatingSubUnsigned[T SignedInt](a T, rhs uint64) T {
	difference, overflow := overflowingSubUnsigned(a, rhs)
	if overflow {
		return intMin[T]()
	}
	return difference
}

// ---------------------------------------------------------------------------
// Wrapping arithmetic: the result modulo 2^BITS. Go's own operators wrap, so
// most of these are the operator; they exist so that a `wrapping_` call in
// the Rust source stays one in the port.

// WrappingAdd is `a.wrapping_add(b)`.
func WrappingAdd[T FixedInt](a, b T) T { return a + b }

// WrappingSub is `a.wrapping_sub(b)`.
func WrappingSub[T FixedInt](a, b T) T { return a - b }

// WrappingMul is `a.wrapping_mul(b)`.
func WrappingMul[T FixedInt](a, b T) T { return a * b }

// WrappingNeg is `a.wrapping_neg()`.
func WrappingNeg[T FixedInt](a T) T { return -a }

// WrappingAbs is `a.wrapping_abs()`: the absolute value of MIN is MIN.
func WrappingAbs[T SignedInt](a T) T {
	if a < 0 {
		return -a
	}
	return a
}

// WrappingPow is `base.wrapping_pow(exp)`.
func WrappingPow[T FixedInt](base T, exp uint32) T {
	power, _ := overflowingPow(base, exp)
	return power
}

// WrappingShl is `a.wrapping_shl(rhs)`: the count is taken modulo the width
// of the type.
func WrappingShl[T FixedInt](a T, rhs uint32) T {
	return a << (rhs & (intBits[T]() - 1))
}

// WrappingShr is `a.wrapping_shr(rhs)`.
func WrappingShr[T FixedInt](a T, rhs uint32) T {
	return a >> (rhs & (intBits[T]() - 1))
}

// WrappingDiv is `a.wrapping_div(b)`: MIN / -1 is MIN. It panics when b is
// zero.
func WrappingDiv[T FixedInt](a, b T) T {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	return a / b
}

// WrappingRem is `a.wrapping_rem(b)`: MIN % -1 is 0. It panics when b is
// zero.
func WrappingRem[T FixedInt](a, b T) T {
	if b == 0 {
		PanicAt(1, panicRemByZero)
	}
	if divOverflows(a, b) {
		return 0
	}
	return a % b
}

// WrappingAddSigned is `a.wrapping_add_signed(rhs)` on an unsigned integer.
func WrappingAddSigned[T UnsignedInt](a T, rhs int64) T {
	return a + T(rhs)
}

// ---------------------------------------------------------------------------
// Overflowing arithmetic: the wrapped result and whether it overflowed.

// OverflowingAdd is `a.overflowing_add(b)`.
func OverflowingAdd[T FixedInt](a, b T) (T, bool) { return overflowingAdd(a, b) }

// OverflowingSub is `a.overflowing_sub(b)`.
func OverflowingSub[T FixedInt](a, b T) (T, bool) { return overflowingSub(a, b) }

// OverflowingMul is `a.overflowing_mul(b)`.
func OverflowingMul[T FixedInt](a, b T) (T, bool) { return overflowingMul(a, b) }

// OverflowingNeg is `a.overflowing_neg()`.
func OverflowingNeg[T FixedInt](a T) (T, bool) { return overflowingNeg(a) }

// OverflowingAbs is `a.overflowing_abs()`.
func OverflowingAbs[T SignedInt](a T) (T, bool) {
	if a < 0 {
		return -a, a == intMin[T]()
	}
	return a, false
}

// OverflowingPow is `base.overflowing_pow(exp)`.
func OverflowingPow[T FixedInt](base T, exp uint32) (T, bool) {
	return overflowingPow(base, exp)
}

// OverflowingShl is `a.overflowing_shl(rhs)`.
func OverflowingShl[T FixedInt](a T, rhs uint32) (T, bool) {
	return a << (rhs & (intBits[T]() - 1)), rhs >= intBits[T]()
}

// OverflowingShr is `a.overflowing_shr(rhs)`.
func OverflowingShr[T FixedInt](a T, rhs uint32) (T, bool) {
	return a >> (rhs & (intBits[T]() - 1)), rhs >= intBits[T]()
}

// OverflowingDiv is `a.overflowing_div(b)`. It panics when b is zero.
func OverflowingDiv[T FixedInt](a, b T) (T, bool) {
	if b == 0 {
		PanicAt(1, panicDivByZero)
	}
	if divOverflows(a, b) {
		return a, true
	}
	return a / b, false
}

// OverflowingRem is `a.overflowing_rem(b)`. It panics when b is zero.
func OverflowingRem[T FixedInt](a, b T) (T, bool) {
	if b == 0 {
		PanicAt(1, panicRemByZero)
	}
	if intIsSigned[T]() && b == ^T(0) {
		return 0, a == intMin[T]()
	}
	return a % b, false
}

func overflowingAddSigned[T UnsignedInt](a T, rhs int64) (T, bool) {
	// let (res, overflowed) = self.overflowing_add(rhs as Self);
	// (res, overflowed ^ (rhs < 0))
	sum := a + T(rhs)
	overflowed := sum < a
	return sum, overflowed != (rhs < 0)
}

func overflowingSubSigned[T UnsignedInt](a T, rhs int64) (T, bool) {
	// let (res, overflow) = self.overflowing_sub(rhs as Self);
	// (res, overflow ^ (rhs < 0))
	difference := a - T(rhs)
	overflowed := a < T(rhs)
	return difference, overflowed != (rhs < 0)
}

func overflowingAddUnsigned[T SignedInt | ~int](a T, rhs uint64) (T, bool) {
	// let rhs = rhs as Self;
	// let (res, overflowed) = self.overflowing_add(rhs);
	// (res, overflowed ^ (rhs < 0))
	signed := T(rhs)
	sum, overflowed := overflowingAdd(a, signed)
	return sum, overflowed != (signed < 0)
}

func overflowingSubUnsigned[T SignedInt | ~int](a T, rhs uint64) (T, bool) {
	// let rhs = rhs as Self;
	// let (res, overflowed) = self.overflowing_sub(rhs);
	// (res, overflowed ^ (rhs < 0))
	signed := T(rhs)
	difference, overflowed := overflowingSub(a, signed)
	return difference, overflowed != (signed < 0)
}

// ---------------------------------------------------------------------------
// The rest of the integer methods.

// AbsDiff is `a.abs_diff(b)` on an unsigned integer.
func AbsDiff[T UnsignedInt](a, b T) T {
	if a < b {
		return b - a
	}
	return a - b
}

// AbsDiffSigned is `a.abs_diff(b)` on a signed integer. Rust returns the
// unsigned integer of the same width; here it is widened to uint64, and
// converting it to that width loses nothing.
func AbsDiffSigned[T SignedInt](a, b T) uint64 {
	if a < b {
		return uint64(int64(b)) - uint64(int64(a))
	}
	return uint64(int64(a)) - uint64(int64(b))
}

// Abs is `a.abs()`. The absolute value of MIN does not fit: that panics when
// DebugAssertions is on and is MIN otherwise.
func Abs[T SignedInt](a T) T {
	if a < 0 {
		if DebugAssertions && a == intMin[T]() {
			PanicAt(1, panicNegOverflow)
		}
		return -a
	}
	return a
}

// UnsignedAbs is `a.unsigned_abs()`, widened to uint64. Converting it to the
// unsigned integer of a's width loses nothing.
func UnsignedAbs[T SignedInt](a T) uint64 {
	if a < 0 {
		return -uint64(int64(a))
	}
	return uint64(a)
}

// Signum is `a.signum()`: -1, 0 or 1.
func Signum[T SignedInt](a T) T {
	switch {
	case a < 0:
		return ^T(0)
	case a > 0:
		return 1
	default:
		return 0
	}
}

// Pow is `base.pow(exp)`. Overflow panics when DebugAssertions is on and
// wraps otherwise.
func Pow[T FixedInt](base T, exp uint32) T {
	return pow(1, base, exp)
}

func pow[T anyInt](skip int, base T, exp uint32) T {
	if DebugAssertions {
		power, ok := checkedPow(base, exp)
		if !ok {
			PanicAt(skip+1, panicPowOverflow)
		}
		return power
	}
	power, _ := overflowingPow(base, exp)
	return power
}

// Isqrt is `a.isqrt()`. It panics for a negative value.
func Isqrt[T FixedInt](a T) T {
	if a < 0 {
		PanicAt(1, "argument of integer square root cannot be negative")
	}
	return T(isqrt(lowBits(a)))
}

// Ilog2 is `a.ilog2()`. It panics when a is not positive.
func Ilog2[T FixedInt](a T) uint32 {
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog2(a)
}

// Ilog10 is `a.ilog10()`. It panics when a is not positive.
func Ilog10[T FixedInt](a T) uint32 {
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog10(a)
}

// Ilog is `a.ilog(base)`. It panics when a is not positive or base is less
// than 2.
func Ilog[T FixedInt](a, base T) uint32 {
	if base < 2 {
		PanicAt(1, panicIlogBase)
	}
	if a <= 0 {
		PanicAt(1, panicIlogArgument)
	}
	return ilog(a, base)
}

const (
	panicIlogArgument = "argument of integer logarithm must be positive"
	panicIlogBase     = "base of integer logarithm must be at least 2"
)

// DivEuclid is `a.div_euclid(b)`. It panics when b is zero and for MIN / -1.
func DivEuclid[T FixedInt](a, b T) T {
	checkDivisor(1, a, b)
	return divEuclid(a, b)
}

// RemEuclid is `a.rem_euclid(b)`: the remainder that is never negative. It
// panics when b is zero and for MIN % -1.
func RemEuclid[T FixedInt](a, b T) T {
	checkRemDivisor(1, a, b)
	return remEuclid(a, b)
}

// DivFloor is `a.div_floor(b)`: the quotient rounded toward negative
// infinity.
func DivFloor[T FixedInt](a, b T) T {
	return divFloor(1, a, b)
}

// DivCeil is `a.div_ceil(b)`: the quotient rounded toward positive infinity.
func DivCeil[T FixedInt](a, b T) T {
	return divCeil(1, a, b)
}

// NextMultipleOf is `a.next_multiple_of(b)`. It panics when b is zero;
// overflow panics when DebugAssertions is on and wraps otherwise.
func NextMultipleOf[T FixedInt](a, b T) T {
	return nextMultipleOf(1, a, b)
}

// IsMultipleOf is `a.is_multiple_of(b)` on an unsigned integer. Zero is a
// multiple of zero and nothing else is.
func IsMultipleOf[T UnsignedInt](a, b T) bool {
	if b == 0 {
		return a == 0
	}
	return a%b == 0
}

// IsPowerOfTwo is `a.is_power_of_two()`.
func IsPowerOfTwo[T UnsignedInt](a T) bool {
	return bits.OnesCount64(uint64(a)) == 1
}

// oneLessThanNextPowerOfTwo follows the helper of the same name: it cannot
// overflow, and is MAX where the next power of two does not fit.
func oneLessThanNextPowerOfTwo[T UnsignedInt](a T) T {
	if a <= 1 {
		return 0
	}
	p := a - 1
	z := uint32(bits.LeadingZeros64(uint64(p))) - (64 - intBits[T]())
	return ^T(0) >> z
}

// NextPowerOfTwo is `a.next_power_of_two()`. Where the next power of two
// does not fit it panics when DebugAssertions is on and is zero otherwise.
func NextPowerOfTwo[T UnsignedInt](a T) T {
	return addChecked(1, oneLessThanNextPowerOfTwo(a), 1)
}

// Clamp is `v.clamp(min, max)` on an integer. It panics when min is greater
// than max. An int is accepted: a usize and an isize order the same way.
// Floats have their own rules (F64Clamp, F32Clamp), and any other ordered
// type goes through ClampBy.
func Clamp[T anyInt](v, min, max T) T {
	if !(min <= max) {
		PanicAt(1, "min > max. min = "+intText(min)+", max = "+intText(max))
	}
	if v < min {
		return min
	}
	if v > max {
		return max
	}
	return v
}

// ClampBy is `v.clamp(min, max)` for a type ordered by compare: the default
// method of Ord, which strings and every type other than the integers and
// floats use. It panics when min is greater than max.
func ClampBy[T any](v, min, max T, compare func(a, b T) int) T {
	if !(compare(min, max) <= 0) {
		PanicAt(1, "assertion failed: min <= max")
	}
	if compare(v, min) < 0 {
		return min
	}
	if compare(v, max) > 0 {
		return max
	}
	return v
}

// intText is an integer in decimal, its Display and Debug form.
func intText[T anyInt](v T) string {
	if intIsSigned[T]() {
		return strconv.FormatInt(int64(v), 10)
	}
	return strconv.FormatUint(uint64(v), 10)
}

// ---------------------------------------------------------------------------
// Bits. These do not depend on whether an int is a usize or an isize, so int
// is accepted.

// CountOnes is `a.count_ones()`.
func CountOnes[T anyInt](a T) uint32 {
	return uint32(bits.OnesCount64(lowBits(a)))
}

// CountZeros is `a.count_zeros()`.
func CountZeros[T anyInt](a T) uint32 {
	return intBits[T]() - CountOnes(a)
}

// LeadingZeros is `a.leading_zeros()`.
func LeadingZeros[T anyInt](a T) uint32 {
	return uint32(bits.LeadingZeros64(lowBits(a))) - (64 - intBits[T]())
}

// TrailingZeros is `a.trailing_zeros()`: the width of the type for zero.
func TrailingZeros[T anyInt](a T) uint32 {
	if a == 0 {
		return intBits[T]()
	}
	return uint32(bits.TrailingZeros64(lowBits(a)))
}

// LeadingOnes is `a.leading_ones()`.
func LeadingOnes[T anyInt](a T) uint32 {
	return LeadingZeros(^a)
}

// TrailingOnes is `a.trailing_ones()`.
func TrailingOnes[T anyInt](a T) uint32 {
	return TrailingZeros(^a)
}

// RotateLeft is `a.rotate_left(n)`.
func RotateLeft[T anyInt](a T, n uint32) T {
	width := intBits[T]()
	n %= width
	if n == 0 {
		return a
	}
	v := lowBits(a)
	return T(v<<n | v>>(width-n))
}

// RotateRight is `a.rotate_right(n)`.
func RotateRight[T anyInt](a T, n uint32) T {
	width := intBits[T]()
	n %= width
	if n == 0 {
		return a
	}
	v := lowBits(a)
	return T(v>>n | v<<(width-n))
}

// SwapBytes is `a.swap_bytes()`.
func SwapBytes[T anyInt](a T) T {
	return T(bits.ReverseBytes64(lowBits(a)) >> (64 - intBits[T]()))
}

// ReverseBits is `a.reverse_bits()`.
func ReverseBits[T anyInt](a T) T {
	return T(bits.Reverse64(lowBits(a)) >> (64 - intBits[T]()))
}
