// Port of chrono 0.4.44 src/offset/fixed.rs

package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// FixedOffset is the time zone with fixed offset, from UTC-23:59:59 to
// UTC+23:59:59.
//
// Using the TimeZone methods on a FixedOffset value is the preferred way to
// construct DateTime<FixedOffset> instances. See FixedOffsetEastOpt and
// FixedOffsetWestOpt for examples.
//
// It is a Copy type, comparable with ==. The zero value is the offset
// +00:00.
type FixedOffset struct {
	localMinusUtc int32
}

// FixedOffsetEastOpt is `FixedOffset::east_opt`: a FixedOffset for the
// Eastern Hemisphere with given timezone difference. The negative secs means
// the Western Hemisphere.
//
// It returns None on the out-of-bound secs.
func FixedOffsetEastOpt(secs int32) rs.Option[FixedOffset] {
	if -86_400 < secs && secs < 86_400 {
		return rs.Some(FixedOffset{localMinusUtc: secs})
	}
	return rs.None[FixedOffset]()
}

// FixedOffsetWestOpt is `FixedOffset::west_opt`: a FixedOffset for the
// Western Hemisphere with given timezone difference. The negative secs means
// the Eastern Hemisphere.
//
// It returns None on the out-of-bound secs.
func FixedOffsetWestOpt(secs int32) rs.Option[FixedOffset] {
	if -86_400 < secs && secs < 86_400 {
		return rs.Some(FixedOffset{localMinusUtc: -secs})
	}
	return rs.None[FixedOffset]()
}

// LocalMinusUtc is `FixedOffset::local_minus_utc`: the number of seconds to
// add to convert from UTC to the local time.
func (o FixedOffset) LocalMinusUtc() int32 {
	return o.localMinusUtc
}

// UtcMinusLocal is `FixedOffset::utc_minus_local`: the number of seconds to
// add to convert from the local time to UTC.
func (o FixedOffset) UtcMinusLocal() int32 {
	return -o.localMinusUtc
}

// FixedOffsetFromStr is `str::parse::<FixedOffset>()`: an offset such as
// "+09:00", "-0500" or "+03 30" (a sign, two digits of hours, any number of
// colons and white space, two digits of minutes). The minus sign may be
// U+2212.
//
// The error is a ParseError.
func FixedOffsetFromStr(s string) (FixedOffset, error) {
	_, offset, err := scanTimezoneOffset(s, scanColonOrSpace, false, false, true)
	if err != nil {
		return FixedOffset{}, err
	}
	fixed, ok := FixedOffsetEastOpt(offset).Get()
	if !ok {
		return FixedOffset{}, errOutOfRange
	}
	return fixed, nil
}

// FromOffset is `TimeZone::from_offset`.
func (FixedOffset) FromOffset(offset Offset) TimeZone {
	return offset.(FixedOffset)
}

// OffsetFromLocalDate is `TimeZone::offset_from_local_date`.
func (o FixedOffset) OffsetFromLocalDate(_ NaiveDate) MappedLocalTime[Offset] {
	return &MappedLocalTime_Single[Offset]{F0: o}
}

// OffsetFromLocalDatetime is `TimeZone::offset_from_local_datetime`.
func (o FixedOffset) OffsetFromLocalDatetime(_ NaiveDateTime) MappedLocalTime[Offset] {
	return &MappedLocalTime_Single[Offset]{F0: o}
}

// OffsetFromUtcDate is `TimeZone::offset_from_utc_date`.
func (o FixedOffset) OffsetFromUtcDate(_ NaiveDate) Offset {
	return o
}

// OffsetFromUtcDatetime is `TimeZone::offset_from_utc_datetime`.
func (o FixedOffset) OffsetFromUtcDatetime(_ NaiveDateTime) Offset {
	return o
}

// WithYmdAndHms is `TimeZone::with_ymd_and_hms`.
func (o FixedOffset) WithYmdAndHms(year int32, month, day, hour, min, sec uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultWithYmdAndHms(o, year, month, day, hour, min, sec)
}

// TimestampOpt is `TimeZone::timestamp_opt`.
func (o FixedOffset) TimestampOpt(secs int64, nsecs uint32) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampOpt(o, secs, nsecs)
}

// TimestampMillisOpt is `TimeZone::timestamp_millis_opt`.
func (o FixedOffset) TimestampMillisOpt(millis int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMillisOpt(o, millis)
}

// TimestampNanos is `TimeZone::timestamp_nanos`.
func (o FixedOffset) TimestampNanos(nanos int64) DateTime {
	return TimeZoneDefaultTimestampNanos(o, nanos)
}

// TimestampMicros is `TimeZone::timestamp_micros`.
func (o FixedOffset) TimestampMicros(micros int64) MappedLocalTime[DateTime] {
	return TimeZoneDefaultTimestampMicros(o, micros)
}

// FromLocalDatetime is `TimeZone::from_local_datetime`.
func (o FixedOffset) FromLocalDatetime(local NaiveDateTime) MappedLocalTime[DateTime] {
	return TimeZoneDefaultFromLocalDatetime(o, local)
}

// FromUtcDatetime is `TimeZone::from_utc_datetime`.
func (o FixedOffset) FromUtcDatetime(utc NaiveDateTime) DateTime {
	return TimeZoneDefaultFromUtcDatetime(o, utc)
}

// Fix is `Offset::fix`.
func (o FixedOffset) Fix() FixedOffset {
	return o
}

// DebugString is the Debug form: "+09:00", with seconds when they are not
// zero ("-00:00:30").
func (o FixedOffset) DebugString() string {
	offset := o.localMinusUtc
	sign := byte('+')
	if offset < 0 {
		sign, offset = '-', -offset
	}
	sec := remEuclid(offset, 60)
	mins := divEuclid(offset, 60)
	min := remEuclid(mins, 60)
	hour := divEuclid(mins, 60)
	out := []byte{sign}
	out = append(out, formatInt(int64(hour), 2, false, true)...)
	out = append(out, ':')
	out = append(out, formatInt(int64(min), 2, false, true)...)
	if sec != 0 {
		out = append(out, ':')
		out = append(out, formatInt(int64(sec), 2, false, true)...)
	}
	return string(out)
}

// String is the Display form, which is the Debug form.
func (o FixedOffset) String() string { return o.DebugString() }

// FmtDebug is `impl fmt::Debug for FixedOffset`.
func (o FixedOffset) FmtDebug(f *rs.Formatter) error { return f.WriteStr(o.DebugString()) }

// FmtDisplay is `impl fmt::Display for FixedOffset`.
func (o FixedOffset) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(o.String()) }

// Equal is `==` (derived PartialEq).
func (o FixedOffset) Equal(other FixedOffset) bool { return o == other }
