// Port of coretests 1.99.1 tests/num/int_macros.rs and
// tests/num/uint_macros.rs (the tests of the methods num_int.go provides),
// run for i8, i16, i32 and i64, and for u8, u16, u32 and u64
//
// u128 and i128 go through rs.U128 and rs.I128, whose methods the oracle
// tests (num_oracle_test.go) cover. Left out: test_from_str and
// test_from_str_radix (parsing is go/rs/strs's, with its own port of these
// tests); test_num, test_bitwise_operators and test_le/test_be, which test
// operators and conversions that are Go's own; the tests of methods the
// port does not have (bit_width, isolate_*_one, highest_one, lowest_one,
// funnel shifts, extract_bits/deposit_bits, carryless_mul, carrying_*,
// borrowing_sub, midpoint, unbounded_shl/shr, div_exact). The pow tables
// below are the assertions of test_pow, one row each.

package rs_test

import (
	"math"
	"reflect"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func numStdEq(t *testing.T, what string, got, want any) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Errorf("%s: got %v, want %v", what, got, want)
	}
}

func numStdBits[T rs.FixedInt]() uint32 { return uint32(reflect.TypeFor[T]().Bits()) }

func numStdSigned[T rs.FixedInt]() bool {
	var zero T
	return zero-1 < 0
}

func numStdMax[T rs.FixedInt]() T {
	if numStdSigned[T]() {
		return ^(T(1) << (numStdBits[T]() - 1))
	}
	return ^T(0)
}

func numStdMin[T rs.FixedInt]() T {
	if numStdSigned[T]() {
		return T(1) << (numStdBits[T]() - 1)
	}
	return 0
}

// numStdValue reads a table entry: an integer, R (the base), minT or maxT.
func numStdValue[T rs.FixedInt](text string, r T) T {
	switch text {
	case "R":
		return r
	case "minT":
		return numStdMin[T]()
	case "maxT":
		return numStdMax[T]()
	}
	n, err := strconv.ParseInt(text, 10, 64)
	if err != nil {
		panic(err)
	}
	return T(n)
}

// numStdExp reads an exponent: an integer, bits (T::BITS) plus or minus
// one, or HALF (u32::MAX / 2 + 1).
func numStdExp[T rs.FixedInt](text string) uint32 {
	switch text {
	case "bits":
		return numStdBits[T]()
	case "bits - 1":
		return numStdBits[T]() - 1
	case "bits + 1":
		return numStdBits[T]() + 1
	case "HALF":
		return math.MaxUint32/2 + 1
	}
	n, err := strconv.ParseUint(text, 10, 32)
	if err != nil {
		panic(err)
	}
	return uint32(n)
}

type numStdPowCase struct{ base, method, exp, want, overflow string }

func numStdRunPow[T rs.FixedInt](t *testing.T, cases []numStdPowCase) {
	for _, c := range cases {
		r := numStdValue[T](c.base, 0)
		exp := numStdExp[T](c.exp)
		what := "(" + c.base + ")." + c.method + "_pow(" + c.exp + ")"
		switch c.method {
		case "wrapping":
			numStdEq(t, what, rs.WrappingPow(r, exp), numStdValue[T](c.want, r))
		case "saturating":
			numStdEq(t, what, rs.SaturatingPow(r, exp), numStdValue[T](c.want, r))
		case "checked":
			want := rs.None[T]()
			if c.want != "none" {
				want = rs.Some(numStdValue[T](c.want, r))
			}
			numStdEq(t, what, rs.CheckedPow(r, exp), want)
		case "overflowing":
			value, overflow := rs.OverflowingPow(r, exp)
			numStdEq(t, what, [2]any{value, overflow}, [2]any{numStdValue[T](c.want, r), c.overflow == "true"})
		default:
			t.Fatalf("unknown pow method %s", c.method)
		}
	}
}

