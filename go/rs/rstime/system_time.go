// Port of std 1.99.1 src/time.rs (SystemTime) and src/sys/time/{unix,windows}.rs

package rstime

import (
	"math"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// UnixEpoch is std::time::UNIX_EPOCH: 1970-01-01 00:00:00 UTC.
var UnixEpoch = time.Unix(0, 0).UTC()

// SystemTimeUnixEpoch is SystemTime::UNIX_EPOCH.
var SystemTimeUnixEpoch = UnixEpoch

const (
	// intervalNanos is the resolution of a Windows SystemTime.
	intervalNanos = 100
	// intervalsPerSec is INTERVALS_PER_SEC.
	intervalsPerSec = nanosPerSec / intervalNanos
	// windowsEpochSecs is the seconds from 1601-01-01, where Windows counts
	// from, to the Unix epoch.
	windowsEpochSecs = 11_644_473_600
)

// The seconds since the Unix epoch that time.Time can hold: it counts from
// year 1 in an int64.
const (
	unixToInternal = (1969*365 + 1969/4 - 1969/100 + 1969/400) * 86400
	minUnixSecs    = math.MinInt64 + unixToInternal
	maxUnixSecs    = math.MaxInt64 - unixToInternal
)

// systemTime is a SystemTime as Rust's Unix Timespec holds it: whole seconds
// since the Unix epoch (negative before it) and the nanoseconds past them.
type systemTime struct {
	secs  int64
	nanos uint32
}

func systemTimeOf(t time.Time) systemTime {
	st := systemTime{secs: t.Unix(), nanos: uint32(t.Nanosecond())}
	if rs.CfgWindows {
		st.nanos -= st.nanos % intervalNanos
	}
	return st
}

func (st systemTime) time() time.Time {
	return time.Unix(st.secs, int64(st.nanos)).UTC()
}

func (st systemTime) cmp(other systemTime) int {
	switch {
	case st.secs != other.secs:
		if st.secs < other.secs {
			return -1
		}
		return 1
	case st.nanos != other.nanos:
		if st.nanos < other.nanos {
			return -1
		}
		return 1
	default:
		return 0
	}
}

// since is Timespec::sub_timespec for st >= earlier: the time between them,
// saturated at DurationMax.
func (st systemTime) since(earlier systemTime) time.Duration {
	var secs uint64
	var nanos uint32
	if st.nanos >= earlier.nanos {
		secs = uint64(st.secs) - uint64(earlier.secs)
		nanos = st.nanos - earlier.nanos
	} else {
		secs = uint64(st.secs-1) - uint64(earlier.secs)
		nanos = st.nanos + nanosPerSec - earlier.nanos
	}
	return fromParts(secs, nanos)
}

// SystemTimeCanonical returns t in the form this package returns every
// SystemTime in: UTC, with no monotonic reading and, on Windows, cut to
// 100ns. Two canonical values for the same moment are ==.
func SystemTimeCanonical(t time.Time) time.Time {
	return systemTimeOf(t).time()
}

// SystemTimeNow is SystemTime::now.
func SystemTimeNow() time.Time {
	return SystemTimeCanonical(time.Now())
}

// SystemTimeError is std::time::SystemTimeError: what
// SystemTimeDurationSince returns when the second time is later than the
// first.
type SystemTimeError struct {
	duration time.Duration
}

// Duration is SystemTimeError::duration: how far the second time is past the
// first.
func (e *SystemTimeError) Duration() time.Duration { return e.duration }

// String is the error's Display text.
func (e *SystemTimeError) String() string {
	return "second time provided was later than self"
}

// Error makes the value a Go error with its Display text.
func (e *SystemTimeError) Error() string { return e.String() }

// FmtDisplay is the Display impl. It writes the text as it is: Rust's impl
// uses write!, which ignores the width.
func (e *SystemTimeError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.String())
}

// FmtDebug is the derived Debug impl: `SystemTimeError(1.5s)`.
func (e *SystemTimeError) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("SystemTimeError").Field(e.duration).Finish()
}

// SystemTimeDurationSince is `t.duration_since(earlier)`: the time from
// earlier to t. When earlier is the later of the two the error is a
// *SystemTimeError holding the distance the other way.
func SystemTimeDurationSince(t, earlier time.Time) (time.Duration, error) {
	a, b := systemTimeOf(t), systemTimeOf(earlier)
	if a.cmp(b) >= 0 {
		return a.since(b), nil
	}
	return 0, &SystemTimeError{duration: b.since(a)}
}

