// Port of coretests 1.99.1 tests/num/mod.rs, tests/num/int_macros.rs and
// tests/num/uint_macros.rs: the tests of parsing integers
//
// Not ported: test_can_not_overflow, which tests the internal
// can_not_overflow (this port has one checked loop where Rust adds an
// unchecked one for short input; the oracle corpus covers both lengths), and
// the const evaluation of _TEST_CONST_PARSE and of the
// test_runtime_and_compiletime! tests, which run here at run time only.

package strs

import (
	"math"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// testParseOk and testParseErr are test_parse: parsing numStr gives expected,
// or fails with the expected IntErrorKind.
func testParseOk[T comparable](t *testing.T, parse func(string) (T, error), numStr string, expected T) {
	t.Helper()
	got, err := parse(numStr)
	if err != nil || got != expected {
		t.Errorf("parse(%q) = %v, %v; want Ok(%v)", numStr, got, err, expected)
	}
}

func testParseErr[T any](t *testing.T, parse func(string) (T, error), numStr string, expected IntErrorKind) {
	t.Helper()
	got, err := parse(numStr)
	parseErr, ok := err.(*ParseIntError)
	if !ok || parseErr.Kind() != expected {
		t.Errorf("parse(%q) = %v, %v; want Err(%s)", numStr, got, err, expected.name())
	}
}

func TestNum_from_str_issue7588(t *testing.T) {
	_, err := U8FromStrRadix("1000", 10)
	isTrue(t, err != nil)
	_, err = I16FromStrRadix("80000", 10)
	isTrue(t, err != nil)
}

func TestNum_from_ascii_bytes_radix_panic(t *testing.T) {
	radix := uint32(1)
	shouldPanic(t, "radix must lie in the range `[2, 36]`", func() { U64FromStrRadix("12345ABCD", radix) })
}

func TestNum_test_int_from_str_overflow(t *testing.T) {
	testParseOk(t, ParseI8, "127", 127)
	testParseErr(t, ParseI8, "128", IntErrorKind_PosOverflow)

	testParseOk(t, ParseI8, "-128", -128)
	testParseErr(t, ParseI8, "-129", IntErrorKind_NegOverflow)

	testParseOk(t, ParseI16, "32767", 32_767)
	testParseErr(t, ParseI16, "32768", IntErrorKind_PosOverflow)

	testParseOk(t, ParseI16, "-32768", -32_768)
	testParseErr(t, ParseI16, "-32769", IntErrorKind_NegOverflow)

	testParseOk(t, ParseI32, "2147483647", 2_147_483_647)
	testParseErr(t, ParseI32, "2147483648", IntErrorKind_PosOverflow)

	testParseOk(t, ParseI32, "-2147483648", -2_147_483_648)
	testParseErr(t, ParseI32, "-2147483649", IntErrorKind_NegOverflow)

	testParseOk(t, ParseI64, "9223372036854775807", 9_223_372_036_854_775_807)
	testParseErr(t, ParseI64, "9223372036854775808", IntErrorKind_PosOverflow)

	testParseOk(t, ParseI64, "-9223372036854775808", -9_223_372_036_854_775_808)
	testParseErr(t, ParseI64, "-9223372036854775809", IntErrorKind_NegOverflow)
}

func TestNum_test_leading_plus(t *testing.T) {
	testParseOk(t, ParseU8, "+127", 127)
	testParseOk(t, ParseI64, "+9223372036854775807", 9223372036854775807)
}

func TestNum_test_invalid(t *testing.T) {
	testParseErr(t, ParseI8, "--129", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseI8, "++129", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseU8, "Съешь", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseU8, "123Hello", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseI8, "--", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseI8, "-", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseI8, "+", IntErrorKind_InvalidDigit)
	testParseErr(t, ParseU8, "-1", IntErrorKind_InvalidDigit)
}

func TestNum_test_empty(t *testing.T) {
	testParseErr(t, ParseU8, "", IntErrorKind_Empty)
}

// The int_module! and uint_module! macros instantiate their tests once for
// each integer type; intType is what they need of one type $T.

// wide is a 128-bit integer as this package returns it: the high and low
// 64-bit halves of its two's complement form.
type wide struct {
	hi, lo uint64
}

type intType[T comparable] struct {
	fromStr      func(string) (T, error)
	fromStrRadix func(string, uint32) (T, error)
	// of is `n as $T`.
	of func(n int64) T
}

func narrow[T int8 | int16 | int32 | int64 | uint8 | uint16 | uint32 | uint64](
	fromStr func(string) (T, error), fromStrRadix func(string, uint32) (T, error),
) intType[T] {
	return intType[T]{fromStr: fromStr, fromStrRadix: fromStrRadix, of: func(n int64) T { return T(n) }}
}

func wideOf(n int64) wide { return wide{hi: uint64(n >> 63), lo: uint64(n)} }

var i128Type = intType[wide]{
	fromStr: func(s string) (wide, error) {
		hi, lo, err := ParseI128Parts(s)
		return wide{uint64(hi), lo}, err
	},
	fromStrRadix: func(s string, radix uint32) (wide, error) {
		hi, lo, err := I128FromStrRadixParts(s, radix)
		return wide{uint64(hi), lo}, err
	},
	of: wideOf,
}

var u128Type = intType[wide]{
	fromStr: func(s string) (wide, error) {
		hi, lo, err := ParseU128Parts(s)
		return wide{hi, lo}, err
	},
	fromStrRadix: func(s string, radix uint32) (wide, error) {
		hi, lo, err := U128FromStrRadixParts(s, radix)
		return wide{hi, lo}, err
	},
	of: wideOf,
}

// fromStrOk is the macros' from_str: `std::str::FromStr::from_str(t).ok()`.
func fromStrOk[T any](parse func(string) (T, error), s string) rs.Option[T] {
	value, err := parse(s)
	if err != nil {
		return rs.None[T]()
	}
	return rs.Some(value)
}

// radixResult is a from_str_radix result written as the tests compare it.
func radixResult[T any](value T, err error) rs.Option[T] {
	if err != nil {
		return rs.None[T]()
	}
	return rs.Some(value)
}

// intTestFromStr is int_module!'s test_from_str.
func intTestFromStr[T comparable](t *testing.T, ty intType[T]) {
	t.Helper()
	eq(t, fromStrOk(ty.fromStr, "0"), rs.Some(ty.of(0)))
	eq(t, fromStrOk(ty.fromStr, "3"), rs.Some(ty.of(3)))
	eq(t, fromStrOk(ty.fromStr, "10"), rs.Some(ty.of(10)))
	eq(t, fromStrOk(ParseI32, "123456789"), rs.Some(int32(123456789)))
	eq(t, fromStrOk(ty.fromStr, "00100"), rs.Some(ty.of(100)))

	eq(t, fromStrOk(ty.fromStr, "-1"), rs.Some(ty.of(-1)))
	eq(t, fromStrOk(ty.fromStr, "-3"), rs.Some(ty.of(-3)))
	eq(t, fromStrOk(ty.fromStr, "-10"), rs.Some(ty.of(-10)))
	eq(t, fromStrOk(ParseI32, "-123456789"), rs.Some(int32(-123456789)))
	eq(t, fromStrOk(ty.fromStr, "-00100"), rs.Some(ty.of(-100)))

	eq(t, fromStrOk(ty.fromStr, ""), rs.None[T]())
	eq(t, fromStrOk(ty.fromStr, " "), rs.None[T]())
	eq(t, fromStrOk(ty.fromStr, "x"), rs.None[T]())
}

// intTestFromStrRadix is int_module!'s test_from_str_radix.
func intTestFromStrRadix[T comparable](t *testing.T, ty intType[T]) {
	t.Helper()
	eq(t, radixResult(ty.fromStrRadix("123", 10)), rs.Some(ty.of(123)))
	eq(t, radixResult(ty.fromStrRadix("1001", 2)), rs.Some(ty.of(9)))
	eq(t, radixResult(ty.fromStrRadix("123", 8)), rs.Some(ty.of(83)))
	eq(t, radixResult(I32FromStrRadix("123", 16)), rs.Some(int32(291)))
	eq(t, radixResult(I32FromStrRadix("ffff", 16)), rs.Some(int32(65535)))
	eq(t, radixResult(I32FromStrRadix("FFFF", 16)), rs.Some(int32(65535)))
	eq(t, radixResult(ty.fromStrRadix("z", 36)), rs.Some(ty.of(35)))
	eq(t, radixResult(ty.fromStrRadix("Z", 36)), rs.Some(ty.of(35)))

	eq(t, radixResult(ty.fromStrRadix("-123", 10)), rs.Some(ty.of(-123)))
	eq(t, radixResult(ty.fromStrRadix("-1001", 2)), rs.Some(ty.of(-9)))
	eq(t, radixResult(ty.fromStrRadix("-123", 8)), rs.Some(ty.of(-83)))
	eq(t, radixResult(I32FromStrRadix("-123", 16)), rs.Some(int32(-291)))
	eq(t, radixResult(I32FromStrRadix("-ffff", 16)), rs.Some(int32(-65535)))
	eq(t, radixResult(I32FromStrRadix("-FFFF", 16)), rs.Some(int32(-65535)))
	eq(t, radixResult(ty.fromStrRadix("-z", 36)), rs.Some(ty.of(-35)))
	eq(t, radixResult(ty.fromStrRadix("-Z", 36)), rs.Some(ty.of(-35)))

	_, err := ty.fromStrRadix("Z", 35)
	isTrue(t, err != nil)
	_, err = ty.fromStrRadix("-9", 2)
	isTrue(t, err != nil)
	_, err = ty.fromStrRadix("10_0", 10)
	isTrue(t, err != nil)
	_, err = U32FromStrRadix("-9", 10)
	isTrue(t, err != nil)
}

// uintTestFromStr is uint_module!'s test_from_str.
func uintTestFromStr[T comparable](t *testing.T, ty intType[T]) {
	t.Helper()
	eq(t, fromStrOk(ty.fromStr, "0"), rs.Some(ty.of(0)))
	eq(t, fromStrOk(ty.fromStr, "3"), rs.Some(ty.of(3)))
	eq(t, fromStrOk(ty.fromStr, "10"), rs.Some(ty.of(10)))
	eq(t, fromStrOk(ParseU32, "123456789"), rs.Some(uint32(123456789)))
	eq(t, fromStrOk(ty.fromStr, "00100"), rs.Some(ty.of(100)))

	eq(t, fromStrOk(ty.fromStr, ""), rs.None[T]())
	eq(t, fromStrOk(ty.fromStr, " "), rs.None[T]())
	eq(t, fromStrOk(ty.fromStr, "x"), rs.None[T]())
}

// uintTestParseBytes is uint_module!'s test_parse_bytes.
func uintTestParseBytes[T comparable](t *testing.T, ty intType[T]) {
	t.Helper()
	eq(t, radixResult(ty.fromStrRadix("123", 10)), rs.Some(ty.of(123)))
	eq(t, radixResult(ty.fromStrRadix("1001", 2)), rs.Some(ty.of(9)))
	eq(t, radixResult(ty.fromStrRadix("123", 8)), rs.Some(ty.of(83)))
	eq(t, radixResult(U16FromStrRadix("123", 16)), rs.Some(uint16(291)))
	eq(t, radixResult(U16FromStrRadix("ffff", 16)), rs.Some(uint16(65535)))
	eq(t, radixResult(ty.fromStrRadix("z", 36)), rs.Some(ty.of(35)))

	_, err := ty.fromStrRadix("Z", 10)
	isTrue(t, err != nil)
	_, err = ty.fromStrRadix("_", 2)
	isTrue(t, err != nil)
}

func TestNumI8_test_from_str(t *testing.T) {
	intTestFromStr(t, narrow(ParseI8, I8FromStrRadix))
}

func TestNumI8_test_from_str_radix(t *testing.T) {
	intTestFromStrRadix(t, narrow(ParseI8, I8FromStrRadix))
}

func TestNumI16_test_from_str(t *testing.T) {
	intTestFromStr(t, narrow(ParseI16, I16FromStrRadix))
}

func TestNumI16_test_from_str_radix(t *testing.T) {
	intTestFromStrRadix(t, narrow(ParseI16, I16FromStrRadix))
}

func TestNumI32_test_from_str(t *testing.T) {
	intTestFromStr(t, narrow(ParseI32, I32FromStrRadix))
}

func TestNumI32_test_from_str_radix(t *testing.T) {
	intTestFromStrRadix(t, narrow(ParseI32, I32FromStrRadix))
}

func TestNumI64_test_from_str(t *testing.T) {
	intTestFromStr(t, narrow(ParseI64, I64FromStrRadix))
}

func TestNumI64_test_from_str_radix(t *testing.T) {
	intTestFromStrRadix(t, narrow(ParseI64, I64FromStrRadix))
}

func TestNumI128_test_from_str(t *testing.T) {
	intTestFromStr(t, i128Type)
}

func TestNumI128_test_from_str_radix(t *testing.T) {
	intTestFromStrRadix(t, i128Type)
}

func TestNumU8_test_from_str(t *testing.T) {
	uintTestFromStr(t, narrow(ParseU8, U8FromStrRadix))
}

func TestNumU8_test_parse_bytes(t *testing.T) {
	uintTestParseBytes(t, narrow(ParseU8, U8FromStrRadix))
}

func TestNumU16_test_from_str(t *testing.T) {
	uintTestFromStr(t, narrow(ParseU16, U16FromStrRadix))
}

func TestNumU16_test_parse_bytes(t *testing.T) {
	uintTestParseBytes(t, narrow(ParseU16, U16FromStrRadix))
}

func TestNumU32_test_from_str(t *testing.T) {
	uintTestFromStr(t, narrow(ParseU32, U32FromStrRadix))
}

func TestNumU32_test_parse_bytes(t *testing.T) {
	uintTestParseBytes(t, narrow(ParseU32, U32FromStrRadix))
}

func TestNumU64_test_from_str(t *testing.T) {
	uintTestFromStr(t, narrow(ParseU64, U64FromStrRadix))
}

func TestNumU64_test_parse_bytes(t *testing.T) {
	uintTestParseBytes(t, narrow(ParseU64, U64FromStrRadix))
}

func TestNumU128_test_from_str(t *testing.T) {
	uintTestFromStr(t, u128Type)
}

func TestNumU128_test_parse_bytes(t *testing.T) {
	uintTestParseBytes(t, u128Type)
}

// usize and isize are Go ints here; the macros do not run for them, so this
// checks the one difference the port makes (doc.go): a usize above the
// largest int is too large.
func TestNum_usize_above_int(t *testing.T) {
	testParseOk(t, ParseUsize, "9223372036854775807", math.MaxInt)
	testParseErr(t, ParseUsize, "9223372036854775808", IntErrorKind_PosOverflow)
	testParseOk(t, ParseIsize, "-9223372036854775808", math.MinInt)
}