var numStdIntPowCases = []numStdPowCase{
	{"0", "wrapping", "0", "1", ""},
	{"0", "wrapping", "1", "0", ""},
	{"0", "wrapping", "2", "0", ""},
	{"0", "wrapping", "bits - 1", "0", ""},
	{"0", "wrapping", "bits", "0", ""},
	{"0", "wrapping", "bits + 1", "0", ""},
	{"0", "wrapping", "128", "0", ""},
	{"0", "wrapping", "129", "0", ""},
	{"0", "checked", "0", "1", ""},
	{"0", "checked", "1", "0", ""},
	{"0", "checked", "2", "0", ""},
	{"0", "checked", "bits - 1", "0", ""},
	{"0", "checked", "bits", "0", ""},
	{"0", "checked", "bits + 1", "0", ""},
	{"0", "checked", "128", "0", ""},
	{"0", "checked", "129", "0", ""},
	{"0", "overflowing", "0", "1", "false"},
	{"0", "overflowing", "1", "0", "false"},
	{"0", "overflowing", "2", "0", "false"},
	{"0", "overflowing", "bits - 1", "0", "false"},
	{"0", "overflowing", "bits", "0", "false"},
	{"0", "overflowing", "bits + 1", "0", "false"},
	{"0", "overflowing", "128", "0", "false"},
	{"0", "overflowing", "129", "0", "false"},
	{"0", "saturating", "0", "1", ""},
	{"0", "saturating", "1", "0", ""},
	{"0", "saturating", "2", "0", ""},
	{"0", "saturating", "bits - 1", "0", ""},
	{"0", "saturating", "bits", "0", ""},
	{"0", "saturating", "bits + 1", "0", ""},
	{"0", "saturating", "128", "0", ""},
	{"0", "saturating", "129", "0", ""},
	{"1", "wrapping", "0", "1", ""},
	{"1", "wrapping", "1", "1", ""},
	{"1", "wrapping", "2", "1", ""},
	{"1", "wrapping", "bits - 1", "1", ""},
	{"1", "wrapping", "bits", "1", ""},
	{"1", "wrapping", "bits + 1", "1", ""},
	{"1", "wrapping", "128", "1", ""},
	{"1", "wrapping", "129", "1", ""},
	{"1", "checked", "0", "1", ""},
	{"1", "checked", "1", "1", ""},
	{"1", "checked", "2", "1", ""},
	{"1", "checked", "bits - 1", "1", ""},
	{"1", "checked", "bits", "1", ""},
	{"1", "checked", "bits + 1", "1", ""},
	{"1", "checked", "128", "1", ""},
	{"1", "checked", "129", "1", ""},
	{"1", "overflowing", "0", "1", "false"},
	{"1", "overflowing", "1", "1", "false"},
	{"1", "overflowing", "2", "1", "false"},
	{"1", "overflowing", "bits - 1", "1", "false"},
	{"1", "overflowing", "bits", "1", "false"},
	{"1", "overflowing", "bits + 1", "1", "false"},
	{"1", "overflowing", "128", "1", "false"},
	{"1", "overflowing", "129", "1", "false"},
	{"1", "saturating", "0", "1", ""},
	{"1", "saturating", "1", "1", ""},
	{"1", "saturating", "2", "1", ""},
	{"1", "saturating", "bits - 1", "1", ""},
	{"1", "saturating", "bits", "1", ""},
	{"1", "saturating", "bits + 1", "1", ""},
	{"1", "saturating", "128", "1", ""},
	{"1", "saturating", "129", "1", ""},
	{"-1", "wrapping", "0", "1", ""},
	{"-1", "wrapping", "1", "-1", ""},
	{"-1", "wrapping", "2", "1", ""},
	{"-1", "wrapping", "bits - 1", "-1", ""},
	{"-1", "wrapping", "bits", "1", ""},
	{"-1", "wrapping", "bits + 1", "-1", ""},
	{"-1", "wrapping", "128", "1", ""},
	{"-1", "wrapping", "129", "-1", ""},
	{"-1", "checked", "0", "1", ""},
	{"-1", "checked", "1", "-1", ""},
	{"-1", "checked", "2", "1", ""},
	{"-1", "checked", "bits - 1", "-1", ""},
	{"-1", "checked", "bits", "1", ""},
	{"-1", "checked", "bits + 1", "-1", ""},
	{"-1", "checked", "128", "1", ""},
	{"-1", "checked", "129", "-1", ""},
	{"-1", "overflowing", "0", "1", "false"},
	{"-1", "overflowing", "1", "-1", "false"},
	{"-1", "overflowing", "2", "1", "false"},
	{"-1", "overflowing", "bits - 1", "-1", "false"},
	{"-1", "overflowing", "bits", "1", "false"},
	{"-1", "overflowing", "bits + 1", "-1", "false"},
	{"-1", "overflowing", "128", "1", "false"},
	{"-1", "overflowing", "129", "-1", "false"},
	{"-1", "saturating", "0", "1", ""},
	{"-1", "saturating", "1", "-1", ""},
	{"-1", "saturating", "2", "1", ""},
	{"-1", "saturating", "bits - 1", "-1", ""},
	{"-1", "saturating", "bits", "1", ""},
	{"-1", "saturating", "bits + 1", "-1", ""},
	{"-1", "saturating", "128", "1", ""},
	{"-1", "saturating", "129", "-1", ""},
	{"2", "wrapping", "0", "1", ""},
	{"2", "wrapping", "1", "2", ""},
	{"2", "wrapping", "2", "4", ""},
	{"2", "wrapping", "bits - 1", "minT", ""},
	{"2", "wrapping", "bits", "0", ""},
	{"2", "wrapping", "bits + 1", "0", ""},
	{"2", "wrapping", "128", "0", ""},
	{"2", "wrapping", "129", "0", ""},
	{"2", "checked", "0", "1", ""},
	{"2", "checked", "1", "2", ""},
	{"2", "checked", "2", "4", ""},
	{"2", "checked", "bits - 1", "none", ""},
	{"2", "checked", "bits", "none", ""},
	{"2", "checked", "bits + 1", "none", ""},
	{"2", "checked", "128", "none", ""},
	{"2", "checked", "129", "none", ""},
	{"2", "overflowing", "0", "1", "false"},
	{"2", "overflowing", "1", "2", "false"},
	{"2", "overflowing", "2", "4", "false"},
	{"2", "overflowing", "bits - 1", "minT", "true"},
	{"2", "overflowing", "bits", "0", "true"},
	{"2", "overflowing", "bits + 1", "0", "true"},
	{"2", "overflowing", "128", "0", "true"},
	{"2", "overflowing", "129", "0", "true"},
	{"2", "saturating", "0", "1", ""},
	{"2", "saturating", "1", "2", ""},
	{"2", "saturating", "2", "4", ""},
	{"2", "saturating", "bits - 1", "maxT", ""},
	{"2", "saturating", "bits", "maxT", ""},
	{"2", "saturating", "bits + 1", "maxT", ""},
	{"2", "saturating", "128", "maxT", ""},
	{"2", "saturating", "129", "maxT", ""},
	{"-2", "wrapping", "0", "1", ""},
	{"-2", "wrapping", "1", "-2", ""},
	{"-2", "wrapping", "2", "4", ""},
	{"-2", "wrapping", "bits - 1", "minT", ""},
	{"-2", "wrapping", "bits", "0", ""},
	{"-2", "wrapping", "bits + 1", "0", ""},
	{"-2", "wrapping", "128", "0", ""},
	{"-2", "wrapping", "129", "0", ""},
	{"-2", "checked", "0", "1", ""},
	{"-2", "checked", "1", "-2", ""},
	{"-2", "checked", "2", "4", ""},
	{"-2", "checked", "bits - 1", "minT", ""},
	{"-2", "checked", "bits", "none", ""},
	{"-2", "checked", "bits + 1", "none", ""},
	{"-2", "checked", "128", "none", ""},
	{"-2", "checked", "129", "none", ""},
	{"-2", "overflowing", "0", "1", "false"},
	{"-2", "overflowing", "1", "-2", "false"},
	{"-2", "overflowing", "2", "4", "false"},
	{"-2", "overflowing", "bits - 1", "minT", "false"},
	{"-2", "overflowing", "bits", "0", "true"},
	{"-2", "overflowing", "bits + 1", "0", "true"},
	{"-2", "overflowing", "128", "0", "true"},
	{"-2", "overflowing", "129", "0", "true"},
	{"-2", "saturating", "0", "1", ""},
	{"-2", "saturating", "1", "-2", ""},
	{"-2", "saturating", "2", "4", ""},
	{"-2", "saturating", "bits - 1", "minT", ""},
	{"-2", "saturating", "bits", "maxT", ""},
	{"-2", "saturating", "bits + 1", "minT", ""},
	{"-2", "saturating", "128", "maxT", ""},
	{"-2", "saturating", "129", "minT", ""},
	{"4", "wrapping", "HALF", "0", ""},
	{"4", "checked", "HALF", "none", ""},
	{"4", "overflowing", "HALF", "0", "true"},
	{"4", "saturating", "HALF", "maxT", ""},
	{"-4", "wrapping", "HALF", "0", ""},
	{"-4", "checked", "HALF", "none", ""},
	{"-4", "overflowing", "HALF", "0", "true"},
	{"-4", "saturating", "HALF", "maxT", ""},
	{"maxT", "wrapping", "0", "1", ""},
	{"maxT", "wrapping", "1", "R", ""},
	{"maxT", "wrapping", "2", "1", ""},
	{"maxT", "wrapping", "128", "1", ""},
	{"maxT", "wrapping", "129", "R", ""},
	{"maxT", "checked", "0", "1", ""},
	{"maxT", "checked", "1", "R", ""},
	{"maxT", "checked", "2", "none", ""},
	{"maxT", "checked", "128", "none", ""},
	{"maxT", "checked", "129", "none", ""},
	{"maxT", "overflowing", "0", "1", "false"},
	{"maxT", "overflowing", "1", "R", "false"},
	{"maxT", "overflowing", "2", "1", "true"},
	{"maxT", "overflowing", "128", "1", "true"},
	{"maxT", "overflowing", "129", "R", "true"},
	{"maxT", "saturating", "0", "1", ""},
	{"maxT", "saturating", "1", "R", ""},
	{"maxT", "saturating", "2", "maxT", ""},
	{"maxT", "saturating", "bits - 1", "maxT", ""},
	{"maxT", "saturating", "bits", "maxT", ""},
	{"maxT", "saturating", "bits + 1", "maxT", ""},
	{"maxT", "saturating", "128", "maxT", ""},
	{"maxT", "saturating", "129", "maxT", ""},
	{"minT", "wrapping", "0", "1", ""},
	{"minT", "wrapping", "1", "R", ""},
	{"minT", "wrapping", "2", "0", ""},
	{"minT", "wrapping", "bits - 1", "0", ""},
	{"minT", "wrapping", "bits", "0", ""},
	{"minT", "wrapping", "bits + 1", "0", ""},
	{"minT", "wrapping", "128", "0", ""},
	{"minT", "wrapping", "129", "0", ""},
	{"minT", "checked", "0", "1", ""},
	{"minT", "checked", "1", "R", ""},
	{"minT", "checked", "2", "none", ""},
	{"minT", "checked", "bits - 1", "none", ""},
	{"minT", "checked", "bits", "none", ""},
	{"minT", "checked", "bits + 1", "none", ""},
	{"minT", "checked", "128", "none", ""},
	{"minT", "checked", "129", "none", ""},
	{"minT", "overflowing", "0", "1", "false"},
	{"minT", "overflowing", "1", "R", "false"},
	{"minT", "overflowing", "2", "0", "true"},
	{"minT", "overflowing", "bits - 1", "0", "true"},
	{"minT", "overflowing", "bits", "0", "true"},
	{"minT", "overflowing", "bits + 1", "0", "true"},
	{"minT", "overflowing", "128", "0", "true"},
	{"minT", "overflowing", "129", "0", "true"},
	{"minT", "saturating", "0", "1", ""},
	{"minT", "saturating", "1", "R", ""},
	{"minT", "saturating", "2", "maxT", ""},
	{"minT", "saturating", "bits - 1", "minT", ""},
	{"minT", "saturating", "bits", "maxT", ""},
	{"minT", "saturating", "bits + 1", "minT", ""},
	{"minT", "saturating", "128", "maxT", ""},
	{"minT", "saturating", "129", "minT", ""},
}

