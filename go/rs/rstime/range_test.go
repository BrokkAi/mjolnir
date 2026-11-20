package rstime

import (
	"math"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// These tests cover what the port adds to Rust's behaviour: the end of Go's
// range, negative values, and the canonical form of a SystemTime.

func TestRange_constructors_saturate(t *testing.T) {
	assertEq(t, DurationFromSecs(u64Max), DurationMax)
	assertEq(t, DurationFromSecs(9_223_372_036), 9_223_372_036*time.Second)
	assertEq(t, DurationFromSecs(9_223_372_037), DurationMax)
	assertEq(t, DurationFromMillis(u64Max), DurationMax)
	assertEq(t, DurationFromMicros(u64Max), DurationMax)
	assertEq(t, DurationFromNanos(u64Max), DurationMax)
	assertEq(t, DurationFromNanos(math.MaxInt64), DurationMax)
	assertEq(t, DurationFromNanos(math.MaxInt64-1), DurationMax-1)
	assertEq(t, DurationNew(9_223_372_036, 854_775_807), DurationMax)
	assertEq(t, DurationNew(9_223_372_036, 854_775_808), DurationMax)
	assertEq(t, DurationFromMins(u64Max/60), DurationMax)
	assertEq(t, DurationFromHours(u64Max/3600), DurationMax)
	assertEq(t, DurationFromSecsF64(1e12), DurationMax)
	assertEq(t, DurationFromSecsF32(1e12), DurationMax)
	assertEq(t, DurationMulF64(DurationMax, 2), DurationMax)
	d, err := DurationTryFromSecsF64(1e19)
	assert(t, err == nil)
	assertEq(t, d, DurationMax)
}

func TestRange_arithmetic_overflows_at_the_ports_maximum(t *testing.T) {
	assertEq(t, DurationCheckedAdd(DurationMax, nanosecond), none())
	assertEq(t, DurationCheckedAdd(DurationMax, DurationZero), some(DurationMax))
	assertEq(t, DurationCheckedAdd(DurationMax-1, nanosecond), some(DurationMax))
	assertEq(t, DurationSaturatingAdd(DurationMax, DurationMax), DurationMax)
	assertEq(t, DurationCheckedMul(DurationMax, 1), some(DurationMax))
	assertEq(t, DurationCheckedMul(DurationMax/2+1, 2), none())
	assertEq(t, DurationCheckedMul(DurationZero, math.MaxUint32), some(DurationZero))
	assertEq(t, DurationSaturatingMul(second, math.MaxUint32), math.MaxUint32*time.Second)
	assertEq(t, panicMessage(t, func() { DurationAdd(DurationMax, nanosecond) }), "overflow when adding durations")
	assertEq(t, panicMessage(t, func() { DurationMul(DurationMax, 2) }), "overflow when multiplying duration by scalar")
	assertEq(t, panicMessage(t, func() { DurationDiv(second, 0) }), "divide by zero error when dividing duration by scalar")
	assertEq(t, panicMessage(t, func() { DurationSum([]time.Duration{DurationMax, nanosecond}) }), "overflow in iter::sum over durations")
	assertEq(t, DurationSum(nil), DurationZero)
}

func TestRange_negative_durations_are_refused(t *testing.T) {
	negative := -time.Nanosecond
	for name, f := range map[string]func(){
		"as_secs":     func() { DurationAsSecs(negative) },
		"as_millis":   func() { DurationAsMillis(negative) },
		"debug":       func() { _ = DurationDebug(negative) },
		"checked_add": func() { DurationCheckedAdd(second, negative) },
		"sub":         func() { DurationSub(negative, second) },
		"instant_add": func() { InstantNow().Add(negative) },
		"system_add":  func() { SystemTimeCheckedAdd(UnixEpoch, negative) },
		"as_secs_f64": func() { DurationAsSecsF64(negative) },
		"mul_f64":     func() { DurationMulF64(negative, 2) },
	} {
		message := panicMessage(t, f)
		assert(t, len(message) > 0 && message[:len("rstime: negative Duration")] == "rstime: negative Duration", name, message)
	}
}

func TestRange_the_maximum_overflows_an_instant_and_a_system_time(t *testing.T) {
	now := InstantNow()
	assertEq(t, now.CheckedAdd(DurationMax), rs.None[Instant]())
	assertEq(t, now.CheckedSub(DurationMax), rs.None[Instant]())
	assertEq(t, Instant{}.CheckedAdd(DurationMax), rs.None[Instant]())
	assertEq(t, panicMessage(t, func() { now.Add(DurationMax) }), "overflow when adding duration to instant")
	assertEq(t, panicMessage(t, func() { now.Sub(DurationMax) }), "overflow when subtracting duration from instant")
	assert(t, now.CheckedAdd(DurationMax-time.Hour*24*365).IsSome())
	assert(t, now.CheckedSub(DurationMax-1).IsSome() || rs.CfgWindows)

	system := SystemTimeNow()
	assertEq(t, SystemTimeCheckedAdd(system, DurationMax), rs.None[time.Time]())
	assertEq(t, SystemTimeCheckedSub(system, DurationMax), rs.None[time.Time]())
	assertEq(t, panicMessage(t, func() { SystemTimeAdd(system, DurationMax) }), "overflow when adding duration to `SystemTime`")
	assertEq(t, panicMessage(t, func() { SystemTimeSub(system, DurationMax) }), "overflow when subtracting duration from `SystemTime`")
	assert(t, SystemTimeCheckedAdd(system, DurationMax-1).IsSome())
}

func TestRange_instant_differences_saturate(t *testing.T) {
	early := Instant{nanos: math.MinInt64 + 1}
	late := Instant{nanos: math.MaxInt64}
	assertEq(t, late.DurationSince(early), DurationMax)
	assertEq(t, early.DurationSince(late), DurationZero)
	assertEq(t, early.CheckedDurationSince(late), none())
	assertEq(t, late.Cmp(early), 1)
	assertEq(t, early.Cmp(late), -1)
	assertEq(t, early.Compare(early), 0)
}

func TestRange_an_instant_is_a_comparable_value(t *testing.T) {
	now := InstantNow()
	seen := map[Instant]string{now: "now"}
	assertEq(t, seen[now.Add(second).Sub(second)], "now")
	assert(t, now == now.Add(DurationZero))
	assert(t, now.Add(nanosecond) != now)
	assert(t, now.Before(now.Add(nanosecond)) && now.Add(nanosecond).After(now))
}

func TestRange_a_system_time_is_canonical(t *testing.T) {
	now := SystemTimeNow()
	assertEq(t, now.Location(), time.UTC)
	// No monotonic reading: the text form would carry "m=".
	assert(t, now == now.Round(0))
	local := time.Unix(1_700_000_000, 123_456_700).In(time.FixedZone("x", 3600))
	canonical := SystemTimeCanonical(local)
	assertEq(t, canonical, SystemTimeAdd(UnixEpoch, DurationNew(1_700_000_000, 123_456_700)))
	assert(t, canonical.Equal(local))
	assertEq(t, SystemTimeCmp(local, canonical), 0)
	// Before the epoch, the nanoseconds still count up from the whole second.
	before := SystemTimeSub(UnixEpoch, DurationNew(1, 250_000_000))
	assertEq(t, before.Unix(), -2)
	assertEq(t, before.Nanosecond(), 750_000_000)
	if !rs.CfgWindows {
		assertEq(t, SystemTimeDebug(before), "SystemTime { tv_sec: -2, tv_nsec: 750000000 }")
		assertEq(t, SystemTimeDebug(UnixEpoch), "SystemTime { tv_sec: 0, tv_nsec: 0 }")
	}
	_, err := SystemTimeDurationSince(before, UnixEpoch)
	assertEq(t, err.(*SystemTimeError).Duration(), DurationNew(1, 250_000_000))
	assertEq(t, err.Error(), "second time provided was later than self")
}

func TestRange_a_system_time_far_from_another_saturates(t *testing.T) {
	early := time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)
	late := time.Date(9000, 1, 1, 0, 0, 0, 0, time.UTC)
	d, err := SystemTimeDurationSince(late, early)
	assert(t, err == nil)
	assertEq(t, d, DurationMax)
	_, err = SystemTimeDurationSince(early, late)
	assertEq(t, err.(*SystemTimeError).Duration(), DurationMax)
}

func TestRange_floor_parts(t *testing.T) {
	for _, c := range []struct {
		total int64
		secs  int64
		nanos uint32
	}{
		{0, 0, 0}, {1, 0, 1}, {999_999_999, 0, 999_999_999}, {1_000_000_000, 1, 0},
		{-1, -1, 999_999_999}, {-1_000_000_000, -1, 0}, {-1_000_000_001, -2, 999_999_999},
	} {
		secs, nanos := floorParts(c.total)
		assertEq(t, secs, c.secs)
		assertEq(t, nanos, c.nanos)
	}
}

func TestRange_panics_name_the_callers_line(t *testing.T) {
	defer func() {
		info := rs.PanicInfoOf(recover())
		assert(t, len(info.File) > len("range_test.go") && info.File[len(info.File)-len("range_test.go"):] == "range_test.go", info.File)
	}()
	DurationSub(DurationZero, second)
}
