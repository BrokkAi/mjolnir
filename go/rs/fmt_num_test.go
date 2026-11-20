// Port of coretests 1.99.1 tests/fmt/num.rs

package rs

import (
	"math"
	"strconv"
	"strings"
	"testing"
)

// u128 and i128 are written as the formatter's test values (fmtTestInt128)
// until rs.U128 and rs.I128 present themselves to it.

func fmtTestU128(hi, lo uint64) any { return fmtTestInt128{hi: hi, lo: lo, signed: false} }

func fmtTestI128(v int64) any {
	hi := uint64(0)
	if v < 0 {
		hi = math.MaxUint64
	}
	return fmtTestInt128{hi: hi, lo: uint64(v), signed: true}
}

var (
	fmtTestI128Min = fmtTestInt128{hi: 1 << 63, lo: 0, signed: true}
	fmtTestI128Max = fmtTestInt128{hi: math.MaxInt64, lo: math.MaxUint64, signed: true}
	fmtTestU128Max = fmtTestInt128{hi: math.MaxUint64, lo: math.MaxUint64, signed: false}
)

func TestFmtNum_test_format_int_zero(t *testing.T) {
	fmtExpectEq(t, Format("{}", 0), "0")
	fmtExpectEq(t, Format("{:?}", 0), "0")
	fmtExpectEq(t, Format("{:b}", 0), "0")
	fmtExpectEq(t, Format("{:o}", 0), "0")
	fmtExpectEq(t, Format("{:x}", 0), "0")
	fmtExpectEq(t, Format("{:X}", 0), "0")
	fmtExpectEq(t, Format("{:e}", 0), "0e0")
	fmtExpectEq(t, Format("{:E}", 0), "0E0")

	// again, with unsigned
	fmtExpectEq(t, Format("{}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:?}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:b}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:o}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:x}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:X}", uint32(0)), "0")
	fmtExpectEq(t, Format("{:e}", uint32(0)), "0e0")
	fmtExpectEq(t, Format("{:E}", uint32(0)), "0E0")
}