// SystemTimeElapsed is `t.elapsed()`: the time from t to now, with
// SystemTimeDurationSince's error when t is in the future.
func SystemTimeElapsed(t time.Time) (time.Duration, error) {
	return SystemTimeDurationSince(SystemTimeNow(), t)
}

// SystemTimeCheckedAdd is `t.checked_add(d)`: the time d after t, or None
// when it is out of range.
func SystemTimeCheckedAdd(t time.Time, d time.Duration) rs.Option[time.Time] {
	check(d)
	if d == DurationMax {
		return rs.None[time.Time]()
	}
	st := systemTimeOf(t)
	addSecs, addNanos := durationOnClock(d)
	// Nano calculations can't overflow because nanos are <1B which fit
	// in a u32.
	secs := st.secs + addSecs
	nanos := st.nanos + addNanos
	if nanos >= nanosPerSec {
		nanos -= nanosPerSec
		secs++
	}
	if secs < st.secs || secs > maxUnixSecs {
		return rs.None[time.Time]()
	}
	// A Windows time is a count of intervals in an i64.
	if rs.CfgWindows && secs > math.MaxInt64/intervalsPerSec-windowsEpochSecs-1 {
		return rs.None[time.Time]()
	}
	return rs.Some(systemTime{secs: secs, nanos: nanos}.time())
}

// SystemTimeCheckedSub is `t.checked_sub(d)`: the time d before t, or None
// when it is out of range.
func SystemTimeCheckedSub(t time.Time, d time.Duration) rs.Option[time.Time] {
	check(d)
	if d == DurationMax {
		return rs.None[time.Time]()
	}
	st := systemTimeOf(t)
	subSecs, subNanos := durationOnClock(d)
	secs := st.secs - subSecs
	nanos := int32(st.nanos) - int32(subNanos)
	if nanos < 0 {
		nanos += nanosPerSec
		secs--
	}
	if secs > st.secs || secs < minUnixSecs {
		return rs.None[time.Time]()
	}
	// Windows does not support times before 1601.
	if rs.CfgWindows && secs < -windowsEpochSecs {
		return rs.None[time.Time]()
	}
	return rs.Some(systemTime{secs: secs, nanos: uint32(nanos)}.time())
}

// durationOnClock splits d into whole seconds and nanoseconds as the system
// clock counts them: on Windows, cut to 100ns intervals.
func durationOnClock(d time.Duration) (secs int64, nanos uint32) {
	secs, nanos = int64(d/nanosPerSec), uint32(d%nanosPerSec)
	if rs.CfgWindows {
		nanos -= nanos % intervalNanos
	}
	return secs, nanos
}

// SystemTimeAdd is `t + d`. It panics when the time is out of range.
func SystemTimeAdd(t time.Time, d time.Duration) time.Time {
	check(d)
	sum, ok := SystemTimeCheckedAdd(t, d).Get()
	if !ok {
		rs.PanicAt(1, "overflow when adding duration to `SystemTime`")
	}
	return sum
}

// SystemTimeSub is `t - d`. It panics when the time is out of range.
func SystemTimeSub(t time.Time, d time.Duration) time.Time {
	check(d)
	difference, ok := SystemTimeCheckedSub(t, d).Get()
	if !ok {
		rs.PanicAt(1, "overflow when subtracting duration from `SystemTime`")
	}
	return difference
}

// SystemTimeCmp is `t.cmp(&other)` on two SystemTimes: negative, zero or
// positive.
func SystemTimeCmp(t, other time.Time) int {
	return systemTimeOf(t).cmp(systemTimeOf(other))
}

// SystemTimeDebug is `format!("{t:?}")` for a SystemTime, which prints the
// platform's representation: `SystemTime { tv_sec: 5, tv_nsec: 6 }` on Unix
// and `SystemTime { intervals: 116444736050000000 }` on Windows. The
// formatter (go/rs/fmt_time.go) holds that algorithm and prints every
// time.Time this way.
func SystemTimeDebug(t time.Time) string {
	return rs.DebugString(SystemTimeCanonical(t))
}

// SystemTimeFmtDebug is `fmt::Debug::fmt(&t, f)` for a SystemTime, which
// follows f's flags (the `#` of `{:#?}` among them).
func SystemTimeFmtDebug(t time.Time, f *rs.Formatter) error {
	return rs.DebugFmt(SystemTimeCanonical(t), f)
}
