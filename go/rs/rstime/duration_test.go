// Port of core 1.99.1 coretests/tests/time.rs
//
// A Rust Duration reaches u64::MAX seconds and a Go one a little over 292
// years. Where a Rust test uses a larger value, the port's test uses the
// functions that work on whole seconds and nanoseconds
// (DurationTryFromSecsF64Parts, mulF64, divF64), which cover Rust's range, or
// uses DurationMax where the Rust test means "the largest duration". Each
// such place says so.
//
// debug_formatting_extreme_values is not here: Duration's Debug output is
// the formatter's (go/rs/fmt_time.go), and its port of that test
// (TestFmtTime_debug_formatting_extreme_values) prints durations beyond what
// a time.Duration holds, which this package cannot build.

package rstime

import (
	"math"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const (
	second      = time.Second
	nanosecond  = time.Nanosecond
	u64Max      = uint64(math.MaxUint64)
	alignLeft   = rs.Alignment_Left
	alignRight  = rs.Alignment_Right
	alignCenter = rs.Alignment_Center
)

func TestDuration_creation(t *testing.T) {
	assertNe(t, DurationFromSecs(1), DurationFromSecs(0))
	assertEq(t, DurationAdd(DurationFromSecs(1), DurationFromSecs(2)), DurationFromSecs(3))
	assertEq(t, DurationAdd(DurationFromMillis(10), DurationFromSecs(4)), DurationNew(4, 10*1_000_000))
	assertEq(t, DurationFromMillis(4000), DurationNew(4, 0))
}

func TestDuration_new_overflow(t *testing.T) {
	shouldPanic(t, "overflow in Duration::new", func() {
		_ = DurationNew(u64Max, 1_000_000_000)
	})
}

func TestDuration_from_mins_overflow(t *testing.T) {
	shouldPanic(t, "overflow in Duration::from_mins", func() {
		overflow := u64Max/60 + 1
		_ = DurationFromMins(overflow)
	})
}

func TestDuration_from_hours_overflow(t *testing.T) {
	shouldPanic(t, "overflow in Duration::from_hours", func() {
		overflow := u64Max/(60*60) + 1
		_ = DurationFromHours(overflow)
	})
}

// from_days_overflow, from_weeks_overflow, constructor_weeks and
// constructor_days test unstable constructors the port does not have, and
// from_nanos_u128_overflow a constructor that needs a u128.

func TestDuration_constructor_hours(t *testing.T) {
	assertEq(t, DurationFromHours(1), DurationFromSecs(3_600))
	assertEq(t, DurationFromHours(0), DurationZero)
}

func TestDuration_constructor_minutes(t *testing.T) {
	assertEq(t, DurationFromMins(1), DurationFromSecs(60))
	assertEq(t, DurationFromMins(0), DurationZero)
}

func TestDuration_secs(t *testing.T) {
	assertEq(t, DurationAsSecs(DurationNew(0, 0)), 0)
	assertEq(t, DurationAsSecs(DurationNew(0, 500_000_005)), 0)
	assertEq(t, DurationAsSecs(DurationNew(0, 1_050_000_001)), 1)
	assertEq(t, DurationAsSecs(DurationFromSecs(1)), 1)
	assertEq(t, DurationAsSecs(DurationFromMillis(999)), 0)
	assertEq(t, DurationAsSecs(DurationFromMillis(1001)), 1)
	assertEq(t, DurationAsSecs(DurationFromMicros(999_999)), 0)
	assertEq(t, DurationAsSecs(DurationFromMicros(1_000_001)), 1)
	assertEq(t, DurationAsSecs(DurationFromNanos(999_999_999)), 0)
	assertEq(t, DurationAsSecs(DurationFromNanos(1_000_000_001)), 1)
}

func TestDuration_millis(t *testing.T) {
	assertEq(t, DurationSubsecMillis(DurationNew(0, 0)), 0)
	assertEq(t, DurationSubsecMillis(DurationNew(0, 500_000_005)), 500)
	assertEq(t, DurationSubsecMillis(DurationNew(0, 1_050_000_001)), 50)
	assertEq(t, DurationSubsecMillis(DurationFromSecs(1)), 0)
	assertEq(t, DurationSubsecMillis(DurationFromMillis(999)), 999)
	assertEq(t, DurationSubsecMillis(DurationFromMillis(1001)), 1)
	assertEq(t, DurationSubsecMillis(DurationFromMicros(999_999)), 999)
	assertEq(t, DurationSubsecMillis(DurationFromMicros(1_001_000)), 1)
	assertEq(t, DurationSubsecMillis(DurationFromNanos(999_999_999)), 999)
	assertEq(t, DurationSubsecMillis(DurationFromNanos(1_001_000_000)), 1)
}

func TestDuration_micros(t *testing.T) {
	assertEq(t, DurationSubsecMicros(DurationNew(0, 0)), 0)
	assertEq(t, DurationSubsecMicros(DurationNew(0, 500_000_005)), 500_000)
	assertEq(t, DurationSubsecMicros(DurationNew(0, 1_050_000_001)), 50_000)
	assertEq(t, DurationSubsecMicros(DurationFromSecs(1)), 0)
	assertEq(t, DurationSubsecMicros(DurationFromMillis(999)), 999_000)
	assertEq(t, DurationSubsecMicros(DurationFromMillis(1001)), 1_000)
	assertEq(t, DurationSubsecMicros(DurationFromMicros(999_999)), 999_999)
	assertEq(t, DurationSubsecMicros(DurationFromMicros(1_000_001)), 1)
	assertEq(t, DurationSubsecMicros(DurationFromNanos(999_999_999)), 999_999)
	assertEq(t, DurationSubsecMicros(DurationFromNanos(1_000_001_000)), 1)
}

func TestDuration_nanos(t *testing.T) {
	assertEq(t, DurationSubsecNanos(DurationNew(0, 0)), 0)
	assertEq(t, DurationSubsecNanos(DurationNew(0, 5)), 5)
	assertEq(t, DurationSubsecNanos(DurationNew(0, 1_000_000_001)), 1)
	assertEq(t, DurationSubsecNanos(DurationFromSecs(1)), 0)
	assertEq(t, DurationSubsecNanos(DurationFromMillis(999)), 999_000_000)
	assertEq(t, DurationSubsecNanos(DurationFromMillis(1001)), 1_000_000)
	assertEq(t, DurationSubsecNanos(DurationFromMicros(999_999)), 999_999_000)
	assertEq(t, DurationSubsecNanos(DurationFromMicros(1_000_001)), 1000)
	assertEq(t, DurationSubsecNanos(DurationFromNanos(999_999_999)), 999_999_999)
	assertEq(t, DurationSubsecNanos(DurationFromNanos(1_000_000_001)), 1)
}

func TestDuration_abs_diff(t *testing.T) {
	assertEq(t, DurationAbsDiff(DurationNew(2, 0), DurationNew(1, 0)), DurationNew(1, 0))
	assertEq(t, DurationAbsDiff(DurationNew(1, 0), DurationNew(2, 0)), DurationNew(1, 0))
	assertEq(t, DurationAbsDiff(DurationNew(1, 0), DurationNew(1, 0)), DurationNew(0, 0))
	assertEq(t, DurationAbsDiff(DurationNew(1, 1), DurationNew(0, 2)), DurationNew(0, 999_999_999))
	assertEq(t, DurationAbsDiff(DurationNew(1, 1), DurationNew(2, 1)), DurationNew(1, 0))
	assertEq(t, DurationAbsDiff(DurationMax, DurationMax), DurationZero)
	assertEq(t, DurationAbsDiff(DurationZero, DurationZero), DurationZero)
	assertEq(t, DurationAbsDiff(DurationMax, DurationZero), DurationMax)
	assertEq(t, DurationAbsDiff(DurationZero, DurationMax), DurationMax)
}

func TestDuration_add(t *testing.T) {
	assertEq(t, DurationAdd(DurationNew(0, 0), DurationNew(0, 1)), DurationNew(0, 1))
	assertEq(t, DurationAdd(DurationNew(0, 500_000_000), DurationNew(0, 500_000_001)), DurationNew(1, 1))
}

func TestDuration_checked_add(t *testing.T) {
	assertEq(t, DurationCheckedAdd(DurationNew(0, 0), DurationNew(0, 1)), some(DurationNew(0, 1)))
	assertEq(t, DurationCheckedAdd(DurationNew(0, 500_000_000), DurationNew(0, 500_000_001)), some(DurationNew(1, 1)))
	// Duration::new(u64::MAX, 0) is DurationMax in the port.
	assertEq(t, DurationCheckedAdd(DurationNew(1, 0), DurationNew(u64Max, 0)), none())
}

func TestDuration_saturating_add(t *testing.T) {
	assertEq(t, DurationSaturatingAdd(DurationNew(0, 0), DurationNew(0, 1)), DurationNew(0, 1))
	assertEq(t, DurationSaturatingAdd(DurationNew(0, 500_000_000), DurationNew(0, 500_000_001)), DurationNew(1, 1))
	assertEq(t, DurationSaturatingAdd(DurationNew(1, 0), DurationNew(u64Max, 0)), DurationMax)
}

func TestDuration_sub(t *testing.T) {
	assertEq(t, DurationSub(DurationNew(0, 1), DurationNew(0, 0)), DurationNew(0, 1))
	assertEq(t, DurationSub(DurationNew(0, 500_000_001), DurationNew(0, 500_000_000)), DurationNew(0, 1))
	assertEq(t, DurationSub(DurationNew(1, 0), DurationNew(0, 1)), DurationNew(0, 999_999_999))
}

func TestDuration_checked_sub(t *testing.T) {
	assertEq(t, DurationCheckedSub(nanosecond, DurationZero), some(nanosecond))
	assertEq(t, DurationCheckedSub(second, nanosecond), some(DurationNew(0, 999_999_999)))
	assertEq(t, DurationCheckedSub(DurationZero, nanosecond), none())
	assertEq(t, DurationCheckedSub(DurationZero, second), none())
}

func TestDuration_saturating_sub(t *testing.T) {
	assertEq(t, DurationSaturatingSub(nanosecond, DurationZero), nanosecond)
	assertEq(t, DurationSaturatingSub(second, nanosecond), DurationNew(0, 999_999_999))
	assertEq(t, DurationSaturatingSub(DurationZero, nanosecond), DurationZero)
	assertEq(t, DurationSaturatingSub(DurationZero, second), DurationZero)
}

func TestDuration_sub_bad1(t *testing.T) {
	shouldPanic(t, "overflow when subtracting durations", func() {
		_ = DurationSub(DurationNew(0, 0), DurationNew(0, 1))
	})
}

func TestDuration_sub_bad2(t *testing.T) {
	shouldPanic(t, "overflow when subtracting durations", func() {
		_ = DurationSub(DurationNew(0, 0), DurationNew(1, 0))
	})
}

func TestDuration_mul(t *testing.T) {
	assertEq(t, DurationMul(DurationNew(0, 1), 2), DurationNew(0, 2))
	assertEq(t, DurationMul(DurationNew(1, 1), 3), DurationNew(3, 3))
	assertEq(t, DurationMul(DurationNew(0, 500_000_001), 4), DurationNew(2, 4))
	assertEq(t, DurationMul(DurationNew(0, 500_000_001), 4000), DurationNew(2000, 4000))
}

func TestDuration_checked_mul(t *testing.T) {
	assertEq(t, DurationCheckedMul(DurationNew(0, 1), 2), some(DurationNew(0, 2)))
	assertEq(t, DurationCheckedMul(DurationNew(1, 1), 3), some(DurationNew(3, 3)))
	assertEq(t, DurationCheckedMul(DurationNew(0, 500_000_001), 4), some(DurationNew(2, 4)))
	assertEq(t, DurationCheckedMul(DurationNew(0, 500_000_001), 4000), some(DurationNew(2000, 4000)))
	assertEq(t, DurationCheckedMul(DurationNew(u64Max-1, 0), 2), none())
}

func TestDuration_saturating_mul(t *testing.T) {
	assertEq(t, DurationSaturatingMul(DurationNew(0, 1), 2), DurationNew(0, 2))
	assertEq(t, DurationSaturatingMul(DurationNew(1, 1), 3), DurationNew(3, 3))
	assertEq(t, DurationSaturatingMul(DurationNew(0, 500_000_001), 4), DurationNew(2, 4))
	assertEq(t, DurationSaturatingMul(DurationNew(0, 500_000_001), 4000), DurationNew(2000, 4000))
	assertEq(t, DurationSaturatingMul(DurationNew(u64Max-1, 0), 2), DurationMax)
}

func TestDuration_div(t *testing.T) {
	assertEq(t, DurationDiv(DurationNew(0, 1), 2), DurationNew(0, 0))
	assertEq(t, DurationDiv(DurationNew(1, 1), 3), DurationNew(0, 333_333_333))
	assertEq(t, DurationDiv(DurationNew(1, 1), 7), DurationNew(0, 142_857_143))
	assertEq(t, DurationDiv(DurationNew(99, 999_999_000), 100), DurationNew(0, 999_999_990))
}

func TestDuration_div_duration_f32(t *testing.T) {
	assertEq(t, DurationDivDurationF32(DurationZero, DurationMax), 0.0)
	assertEq(t, DurationDivDurationF32(DurationMax, DurationZero), float32(math.Inf(1)))
	assertEq(t, DurationDivDurationF32(DurationMul(second, 2), second), 2.0)
	assert(t, math.IsNaN(float64(DurationDivDurationF32(DurationZero, DurationZero))))
	// These tests demonstrate it doesn't panic with extreme values.
	// Accuracy of the computed value is not a huge concern, we know floats don't work well
	// at these extremes. (Rust's Duration::MAX gives more than 1e28 here; the
	// port's gives more than 1e18.)
	assert(t, DurationDivDurationF32(DurationMax, nanosecond) > float32(math.Pow(10, 18)))
	assert(t, DurationDivDurationF32(nanosecond, DurationMax) < 0.1)
}

func TestDuration_div_duration_f64(t *testing.T) {
	assertEq(t, DurationDivDurationF64(DurationZero, DurationMax), 0.0)
	assertEq(t, DurationDivDurationF64(DurationMax, DurationZero), math.Inf(1))
	assertEq(t, DurationDivDurationF64(DurationMul(second, 2), second), 2.0)
	assert(t, math.IsNaN(DurationDivDurationF64(DurationZero, DurationZero)))
	// See div_duration_f32 for the bound.
	assert(t, DurationDivDurationF64(DurationMax, nanosecond) > math.Pow(10, 18))
	assert(t, DurationDivDurationF64(nanosecond, DurationMax) < 0.1)
}

func TestDuration_checked_div(t *testing.T) {
	assertEq(t, DurationCheckedDiv(DurationNew(2, 0), 2), some(DurationNew(1, 0)))
	assertEq(t, DurationCheckedDiv(DurationNew(1, 0), 2), some(DurationNew(0, 500_000_000)))
	assertEq(t, DurationCheckedDiv(DurationNew(2, 0), 0), none())
}

func TestDuration_correct_sum(t *testing.T) {
	durations := []time.Duration{
		DurationNew(1, 999_999_999),
		DurationNew(2, 999_999_999),
		DurationNew(0, 999_999_999),
		DurationNew(0, 999_999_999),
		DurationNew(0, 999_999_999),
		DurationNew(5, 0),
	}
	sum := DurationSum(durations)
	assertEq(t, sum, DurationNew(1+2+5+4, 1_000_000_000-5))
}

func TestDuration_debug_formatting_secs(t *testing.T) {
	assertEq(t, DurationDebug(DurationNew(7, 0)), "7s")
	assertEq(t, DurationDebug(DurationNew(7, 100_000_000)), "7.1s")
	assertEq(t, DurationDebug(DurationNew(7, 10_000)), "7.00001s")
	assertEq(t, DurationDebug(DurationNew(7, 1)), "7.000000001s")
	assertEq(t, DurationDebug(DurationNew(7, 123_456_789)), "7.123456789s")

	assertEq(t, DurationDebug(DurationNew(88, 0)), "88s")
	assertEq(t, DurationDebug(DurationNew(88, 100_000_000)), "88.1s")
	assertEq(t, DurationDebug(DurationNew(88, 10_000)), "88.00001s")
	assertEq(t, DurationDebug(DurationNew(88, 1)), "88.000000001s")
	assertEq(t, DurationDebug(DurationNew(88, 123_456_789)), "88.123456789s")

	assertEq(t, DurationDebug(DurationNew(999, 0)), "999s")
	assertEq(t, DurationDebug(DurationNew(999, 100_000_000)), "999.1s")
	assertEq(t, DurationDebug(DurationNew(999, 10_000)), "999.00001s")
	assertEq(t, DurationDebug(DurationNew(999, 1)), "999.000000001s")
	assertEq(t, DurationDebug(DurationNew(999, 123_456_789)), "999.123456789s")
}

func TestDuration_debug_formatting_millis(t *testing.T) {
	assertEq(t, DurationDebug(DurationNew(0, 7_000_000)), "7ms")
	assertEq(t, DurationDebug(DurationNew(0, 7_100_000)), "7.1ms")
	assertEq(t, DurationDebug(DurationNew(0, 7_000_001)), "7.000001ms")
	assertEq(t, DurationDebug(DurationNew(0, 7_123_456)), "7.123456ms")

	assertEq(t, DurationDebug(DurationNew(0, 88_000_000)), "88ms")
	assertEq(t, DurationDebug(DurationNew(0, 88_100_000)), "88.1ms")
	assertEq(t, DurationDebug(DurationNew(0, 88_000_001)), "88.000001ms")
	assertEq(t, DurationDebug(DurationNew(0, 88_123_456)), "88.123456ms")

	assertEq(t, DurationDebug(DurationNew(0, 999_000_000)), "999ms")
	assertEq(t, DurationDebug(DurationNew(0, 999_100_000)), "999.1ms")
	assertEq(t, DurationDebug(DurationNew(0, 999_000_001)), "999.000001ms")
	assertEq(t, DurationDebug(DurationNew(0, 999_123_456)), "999.123456ms")
}

func TestDuration_debug_formatting_micros(t *testing.T) {
	assertEq(t, DurationDebug(DurationNew(0, 7_000)), "7µs")
	assertEq(t, DurationDebug(DurationNew(0, 7_100)), "7.1µs")
	assertEq(t, DurationDebug(DurationNew(0, 7_001)), "7.001µs")
	assertEq(t, DurationDebug(DurationNew(0, 7_123)), "7.123µs")

	assertEq(t, DurationDebug(DurationNew(0, 88_000)), "88µs")
	assertEq(t, DurationDebug(DurationNew(0, 88_100)), "88.1µs")
	assertEq(t, DurationDebug(DurationNew(0, 88_001)), "88.001µs")
	assertEq(t, DurationDebug(DurationNew(0, 88_123)), "88.123µs")

	assertEq(t, DurationDebug(DurationNew(0, 999_000)), "999µs")
	assertEq(t, DurationDebug(DurationNew(0, 999_100)), "999.1µs")
	assertEq(t, DurationDebug(DurationNew(0, 999_001)), "999.001µs")
	assertEq(t, DurationDebug(DurationNew(0, 999_123)), "999.123µs")
}

func TestDuration_debug_formatting_nanos(t *testing.T) {
	assertEq(t, DurationDebug(DurationNew(0, 0)), "0ns")
	assertEq(t, DurationDebug(DurationNew(0, 1)), "1ns")
	assertEq(t, DurationDebug(DurationNew(0, 88)), "88ns")
	assertEq(t, DurationDebug(DurationNew(0, 999)), "999ns")
}

func TestDuration_debug_formatting_precision_zero(t *testing.T) {
	assertEq(t, debugPrecision(0, 0, 0), "0ns")
	assertEq(t, debugPrecision(0, 0, 123), "123ns")

	assertEq(t, debugPrecision(0, 0, 1_001), "1µs")
	assertEq(t, debugPrecision(0, 0, 1_499), "1µs")
	assertEq(t, debugPrecision(0, 0, 1_500), "2µs")
	assertEq(t, debugPrecision(0, 0, 1_999), "2µs")

	assertEq(t, debugPrecision(0, 0, 1_000_001), "1ms")
	assertEq(t, debugPrecision(0, 0, 1_499_999), "1ms")
	assertEq(t, debugPrecision(0, 0, 1_500_000), "2ms")
	assertEq(t, debugPrecision(0, 0, 1_999_999), "2ms")

	assertEq(t, debugPrecision(0, 1, 1), "1s")
	assertEq(t, debugPrecision(0, 1, 499_999_999), "1s")
	assertEq(t, debugPrecision(0, 1, 500_000_000), "2s")
	assertEq(t, debugPrecision(0, 1, 999_999_999), "2s")
}

func TestDuration_debug_formatting_precision_two(t *testing.T) {
	assertEq(t, debugPrecision(2, 0, 0), "0.00ns")
	assertEq(t, debugPrecision(2, 0, 123), "123.00ns")

	assertEq(t, debugPrecision(2, 0, 1_000), "1.00µs")
	assertEq(t, debugPrecision(2, 0, 7_001), "7.00µs")
	assertEq(t, debugPrecision(2, 0, 7_100), "7.10µs")
	assertEq(t, debugPrecision(2, 0, 7_109), "7.11µs")
	assertEq(t, debugPrecision(2, 0, 7_199), "7.20µs")
	assertEq(t, debugPrecision(2, 0, 1_999), "2.00µs")

	assertEq(t, debugPrecision(2, 0, 1_000_000), "1.00ms")
	assertEq(t, debugPrecision(2, 0, 3_001_000), "3.00ms")
	assertEq(t, debugPrecision(2, 0, 3_100_000), "3.10ms")
	assertEq(t, debugPrecision(2, 0, 1_999_999), "2.00ms")

	assertEq(t, debugPrecision(2, 1, 0), "1.00s")
	assertEq(t, debugPrecision(2, 4, 1_000_000), "4.00s")
	assertEq(t, debugPrecision(2, 2, 100_000_000), "2.10s")
	assertEq(t, debugPrecision(2, 2, 104_990_000), "2.10s")
	assertEq(t, debugPrecision(2, 8, 999_999_999), "9.00s")
}

func TestDuration_debug_formatting_padding(t *testing.T) {
	left, right, center := rs.Some(alignLeft), rs.Some(alignRight), rs.Some(alignCenter)
	assertEq(t, "0ns      ", debugPadded(left, 9, -1, 0, 0))
	assertEq(t, "      0ns", debugPadded(right, 9, -1, 0, 0))
	assertEq(t, "   0ns   ", debugPadded(center, 9, -1, 0, 0))
	assertEq(t, "123ns    ", debugPadded(left, 9, 0, 0, 123))
	assertEq(t, "    123ns", debugPadded(right, 9, 0, 0, 123))
	assertEq(t, "  123ns  ", debugPadded(center, 9, 0, 0, 123))
	assertEq(t, "123.0ns  ", debugPadded(left, 9, 1, 0, 123))
	assertEq(t, "  123.0ns", debugPadded(right, 9, 1, 0, 123))
	assertEq(t, " 123.0ns ", debugPadded(center, 9, 1, 0, 123))
	assertEq(t, "7.1µs    ", debugPadded(left, 9, -1, 0, 7_100))
	assertEq(t, "    7.1µs", debugPadded(right, 9, -1, 0, 7_100))
	assertEq(t, "  7.1µs  ", debugPadded(center, 9, -1, 0, 7_100))
	assertEq(t, "999.123456ms", debugPadded(left, 9, -1, 0, 999_123_456))
	assertEq(t, "999.123456ms", debugPadded(right, 9, -1, 0, 999_123_456))
	assertEq(t, "999.123456ms", debugPadded(center, 9, -1, 0, 999_123_456))
	assertEq(t, "5s       ", debugPadded(left, 9, -1, 5, 0))
	assertEq(t, "       5s", debugPadded(right, 9, -1, 5, 0))
	assertEq(t, "   5s    ", debugPadded(center, 9, -1, 5, 0))
	assertEq(t, "5.000000000000s", debugPadded(left, 9, 12, 5, 0))
	assertEq(t, "5.000000000000s", debugPadded(right, 9, 12, 5, 0))
	assertEq(t, "5.000000000000s", debugPadded(center, 9, 12, 5, 0))

	// default alignment is left:
	assertEq(t, "5s       ", debugPadded(rs.None[rs.Alignment](), 9, -1, 5, 0))
}

func TestDuration_debug_formatting_precision_high(t *testing.T) {
	assertEq(t, debugPrecision(5, 0, 23_678), "23.67800µs")

	assertEq(t, debugPrecision(9, 1, 0), "1.000000000s")
	assertEq(t, debugPrecision(10, 4, 1_000_000), "4.0010000000s")
	assertEq(t, debugPrecision(20, 4, 1_000_000), "4.00100000000000000000s")
}

func TestDuration_debug_formatting_round_to_even(t *testing.T) {
	assertEq(t, debugPrecision(0, 1, 500_000_000), "2s")
	assertEq(t, debugPrecision(0, 2, 500_000_000), "2s")
	assertEq(t, debugPrecision(0, 0, 1_500_000), "2ms")
	assertEq(t, debugPrecision(0, 0, 2_500_000), "2ms")
	assertEq(t, debugPrecision(2, 2, 105_000_000), "2.10s")
}

func TestDuration_duration_const(t *testing.T) {
	// The Rust test checks that these are usable in a const context; the
	// port keeps its values.
	duration := DurationNew(0, 123_456_789)

	assertEq(t, DurationSubsecMillis(duration), 123)
	assertEq(t, DurationSubsecMicros(duration), 123_456)
	assertEq(t, DurationSubsecNanos(duration), 123_456_789)

	assert(t, DurationIsZero(DurationZero))

	assertEq(t, DurationAsSecs(second), 1)
	assertEq(t, DurationFromSecs(1), second)

	assertEq(t, DurationAsSecsF32(second), 1.0)
	assertEq(t, DurationAsSecsF64(second), 1.0)

	assertEq(t, DurationAsMillis(second), 1_000)
	assertEq(t, DurationFromMillis(1_000), second)

	assertEq(t, DurationAsMicros(second), 1_000_000)
	assertEq(t, DurationFromMicros(1_000_000), second)

	assertEq(t, DurationAsNanos(second), 1_000_000_000)
	assertEq(t, DurationFromNanos(1_000_000_000), second)

	// Duration::new(u64::MAX, 999_999_999) is DurationMax in the port.
	max := DurationNew(u64Max, 999_999_999)
	assertEq(t, max, DurationMax)

	assertEq(t, DurationCheckedAdd(max, second), none())
	assertEq(t, DurationCheckedSub(DurationZero, second), none())
	assertEq(t, DurationCheckedMul(second, 1), some(second))

	assertEq(t, DurationDivDurationF32(second, second), 1.0)
	assertEq(t, DurationDivDurationF64(second, second), 1.0)

	assertEq(t, DurationSaturatingAdd(max, second), max)
	assertEq(t, DurationSaturatingSub(DurationZero, second), DurationZero)
	assertEq(t, DurationSaturatingMul(max, 2), max)
}

func TestDuration_from_neg_zero(t *testing.T) {
	negativeZero32 := float32(math.Copysign(0, -1))
	negativeZero64 := math.Copysign(0, -1)
	d, err := DurationTryFromSecsF32(negativeZero32)
	assert(t, err == nil)
	assertEq(t, d, DurationZero)
	d, err = DurationTryFromSecsF64(negativeZero64)
	assert(t, err == nil)
	assertEq(t, d, DurationZero)
	assertEq(t, DurationFromSecsF32(negativeZero32), DurationZero)
	assertEq(t, DurationFromSecsF64(negativeZero64), DurationZero)
}

func TestDuration_duration_fp_mul_nan(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationMulF64(nanosecond, math.NaN())
	})
}