func TestFmtNum_test_format_int_one(t *testing.T) {
	// Formatting integers should select the right implementation based off
	// the type of the argument. Also, hex/octal/binary should be defined
	// for integers, but they shouldn't emit the negative sign.
	signedOnes := []any{1, int8(1), int16(1), int32(1), int64(1), fmtTestI128(1)}
	unsignedOnes := []any{uint(1), uint8(1), uint16(1), uint32(1), uint64(1), fmtTestU128(0, 1)}
	for _, spec := range []string{"{}", "{:?}", "{:b}", "{:x}", "{:X}", "{:o}"} {
		for _, one := range append(append([]any{}, signedOnes...), unsignedOnes...) {
			fmtExpectEq(t, Format(spec, one), "1")
		}
	}
	for _, one := range signedOnes {
		fmtExpectEq(t, Format("{:e}", one), "1e0")
		fmtExpectEq(t, Format("{:E}", one), "1E0")
	}
	// (Rust leaves usize out of these two.)
	for _, one := range unsignedOnes[1:] {
		fmtExpectEq(t, Format("{:e}", one), "1e0")
		fmtExpectEq(t, Format("{:E}", one), "1E0")
	}

	// again, with negative
	signedMinusOnes := []any{-1, int8(-1), int16(-1), int32(-1), int64(-1), fmtTestI128(-1)}
	for _, minusOne := range signedMinusOnes {
		fmtExpectEq(t, Format("{}", minusOne), "-1")
		fmtExpectEq(t, Format("{:?}", minusOne), "-1")
		fmtExpectEq(t, Format("{:e}", minusOne), "-1e0")
		fmtExpectEq(t, Format("{:E}", minusOne), "-1E0")
	}
	fmtExpectEq(t, Format("{:b}", -1), strings.Repeat("1", strconv.IntSize))
	fmtExpectEq(t, Format("{:b}", int8(-1)), "11111111")
	fmtExpectEq(t, Format("{:b}", int16(-1)), "1111111111111111")
	fmtExpectEq(t, Format("{:b}", int32(-1)), "11111111111111111111111111111111")
	fmtExpectEq(t,
		Format("{:b}", int64(-1)),
		"1111111111111111111111111111111111111111111111111111111111111111",
	)
	fmtExpectEq(t,
		Format("{:b}", fmtTestI128(-1)),
		"11111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111",
	)
	fmtExpectEq(t, Format("{:x}", -1), strings.Repeat("ff", strconv.IntSize/8))
	fmtExpectEq(t, Format("{:x}", int8(-1)), "ff")
	fmtExpectEq(t, Format("{:x}", int16(-1)), "ffff")
	fmtExpectEq(t, Format("{:x}", int32(-1)), "ffffffff")
	fmtExpectEq(t, Format("{:x}", int64(-1)), "ffffffffffffffff")
	fmtExpectEq(t, Format("{:x}", fmtTestI128(-1)), "ffffffffffffffffffffffffffffffff")
	fmtExpectEq(t, Format("{:X}", -1), strings.Repeat("FF", strconv.IntSize/8))
	fmtExpectEq(t, Format("{:X}", int8(-1)), "FF")
	fmtExpectEq(t, Format("{:X}", int16(-1)), "FFFF")
	fmtExpectEq(t, Format("{:X}", int32(-1)), "FFFFFFFF")
	fmtExpectEq(t, Format("{:X}", int64(-1)), "FFFFFFFFFFFFFFFF")
	fmtExpectEq(t, Format("{:X}", fmtTestI128(-1)), "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF")
	// octal test for isize omitted
	fmtExpectEq(t, Format("{:o}", int8(-1)), "377")
	fmtExpectEq(t, Format("{:o}", int16(-1)), "177777")
	fmtExpectEq(t, Format("{:o}", int32(-1)), "37777777777")
	fmtExpectEq(t, Format("{:o}", int64(-1)), "1777777777777777777777")
	fmtExpectEq(t, Format("{:o}", fmtTestI128(-1)), "3777777777777777777777777777777777777777777")
}

func TestFmtNum_test_format_int_misc(t *testing.T) {
	fmtExpectEq(t, Format("{:b}", 55), "110111")
	fmtExpectEq(t, Format("{:o}", 55), "67")
	fmtExpectEq(t, Format("{}", 55), "55")
	fmtExpectEq(t, Format("{:x}", 55), "37")
	fmtExpectEq(t, Format("{:X}", 55), "37")
	fmtExpectEq(t, Format("{:e}", 55), "5.5e1")
	fmtExpectEq(t, Format("{:E}", 55), "5.5E1")
	fmtExpectEq(t, Format("{:e}", uint64(10000000000)), "1e10")
	fmtExpectEq(t, Format("{:E}", uint64(10000000000)), "1E10")
	fmtExpectEq(t, Format("{:e}", uint64(10000000001)), "1.0000000001e10")
	fmtExpectEq(t, Format("{:E}", uint64(10000000001)), "1.0000000001E10")
}

func TestFmtNum_test_format_int_limits(t *testing.T) {
	fmtExpectEq(t, Format("{}", int8(math.MinInt8)), "-128")
	fmtExpectEq(t, Format("{}", int8(math.MaxInt8)), "127")
	fmtExpectEq(t, Format("{}", int16(math.MinInt16)), "-32768")
	fmtExpectEq(t, Format("{}", int16(math.MaxInt16)), "32767")
	fmtExpectEq(t, Format("{}", int32(math.MinInt32)), "-2147483648")
	fmtExpectEq(t, Format("{}", int32(math.MaxInt32)), "2147483647")
	fmtExpectEq(t, Format("{}", int64(math.MinInt64)), "-9223372036854775808")
	fmtExpectEq(t, Format("{}", int64(math.MaxInt64)), "9223372036854775807")
	fmtExpectEq(t, Format("{}", fmtTestI128Min), "-170141183460469231731687303715884105728")
	fmtExpectEq(t, Format("{}", fmtTestI128Max), "170141183460469231731687303715884105727")

	fmtExpectEq(t, Format("{}", uint8(math.MaxUint8)), "255")
	fmtExpectEq(t, Format("{}", uint16(math.MaxUint16)), "65535")
	fmtExpectEq(t, Format("{}", uint32(math.MaxUint32)), "4294967295")
	fmtExpectEq(t, Format("{}", uint64(math.MaxUint64)), "18446744073709551615")
	fmtExpectEq(t, Format("{}", fmtTestU128Max), "340282366920938463463374607431768211455")
}