var numStdUintPowCases = []numStdPowCase{
	{"0", "wrapping", "0", "1", ""},
	{"0", "wrapping", "1", "0", ""},
	{"0", "wrapping", "2", "0", ""},
	{"0", "wrapping", "128", "0", ""},
	{"0", "wrapping", "129", "0", ""},
	{"0", "checked", "0", "1", ""},
	{"0", "checked", "1", "0", ""},
	{"0", "checked", "2", "0", ""},
	{"0", "checked", "128", "0", ""},
	{"0", "checked", "129", "0", ""},
	{"0", "overflowing", "0", "1", "false"},
	{"0", "overflowing", "1", "0", "false"},
	{"0", "overflowing", "2", "0", "false"},
	{"0", "overflowing", "128", "0", "false"},
	{"0", "overflowing", "129", "0", "false"},
	{"0", "saturating", "0", "1", ""},
	{"0", "saturating", "1", "0", ""},
	{"0", "saturating", "2", "0", ""},
	{"0", "saturating", "128", "0", ""},
	{"0", "saturating", "129", "0", ""},
	{"1", "wrapping", "0", "1", ""},
	{"1", "wrapping", "1", "1", ""},
	{"1", "wrapping", "2", "1", ""},
	{"1", "wrapping", "128", "1", ""},
	{"1", "wrapping", "129", "1", ""},
	{"1", "checked", "0", "1", ""},
	{"1", "checked", "1", "1", ""},
	{"1", "checked", "2", "1", ""},
	{"1", "checked", "128", "1", ""},
	{"1", "checked", "129", "1", ""},
	{"1", "overflowing", "0", "1", "false"},
	{"1", "overflowing", "1", "1", "false"},
	{"1", "overflowing", "2", "1", "false"},
	{"1", "overflowing", "128", "1", "false"},
	{"1", "overflowing", "129", "1", "false"},
	{"1", "saturating", "0", "1", ""},
	{"1", "saturating", "1", "1", ""},
	{"1", "saturating", "2", "1", ""},
	{"1", "saturating", "128", "1", ""},
	{"1", "saturating", "129", "1", ""},
	{"2", "wrapping", "0", "1", ""},
	{"2", "wrapping", "1", "2", ""},
	{"2", "wrapping", "2", "4", ""},
	{"2", "wrapping", "128", "0", ""},
	{"2", "wrapping", "129", "0", ""},
	{"2", "checked", "0", "1", ""},
	{"2", "checked", "1", "2", ""},
	{"2", "checked", "2", "4", ""},
	{"2", "checked", "128", "none", ""},
	{"2", "checked", "129", "none", ""},
	{"2", "overflowing", "0", "1", "false"},
	{"2", "overflowing", "1", "2", "false"},
	{"2", "overflowing", "2", "4", "false"},
	{"2", "overflowing", "128", "0", "true"},
	{"2", "overflowing", "129", "0", "true"},
	{"2", "saturating", "0", "1", ""},
	{"2", "saturating", "1", "2", ""},
	{"2", "saturating", "2", "4", ""},
	{"2", "saturating", "128", "maxT", ""},
	{"2", "saturating", "129", "maxT", ""},
	{"4", "wrapping", "HALF", "0", ""},
	{"4", "checked", "HALF", "none", ""},
	{"4", "overflowing", "HALF", "0", "true"},
	{"4", "saturating", "HALF", "maxT", ""},
	{"maxT", "wrapping", "0", "1", ""},
	{"maxT", "wrapping", "1", "R", ""},
	{"maxT", "wrapping", "2", "1", ""},
	{"maxT", "wrapping", "128", "1", ""},
	{"maxT", "wrapping", "129", "R", ""},
	{"maxT", "checked", "0", "1", ""},
	{"maxT", "checked", "1", "R", ""},
	{"maxT", "checked", "2", "none", ""},
	{"maxT", "checked", "128", "none", ""},
	{"maxT", "checked", "129", "none", ""},
	{"maxT", "overflowing", "0", "1", "false"},
	{"maxT", "overflowing", "1", "R", "false"},
	{"maxT", "overflowing", "2", "1", "true"},
	{"maxT", "overflowing", "128", "1", "true"},
	{"maxT", "overflowing", "129", "R", "true"},
	{"maxT", "saturating", "0", "1", ""},
	{"maxT", "saturating", "1", "R", ""},
	{"maxT", "saturating", "2", "maxT", ""},
	{"maxT", "saturating", "128", "maxT", ""},
	{"maxT", "saturating", "129", "maxT", ""},
}

