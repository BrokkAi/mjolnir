// Port of coretests 1.99.1 tests/time.rs (the debug_formatting tests)

package rs

import (
	"math"
	"testing"
	"time"
)

// fmtTestDurationNew is Duration::new for the durations a time.Duration
// holds.
func fmtTestDurationNew(secs uint64, nanos uint32) time.Duration {
	return time.Duration(secs)*time.Second + time.Duration(nanos)
}

// fmtTestWideDuration is a Duration beyond what a time.Duration holds (about
// 292 years), printed by the same code.
type fmtTestWideDuration struct {
	secs  uint64
	nanos uint32
}

func (d fmtTestWideDuration) FmtDebug(f *Formatter) error {
	prefix := ""
	if f.SignPlus() {
		prefix = "+"
	}
	return fmtDebugDurationParts(d.secs, d.nanos, prefix, f)
}

func TestFmtTime_debug_formatting_extreme_values(t *testing.T) {
	durationMax := fmtTestWideDuration{secs: math.MaxUint64, nanos: 999_999_999}
	fmtExpectEq(t,
		Format("{:?}", fmtTestWideDuration{math.MaxUint64, 123_456_789}),
		"18446744073709551615.123456789s",
	)
	fmtExpectEq(t, Format("{:.0?}", durationMax), "18446744073709551616s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestWideDuration{math.MaxUint64, 500_000_000}), "18446744073709551616s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestWideDuration{math.MaxUint64, 499_999_999}), "18446744073709551615s")
	fmtExpectEq(t,
		Format("{:.3?}", fmtTestWideDuration{math.MaxUint64, 999_500_000}),
		"18446744073709551616.000s",
	)
	fmtExpectEq(t,
		Format("{:.3?}", fmtTestWideDuration{math.MaxUint64, 999_499_999}),
		"18446744073709551615.999s",
	)
	fmtExpectEq(t,
		Format("{:.8?}", fmtTestWideDuration{math.MaxUint64, 999_999_995}),
		"18446744073709551616.00000000s",
	)
	fmtExpectEq(t,
		Format("{:.8?}", fmtTestWideDuration{math.MaxUint64, 999_999_994}),
		"18446744073709551615.99999999s",
	)
	fmtExpectEq(t, Format("{:21.0?}", durationMax), "18446744073709551616s")
	fmtExpectEq(t, Format("{:22.0?}", durationMax), "18446744073709551616s ")
	fmtExpectEq(t, Format("{:24.0?}", durationMax), "18446744073709551616s   ")
}

func TestFmtTime_debug_formatting_secs(t *testing.T) {
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(7, 0)), "7s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(7, 100_000_000)), "7.1s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(7, 10_000)), "7.00001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(7, 1)), "7.000000001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(7, 123_456_789)), "7.123456789s")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(88, 0)), "88s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(88, 100_000_000)), "88.1s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(88, 10_000)), "88.00001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(88, 1)), "88.000000001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(88, 123_456_789)), "88.123456789s")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(999, 0)), "999s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(999, 100_000_000)), "999.1s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(999, 10_000)), "999.00001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(999, 1)), "999.000000001s")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(999, 123_456_789)), "999.123456789s")
}

func TestFmtTime_debug_formatting_millis(t *testing.T) {
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_000_000)), "7ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_100_000)), "7.1ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_000_001)), "7.000001ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_123_456)), "7.123456ms")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_000_000)), "88ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_100_000)), "88.1ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_000_001)), "88.000001ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_123_456)), "88.123456ms")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_000_000)), "999ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_100_000)), "999.1ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_000_001)), "999.000001ms")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_123_456)), "999.123456ms")
}

func TestFmtTime_debug_formatting_micros(t *testing.T) {
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_000)), "7µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_100)), "7.1µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_001)), "7.001µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 7_123)), "7.123µs")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_000)), "88µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_100)), "88.1µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_001)), "88.001µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88_123)), "88.123µs")

	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_000)), "999µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_100)), "999.1µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_001)), "999.001µs")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999_123)), "999.123µs")
}

func TestFmtTime_debug_formatting_nanos(t *testing.T) {
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 0)), "0ns")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 1)), "1ns")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 88)), "88ns")
	fmtExpectEq(t, Format("{:?}", fmtTestDurationNew(0, 999)), "999ns")
}