func TestFmtNum_test_format_int_exp_limits(t *testing.T) {
	fmtExpectEq(t, Format("{:e}", int8(math.MinInt8)), "-1.28e2")
	fmtExpectEq(t, Format("{:e}", int8(math.MaxInt8)), "1.27e2")
	fmtExpectEq(t, Format("{:e}", int16(math.MinInt16)), "-3.2768e4")
	fmtExpectEq(t, Format("{:e}", int16(math.MaxInt16)), "3.2767e4")
	fmtExpectEq(t, Format("{:e}", int32(math.MinInt32)), "-2.147483648e9")
	fmtExpectEq(t, Format("{:e}", int32(math.MaxInt32)), "2.147483647e9")
	fmtExpectEq(t, Format("{:e}", int64(math.MinInt64)), "-9.223372036854775808e18")
	fmtExpectEq(t, Format("{:e}", int64(math.MaxInt64)), "9.223372036854775807e18")
	fmtExpectEq(t, Format("{:e}", fmtTestI128Min), "-1.70141183460469231731687303715884105728e38")
	fmtExpectEq(t, Format("{:e}", fmtTestI128Max), "1.70141183460469231731687303715884105727e38")

	fmtExpectEq(t, Format("{:e}", uint8(math.MaxUint8)), "2.55e2")
	fmtExpectEq(t, Format("{:e}", uint16(math.MaxUint16)), "6.5535e4")
	fmtExpectEq(t, Format("{:e}", uint32(math.MaxUint32)), "4.294967295e9")
	fmtExpectEq(t, Format("{:e}", uint64(math.MaxUint64)), "1.8446744073709551615e19")
	fmtExpectEq(t, Format("{:e}", fmtTestU128Max), "3.40282366920938463463374607431768211455e38")
}

func TestFmtNum_test_format_int_exp_precision(t *testing.T) {
	//test that float and integer match
	bigInt := uint32(314_159_265)
	fmtExpectEq(t, Format("{big_int:.1e}", Arg("big_int", bigInt)), Format("{:.1e}", float64(bigInt)))

	// test adding precision
	fmtExpectEq(t, Format("{:.10e}", int8(math.MinInt8)), "-1.2800000000e2")
	fmtExpectEq(t, Format("{:.10e}", int16(math.MinInt16)), "-3.2768000000e4")
	fmtExpectEq(t, Format("{:.10e}", int32(math.MinInt32)), "-2.1474836480e9")
	fmtExpectEq(t, Format("{:.20e}", int64(math.MinInt64)), "-9.22337203685477580800e18")
	fmtExpectEq(t, Format("{:.40e}", fmtTestI128Min), "-1.7014118346046923173168730371588410572800e38")

	// test rounding
	fmtExpectEq(t, Format("{:.1e}", int8(math.MinInt8)), "-1.3e2")
	fmtExpectEq(t, Format("{:.1e}", int16(math.MinInt16)), "-3.3e4")
	fmtExpectEq(t, Format("{:.1e}", int32(math.MinInt32)), "-2.1e9")
	fmtExpectEq(t, Format("{:.1e}", int64(math.MinInt64)), "-9.2e18")
	fmtExpectEq(t, Format("{:.1e}", fmtTestI128Min), "-1.7e38")

	// test huge precision
	fmtExpectEq(t, Format("{:.1000e}", 1), Format("1.{}e0", strings.Repeat("0", 1000)))
	//test zero precision
	fmtExpectEq(t, Format("{:.0e}", 1), Format("1e0"))
	fmtExpectEq(t, Format("{:.0e}", 35), Format("4e1"))

	// test padding with precision (and sign)
	fmtExpectEq(t, Format("{:+10.3e}", 1), "  +1.000e0")

	// test precision remains correct when rounding to next power
	for i := math.MinInt16; i <= math.MaxInt16; i++ {
		for p := 0; p <= 5; p++ {
			integer := Format("{i:.p$e}", Arg("i", int16(i)), Arg("p", p))
			float := Format("{:.p$e}", float32(i), Arg("p", p))
			if integer != float {
				t.Fatalf("integer %d at precision %d: %q, the f32 gives %q", i, p, integer, float)
			}
		}
	}
}

