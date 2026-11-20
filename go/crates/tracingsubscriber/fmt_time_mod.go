// Port of tracing-subscriber 0.3.23 src/fmt/time/mod.rs

package tracingsubscriber

import (
	"strconv"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// FmtTimeFormatTime is tracing_subscriber::fmt::time::FormatTime: what
// writes an event's timestamp.
type FmtTimeFormatTime interface {
	// FormatTime writes the current time. An error makes the formatter
	// write "<unknown time>" instead.
	FormatTime(w *FmtFormatWriter) error
}

// FmtTimeFormatTimeFn is `impl FormatTime for fn(&mut Writer) -> fmt::Result`.
type FmtTimeFormatTimeFn func(w *FmtFormatWriter) error

// FormatTime calls the function.
func (f FmtTimeFormatTimeFn) FormatTime(w *FmtFormatWriter) error { return f(w) }

// fmtTimeNone is `impl FormatTime for ()`: nothing, the timer of
// without_time.
type fmtTimeNone struct{}

func (fmtTimeNone) FormatTime(w *FmtFormatWriter) error { return nil }

// FmtTimeSystemTime is tracing_subscriber::fmt::time::SystemTime: the
// current time in UTC as RFC 3339 with microseconds,
// "2026-11-20T11:36:12.123456Z". It is the default timer.
type FmtTimeSystemTime struct{}

// FmtTimeTime is fmt::time::time: SystemTime.
func FmtTimeTime() FmtTimeSystemTime { return FmtTimeSystemTime{} }

// FmtTimeUptime is tracing_subscriber::fmt::time::Uptime: the time since
// the timer was made, as seconds and nanoseconds.
type FmtTimeUptime struct {
	epoch rstime.Instant
}

// FmtTimeUptimeDefault is `impl Default for Uptime`: from now.
func FmtTimeUptimeDefault() FmtTimeUptime {
	return FmtTimeUptime{epoch: rstime.InstantNow()}
}

// FmtTimeUptimeFromInstant is `impl From<Instant> for Uptime`.
func FmtTimeUptimeFromInstant(epoch rstime.Instant) FmtTimeUptime {
	return FmtTimeUptime{epoch: epoch}
}

// FormatTime is `impl FormatTime for SystemTime`.
func (FmtTimeSystemTime) FormatTime(w *FmtFormatWriter) error {
	return w.WriteStr(dateTimeFromSystemTime(time.Now()).String())
}

// FormatTime is `impl FormatTime for Uptime`: `{:4}.{:09}s`.
func (u FmtTimeUptime) FormatTime(w *FmtFormatWriter) error {
	e := u.epoch.Elapsed()
	secs := strconv.FormatUint(uint64(e/time.Second), 10)
	for len(secs) < 4 {
		secs = " " + secs
	}
	nanos := strconv.FormatUint(uint64(e%time.Second), 10)
	for len(nanos) < 9 {
		nanos = "0" + nanos
	}
	return w.WriteStr(secs + "." + nanos + "s")
}