func numStdEachSigned(t *testing.T, test string) {
	t.Run("i8", func(t *testing.T) { numStdIntModule[int8](t, test) })
	t.Run("i16", func(t *testing.T) { numStdIntModule[int16](t, test) })
	t.Run("i32", func(t *testing.T) { numStdIntModule[int32](t, test) })
	t.Run("i64", func(t *testing.T) { numStdIntModule[int64](t, test) })
}

func numStdEachUnsigned(t *testing.T, test string) {
	t.Run("u8", func(t *testing.T) { numStdUintModule[uint8](t, test) })
	t.Run("u16", func(t *testing.T) { numStdUintModule[uint16](t, test) })
	t.Run("u32", func(t *testing.T) { numStdUintModule[uint32](t, test) })
	t.Run("u64", func(t *testing.T) { numStdUintModule[uint64](t, test) })
}

// numStdIntModule holds the bodies of int_module!'s tests for one type.
func numStdIntModule[T rs.SignedInt](t *testing.T, test string) {
	MAX, MIN := numStdMax[T](), numStdMin[T]()
	BITS := numStdBits[T]()
	A, B, C := T(0b0101100), T(0b0100001), T(0b1111001)
	_0, _1 := T(0), ^T(0)
	switch test {
	case "test_overflows":
		numStdEq(t, "MAX > 0", MAX > 0, true)
		numStdEq(t, "MIN <= 0", MIN <= 0, true)
		numStdEq(t, "MIN + MAX + 1", MIN+MAX+1, T(0))
	case "test_rem_euclid":
		numStdEq(t, "(-1).rem_euclid(MIN)", rs.RemEuclid(T(-1), MIN), MAX)
	case "test_abs":
		numStdEq(t, "(1).abs()", rs.Abs(T(1)), T(1))
		numStdEq(t, "(0).abs()", rs.Abs(T(0)), T(0))
		numStdEq(t, "(-1).abs()", rs.Abs(T(-1)), T(1))
	case "test_signum":
		numStdEq(t, "(1).signum()", rs.Signum(T(1)), T(1))
		numStdEq(t, "(0).signum()", rs.Signum(T(0)), T(0))
		numStdEq(t, "(-0).signum()", rs.Signum(T(-0)), T(0))
		numStdEq(t, "(-1).signum()", rs.Signum(T(-1)), T(-1))
	case "test_count_ones":
		numStdEq(t, "A.count_ones()", rs.CountOnes(A), uint32(3))
		numStdEq(t, "B.count_ones()", rs.CountOnes(B), uint32(2))
		numStdEq(t, "C.count_ones()", rs.CountOnes(C), uint32(5))
	case "test_count_zeros":
		numStdEq(t, "A.count_zeros()", rs.CountZeros(A), BITS-3)
		numStdEq(t, "B.count_zeros()", rs.CountZeros(B), BITS-2)
		numStdEq(t, "C.count_zeros()", rs.CountZeros(C), BITS-5)
	case "test_leading_trailing_ones":
		A := T(0b0101_1111)
		numStdEq(t, "A.trailing_ones()", rs.TrailingOnes(A), uint32(5))
		numStdEq(t, "(!A).leading_ones()", rs.LeadingOnes(^A), BITS-7)
		numStdEq(t, "A.reverse_bits().leading_ones()", rs.LeadingOnes(rs.ReverseBits(A)), uint32(5))
		numStdEq(t, "_1.leading_ones()", rs.LeadingOnes(_1), BITS)
		numStdEq(t, "_1.trailing_ones()", rs.TrailingOnes(_1), BITS)
		numStdEq(t, "(_1 << 1).trailing_ones()", rs.TrailingOnes(_1<<1), uint32(0))
		numStdEq(t, "MAX.leading_ones()", rs.LeadingOnes(MAX), uint32(0))
		numStdEq(t, "(_1 << 1).leading_ones()", rs.LeadingOnes(_1<<1), BITS-1)
		numStdEq(t, "MAX.trailing_ones()", rs.TrailingOnes(MAX), BITS-1)
		numStdEq(t, "_0.leading_ones()", rs.LeadingOnes(_0), uint32(0))
		numStdEq(t, "_0.trailing_ones()", rs.TrailingOnes(_0), uint32(0))
		X := T(0b0010_1100)
		numStdEq(t, "X.leading_ones()", rs.LeadingOnes(X), uint32(0))
		numStdEq(t, "X.trailing_ones()", rs.TrailingOnes(X), uint32(0))
	case "test_rotate":
		numStdRotate(t, A, B, C, _0, _1)
	case "test_swap_bytes":
		numStdSwapBytes(t, A, B, C, _0, _1)
	case "test_signed_checked_div":
		numStdEq(t, "(10).checked_div(2)", rs.CheckedDiv(T(10), 2), rs.Some(T(5)))
		numStdEq(t, "(5).checked_div(0)", rs.CheckedDiv(T(5), 0), rs.None[T]())
		numStdEq(t, "MIN.checked_div(-1)", rs.CheckedDiv(MIN, -1), rs.None[T]())
	case "test_saturating_abs":
		for _, c := range [][2]T{{0, 0}, {123, 123}, {-123, 123}, {MAX - 2, MAX - 2}, {MAX - 1, MAX - 1},
			{MAX, MAX}, {MIN + 2, MAX - 1}, {MIN + 1, MAX}, {MIN, MAX}} {
			numStdEq(t, "("+strconv.FormatInt(int64(c[0]), 10)+").saturating_abs()", rs.SaturatingAbs(c[0]), c[1])
		}
	case "test_saturating_neg":
		for _, c := range [][2]T{{0, 0}, {123, -123}, {-123, 123}, {MAX - 2, MIN + 3}, {MAX - 1, MIN + 2},
			{MAX, MIN + 1}, {MIN + 2, MAX - 1}, {MIN + 1, MAX}, {MIN, MAX}} {
			numStdEq(t, "("+strconv.FormatInt(int64(c[0]), 10)+").saturating_neg()", rs.SaturatingNeg(c[0]), c[1])
		}
	case "test_pow":
		numStdRunPow[T](t, numStdIntPowCases)
	case "test_div_floor":
		a, b := T(8), T(3)
		numStdEq(t, "A.div_floor(B)", rs.DivFloor(a, b), T(2))
		numStdEq(t, "A.div_floor(-B)", rs.DivFloor(a, -b), T(-3))
		numStdEq(t, "(-A).div_floor(B)", rs.DivFloor(-a, b), T(-3))
		numStdEq(t, "(-A).div_floor(-B)", rs.DivFloor(-a, -b), T(2))
	case "test_div_ceil":
		a, b := T(8), T(3)
		numStdEq(t, "A.div_ceil(B)", rs.DivCeil(a, b), T(3))
		numStdEq(t, "A.div_ceil(-B)", rs.DivCeil(a, -b), T(-2))
		numStdEq(t, "(-A).div_ceil(B)", rs.DivCeil(-a, b), T(-2))
		numStdEq(t, "(-A).div_ceil(-B)", rs.DivCeil(-a, -b), T(3))
	case "test_next_multiple_of":
		for _, c := range [][3]T{{16, 8, 16}, {23, 8, 24}, {16, -8, 16}, {23, -8, 16}, {-16, 8, -16},
			{-23, 8, -16}, {-16, -8, -16}, {-23, -8, -24}, {MIN, -1, MIN}} {
			numStdEq(t, "next_multiple_of", rs.NextMultipleOf(c[0], c[1]), c[2])
		}
	case "test_checked_next_multiple_of":
		for _, c := range []struct {
			a, b T
			want rs.Option[T]
		}{
			{16, 8, rs.Some(T(16))}, {23, 8, rs.Some(T(24))}, {16, -8, rs.Some(T(16))}, {23, -8, rs.Some(T(16))},
			{-16, 8, rs.Some(T(-16))}, {-23, 8, rs.Some(T(-16))}, {-16, -8, rs.Some(T(-16))},
			{-23, -8, rs.Some(T(-24))}, {1, 0, rs.None[T]()}, {MAX, 2, rs.None[T]()}, {MIN, -3, rs.None[T]()},
			{MIN, -1, rs.Some(MIN)},
		} {
			numStdEq(t, "checked_next_multiple_of", rs.CheckedNextMultipleOf(c.a, c.b), c.want)
		}
	default:
		t.Fatalf("no test %s", test)
	}
}

