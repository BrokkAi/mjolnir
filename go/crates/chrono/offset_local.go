// Port of chrono 0.4.44 src/offset/local/mod.rs

package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// Local is the local timescale.
//
// Using the TimeZone methods on the Local value is the preferred way to
// construct DateTime<Local> instances. Rust writes the unit value as `Local`;
// Go writes `chrono.Local{}`.
//
// Like the crate, it finds the system's zone itself rather than through the
// language's standard library: on Unix it reads the `TZ` variable and
// `/etc/localtime` and looks at them again when a second has passed, and on
// Windows it asks the system for each year's rules. doc.go lists what differs
// from the crate.
type Local struct{}

var _ = rs.RegisterUnitStruct[Local]("Local")

// LocalNow is `Local::now()`: a DateTime<Local> which corresponds to the
// current date, time and offset from UTC.
func LocalNow() DateTime {
	return UtcNow().WithTimezone(Local{})
}

// FromOffset is `TimeZone::from_offset`.
func (Local) FromOffset(_ Offset) TimeZone {
	return Local{}
}

// OffsetFromLocalDate is `TimeZone::offset_from_local_date`.
func (tz Local) OffsetFromLocalDate(local NaiveDate) MappedLocalTime[Offset] {
	// Get the offset at local midnight.
	return tz.OffsetFromLocalDatetime(local.AndTime(NaiveTimeMin))
}

// OffsetFromLocalDatetime is `TimeZone::offset_from_local_datetime`.
func (Local) OffsetFromLocalDatetime(local NaiveDateTime) MappedLocalTime[Offset] {
	return MappedLocalTimeMap(localInnerOffsetFromLocalDatetime(local), func(offset FixedOffset) Offset {
		return offset
	})
}

// OffsetFromUtcDate is `TimeZone::offset_from_utc_date`.
func (tz Local) OffsetFromUtcDate(utc NaiveDate) Offset {
	// Get the offset at midnight.
	return tz.OffsetFromUtcDatetime(utc.AndTime(NaiveTimeMin))
}

// OffsetFromUtcDatetime is `TimeZone::offset_from_utc_datetime`.
func (Local) OffsetFromUtcDatetime(utc NaiveDateTime) Offset {
	return localInnerOffsetFromUtcDatetime(utc).Unwrap()
}

// WithYmdAndHms is `TimeZone::with_ymd_and_hms`.
func (tz Local) WithYmdAndHms(year int32, month, day, hour, min, sec uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultWithYmdAndHms(tz, year, month, day, hour, min, sec)
}

// TimestampOpt is `TimeZone::timestamp_opt`.
func (tz Local) TimestampOpt(secs int64, nsecs uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampOpt(tz, secs, nsecs)
}

// TimestampMillisOpt is `TimeZone::timestamp_millis_opt`.
func (tz Local) TimestampMillisOpt(millis int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMillisOpt(tz, millis)
}

// TimestampNanos is `TimeZone::timestamp_nanos`.
func (tz Local) TimestampNanos(nanos int64) DateTime {
	return TimeZoneDefaultTimestampNanos(tz, nanos)
}

// TimestampMicros is `TimeZone::timestamp_micros`.
func (tz Local) TimestampMicros(micros int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMicros(tz, micros)
}

// FromLocalDatetime is `TimeZone::from_local_datetime`.
func (tz Local) FromLocalDatetime(local NaiveDateTime) MappedLocalTime[DateTime] {
	return TimeZoneDefaultFromLocalDatetime(tz, local)
}

// FromUtcDatetime is `TimeZone::from_utc_datetime`.
func (tz Local) FromUtcDatetime(utc NaiveDateTime) DateTime {
	return TimeZoneDefaultFromUtcDatetime(tz, utc)
}

// DebugString is the Debug form, which is derived: "Local".
func (Local) DebugString() string { return "Local" }