func TestDuration_duration_fp_mul_posinfinity(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationMulF64(nanosecond, math.Inf(1))
	})
}

func TestDuration_duration_fp_mul_neginfinity(t *testing.T) {
	shouldPanic(t, "value is negative", func() {
		_ = DurationMulF64(nanosecond, math.Inf(-1))
	})
}

func TestDuration_duration_fp_div_nan(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationDivF64(nanosecond, math.NaN())
	})
}

func TestDuration_duration_fp_div_poszero(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationDivF64(nanosecond, 0.0)
	})
}

func TestDuration_duration_fp_div_negzero(t *testing.T) {
	shouldPanic(t, "value is negative", func() {
		_ = DurationDivF64(nanosecond, math.Copysign(0, -1))
	})
}

func TestDuration_duration_fp_div_negative(t *testing.T) {
	shouldPanic(t, "value is negative", func() {
		_ = DurationDivF64(nanosecond, -math.MaxFloat64)
	})
}

// The boundary tests work on Rust's Duration::MAX, which the port cannot
// hold, so they run on whole seconds and nanoseconds.

// rustMaxSecs and rustMaxNanos are Rust's Duration::MAX.
const (
	rustMaxSecs  = u64Max
	rustMaxNanos = uint32(999_999_999)
)

