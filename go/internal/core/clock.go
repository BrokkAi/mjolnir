// Port of mj-core/src/clock.rs.

// Small, dependency-free helpers for reading the wall clock as Unix epoch
// values. Kept free of other modules so anything can use it.

package core

import (
	"strings"
	"time"
)

// EpochSeconds is the number of seconds since the Unix epoch. Reads before
// the epoch report as 0.
func EpochSeconds() uint64 {
	seconds := time.Now().Unix()
	if seconds < 0 {
		return 0
	}
	return uint64(seconds)
}

// EpochMillis is the number of milliseconds since the Unix epoch, saturating
// at math.MaxInt64 (time.Time.UnixMilli cannot exceed it). Reads before the
// epoch report as 0.
func EpochMillis() int64 {
	millis := time.Now().UnixMilli()
	if millis < 0 {
		return 0
	}
	return millis
}

// ParseChronoRFC3339 parses an RFC 3339 timestamp as chrono's
// DateTime::parse_from_rfc3339 does: `T`, `t` or a space between date and
// time, any number of fractional digits (beyond nanoseconds ignored), a leap
// second as `:60`, and `Z`, `z` or a `±HH:MM` offset. The result keeps the
// instant; its location is the offset as given.
func ParseChronoRFC3339(text string) (time.Time, bool) {
	digits := func(at, count int) (int, bool) {
		if at+count > len(text) {
			return 0, false
		}
		value := 0
		for _, c := range []byte(text[at : at+count]) {
			if c < '0' || c > '9' {
				return 0, false
			}
			value = value*10 + int(c-'0')
		}
		return value, true
	}
	expect := func(at int, allowed string) bool {
		return at < len(text) && strings.IndexByte(allowed, text[at]) >= 0
	}
	year, ok1 := digits(0, 4)
	month, ok2 := digits(5, 2)
	day, ok3 := digits(8, 2)
	hour, ok4 := digits(11, 2)
	minute, ok5 := digits(14, 2)
	second, ok6 := digits(17, 2)
	if !ok1 || !ok2 || !ok3 || !ok4 || !ok5 || !ok6 || !expect(4, "-") || !expect(7, "-") ||
		!expect(10, "Tt ") || !expect(13, ":") || !expect(16, ":") {
		return time.Time{}, false
	}
	at := 19
	nanos := 0
	if expect(at, ".") {
		at++
		start := at
		scale := 100_000_000
		for at < len(text) && text[at] >= '0' && text[at] <= '9' {
			nanos += int(text[at]-'0') * scale
			scale /= 10
			at++
		}
		if at == start {
			return time.Time{}, false
		}
	}
	offset := 0
	switch {
	case expect(at, "Zz") && at+1 == len(text):
	case expect(at, "+-") && at+6 == len(text) && text[at+3] == ':':
		hours, okHours := digits(at+1, 2)
		minutes, okMinutes := digits(at+4, 2)
		if !okHours || !okMinutes || hours > 23 || minutes > 59 {
			return time.Time{}, false
		}
		offset = (hours*60 + minutes) * 60
		if text[at] == '-' {
			offset = -offset
		}
	default:
		return time.Time{}, false
	}
	leap := second == 60
	if leap {
		second = 59
	}
	if month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 || second > 59 {
		return time.Time{}, false
	}
	zone := time.UTC
	if offset != 0 {
		zone = time.FixedZone("", offset)
	}
	instant := time.Date(year, time.Month(month), day, hour, minute, second, nanos, zone)
	if instant.Day() != day {
		// The day does not exist in that month.
		return time.Time{}, false
	}
	if leap {
		// chrono keeps a leap second as 59 seconds and a second's worth of
		// nanoseconds; its timestamps count that second.
		instant = instant.Add(time.Second)
	}
	return instant, true
}

// FormatChronoRFC3339 writes t in UTC as chrono's `Utc::now().to_rfc3339()`
// does: as many fractional digits (none, 3, 6 or 9) as the nanoseconds need,
// and a `+00:00` offset.
func FormatChronoRFC3339(t time.Time) string {
	t = t.UTC()
	nanos := t.Nanosecond()
	layout := "2006-01-02T15:04:05"
	switch {
	case nanos == 0:
	case nanos%1_000_000 == 0:
		layout += ".000"
	case nanos%1_000 == 0:
		layout += ".000000"
	default:
		layout += ".000000000"
	}
	return t.Format(layout) + "+00:00"
}
