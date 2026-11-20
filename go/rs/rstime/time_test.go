// Port of std 1.99.1 tests/time.rs

package rstime

import (
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// assertAlmostEqInstant is the Rust test's assert_almost_eq! for instants:
// equal, or within a microsecond.
func assertAlmostEqInstant(t *testing.T, a, b Instant) {
	t.Helper()
	if a != b {
		if !a.After(b) {
			a, b = b, a
		}
		assert(t, a.Sub(DurationFromMicros(1)).Cmp(b) <= 0, "instants are not almost equal", a, b)
	}
}

// assertAlmostEqSystemTime is assert_almost_eq! for system times.
func assertAlmostEqSystemTime(t *testing.T, a, b time.Time) {
	t.Helper()
	if !a.Equal(b) {
		if !a.After(b) {
			a, b = b, a
		}
		assert(t, SystemTimeCmp(SystemTimeSub(a, DurationFromMicros(1)), b) <= 0, "times are not almost equal", a, b)
	}
}

// assertAlmostEqDuration is assert_almost_eq! for durations.
func assertAlmostEqDuration(t *testing.T, a, b time.Duration) {
	t.Helper()
	if a != b {
		if a < b {
			a, b = b, a
		}
		assert(t, DurationSub(a, DurationFromMicros(1)) <= b, "durations are not almost equal", a, b)
	}
}

func TestTime_instant_monotonic(t *testing.T) {
	a := InstantNow()
	for {
		b := InstantNow()
		assert(t, b.Cmp(a) >= 0)
		if b.After(a) {
			break
		}
	}
}

func TestTime_instant_monotonic_concurrent(t *testing.T) {
	failures := make(chan string, 8)
	for range 8 {
		go func() {
			old := InstantNow()
			for range 5_000_000 {
				now := InstantNow()
				if now.Before(old) {
					failures <- "an instant went backwards"
					return
				}
				old = now
			}
			failures <- ""
		}()
	}
	for range 8 {
		if failure := <-failures; failure != "" {
			t.Fatal(failure)
		}
	}
}

func TestTime_instant_elapsed(t *testing.T) {
	a := InstantNow()
	_ = a.Elapsed()
}

func TestTime_instant_math(t *testing.T) {
	a := InstantNow()
	b := InstantNow()
	dur := b.DurationSince(a)
	assertAlmostEqInstant(t, b.Sub(dur), a)
	assertAlmostEqInstant(t, a.Add(dur), b)

	assertAlmostEqInstant(t, a.Sub(second).Add(second), a)
	assertAlmostEqInstant(t, a.CheckedSub(second).Unwrap().CheckedAdd(second).Unwrap(), a)

	// checked_add_duration will not panic on overflow
	maybeT := rs.Some(InstantNow())
	maxDuration := DurationFromSecs(u64Max)
	// in case `Instant` can store `>= now + max_duration`.
	for range 2 {
		maybeT = rs.OptionAndThen(maybeT, func(instant Instant) rs.Option[Instant] {
			return instant.CheckedAdd(maxDuration)
		})
	}
	assertEq(t, maybeT, rs.None[Instant]())

	// checked_add_duration calculates the right time and will work for another year
	year := DurationFromSecs(60 * 60 * 24 * 365)
	assertEq(t, a.Add(year), a.CheckedAdd(year).Unwrap())
}

func TestTime_instant_math_is_associative(t *testing.T) {
	now := InstantNow()
	offset := DurationFromMillis(5)
	// Changing the order of instant math shouldn't change the results,
	// especially when the expression reduces to X + identity.
	assertEq(t, now.Add(offset).SubInstant(now), DurationAdd(now.SubInstant(now), offset))

	// On any platform, `Instant` should have the same resolution as `Duration` (e.g. 1 nanosecond)
	// or better. Otherwise, math will be non-associative (see #91417).
	now = InstantNow()
	providedOffset := DurationFromNanos(1)
	later := now.Add(providedOffset)
	measuredOffset := later.SubInstant(now)
	assertEq(t, measuredOffset, providedOffset)
}

func TestTime_instant_duration_since_saturates(t *testing.T) {
	a := InstantNow()
	// This also checks that computing an instant before program startup works.
	assertEq(t, a.Sub(DurationFromHours(24)).DurationSince(a), DurationZero)
}

func TestTime_instant_checked_duration_since_nopanic(t *testing.T) {
	now := InstantNow()
	earlier := now.Sub(second)
	later := now.Add(second)
	assertEq(t, earlier.CheckedDurationSince(now), none())
	assertEq(t, later.CheckedDurationSince(now), some(second))
	assertEq(t, now.CheckedDurationSince(now), some(DurationZero))
}

func TestTime_instant_saturating_duration_since_nopanic(t *testing.T) {
	a := InstantNow()
	ret := a.Sub(second).SaturatingDurationSince(a)
	assertEq(t, ret, DurationZero)
}

func TestTime_system_time_math(t *testing.T) {
	a := SystemTimeNow()
	b := SystemTimeNow()
	dur, err := SystemTimeDurationSince(b, a)
	switch {
	case err == nil && dur == DurationZero:
		assertAlmostEqSystemTime(t, a, b)
	case err == nil:
		assert(t, b.After(a))
		assertAlmostEqSystemTime(t, SystemTimeSub(b, dur), a)
		assertAlmostEqSystemTime(t, SystemTimeAdd(a, dur), b)
	default:
		dur := err.(*SystemTimeError).Duration()
		assert(t, a.After(b))
		assertAlmostEqSystemTime(t, SystemTimeAdd(b, dur), a)
		assertAlmostEqSystemTime(t, SystemTimeSub(a, dur), b)
	}

	since, err := SystemTimeDurationSince(a, SystemTimeSub(a, second))
	assert(t, err == nil)
	assertAlmostEqDuration(t, since, second)
	_, err = SystemTimeDurationSince(a, SystemTimeAdd(a, second))
	assertAlmostEqDuration(t, err.(*SystemTimeError).Duration(), second)

	assertAlmostEqSystemTime(t, SystemTimeAdd(SystemTimeSub(a, second), second), a)
	assertAlmostEqSystemTime(t, SystemTimeCheckedAdd(SystemTimeCheckedSub(a, second).Unwrap(), second).Unwrap(), a)

	oneSecondFromEpoch := SystemTimeAdd(UnixEpoch, second)
	oneSecondFromEpoch2 := SystemTimeAdd(SystemTimeAdd(UnixEpoch, DurationFromMillis(500)), DurationFromMillis(500))
	assertEq(t, oneSecondFromEpoch, oneSecondFromEpoch2)

	// checked_add_duration will not panic on overflow
	maybeT := rs.Some(SystemTimeUnixEpoch)
	maxDuration := DurationFromSecs(u64Max)
	// in case `SystemTime` can store `>= UNIX_EPOCH + max_duration`.
	for range 2 {
		maybeT = rs.OptionAndThen(maybeT, func(time_ time.Time) rs.Option[time.Time] {
			return SystemTimeCheckedAdd(time_, maxDuration)
		})
	}
	assertEq(t, maybeT, rs.None[time.Time]())

	// checked_add_duration calculates the right time and will work for another year
	year := DurationFromSecs(60 * 60 * 24 * 365)
	assertEq(t, SystemTimeAdd(a, year), SystemTimeCheckedAdd(a, year).Unwrap())
}

func TestTime_system_time_elapsed(t *testing.T) {
	a := SystemTimeNow()
	_, _ = SystemTimeElapsed(a)
}

func TestTime_since_epoch(t *testing.T) {
	ts := SystemTimeNow()
	a, err := SystemTimeDurationSince(ts, SystemTimeAdd(UnixEpoch, second))
	assert(t, err == nil)
	b, err := SystemTimeDurationSince(ts, UnixEpoch)
	assert(t, err == nil)
	assert(t, b > a)
	assertEq(t, DurationSub(b, a), second)

	thirtyYears := DurationMul(DurationMul(DurationMul(DurationMul(DurationMul(second, 60), 60), 24), 365), 30)

	// Otherwise let's assume that we're all running computers later than
	// 2000.
	assert(t, a > thirtyYears)

	// let's assume that we're all running computers earlier than 2090.
	// Should give us ~70 years to fix this!
	hundredTwentyYears := DurationMul(thirtyYears, 4)
	assert(t, a < hundredTwentyYears)
}

// big_math checks that adding or subtracting each duration one at a time
// gives the same result as doing them all at once. Rust uses
// Duration::from_secs(i64::MAX) and 50 seconds; the first is beyond the
// port's range, so this uses 200 years, which leaves the sum (and every
// instant and time reached) inside it.
func TestTime_big_math(t *testing.T) {
	durations := [2]time.Duration{DurationFromSecs(200 * 365 * 24 * 60 * 60), DurationFromSecs(50)}

	checkSystemTime := func(start rs.Option[time.Time], op func(time.Time, time.Duration) rs.Option[time.Time]) {
		t.Helper()
		if start, ok := start.Get(); ok {
			allAtOnce := op(start, DurationSum(durations[:]))
			oneAtATime := rs.Some(start)
			for _, d := range durations {
				oneAtATime = rs.OptionAndThen(oneAtATime, func(time_ time.Time) rs.Option[time.Time] { return op(time_, d) })
			}
			assertEq(t, allAtOnce, oneAtATime)
		}
	}
	checkInstant := func(start rs.Option[Instant], op func(Instant, time.Duration) rs.Option[Instant]) {
		t.Helper()
		if start, ok := start.Get(); ok {
			allAtOnce := op(start, DurationSum(durations[:]))
			oneAtATime := rs.Some(start)
			for _, d := range durations {
				oneAtATime = rs.OptionAndThen(oneAtATime, func(instant Instant) rs.Option[Instant] { return op(instant, d) })
			}
			assertEq(t, allAtOnce, oneAtATime)
		}
	}

	checkSystemTime(SystemTimeCheckedSub(SystemTimeUnixEpoch, DurationFromSecs(100)), SystemTimeCheckedAdd)
	checkSystemTime(SystemTimeCheckedAdd(SystemTimeUnixEpoch, DurationFromSecs(100)), SystemTimeCheckedSub)

	instant := InstantNow()
	checkInstant(instant.CheckedSub(DurationFromSecs(100)), Instant.CheckedAdd)
	checkInstant(instant.CheckedSub(durations[0]), Instant.CheckedAdd)
	checkInstant(instant.CheckedAdd(DurationFromSecs(100)), Instant.CheckedSub)
	checkInstant(instant.CheckedAdd(durations[0]), Instant.CheckedSub)
}

// system_time_duration_since_max_range_on_unix, system_time_max_min and
// system_time_saturating test SystemTime::MIN, SystemTime::MAX and the
// saturating methods, which are unstable and which the port does not have.