func numStdRotate[T rs.FixedInt](t *testing.T, A, B, C, _0, _1 T) {
	t.Helper()
	numStdEq(t, "A.rotate_left(6).rotate_right(2).rotate_right(4)", rs.RotateRight(rs.RotateRight(rs.RotateLeft(A, 6), 2), 4), A)
	numStdEq(t, "B.rotate_left(3).rotate_left(2).rotate_right(5)", rs.RotateRight(rs.RotateLeft(rs.RotateLeft(B, 3), 2), 5), B)
	numStdEq(t, "C.rotate_left(6).rotate_right(2).rotate_right(4)", rs.RotateRight(rs.RotateRight(rs.RotateLeft(C, 6), 2), 4), C)

	// Rotating these should make no difference
	//
	// We test using 124 bits because to ensure that overlong bit shifts do
	// not cause undefined behavior. See #10183.
	numStdEq(t, "_0.rotate_left(124)", rs.RotateLeft(_0, 124), _0)
	numStdEq(t, "_1.rotate_left(124)", rs.RotateLeft(_1, 124), _1)
	numStdEq(t, "_0.rotate_right(124)", rs.RotateRight(_0, 124), _0)
	numStdEq(t, "_1.rotate_right(124)", rs.RotateRight(_1, 124), _1)

	// Rotating by 0 should have no effect
	numStdEq(t, "A.rotate_left(0)", rs.RotateLeft(A, 0), A)
	numStdEq(t, "B.rotate_left(0)", rs.RotateLeft(B, 0), B)
	numStdEq(t, "C.rotate_left(0)", rs.RotateLeft(C, 0), C)
	// Rotating by a multiple of word size should also have no effect
	numStdEq(t, "A.rotate_left(128)", rs.RotateLeft(A, 128), A)
	numStdEq(t, "B.rotate_left(128)", rs.RotateLeft(B, 128), B)
	numStdEq(t, "C.rotate_left(128)", rs.RotateLeft(C, 128), C)
}