func TestFmtNum_test_format_int_flags(t *testing.T) {
	fmtExpectEq(t, Format("{:3}", 1), "  1")
	fmtExpectEq(t, Format("{:>3}", 1), "  1")
	fmtExpectEq(t, Format("{:>+3}", 1), " +1")
	fmtExpectEq(t, Format("{:<3}", 1), "1  ")
	fmtExpectEq(t, Format("{:#}", 1), "1")
	fmtExpectEq(t, Format("{:#x}", 10), "0xa")
	fmtExpectEq(t, Format("{:#X}", 10), "0xA")
	fmtExpectEq(t, Format("{:#5x}", 10), "  0xa")
	fmtExpectEq(t, Format("{:#o}", 10), "0o12")
	fmtExpectEq(t, Format("{:08x}", 10), "0000000a")
	fmtExpectEq(t, Format("{:8x}", 10), "       a")
	fmtExpectEq(t, Format("{:<8x}", 10), "a       ")
	fmtExpectEq(t, Format("{:>8x}", 10), "       a")
	fmtExpectEq(t, Format("{:#08x}", 10), "0x00000a")
	fmtExpectEq(t, Format("{:08}", -10), "-0000010")
	fmtExpectEq(t, Format("{:x}", ^uint8(0)), "ff")
	fmtExpectEq(t, Format("{:X}", ^uint8(0)), "FF")
	fmtExpectEq(t, Format("{:b}", ^uint8(0)), "11111111")
	fmtExpectEq(t, Format("{:o}", ^uint8(0)), "377")
	fmtExpectEq(t, Format("{:#x}", ^uint8(0)), "0xff")
	fmtExpectEq(t, Format("{:#X}", ^uint8(0)), "0xFF")
	fmtExpectEq(t, Format("{:#b}", ^uint8(0)), "0b11111111")
	fmtExpectEq(t, Format("{:#o}", ^uint8(0)), "0o377")
}

func TestFmtNum_test_format_int_sign_padding(t *testing.T) {
	fmtExpectEq(t, Format("{:+5}", 1), "   +1")
	fmtExpectEq(t, Format("{:+5}", -1), "   -1")
	fmtExpectEq(t, Format("{:05}", 1), "00001")
	fmtExpectEq(t, Format("{:05}", -1), "-0001")
	fmtExpectEq(t, Format("{:+05}", 1), "+0001")
	fmtExpectEq(t, Format("{:+05}", -1), "-0001")
}

func TestFmtNum_test_format_debug_hex(t *testing.T) {
	fmtExpectEq(t, Format("{:02x?}", []byte("Foo\x00")), "[46, 6f, 6f, 00]")
	fmtExpectEq(t, Format("{:02X?}", []byte("Foo\x00")), "[46, 6F, 6F, 00]")
}

func TestFmtNum_test_rt_width_too_long(t *testing.T) {
	defer func() {
		recovered := recover()
		if recovered == nil {
			t.Fatal("did not panic")
		}
		if message := PanicInfoOf(recovered).Message; !strings.Contains(message, "Formatting argument out of range") {
			t.Fatalf("panicked with %q", message)
		}
	}()
	_ = Format("Hello {:width$}!", "x", Arg("width", int(math.MaxUint16)+1))
}
