// Port of std 1.99.1 src/time.rs (Instant) and src/sys/time/{unix,windows}.rs

package rstime

import (
	"math"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// monotonicBase is the moment Instants count from. time.Now carries a
// monotonic reading, and time.Since measures against it.
var monotonicBase = time.Now()

// Instant is std::time::Instant: a reading of the monotonic clock. It is
// opaque and only useful with other Instants and Durations.
//
// An Instant is a small value. == compares two of them, and it can be a map
// key. The zero value is the moment this process started.
type Instant struct {
	// nanos is the reading: nanoseconds since monotonicBase. It may be
	// negative, for an instant before the process started.
	nanos int64
}

// windowsEpochOffset is what Rust adds to a Windows performance-counter
// reading (u64::MAX / 4 nanoseconds), which fixes how far before now a
// Windows Instant can be.
const windowsEpochOffset = uint64(math.MaxUint64 / 4)

// InstantNow is Instant::now.
func InstantNow() Instant {
	return Instant{nanos: int64(time.Since(monotonicBase))}
}

// DurationSince is Instant::duration_since: the time from earlier to i, or
// zero when earlier is the later of the two.
func (i Instant) DurationSince(earlier Instant) time.Duration {
	return i.CheckedDurationSince(earlier).UnwrapOr(0)
}

// CheckedDurationSince is Instant::checked_duration_since: the time from
// earlier to i, or None when earlier is the later of the two.
func (i Instant) CheckedDurationSince(earlier Instant) rs.Option[time.Duration] {
	if earlier.nanos > i.nanos {
		// On windows there's a threshold below which we consider two timestamps
		// equivalent due to measurement error.
		if rs.CfgWindows && subSaturating(earlier.nanos, i.nanos) <= instantEpsilon() {
			return rs.Some(time.Duration(0))
		}
		return rs.None[time.Duration]()
	}
	return rs.Some(subSaturating(i.nanos, earlier.nanos))
}

// subSaturating is a - b for a >= b, saturating at DurationMax.
func subSaturating(a, b int64) time.Duration {
	difference := a - b
	if difference < 0 {
		// The true difference is positive and past the end of int64.
		return DurationMax
	}
	return time.Duration(difference)
}

// SaturatingDurationSince is Instant::saturating_duration_since.
func (i Instant) SaturatingDurationSince(earlier Instant) time.Duration {
	return i.CheckedDurationSince(earlier).UnwrapOr(0)
}

// Elapsed is Instant::elapsed: the time since i.
func (i Instant) Elapsed() time.Duration {
	return InstantNow().DurationSince(i)
}

// CheckedAdd is Instant::checked_add: the instant d after i, or None when it
// is out of range.
func (i Instant) CheckedAdd(d time.Duration) rs.Option[Instant] {
	check(d)
	if d == DurationMax || i.nanos > math.MaxInt64-int64(d) {
		return rs.None[Instant]()
	}
	return rs.Some(Instant{nanos: i.nanos + int64(d)})
}

// CheckedSub is Instant::checked_sub: the instant d before i, or None when it
// is out of range.
func (i Instant) CheckedSub(d time.Duration) rs.Option[Instant] {
	check(d)
	if d == DurationMax || i.nanos < math.MinInt64+int64(d) {
		return rs.None[Instant]()
	}
	if rs.CfgWindows {
		// A Windows Instant is a Duration since a fixed epoch, and cannot
		// be earlier than it.
		reading := windowsReading(i)
		if uint64(d) > reading {
			return rs.None[Instant]()
		}
	}
	return rs.Some(Instant{nanos: i.nanos - int64(d)})
}

// windowsReading is the Duration, in nanoseconds, that Rust holds for this
// instant on Windows: the performance-counter time plus a fixed offset.
func windowsReading(i Instant) uint64 {
	return windowsEpochOffset + uint64(i.nanos)
}

// Add is `i + d`. It panics when the instant is out of range.
func (i Instant) Add(d time.Duration) Instant {
	check(d)
	sum, ok := i.CheckedAdd(d).Get()
	if !ok {
		rs.PanicAt(1, "overflow when adding duration to instant")
	}
	return sum
}

// Sub is `i - d`. It panics when the instant is out of range. The difference
// of two instants, `a - b`, is SubInstant.
func (i Instant) Sub(d time.Duration) Instant {
	check(d)
	difference, ok := i.CheckedSub(d).Get()
	if !ok {
		rs.PanicAt(1, "overflow when subtracting duration from instant")
	}
	return difference
}

// SubInstant is `i - earlier` on two instants, which is DurationSince.
func (i Instant) SubInstant(earlier Instant) time.Duration {
	return i.DurationSince(earlier)
}

// Cmp is Ord::cmp: negative when i is the earlier, zero when they are the
// same instant, positive when i is the later.
func (i Instant) Cmp(other Instant) int {
	switch {
	case i.nanos < other.nanos:
		return -1
	case i.nanos > other.nanos:
		return 1
	default:
		return 0
	}
}

// Compare is Cmp under the name rs.Cmp, rs.Min and rs.Max look for.
func (i Instant) Compare(other Instant) int { return i.Cmp(other) }

// Before is `i < other`.
func (i Instant) Before(other Instant) bool { return i.nanos < other.nanos }

// After is `i > other`.
func (i Instant) After(other Instant) bool { return i.nanos > other.nanos }

// FmtDebug is the Debug impl, which prints the platform's representation:
// seconds and nanoseconds on Unix, a Duration on Windows. The numbers are
// this process's reading, not the operating system's.
func (i Instant) FmtDebug(f *rs.Formatter) error {
	if rs.CfgWindows {
		return f.DebugStruct("Instant").Field("t", time.Duration(windowsReading(i))).Finish()
	}
	secs, nanos := floorParts(i.nanos)
	return f.DebugStruct("Instant").Field("tv_sec", secs).Field("tv_nsec", nanos).Finish()
}

// floorParts splits nanoseconds, which may be negative, into whole seconds
// and the nanoseconds past them, as a timespec holds them.
func floorParts(total int64) (secs int64, nanos uint32) {
	secs = total / nanosPerSec
	remainder := total % nanosPerSec
	if remainder < 0 {
		secs--
		remainder += nanosPerSec
	}
	return secs, uint32(remainder)
}
