// Port of chrono 0.4.44 src/offset/utc.rs

package chrono

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Utc is the UTC time zone. This is the most efficient time zone when you
// don't need the local time. It is also used as an offset (which is also a
// dummy type).
//
// Rust writes the unit value as `Utc`; Go writes `chrono.Utc{}`.
type Utc struct{}

// UtcNow is `Utc::now()`: a DateTime which corresponds to the current date
// and time in UTC.
//
// The precision is that of the system clock as Go's time.Now reads it:
// nanoseconds on Linux and macOS, as with Rust. On Windows Rust reads the
// precise system time (100 ns steps); Go's clock there advances with the
// system timer tick.
func UtcNow() DateTime {
	now := time.Now()
	secs := now.Unix()
	if secs < 0 {
		before := time.Unix(0, 0).Sub(now)
		rs.PanicAt(1, "system time before Unix epoch: SystemTimeError("+rs.DebugString(before)+")")
	}
	return DateTimeFromTimestamp(secs, uint32(now.Nanosecond())).Unwrap()
}

// FromOffset is `TimeZone::from_offset`.
func (Utc) FromOffset(_ Offset) TimeZone { return Utc{} }

// OffsetFromLocalDate is `TimeZone::offset_from_local_date`.
func (Utc) OffsetFromLocalDate(_ NaiveDate) MappedLocalTime[Offset] {
	return &MappedLocalTime_Single[Offset]{F0: Utc{}}
}

// OffsetFromLocalDatetime is `TimeZone::offset_from_local_datetime`.
func (Utc) OffsetFromLocalDatetime(_ NaiveDateTime) MappedLocalTime[Offset] {
	return &MappedLocalTime_Single[Offset]{F0: Utc{}}
}

// OffsetFromUtcDate is `TimeZone::offset_from_utc_date`.
func (Utc) OffsetFromUtcDate(_ NaiveDate) Offset { return Utc{} }

// OffsetFromUtcDatetime is `TimeZone::offset_from_utc_datetime`.
func (Utc) OffsetFromUtcDatetime(_ NaiveDateTime) Offset { return Utc{} }

// WithYmdAndHms is `TimeZone::with_ymd_and_hms`.
func (tz Utc) WithYmdAndHms(year int32, month, day, hour, min, sec uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultWithYmdAndHms(tz, year, month, day, hour, min, sec)
}

// TimestampOpt is `TimeZone::timestamp_opt`.
func (tz Utc) TimestampOpt(secs int64, nsecs uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampOpt(tz, secs, nsecs)
}

// TimestampMillisOpt is `TimeZone::timestamp_millis_opt`.
func (tz Utc) TimestampMillisOpt(millis int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMillisOpt(tz, millis)
}

// TimestampNanos is `TimeZone::timestamp_nanos`.
func (tz Utc) TimestampNanos(nanos int64) DateTime { return TimeZoneDefaultTimestampNanos(tz, nanos) }

// TimestampMicros is `TimeZone::timestamp_micros`.
func (tz Utc) TimestampMicros(micros int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMicros(tz, micros)
}

// FromLocalDatetime is `TimeZone::from_local_datetime`.
func (tz Utc) FromLocalDatetime(local NaiveDateTime) MappedLocalTime[DateTime] {
	return TimeZoneDefaultFromLocalDatetime(tz, local)
}

// FromUtcDatetime is `TimeZone::from_utc_datetime`.
func (tz Utc) FromUtcDatetime(utc NaiveDateTime) DateTime {
	return TimeZoneDefaultFromUtcDatetime(tz, utc)
}

// Fix is `Offset::fix`: the zero offset.
func (Utc) Fix() FixedOffset {
	return FixedOffsetEastOpt(0).Unwrap()
}

// DebugString is the Debug form: "Z".
func (Utc) DebugString() string { return "Z" }

// String is the Display form: "UTC".
func (Utc) String() string { return "UTC" }

// FmtDebug is `impl fmt::Debug for Utc`.
func (u Utc) FmtDebug(f *rs.Formatter) error { return f.WriteStr(u.DebugString()) }

// FmtDisplay is `impl fmt::Display for Utc`.
func (u Utc) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(u.String()) }