func TestFmtTime_debug_formatting_precision_zero(t *testing.T) {
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 0)), "0ns")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 123)), "123ns")

	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_001)), "1µs")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_499)), "1µs")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_500)), "2µs")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_999)), "2µs")

	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_000_001)), "1ms")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_499_999)), "1ms")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_500_000)), "2ms")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_999_999)), "2ms")

	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(1, 1)), "1s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(1, 499_999_999)), "1s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(1, 500_000_000)), "2s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(1, 999_999_999)), "2s")
}

func TestFmtTime_debug_formatting_precision_two(t *testing.T) {
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 0)), "0.00ns")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 123)), "123.00ns")

	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 1_000)), "1.00µs")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 7_001)), "7.00µs")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 7_100)), "7.10µs")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 7_109)), "7.11µs")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 7_199)), "7.20µs")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 1_999)), "2.00µs")

	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 1_000_000)), "1.00ms")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 3_001_000)), "3.00ms")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 3_100_000)), "3.10ms")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(0, 1_999_999)), "2.00ms")

	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(1, 0)), "1.00s")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(4, 1_000_000)), "4.00s")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(2, 100_000_000)), "2.10s")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(2, 104_990_000)), "2.10s")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(8, 999_999_999)), "9.00s")
}

func TestFmtTime_debug_formatting_padding(t *testing.T) {
	fmtExpectEq(t, "0ns      ", Format("{:<9?}", fmtTestDurationNew(0, 0)))
	fmtExpectEq(t, "      0ns", Format("{:>9?}", fmtTestDurationNew(0, 0)))
	fmtExpectEq(t, "   0ns   ", Format("{:^9?}", fmtTestDurationNew(0, 0)))
	fmtExpectEq(t, "123ns    ", Format("{:<9.0?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, "    123ns", Format("{:>9.0?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, "  123ns  ", Format("{:^9.0?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, "123.0ns  ", Format("{:<9.1?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, "  123.0ns", Format("{:>9.1?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, " 123.0ns ", Format("{:^9.1?}", fmtTestDurationNew(0, 123)))
	fmtExpectEq(t, "7.1µs    ", Format("{:<9?}", fmtTestDurationNew(0, 7_100)))
	fmtExpectEq(t, "    7.1µs", Format("{:>9?}", fmtTestDurationNew(0, 7_100)))
	fmtExpectEq(t, "  7.1µs  ", Format("{:^9?}", fmtTestDurationNew(0, 7_100)))
	fmtExpectEq(t, "999.123456ms", Format("{:<9?}", fmtTestDurationNew(0, 999_123_456)))
	fmtExpectEq(t, "999.123456ms", Format("{:>9?}", fmtTestDurationNew(0, 999_123_456)))
	fmtExpectEq(t, "999.123456ms", Format("{:^9?}", fmtTestDurationNew(0, 999_123_456)))
	fmtExpectEq(t, "5s       ", Format("{:<9?}", fmtTestDurationNew(5, 0)))
	fmtExpectEq(t, "       5s", Format("{:>9?}", fmtTestDurationNew(5, 0)))
	fmtExpectEq(t, "   5s    ", Format("{:^9?}", fmtTestDurationNew(5, 0)))
	fmtExpectEq(t, "5.000000000000s", Format("{:<9.12?}", fmtTestDurationNew(5, 0)))
	fmtExpectEq(t, "5.000000000000s", Format("{:>9.12?}", fmtTestDurationNew(5, 0)))
	fmtExpectEq(t, "5.000000000000s", Format("{:^9.12?}", fmtTestDurationNew(5, 0)))

	// default alignment is left:
	fmtExpectEq(t, "5s       ", Format("{:9?}", fmtTestDurationNew(5, 0)))
}

func TestFmtTime_debug_formatting_precision_high(t *testing.T) {
	fmtExpectEq(t, Format("{:.5?}", fmtTestDurationNew(0, 23_678)), "23.67800µs")

	fmtExpectEq(t, Format("{:.9?}", fmtTestDurationNew(1, 0)), "1.000000000s")
	fmtExpectEq(t, Format("{:.10?}", fmtTestDurationNew(4, 1_000_000)), "4.0010000000s")
	fmtExpectEq(t, Format("{:.20?}", fmtTestDurationNew(4, 1_000_000)), "4.00100000000000000000s")
}

func TestFmtTime_debug_formatting_round_to_even(t *testing.T) {
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(1, 500_000_000)), "2s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(2, 500_000_000)), "2s")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 1_500_000)), "2ms")
	fmtExpectEq(t, Format("{:.0?}", fmtTestDurationNew(0, 2_500_000)), "2ms")
	fmtExpectEq(t, Format("{:.2?}", fmtTestDurationNew(2, 105_000_000)), "2.10s")
}