func numStdSwapBytes[T rs.FixedInt](t *testing.T, A, B, C, _0, _1 T) {
	t.Helper()
	numStdEq(t, "A.swap_bytes().swap_bytes()", rs.SwapBytes(rs.SwapBytes(A)), A)
	numStdEq(t, "B.swap_bytes().swap_bytes()", rs.SwapBytes(rs.SwapBytes(B)), B)
	numStdEq(t, "C.swap_bytes().swap_bytes()", rs.SwapBytes(rs.SwapBytes(C)), C)

	// Swapping these should make no difference
	numStdEq(t, "_0.swap_bytes()", rs.SwapBytes(_0), _0)
	numStdEq(t, "_1.swap_bytes()", rs.SwapBytes(_1), _1)
}

// numStdUintModule holds the bodies of uint_module!'s tests for one type.
func numStdUintModule[T rs.UnsignedInt](t *testing.T, test string) {
	MAX := numStdMax[T]()
	BITS := numStdBits[T]()
	A, B, C := T(0b0101100), T(0b0100001), T(0b1111001)
	_0, _1 := T(0), ^T(0)
	switch test {
	case "test_count_ones":
		numStdEq(t, "A.count_ones()", rs.CountOnes(A), uint32(3))
		numStdEq(t, "B.count_ones()", rs.CountOnes(B), uint32(2))
		numStdEq(t, "C.count_ones()", rs.CountOnes(C), uint32(5))
	case "test_count_zeros":
		numStdEq(t, "A.count_zeros()", rs.CountZeros(A), BITS-3)
		numStdEq(t, "B.count_zeros()", rs.CountZeros(B), BITS-2)
		numStdEq(t, "C.count_zeros()", rs.CountZeros(C), BITS-5)
	case "test_leading_trailing_ones":
		A := T(0b0101_1111)
		numStdEq(t, "A.trailing_ones()", rs.TrailingOnes(A), uint32(5))
		numStdEq(t, "(!A).leading_ones()", rs.LeadingOnes(^A), BITS-7)
		numStdEq(t, "A.reverse_bits().leading_ones()", rs.LeadingOnes(rs.ReverseBits(A)), uint32(5))
		numStdEq(t, "_1.leading_ones()", rs.LeadingOnes(_1), BITS)
		numStdEq(t, "_1.trailing_ones()", rs.TrailingOnes(_1), BITS)
		numStdEq(t, "(_1 << 1).trailing_ones()", rs.TrailingOnes(_1<<1), uint32(0))
		numStdEq(t, "(_1 >> 1).leading_ones()", rs.LeadingOnes(_1>>1), uint32(0))
		numStdEq(t, "(_1 << 1).leading_ones()", rs.LeadingOnes(_1<<1), BITS-1)
		numStdEq(t, "(_1 >> 1).trailing_ones()", rs.TrailingOnes(_1>>1), BITS-1)
		numStdEq(t, "_0.leading_ones()", rs.LeadingOnes(_0), uint32(0))
		numStdEq(t, "_0.trailing_ones()", rs.TrailingOnes(_0), uint32(0))
		X := T(0b0010_1100)
		numStdEq(t, "X.leading_ones()", rs.LeadingOnes(X), uint32(0))
		numStdEq(t, "X.trailing_ones()", rs.TrailingOnes(X), uint32(0))
	case "test_rotate":
		numStdRotate(t, A, B, C, _0, _1)
	case "test_swap_bytes":
		numStdSwapBytes(t, A, B, C, _0, _1)
	case "test_reverse_bits":
		numStdEq(t, "A.reverse_bits().reverse_bits()", rs.ReverseBits(rs.ReverseBits(A)), A)
		numStdEq(t, "B.reverse_bits().reverse_bits()", rs.ReverseBits(rs.ReverseBits(B)), B)
		numStdEq(t, "C.reverse_bits().reverse_bits()", rs.ReverseBits(rs.ReverseBits(C)), C)

		// Swapping these should make no difference
		numStdEq(t, "_0.reverse_bits()", rs.ReverseBits(_0), _0)
		numStdEq(t, "_1.reverse_bits()", rs.ReverseBits(_1), _1)
	case "test_unsigned_checked_div":
		numStdEq(t, "(10).checked_div(2)", rs.CheckedDiv(T(10), 2), rs.Some(T(5)))
		numStdEq(t, "(5).checked_div(0)", rs.CheckedDiv(T(5), 0), rs.None[T]())
	case "test_pow":
		numStdRunPow[T](t, numStdUintPowCases)
	case "test_isqrt":
		numStdEq(t, "(0).isqrt()", rs.Isqrt(T(0)), T(0))
		numStdEq(t, "(1).isqrt()", rs.Isqrt(T(1)), T(1))
		numStdEq(t, "(2).isqrt()", rs.Isqrt(T(2)), T(1))
		numStdEq(t, "(99).isqrt()", rs.Isqrt(T(99)), T(9))
		numStdEq(t, "(100).isqrt()", rs.Isqrt(T(100)), T(10))
		numStdEq(t, "MAX.isqrt()", rs.Isqrt(MAX), (T(1)<<(BITS/2))-1)
	case "test_lots_of_isqrt":
		check := func(n T) {
			isqrt := rs.Isqrt(n)
			if !(rs.Pow(isqrt, 2) <= n) {
				t.Fatalf("isqrt(%d) = %d: its square is above n", n, isqrt)
			}
			if !(isqrt+1 == T(1)<<(BITS/2) || rs.Pow(isqrt+1, 2) > n) {
				t.Fatalf("isqrt(%d) = %d: the next square is not above n", n, isqrt)
			}
		}
		nMax := T(min(uint64(1024*1024), uint64(MAX)))
		for n := T(0); ; n++ {
			check(n)
			if n == nMax {
				break
			}
		}
		for n := MAX - 255; ; n++ {
			check(n)
			if n == MAX {
				break
			}
		}
	case "test_div_floor":
		numStdEq(t, "(8).div_floor(3)", rs.DivFloor(T(8), 3), T(2))
	case "test_div_ceil":
		numStdEq(t, "(8).div_ceil(3)", rs.DivCeil(T(8), 3), T(3))
	case "test_next_multiple_of":
		numStdEq(t, "(16).next_multiple_of(8)", rs.NextMultipleOf(T(16), 8), T(16))
		numStdEq(t, "(23).next_multiple_of(8)", rs.NextMultipleOf(T(23), 8), T(24))
		numStdEq(t, "MAX.next_multiple_of(1)", rs.NextMultipleOf(MAX, 1), MAX)
	case "test_checked_next_multiple_of":
		numStdEq(t, "(16).checked_next_multiple_of(8)", rs.CheckedNextMultipleOf(T(16), 8), rs.Some(T(16)))
		numStdEq(t, "(23).checked_next_multiple_of(8)", rs.CheckedNextMultipleOf(T(23), 8), rs.Some(T(24)))
		numStdEq(t, "(1).checked_next_multiple_of(0)", rs.CheckedNextMultipleOf(T(1), 0), rs.None[T]())
		numStdEq(t, "MAX.checked_next_multiple_of(2)", rs.CheckedNextMultipleOf(MAX, 2), rs.None[T]())
	case "test_is_next_multiple_of":
		numStdEq(t, "(12).is_multiple_of(4)", rs.IsMultipleOf(T(12), 4), true)
		numStdEq(t, "(12).is_multiple_of(5)", rs.IsMultipleOf(T(12), 5), false)
		numStdEq(t, "(0).is_multiple_of(0)", rs.IsMultipleOf(T(0), 0), true)
		numStdEq(t, "(12).is_multiple_of(0)", rs.IsMultipleOf(T(12), 0), false)
	default:
		t.Fatalf("no test %s", test)
	}
}

