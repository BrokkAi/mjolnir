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
// Where Mjolnir's Rust code meets the end of Go's range (surveyed against
// the Rust sources of mj-* and voice-worker, 2026-11-20):
//
//   - mj-worker/src/work_facts.rs passes Duration::MAX as "no timeout" to
//     mj_core::subprocess::run_bounded_observed, which hands it to
//     tokio::time::timeout. Tokio takes Instant::checked_add's None as "the
//     far future"; the port's DurationMax gives the same None from
//     Instant.CheckedAdd, so go/crates/tokio must follow that path too.
//   - Durations built from numbers other programs send saturate at
//     DurationMax where Rust holds them exactly, so the addition that follows
//     panics (or, checked, gives None) in Go where Rust's succeeds, for any
//     value of 2^63 nanoseconds (about 292 years) or more:
//     mj-accounts/src/muse_usage.rs (a Retry-After in seconds, added to
//     SystemTime::now()); mj-accounts/src/profile_health.rs (a provider's
//     reset time, UNIX_EPOCH + Duration::from_secs or from_millis);
//     mj-targets/src/aws_assume_role.rs (an AWS expiration in milliseconds);
//     mj-import/src/import/codex.rs (a session's updated_at from Codex's
//     index); mj-session-runtime/src/github_app.rs (a token expiry, through
//     checked_add, so an error instead of a panic); mj-cli/src/api_client.rs
//     (`--timeout-secs` plus WAIT_SLACK, which Rust sends on and the server
//     refuses). Each is a time after 2262-04-11, or a delay of 292 years.
//   - The other conversions of outside numbers (mj-core/src/retry_after.rs,
//     clamped to six hours by mj_accounts::quota::rate_limit_hold;
//     mj-daemon/src/pollers/quota.rs and mj-tui/src/notify.rs, which only
//     sleep, and sleep "for ever" in both; boot times read from /proc/stat)
//     behave alike in both.
//
// Rust durations are never negative. A negative time.Duration reaching this
// package is a porting error (it comes from Go's own subtraction), and the
// functions panic on it rather than return a wrong value.
//
// The functions ending in Parts ([DurationTryFromSecsF64Parts],
// [DurationTryFromSecsF32Parts]) work on whole seconds and nanoseconds and
// cover Rust's full range.
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
// The Debug output of a Duration ("1.5s", "250ms") and of a SystemTime
// (`SystemTime { tv_sec: .., tv_nsec: .. }`) is the formatter's: rs.Format
// prints a time.Duration as a Rust Duration and a time.Time as a Rust
// SystemTime wherever it meets one, under any width, precision, fill,
// alignment or sign (`rs.Format("{:>9.2?}", d)`), with Rust's algorithms
// (go/rs/fmt_time.go).
// [DurationDebug], [DurationFmtDebug], [SystemTimeDebug] and
// [SystemTimeFmtDebug] are those, under Rust's names; there is no second
// implementation here.
//
// # Not covered
//
// Duration::from_nanos_u128 (rs has no U128 yet; as_millis, as_micros and
// as_nanos return uint64, which always holds a Go duration's count), the
// unstable from_days, from_weeks, div_duration_floor and div_duration_ceil,
// SystemTime::MIN and MAX and the saturating SystemTime methods (unstable),
// and thread::sleep_until.
package rstime
