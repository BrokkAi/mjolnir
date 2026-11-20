// Package rstime stands in for Rust's std::time (Rust 1.99.1: std/src/time.rs,
// core/src/time.rs and the platform clocks under std/src/sys/time).
//
// # Types
//
// A Rust Duration is a Go time.Duration, a Rust SystemTime is a Go time.Time,
// and a Rust Instant is an [Instant]. Because the first two are Go's own
// types, their Rust methods are functions here, named for the Rust type and
// method:
//
//	Duration::from_secs(5)        rstime.DurationFromSecs(5)
//	d.as_millis()                 rstime.DurationAsMillis(d)
//	d.checked_sub(e)              rstime.DurationCheckedSub(d, e)
//	a + b, a - b, d * 3, d / 3    rstime.DurationAdd(a, b), DurationSub, DurationMul, DurationDiv
//	Duration::ZERO, Duration::MAX rstime.DurationZero, rstime.DurationMax
//	format!("{d:?}")              rstime.DurationDebug(d)
//	SystemTime::now()             rstime.SystemTimeNow()
//	UNIX_EPOCH                    rstime.UnixEpoch
//	t.duration_since(earlier)     rstime.SystemTimeDurationSince(t, earlier)
//	t + d, t - d                  rstime.SystemTimeAdd(t, d), rstime.SystemTimeSub(t, d)
//	Instant::now()                rstime.InstantNow()
//	i.elapsed(), i + d, a - b     i.Elapsed(), i.Add(d), a.SubInstant(b)
//
// Go's own operators on time.Duration (+, -, *) do not panic when Rust's
// would; use the functions wherever the Rust code can overflow or go below
// zero. Comparisons (<, ==, min, max) are the same in both and need no
// function.
//
// # The range of a Duration
//
// A Rust Duration holds up to u64::MAX seconds; a Go time.Duration holds up
// to math.MaxInt64 nanoseconds, a little over 292 years. The port treats
// Go's range as the range:
//
//   - [DurationMax] is math.MaxInt64 nanoseconds and stands for
//     Duration::MAX.
//   - A constructor whose Rust value is larger (DurationFromSecs(u64::MAX),
//     DurationFromSecsF64(1e12)) saturates at DurationMax.
//   - Arithmetic overflows where Go's range ends: the checked functions
//     return None, the saturating ones return DurationMax, and DurationAdd
//     and DurationMul panic with Rust's text.
//   - Adding DurationMax to, or subtracting it from, an Instant or a
//     SystemTime overflows (None, or Rust's panic), as Duration::MAX does in
//     Rust.
//
// Rust durations are never negative. A negative time.Duration reaching this
// package is a porting error (it comes from Go's own subtraction), and the
// functions panic on it rather than return a wrong value.
//
// The functions ending in Parts ([DurationDebugParts], [DurationTryFromSecsF64Parts])
// work on whole seconds and nanoseconds and cover Rust's full range.
//
// # SystemTime
//
// Every time.Time this package returns is in UTC and carries no monotonic
// reading, so two values for the same moment are == as two SystemTimes are.
// A time.Time from elsewhere compares correctly with Equal, Before and
// Compare; pass it through [SystemTimeCanonical] before using == or a map
// key. On Windows a SystemTime counts 100-nanosecond intervals since 1601, as
// Rust's does there: durations are truncated to 100ns in arithmetic and times
// before 1601 do not exist.
//
// # Instant
//
// An Instant is a reading of Go's monotonic clock, in nanoseconds since this
// process started. It is a small comparable value: == works and it can key a
// map. std::time::Instant does not follow tokio's paused clock, and neither
// does this one.
//
// # Formatting
//
// Duration's Debug output is produced by [DurationDebug] and, with a width,
// precision, fill, alignment or sign, by [DurationDebugWith]. SystemTime's is
// [SystemTimeDebug]. The formatter in package rs cannot import this package
// (this package imports rs), so it needs its own copy of these two
// algorithms; [DurationDebugParts] is written to be moved there unchanged.
//
// # Not covered
//
// Duration::from_nanos_u128 (rs has no U128 yet; as_millis, as_micros and
// as_nanos return uint64, which always holds a Go duration's count), the
// unstable from_days, from_weeks, div_duration_floor and div_duration_ceil,
// SystemTime::MIN and MAX and the saturating SystemTime methods (unstable),
// and thread::sleep_until.
package rstime