func TestNumIntMacros_test_overflows(t *testing.T) { numStdEachSigned(t, "test_overflows") }

func TestNumIntMacros_test_rem_euclid(t *testing.T) { numStdEachSigned(t, "test_rem_euclid") }

func TestNumIntMacros_test_abs(t *testing.T) { numStdEachSigned(t, "test_abs") }

func TestNumIntMacros_test_signum(t *testing.T) { numStdEachSigned(t, "test_signum") }

func TestNumIntMacros_test_count_ones(t *testing.T) { numStdEachSigned(t, "test_count_ones") }

func TestNumIntMacros_test_count_zeros(t *testing.T) { numStdEachSigned(t, "test_count_zeros") }

func TestNumIntMacros_test_leading_trailing_ones(t *testing.T) {
	numStdEachSigned(t, "test_leading_trailing_ones")
}

func TestNumIntMacros_test_rotate(t *testing.T) { numStdEachSigned(t, "test_rotate") }

func TestNumIntMacros_test_swap_bytes(t *testing.T) { numStdEachSigned(t, "test_swap_bytes") }

func TestNumIntMacros_test_signed_checked_div(t *testing.T) {
	numStdEachSigned(t, "test_signed_checked_div")
}

func TestNumIntMacros_test_saturating_abs(t *testing.T) { numStdEachSigned(t, "test_saturating_abs") }