// rustMaxAsNanosF64 is `Duration::MAX.as_nanos() as f64`.
var rustMaxAsNanosF64 = math.Ldexp(1, 64) * 1e9

var (
	tooLargeFactor    = rustMaxAsNanosF64
	tooLargeDivisor   = math.Nextafter(partsAsSecsF64(rustMaxSecs, rustMaxNanos)*2e9, math.Inf(1))
	smallestDivisor   = nextUp(nextUp(1 / tooLargeDivisor * 2.0))
	smallestFactor    = 1 / tooLargeFactor / 2.0
	smallestNegfactor = nextUp(nextDown(0.0) * 0.5e9)
)

func nextUp(value float64) float64 { return math.Nextafter(value, math.Inf(1)) }

func nextDown(value float64) float64 { return math.Nextafter(value, math.Inf(-1)) }

// parts is a Rust Duration as whole seconds and nanoseconds.
type parts struct {
	secs  uint64
	nanos uint32
}

func mustParts(t *testing.T, secs uint64, nanos uint32, err error) parts {
	t.Helper()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	return parts{secs: secs, nanos: nanos}
}

func TestDuration_duration_fp_boundaries(t *testing.T) {
	// DURATION_BITS is Duration::MAX.as_nanos().ilog2() + 1, which is 94.
	const durationBits = 94
	const precision = durationBits - 53 + 1

	zero := parts{}
	one := parts{nanos: 1}
	mulMax := func(factor float64) parts {
		secs, nanos, err := mulF64(rustMaxSecs, rustMaxNanos, factor)
		return mustParts(t, secs, nanos, err)
	}
	divMax := func(divisor float64) parts {
		secs, nanos, err := divF64(rustMaxSecs, rustMaxNanos, divisor)
		return mustParts(t, secs, nanos, err)
	}

	assertEq(t, mulMax(0.0), zero)
	assertEq(t, mulMax(math.Copysign(0, -1)), zero)

	assertEq(t, mulMax(smallestFactor), one)
	assertEq(t, mulMax(nextDown(smallestFactor)), zero)

	assertEq(t, divMax(tooLargeDivisor), zero)
	assertEq(t, divMax(nextDown(tooLargeDivisor)), one)

	assertEq(t, divMax(math.Inf(1)), zero)
	assertEq(t, divMax(math.Inf(-1)), zero)

	// the following assertions pair with the subsequent (`should_panic`) tests

	assertEq(t, DurationMulF64(nanosecond, smallestNegfactor), DurationZero)

	// Duration::MAX - x < Duration::from_nanos(1 << PRECISION), on parts:
	// the result is within 2^42 nanoseconds of the maximum.
	nearMax := func(p parts) bool {
		// (MAX - p) in nanoseconds; the seconds differ by at most a few
		// thousand, so this fits.
		gap := (rustMaxSecs-p.secs)*nanosPerSec + uint64(rustMaxNanos) - uint64(p.nanos)
		return gap < 1<<precision
	}
	secs, nanos, err := mulF64(0, 1, nextDown(tooLargeFactor))
	assert(t, nearMax(mustParts(t, secs, nanos, err)))
	secs, nanos, err = divF64(0, 1, smallestDivisor)
	assert(t, nearMax(mustParts(t, secs, nanos, err)))
}

func TestDuration_duration_fp_mul_negative(t *testing.T) {
	shouldPanic(t, "value is negative", func() {
		_ = DurationMulF64(nanosecond, nextDown(smallestNegfactor))
	})
}

func TestDuration_duration_fp_mul_overflow(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationMulF64(nanosecond, tooLargeFactor)
	})
}

func TestDuration_duration_fp_div_overflow(t *testing.T) {
	shouldPanic(t, "value is either too big or NaN", func() {
		_ = DurationDivF64(nanosecond, nextDown(smallestDivisor))
	})
}