func TestNumIntMacros_test_saturating_neg(t *testing.T) { numStdEachSigned(t, "test_saturating_neg") }

func TestNumIntMacros_test_pow(t *testing.T) { numStdEachSigned(t, "test_pow") }

func TestNumIntMacros_test_div_floor(t *testing.T) { numStdEachSigned(t, "test_div_floor") }

func TestNumIntMacros_test_div_ceil(t *testing.T) { numStdEachSigned(t, "test_div_ceil") }

func TestNumIntMacros_test_next_multiple_of(t *testing.T) {
	numStdEachSigned(t, "test_next_multiple_of")
}

func TestNumIntMacros_test_checked_next_multiple_of(t *testing.T) {
	numStdEachSigned(t, "test_checked_next_multiple_of")
}

func TestNumUintMacros_test_count_ones(t *testing.T) { numStdEachUnsigned(t, "test_count_ones") }

func TestNumUintMacros_test_count_zeros(t *testing.T) { numStdEachUnsigned(t, "test_count_zeros") }

func TestNumUintMacros_test_leading_trailing_ones(t *testing.T) {
	numStdEachUnsigned(t, "test_leading_trailing_ones")
}

func TestNumUintMacros_test_rotate(t *testing.T) { numStdEachUnsigned(t, "test_rotate") }

func TestNumUintMacros_test_swap_bytes(t *testing.T) { numStdEachUnsigned(t, "test_swap_bytes") }

func TestNumUintMacros_test_reverse_bits(t *testing.T) { numStdEachUnsigned(t, "test_reverse_bits") }

func TestNumUintMacros_test_unsigned_checked_div(t *testing.T) {
	numStdEachUnsigned(t, "test_unsigned_checked_div")
}

func TestNumUintMacros_test_pow(t *testing.T) { numStdEachUnsigned(t, "test_pow") }

func TestNumUintMacros_test_isqrt(t *testing.T) { numStdEachUnsigned(t, "test_isqrt") }

func TestNumUintMacros_test_lots_of_isqrt(t *testing.T) { numStdEachUnsigned(t, "test_lots_of_isqrt") }

func TestNumUintMacros_test_div_floor(t *testing.T) { numStdEachUnsigned(t, "test_div_floor") }

func TestNumUintMacros_test_div_ceil(t *testing.T) { numStdEachUnsigned(t, "test_div_ceil") }

func TestNumUintMacros_test_next_multiple_of(t *testing.T) {
	numStdEachUnsigned(t, "test_next_multiple_of")
}

func TestNumUintMacros_test_checked_next_multiple_of(t *testing.T) {
	numStdEachUnsigned(t, "test_checked_next_multiple_of")
}

func TestNumUintMacros_test_is_next_multiple_of(t *testing.T) {
	numStdEachUnsigned(t, "test_is_next_multiple_of")
}
