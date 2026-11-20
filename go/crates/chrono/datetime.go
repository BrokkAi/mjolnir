// Port of chrono 0.4.44 src/datetime/mod.rs

package chrono

import (
	"math"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// DateTime is the ISO 8601 combined date and time with time zone: Rust's
// `DateTime<Tz>` for every zone type.
//
// Go cannot parameterise a type by its zone the way Rust does, so one Go
// type stands for `DateTime<Utc>`, `DateTime<Local>`, `DateTime<FixedOffset>`
// and `DateTime<chrono_tz::Tz>`, and carries its zone as a value:
//
//	Rust type               built by                              Timezone()   Offset()
//	DateTime<Utc>           UtcNow, DateTimeFromTimestamp, ...    Utc{}        Utc{}
//	DateTime<Local>         LocalNow, x.WithTimezone(Local{})     Local{}      a FixedOffset
//	DateTime<FixedOffset>   DateTimeParseFromRfc3339, ...         the offset   the same offset
//	DateTime<Tz>            x.WithTimezone(tz)                    the Tz       a chronotz.TzOffset
//
// A function that Rust types as taking `DateTime<Utc>` takes a DateTime in
// Go; nothing checks the zone. Where Rust converts with `with_timezone`, the
// Go code does too, so the zone is the one the Rust code would have.
//
// As in Rust, the value holds the instant in UTC and the zone's offset at
// that instant, and:
//
//   - equality, ordering and hashing look at the instant only: use Equal and
//     Compare (or rs.Eq and rs.Cmp). Two values for the same instant in
//     different zones are equal. Go's == also compares the zones, so it is
//     not Rust's ==.
//   - the text forms depend on the zone: Display is "2014-11-28 21:00:09
//     UTC", "... +09:00" or "... JST", and Debug is "2014-11-28T21:00:09Z",
//     "...+09:00" or "...JST".
//   - arithmetic keeps the zone and recomputes the offset, so adding a day's
//     worth of seconds to a DateTime<Local> across a clock change gives the
//     new offset.
//
// It is a Copy type: methods take and return values. The zero value is the
// Unix epoch in UTC, which is `DateTime::<Utc>::default()`.
type DateTime struct {
	datetime NaiveDateTime
	// offset is the zone's offset at this instant; nil stands for Utc{}.
	offset Offset
	// tz is the zone, which Rust keeps in the type; nil stands for Utc{}.
	tz TimeZone
}

func (dt DateTime) off() Offset {
	if dt.offset == nil {
		return Utc{}
	}
	return dt.offset
}

func (dt DateTime) zone() TimeZone {
	if dt.tz == nil {
		return Utc{}
	}
	return dt.tz
}

// DateTimeFromNaiveUtcAndOffset is `DateTime::from_naive_utc_and_offset`: a
// DateTime from its components, a NaiveDateTime in UTC and an Offset.
//
// This is a low-level method, intended for use cases such as deserializing a
// DateTime or passing it through FFI. For regular use you may want
// TimeZone.FromLocalDatetime or NaiveDateTime.AndLocalTimezone instead.
//
// tz is the zone the offset belongs to, which Rust takes from the type
// parameter: Utc{} with the Utc{} offset, a FixedOffset with itself,
// Local{} with a FixedOffset, a chronotz.Tz with its TzOffset.
func DateTimeFromNaiveUtcAndOffset(datetime NaiveDateTime, offset Offset, tz TimeZone) DateTime {
	return DateTime{datetime: datetime, offset: offset, tz: tz}
}

// DateNaive is `DateTime::date_naive`: the date component, in the local
// time of the zone. Unless you are immediately planning on turning this into
// a DateTime with the same timezone you should use the DateNaive method.
//
// It panics if the local date is outside NaiveDate's range.
func (dt DateTime) DateNaive() NaiveDate {
	return dt.NaiveLocal().Date()
}

// Time is `DateTime::time`: the time component, in the local time of the
// zone.
func (dt DateTime) Time() NaiveTime {
	return dt.datetime.Time().AddFixedOffset(dt.off().Fix())
}

// Timestamp is `DateTime::timestamp`: the number of non-leap seconds since
// January 1, 1970 0:00:00 UTC (aka "UNIX timestamp").
//
// The reverse operation of creating a DateTime from a timestamp can be
// performed using DateTimeFromTimestamp or TimeZone.TimestampOpt.
func (dt DateTime) Timestamp() int64 {
	gregorianDay := int64(dt.datetime.Date().NumDaysFromCe())
	secondsFromMidnight := int64(dt.datetime.Time().NumSecondsFromMidnight())
	return (gregorianDay-unixEpochDay)*86_400 + secondsFromMidnight
}

// TimestampMillis is `DateTime::timestamp_millis`: the number of non-leap
// milliseconds since January 1, 1970 UTC.
func (dt DateTime) TimestampMillis() int64 {
	asMs := dt.Timestamp() * 1000
	return asMs + int64(dt.TimestampSubsecMillis())
}

// TimestampMicros is `DateTime::timestamp_micros`: the number of non-leap
// microseconds since January 1, 1970 UTC.
func (dt DateTime) TimestampMicros() int64 {
	asUs := dt.Timestamp() * 1_000_000
	return asUs + int64(dt.TimestampSubsecMicros())
}

// TimestampNanosOpt is `DateTime::timestamp_nanos_opt`: the number of
// non-leap nanoseconds since January 1, 1970 UTC.
//
// An i64 with nanosecond precision can span a range of ~584 years. This
// function returns None on an out of range DateTime. The dates that can be
// represented as nanoseconds are between 1677-09-21T00:12:43.145224192 and
// 2262-04-11T23:47:16.854775807.
func (dt DateTime) TimestampNanosOpt() rs.Option[int64] {
	timestamp := dt.Timestamp()
	subsecNanos := int64(dt.TimestampSubsecNanos())
	// `(timestamp * 1_000_000_000) + subsec_nanos` may create a temporary that underflows while
	// the final value can be represented as an `i64`.
	// As workaround we converting the negative case to:
	// `((timestamp + 1) * 1_000_000_000) + (ns - 1_000_000_000)``
	//
	// Also see <https://github.com/chronotope/chrono/issues/1289>.
	if timestamp < 0 {
		subsecNanos -= 1_000_000_000
		timestamp++
	}
	product, ok := checkedMulI64(timestamp, 1_000_000_000)
	if !ok {
		return rs.None[int64]()
	}
	return rs.OptionOf(checkedAddI64(product, subsecNanos))
}

// TimestampSubsecMillis is `DateTime::timestamp_subsec_millis`: the number of
// milliseconds since the last second boundary.
//
// In event of a leap second this may exceed 999.
func (dt DateTime) TimestampSubsecMillis() uint32 {
	return dt.TimestampSubsecNanos() / 1_000_000
}

// TimestampSubsecMicros is `DateTime::timestamp_subsec_micros`: the number of
// microseconds since the last second boundary.
//
// In event of a leap second this may exceed 999,999.
func (dt DateTime) TimestampSubsecMicros() uint32 {
	return dt.TimestampSubsecNanos() / 1_000
}

// TimestampSubsecNanos is `DateTime::timestamp_subsec_nanos`: the number of
// nanoseconds since the last second boundary.
//
// In event of a leap second this may exceed 999,999,999.
func (dt DateTime) TimestampSubsecNanos() uint32 {
	return dt.datetime.Time().Nanosecond()
}

// Offset is `DateTime::offset`: the zone's offset at this instant, from
// which the Display and Debug text of the zone part come.
func (dt DateTime) Offset() Offset {
	return dt.off()
}

// Timezone is `DateTime::timezone`: the zone, which is Rust's type
// parameter Tz as a value.
func (dt DateTime) Timezone() TimeZone {
	return dt.zone().FromOffset(dt.off())
}

// WithTimezone is `DateTime::with_timezone`: the same instant in another
// zone. The returned DateTime references the same instant of time from the
// perspective of the provided time zone.
func (dt DateTime) WithTimezone(tz TimeZone) DateTime {
	return tz.FromUtcDatetime(dt.datetime)
}

// FixedOffset is `DateTime::fixed_offset`: the same instant as a
// DateTime<FixedOffset> with the offset the value has now. It is the
// equivalent of `dt.WithTimezone(dt.Offset().Fix())`.
func (dt DateTime) FixedOffset() DateTime {
	return dt.WithTimezone(dt.off().Fix())
}

// ToUtc is `DateTime::to_utc`: the same instant as a DateTime<Utc>. It is
// the equivalent of `dt.WithTimezone(Utc{})`, without a zone lookup.
func (dt DateTime) ToUtc() DateTime {
	return DateTime{datetime: dt.datetime, offset: Utc{}, tz: Utc{}}
}

// CheckedAddSigned is `DateTime::checked_add_signed`: the instant plus rhs,
// in the same zone, or None if the resulting date would be out of range.
func (dt DateTime) CheckedAddSigned(rhs TimeDelta) rs.Option[DateTime] {
	datetime, ok := dt.datetime.CheckedAddSigned(rhs).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	tz := dt.Timezone()
	return rs.Some(tz.FromUtcDatetime(datetime))
}

// CheckedAddMonths is `DateTime::checked_add_months`: the local date plus
// months, at the same local time of day. The day is clamped to the last day
// of the resulting month.
//
// It returns None if the resulting date would be out of range, or if the
// local time on the resulting date does not exist or is ambiguous (a clock
// change).
func (dt DateTime) CheckedAddMonths(months Months) rs.Option[DateTime] {
	// `NaiveDate::checked_add_months` has a fast path for `Months(0)` that does not validate
	// the resulting date, with which we can return `Some` even for an out of range local
	// datetime.
	naive, ok := dt.overflowingNaiveLocal().CheckedAddMonths(months).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	return naive.AndLocalTimezone(dt.zone().FromOffset(dt.off())).Single()
}

// CheckedSubSigned is `DateTime::checked_sub_signed`: the instant minus rhs,
// in the same zone, or None if the resulting date would be out of range.
func (dt DateTime) CheckedSubSigned(rhs TimeDelta) rs.Option[DateTime] {
	datetime, ok := dt.datetime.CheckedSubSigned(rhs).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	tz := dt.Timezone()
	return rs.Some(tz.FromUtcDatetime(datetime))
}

// CheckedSubMonths is `DateTime::checked_sub_months`: the local date minus
// months, at the same local time of day. The day is clamped to the last day
// of the resulting month.
//
// It returns None if the resulting date would be out of range, or if the
// local time on the resulting date does not exist or is ambiguous (a clock
// change).
func (dt DateTime) CheckedSubMonths(months Months) rs.Option[DateTime] {
	// `NaiveDate::checked_sub_months` has a fast path for `Months(0)` that does not validate
	// the resulting date, with which we can return `Some` even for an out of range local
	// datetime.
	naive, ok := dt.overflowingNaiveLocal().CheckedSubMonths(months).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	return naive.AndLocalTimezone(dt.zone().FromOffset(dt.off())).Single()
}

// CheckedAddDays is `DateTime::checked_add_days`: the local date plus days,
// at the same local time of day.
//
// It returns None if the resulting date would be out of range, or if the
// local time on the resulting date does not exist or is ambiguous (a clock
// change).
func (dt DateTime) CheckedAddDays(days Days) rs.Option[DateTime] {
	if days == DaysNew(0) {
		return rs.Some(dt)
	}
	// `NaiveDate::add_days` has a fast path if the result remains within the same year, that
	// does not validate the resulting date. This allows us to return `Some` even for an out of
	// range local datetime when adding `Days(0)`.
	naive, ok := dt.overflowingNaiveLocal().CheckedAddDays(days).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	result, ok := dt.Timezone().FromLocalDatetime(naive).Single().Get()
	if !ok || result.Compare(DateTimeMaxUtc) > 0 {
		return rs.None[DateTime]()
	}
	return rs.Some(result)
}

// CheckedSubDays is `DateTime::checked_sub_days`: the local date minus days,
// at the same local time of day.
//
// It returns None if the resulting date would be out of range, or if the
// local time on the resulting date does not exist or is ambiguous (a clock
// change).
func (dt DateTime) CheckedSubDays(days Days) rs.Option[DateTime] {
	// `NaiveDate::add_days` has a fast path if the result remains within the same year, that
	// does not validate the resulting date. This allows us to return `Some` even for an out of
	// range local datetime when adding `Days(0)`.
	naive, ok := dt.overflowingNaiveLocal().CheckedSubDays(days).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	result, ok := dt.Timezone().FromLocalDatetime(naive).Single().Get()
	if !ok || result.Compare(DateTimeMinUtc) < 0 {
		return rs.None[DateTime]()
	}
	return rs.Some(result)
}

// SignedDurationSince is `DateTime::signed_duration_since`: the instant
// minus rhs's instant as a TimeDelta, whatever the two zones are. This does
// not overflow or underflow at all.
func (dt DateTime) SignedDurationSince(rhs DateTime) TimeDelta {
	return dt.datetime.SignedDurationSince(rhs.datetime)
}

// NaiveUtc is `DateTime::naive_utc`: a view to the naive UTC datetime.
func (dt DateTime) NaiveUtc() NaiveDateTime {
	return dt.datetime
}

// NaiveLocal is `DateTime::naive_local`: a view to the naive local datetime.
//
// It panics if the local datetime can't be converted to a NaiveDateTime
// because it is out of range, which can happen within a day of
// NaiveDateTimeMin and NaiveDateTimeMax in a zone that is not UTC.
func (dt DateTime) NaiveLocal() NaiveDateTime {
	return expect(dt.datetime.CheckedAddOffset(dt.off().Fix()), "Local time out of range for `NaiveDateTime`")
}

// overflowingNaiveLocal returns the naive local datetime.
//
// This makes use of the buffer space outside of the representable range of
// values of NaiveDateTime. The result can be used as intermediate value, but
// should never be exposed to a user.
func (dt DateTime) overflowingNaiveLocal() NaiveDateTime {
	return dt.datetime.overflowingAddOffset(dt.off().Fix())
}

// YearsSince is `DateTime::years_since`: the number of whole years from the
// given base until the value, or None if base > dt.
func (dt DateTime) YearsSince(base DateTime) rs.Option[uint32] {
	years := dt.Year() - base.Year()
	earlierTime := false
	switch {
	case dt.Month() != base.Month():
		earlierTime = dt.Month() < base.Month()
	case dt.Day() != base.Day():
		earlierTime = dt.Day() < base.Day()
	default:
		earlierTime = dt.Time().Compare(base.Time()) < 0
	}
	if earlierTime {
		years--
	}
	if years >= 0 {
		return rs.Some(uint32(years))
	}
	return rs.None[uint32]()
}

// ToRfc2822 is `DateTime::to_rfc2822`: an RFC 2822 date and time string such
// as "Tue, 1 Jul 2003 10:52:37 +0200".
//
// It panics if the date can not be represented in this format: the year must
// be in the range 0000 through 9999.
func (dt DateTime) ToRfc2822() string {
	var result strings.Builder
	if err := writeRfc2822(&result, dt.overflowingNaiveLocal(), dt.off().Fix()); err != nil {
		rs.PanicAt(1, "date cannot be represented by RFC 2822: Error")
	}
	return result.String()
}

// ToRfc3339 is `DateTime::to_rfc3339`: an RFC 3339 and ISO 8601 date and time
// string such as "1996-12-19T16:39:57-08:00".
//
// The fraction of a second is written with 0, 3, 6 or 9 digits, whichever is
// the shortest that loses nothing, and the offset is always numeric
// ("+00:00" for UTC). A year outside 0 through 9999 is written with a sign,
// which is ISO 8601 but not RFC 3339.
func (dt DateTime) ToRfc3339() string {
	// For some reason a string with a capacity less than 32 is ca 20% slower when benchmarking.
	var result strings.Builder
	naive := dt.overflowingNaiveLocal()
	offset := dt.off().Fix()
	if err := writeRfc3339(&result, naive, offset, SecondsFormat_AutoSi, false); err != nil {
		rs.PanicAt(1, "writing rfc3339 datetime to string should never fail: Error")
	}
	return result.String()
}

// ToRfc3339Opts is `DateTime::to_rfc3339_opts`: an RFC 3339 and ISO 8601 date
// and time string with subseconds formatted as per secform. If useZ is true
// and the timezone is UTC (offset 0), it uses "Z" as per
// FormatFixed_TimezoneOffsetColonZ; otherwise the offset is written as
// "+00:00".
//
// It panics if the local time is out of NaiveDateTime's range.
func (dt DateTime) ToRfc3339Opts(secform SecondsFormat, useZ bool) string {
	var result strings.Builder
	if err := writeRfc3339(&result, dt.NaiveLocal(), dt.off().Fix(), secform, useZ); err != nil {
		rs.PanicAt(1, "writing rfc3339 datetime to string should never fail: Error")
	}
	return result.String()
}

// WithTime is `DateTime::with_time`: the same local date with the time set
// to the given NaiveTime.
//
// The result says whether that local time exists once
// (*MappedLocalTime_Single), twice (*MappedLocalTime_Ambiguous) or not at
// all (*MappedLocalTime_None) in the zone.
func (dt DateTime) WithTime(time NaiveTime) MappedLocalTime[DateTime] {
	return dt.Timezone().FromLocalDatetime(dt.overflowingNaiveLocal().Date().AndTime(time))
}

// DateTimeMinUtc is `DateTime::<Utc>::MIN_UTC`: the minimum possible
// DateTime<Utc>.
var DateTimeMinUtc = DateTime{datetime: NaiveDateTimeMin, offset: Utc{}, tz: Utc{}}

// DateTimeMaxUtc is `DateTime::<Utc>::MAX_UTC`: the maximum possible
// DateTime<Utc>.
var DateTimeMaxUtc = DateTime{datetime: NaiveDateTimeMax, offset: Utc{}, tz: Utc{}}

// DateTimeFromTimestampSecs is `DateTime::from_timestamp_secs`: a
// DateTime<Utc> from the number of non-leap seconds since January 1, 1970
// 0:00:00.000 UTC (aka "UNIX timestamp").
//
// It returns None on out-of-range number of seconds, otherwise Some.
func DateTimeFromTimestampSecs(secs int64) rs.Option[DateTime] {
	return DateTimeFromTimestamp(secs, 0)
}

// DateTimeFromTimestamp is `DateTime::from_timestamp`: a DateTime<Utc> from
// the number of non-leap seconds since January 1, 1970 0:00:00 UTC (aka
// "UNIX timestamp") and the number of nanoseconds since the last whole
// non-leap second.
//
// The nanosecond part can exceed 1,000,000,000 in order to represent a leap
// second, but only when `secs % 60 == 59`. (The true "UNIX timestamp" cannot
// represent a leap second unambiguously.)
//
// It returns None on out-of-range number of seconds and/or invalid
// nanosecond, otherwise Some.
func DateTimeFromTimestamp(secs int64, nsecs uint32) rs.Option[DateTime] {
	days := divEuclid(secs, 86_400) + unixEpochDay
	secs = remEuclid(secs, 86_400)
	if days < math.MinInt32 || days > math.MaxInt32 {
		return rs.None[DateTime]()
	}
	date, ok := NaiveDateFromNumDaysFromCeOpt(int32(days)).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	time, ok := NaiveTimeFromNumSecondsFromMidnightOpt(uint32(secs), nsecs).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	return rs.Some(date.AndTime(time).AndUtc())
}

// DateTimeFromTimestampMillis is `DateTime::from_timestamp_millis`: a
// DateTime<Utc> from the number of non-leap milliseconds since January 1,
// 1970 0:00:00.000 UTC (aka "UNIX timestamp").
//
// It returns None on out-of-range number of milliseconds, otherwise Some.
func DateTimeFromTimestampMillis(millis int64) rs.Option[DateTime] {
	secs := divEuclid(millis, 1000)
	nsecs := uint32(remEuclid(millis, 1000)) * 1_000_000
	return DateTimeFromTimestamp(secs, nsecs)
}

// DateTimeFromTimestampMicros is `DateTime::from_timestamp_micros`: a
// DateTime<Utc> from the number of non-leap microseconds since January 1,
// 1970 0:00:00.000 UTC (aka "UNIX timestamp").
//
// It returns None on out-of-range number of microseconds, otherwise Some.
func DateTimeFromTimestampMicros(micros int64) rs.Option[DateTime] {
	secs := divEuclid(micros, 1_000_000)
	nsecs := uint32(remEuclid(micros, 1_000_000)) * 1000
	return DateTimeFromTimestamp(secs, nsecs)
}

// DateTimeFromTimestampNanos is `DateTime::from_timestamp_nanos`: a
// DateTime<Utc> from the number of non-leap nanoseconds since January 1,
// 1970 0:00:00.000 UTC (aka "UNIX timestamp").
//
// An i64 with nanosecond precision can span a range of ~584 years. Because
// all values can be represented as a DateTime this method never fails.
func DateTimeFromTimestampNanos(nanos int64) DateTime {
	secs := divEuclid(nanos, 1_000_000_000)
	nsecs := uint32(remEuclid(nanos, 1_000_000_000))
	return expect(DateTimeFromTimestamp(secs, nsecs), "timestamp in nanos is always in range")
}

// DateTimeUnixEpoch is `DateTime::UNIX_EPOCH`: the Unix Epoch,
// 1970-01-01 00:00:00 UTC.
var DateTimeUnixEpoch = NaiveDateFromYmdOpt(1970, 1, 1).Unwrap().AndTime(NaiveTimeMin).AndUtc()

// DateTimeDefault is `DateTime::<Utc>::default()`: the Unix epoch in UTC,
// which is the Go zero value.
func DateTimeDefault() DateTime {
	return Utc{}.FromUtcDatetime(NaiveDateTimeDefault())
}

// DateTimeLocalDefault is `DateTime::<Local>::default()`: the Unix epoch in
// the local zone.
func DateTimeLocalDefault() DateTime {
	return Local{}.FromUtcDatetime(NaiveDateTimeDefault())
}

// DateTimeFixedOffsetDefault is `DateTime::<FixedOffset>::default()`: the
// Unix epoch at the offset +00:00.
func DateTimeFixedOffsetDefault() DateTime {
	return FixedOffsetWestOpt(0).Unwrap().FromUtcDatetime(NaiveDateTimeDefault())
}

// DateTimeFixedOffsetFromDateTime is `DateTime::<FixedOffset>::from(src)`:
// a DateTime<Utc> at the offset +00:00, or a DateTime<Local> at the offset
// it has now. Go has one DateTime type, so this is the conversion for either
// source: the instant at its current fixed offset.
func DateTimeFixedOffsetFromDateTime(src DateTime) DateTime {
	return src.WithTimezone(src.off().Fix())
}

// DateTimeLocalFromDateTime is `DateTime::<Local>::from(src)`: the instant
// in the local zone.
func DateTimeLocalFromDateTime(src DateTime) DateTime {
	return src.WithTimezone(Local{})
}

// DateTimeUtcFromDateTime is `DateTime::<Utc>::from(src)`: the instant in
// UTC.
func DateTimeUtcFromDateTime(src DateTime) DateTime {
	return src.WithTimezone(Utc{})
}

// mapLocal maps the local datetime with a function.
func mapLocal(dt DateTime, f func(NaiveDateTime) rs.Option[NaiveDateTime]) rs.Option[DateTime] {
	datetime, ok := f(dt.overflowingNaiveLocal()).Get()
	if !ok {
		return rs.None[DateTime]()
	}
	result, ok := dt.Timezone().FromLocalDatetime(datetime).Single().Get()
	if !ok || result.Compare(DateTimeMinUtc) < 0 || result.Compare(DateTimeMaxUtc) > 0 {
		return rs.None[DateTime]()
	}
	return rs.Some(result)
}

// DateTimeParseFromRfc2822 is `DateTime::parse_from_rfc2822`: an RFC 2822
// date-and-time string parsed into a DateTime<FixedOffset> value.
//
// This parses valid RFC 2822 datetime strings (such as "Tue, 1 Jul 2003
// 10:52:37 +0200") and returns a new DateTime instance with the parsed
// timezone as the FixedOffset.
//
// RFC 2822 is the internet message standard that specifies the
// representation of times in HTTP and email headers. It is the 2001 revision
// of RFC 822, and is itself revised as RFC 5322 in 2008.
//
// The legacy zone names ("GMT", "EST", ...) are accepted, comments in
// parentheses are allowed at the end, and the weekday, when present, must be
// the right one.
//
// The error is a ParseError.
func DateTimeParseFromRfc2822(s string) (DateTime, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, dateTimeRfc2822Items); err != nil {
		return DateTime{}, err
	}
	return parsed.ToDatetime()
}

var dateTimeRfc2822Items = []FormatItem{&FormatItem_Fixed{F0: FormatFixed_RFC2822}}

// DateTimeParseFromRfc3339 is `DateTime::parse_from_rfc3339`: an RFC 3339
// date-and-time string parsed into a DateTime<FixedOffset> value.
//
// Parses all valid RFC 3339 values (as well as the subset of valid ISO 8601
// values that are also valid RFC 3339 date-and-time values) and returns a new
// DateTime with a FixedOffset corresponding to the parsed timezone. While
// RFC 3339 values come in a wide variety of shapes and sizes,
// "1996-12-19T16:39:57-08:00" is an example of the most commonly encountered
// variety of RFC 3339 formats.
//
// Why isn't this named `parse_from_iso8601`? That's because ISO 8601 allows
// representing values in a wide range of formats, only some of which
// represent actual date-and-time instances (rather than periods, ranges,
// dates, or times). Some valid ISO 8601 values are also simultaneously valid
// RFC 3339 values, but not all RFC 3339 values are valid ISO 8601 values (or
// the other way around).
//
// The separator between date and time may be "T", "t" or a space, "Z" and
// "z" stand for +00:00, the offset needs its colon, and second 60 is read as
// a leap second.
//
// The error is a ParseError.
func DateTimeParseFromRfc3339(s string) (DateTime, error) {
	return parseRfc3339(s)
}

// DateTimeParseFromStr is `DateTime::parse_from_str`: a string parsed from a
// user-specified format into a DateTime<FixedOffset> value.
//
// Note that this method *requires a timezone* in the input string. See
// NaiveDateTimeParseFromStr for a version that does not require a timezone
// in the to-be-parsed str. The returned DateTime value will have a
// FixedOffset reflecting the parsed timezone.
//
// See FormatStrftimeItemsNew for the supported escape sequences.
//
// The error is a ParseError.
func DateTimeParseFromStr(s string, fmt string) (DateTime, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, FormatStrftimeItemsNew(fmt).Collect()); err != nil {
		return DateTime{}, err
	}
	return parsed.ToDatetime()
}

// DateTimeParseAndRemainder is `DateTime::parse_and_remainder`: a
// DateTime<FixedOffset> parsed from the beginning of a string with the
// specified format string, and the remaining portion of the string.
//
// Note that this method *requires a timezone* in the input string.
func DateTimeParseAndRemainder(s string, fmt string) (DateTime, string, error) {
	parsed := FormatParsedNew()
	remainder, err := FormatParseAndRemainder(&parsed, s, FormatStrftimeItemsNew(fmt).Collect())
	if err != nil {
		return DateTime{}, "", err
	}
	datetime, err := parsed.ToDatetime()
	if err != nil {
		return DateTime{}, "", err
	}
	return datetime, remainder, nil
}

// FormatWithItems is `DateTime::format_with_items`: the combined date and
// time in the zone's local time, formatted with the specified formatting
// items. The result is formatted when its String method is called, which
// panics if an item is the error item.
func (dt DateTime) FormatWithItems(items []FormatItem) FormatDelayedFormat {
	local := dt.overflowingNaiveLocal()
	return FormatDelayedFormatNewWithOffset(rs.Some(local.Date()), rs.Some(local.Time()), dt.off(), items)
}

// Format is `DateTime::format`: the combined date and time in the zone's
// local time, formatted per the specified format string.
//
// See FormatStrftimeItemsNew for the supported escape sequences.
//
// The result is formatted when its String method is called (Rust's
// `to_string()`), which panics when the format string is invalid.
func (dt DateTime) Format(fmt string) FormatDelayedFormat {
	return dt.FormatWithItems(FormatStrftimeItemsNew(fmt).Collect())
}

// Year is `Datelike::year`, in the zone's local time.
func (dt DateTime) Year() int32 { return dt.overflowingNaiveLocal().Year() }

// YearCe is `Datelike::year_ce`.
func (dt DateTime) YearCe() (bool, uint32) { return DatelikeDefaultYearCe[DateTime](dt) }

// Quarter is `Datelike::quarter`.
func (dt DateTime) Quarter() uint32 { return DatelikeDefaultQuarter[DateTime](dt) }

// Month is `Datelike::month`: the month number starting from 1.
func (dt DateTime) Month() uint32 { return dt.overflowingNaiveLocal().Month() }

// Month0 is `Datelike::month0`: the month number starting from 0.
func (dt DateTime) Month0() uint32 { return dt.overflowingNaiveLocal().Month0() }

// Day is `Datelike::day`: the day of month starting from 1.
func (dt DateTime) Day() uint32 { return dt.overflowingNaiveLocal().Day() }

// Day0 is `Datelike::day0`: the day of month starting from 0.
func (dt DateTime) Day0() uint32 { return dt.overflowingNaiveLocal().Day0() }

// Ordinal is `Datelike::ordinal`: the day of year starting from 1.
func (dt DateTime) Ordinal() uint32 { return dt.overflowingNaiveLocal().Ordinal() }

// Ordinal0 is `Datelike::ordinal0`: the day of year starting from 0.
func (dt DateTime) Ordinal0() uint32 { return dt.overflowingNaiveLocal().Ordinal0() }

// Weekday is `Datelike::weekday`.
func (dt DateTime) Weekday() Weekday { return dt.overflowingNaiveLocal().Weekday() }

// IsoWeek is `Datelike::iso_week`.
func (dt DateTime) IsoWeek() IsoWeek { return dt.overflowingNaiveLocal().IsoWeek() }

// WithYear is `Datelike::with_year`: a new DateTime with the year number
// changed, while keeping the same month and day.
//
// It returns None if the local datetime would be out of range, if the
// resulting date does not exist (February 29 in a non-leap year), or if the
// local time at the resulting date does not exist or is ambiguous.
func (dt DateTime) WithYear(year int32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] {
		if naive.Year() == year {
			return rs.Some(naive)
		}
		return naive.WithYear(year)
	})
}

// WithMonth is `Datelike::with_month`: a new DateTime with the month number
// (starting from 1) changed.
//
// Don't combine multiple `Datelike::with_*` methods. The intermediate value
// may not exist. See NaiveDate.WithMonth.
func (dt DateTime) WithMonth(month uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithMonth(month) })
}

// WithMonth0 is `Datelike::with_month0`: a new DateTime with the month
// number (starting from 0) changed.
func (dt DateTime) WithMonth0(month0 uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithMonth0(month0) })
}

// WithDay is `Datelike::with_day`: a new DateTime with the day of month
// (starting from 1) changed.
func (dt DateTime) WithDay(day uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithDay(day) })
}

// WithDay0 is `Datelike::with_day0`: a new DateTime with the day of month
// (starting from 0) changed.
func (dt DateTime) WithDay0(day0 uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithDay0(day0) })
}

// WithOrdinal is `Datelike::with_ordinal`: a new DateTime with the day of
// year (starting from 1) changed.
func (dt DateTime) WithOrdinal(ordinal uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithOrdinal(ordinal) })
}

// WithOrdinal0 is `Datelike::with_ordinal0`: a new DateTime with the day of
// year (starting from 0) changed.
func (dt DateTime) WithOrdinal0(ordinal0 uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithOrdinal0(ordinal0) })
}

// NumDaysFromCe is `Datelike::num_days_from_ce`.
func (dt DateTime) NumDaysFromCe() int32 { return DatelikeDefaultNumDaysFromCe[DateTime](dt) }

// NumDaysInMonth is `Datelike::num_days_in_month`.
func (dt DateTime) NumDaysInMonth() uint8 { return DatelikeDefaultNumDaysInMonth[DateTime](dt) }

// Hour is `Timelike::hour`, in the zone's local time.
func (dt DateTime) Hour() uint32 { return dt.overflowingNaiveLocal().Hour() }

// Hour12 is `Timelike::hour12`.
func (dt DateTime) Hour12() (bool, uint32) { return TimelikeDefaultHour12[DateTime](dt) }

// Minute is `Timelike::minute`.
func (dt DateTime) Minute() uint32 { return dt.overflowingNaiveLocal().Minute() }

// Second is `Timelike::second`.
func (dt DateTime) Second() uint32 { return dt.overflowingNaiveLocal().Second() }

// Nanosecond is `Timelike::nanosecond`: the number of nanoseconds since the
// whole non-leap second. The range from 1,000,000,000 to 1,999,999,999
// represents the leap second.
func (dt DateTime) Nanosecond() uint32 { return dt.overflowingNaiveLocal().Nanosecond() }

// WithHour is `Timelike::with_hour`: a new DateTime with the hour number
// changed.
//
// It returns None if the value for hour is invalid, the local time at the
// resulting date does not exist or is ambiguous, or the result is out of
// range.
func (dt DateTime) WithHour(hour uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithHour(hour) })
}

// WithMinute is `Timelike::with_minute`: a new DateTime with the minute
// number changed.
func (dt DateTime) WithMinute(min uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithMinute(min) })
}

// WithSecond is `Timelike::with_second`: a new DateTime with the second
// number changed. As with the Second method, the input range is restricted
// to 0 through 59.
func (dt DateTime) WithSecond(sec uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithSecond(sec) })
}

// WithNanosecond is `Timelike::with_nanosecond`: a new DateTime with
// nanoseconds since the whole non-leap second changed. As with the
// Nanosecond method, the input range can exceed 1,000,000,000 for leap
// seconds.
func (dt DateTime) WithNanosecond(nano uint32) rs.Option[DateTime] {
	return mapLocal(dt, func(naive NaiveDateTime) rs.Option[NaiveDateTime] { return naive.WithNanosecond(nano) })
}

// NumSecondsFromMidnight is `Timelike::num_seconds_from_midnight`.
func (dt DateTime) NumSecondsFromMidnight() uint32 {
	return TimelikeDefaultNumSecondsFromMidnight[DateTime](dt)
}

// Equal is `==`: whether the two values are the same instant, whatever
// their zones.
func (dt DateTime) Equal(other DateTime) bool {
	return dt.datetime == other.datetime
}

// Compare is `cmp` and `partial_cmp`: the order of the two instants,
// whatever their zones.
func (dt DateTime) Compare(other DateTime) int {
	return dt.datetime.Compare(other.datetime)
}

// HashKey is `impl Hash for DateTime`: the instant only.
func (dt DateTime) HashKey() string {
	return dt.datetime.DebugString()
}

// Add is `datetime + rhs` for a TimeDelta: the instant plus rhs, in the same
// zone.
//
// As a part of Chrono's leap second handling, the addition assumes that
// *there is no leap second ever*, except when the NaiveDateTime itself
// represents a leap second in which case the assumption becomes that *there
// is exactly a single leap second ever*.
//
// It panics if the resulting date would be out of range. Consider using
// CheckedAddSigned to get an Option instead.
func (dt DateTime) Add(rhs TimeDelta) DateTime {
	return expect(dt.CheckedAddSigned(rhs), "`DateTime + TimeDelta` overflowed")
}

// AddDuration is `datetime + rhs` for a std::time::Duration. It panics if
// the resulting date would be out of range.
func (dt DateTime) AddDuration(rhs time.Duration) DateTime {
	delta := expectStdDuration(rhs)
	return expect(dt.CheckedAddSigned(delta), "`DateTime + TimeDelta` overflowed")
}

// AddAssign is `datetime += rhs` for a TimeDelta. It panics if the resulting
// date would be out of range.
func (dt *DateTime) AddAssign(rhs TimeDelta) {
	datetime := expect(dt.datetime.CheckedAddSigned(rhs), "`DateTime + TimeDelta` overflowed")
	tz := dt.Timezone()
	*dt = tz.FromUtcDatetime(datetime)
}

// AddAssignDuration is `datetime += rhs` for a std::time::Duration. It
// panics if the resulting date would be out of range.
func (dt *DateTime) AddAssignDuration(rhs time.Duration) {
	delta := expectStdDuration(rhs)
	datetime := expect(dt.datetime.CheckedAddSigned(delta), "`DateTime + TimeDelta` overflowed")
	tz := dt.Timezone()
	*dt = tz.FromUtcDatetime(datetime)
}

// AddFixedOffset is `datetime + rhs` for a FixedOffset: the instant moved
// by the offset, with the zone's offset left as it was. It panics if the
// resulting date would be out of range.
func (dt DateTime) AddFixedOffset(rhs FixedOffset) DateTime {
	dt.datetime = expect(dt.NaiveUtc().CheckedAddOffset(rhs), "`DateTime + FixedOffset` overflowed")
	return dt
}

// AddMonths is `datetime + rhs` for Months: the local date plus months at
// the same local time, clamping the day to the last day of the month. It
// panics if the result is out of range, or the local time on the resulting
// date does not exist or is ambiguous. Consider using CheckedAddMonths.
func (dt DateTime) AddMonths(rhs Months) DateTime {
	return expect(dt.CheckedAddMonths(rhs), "`DateTime + Months` out of range")
}

// Sub is `datetime - rhs` for a TimeDelta: the instant minus rhs, in the
// same zone.
//
// It panics if the resulting date would be out of range. Consider using
// CheckedSubSigned to get an Option instead.
func (dt DateTime) Sub(rhs TimeDelta) DateTime {
	return expect(dt.CheckedSubSigned(rhs), "`DateTime - TimeDelta` overflowed")
}

// SubDuration is `datetime - rhs` for a std::time::Duration. It panics if
// the resulting date would be out of range.
func (dt DateTime) SubDuration(rhs time.Duration) DateTime {
	delta := expectStdDuration(rhs)
	return expect(dt.CheckedSubSigned(delta), "`DateTime - TimeDelta` overflowed")
}

// SubAssign is `datetime -= rhs` for a TimeDelta. It panics if the resulting
// date would be out of range.
func (dt *DateTime) SubAssign(rhs TimeDelta) {
	datetime := expect(dt.datetime.CheckedSubSigned(rhs), "`DateTime - TimeDelta` overflowed")
	tz := dt.Timezone()
	*dt = tz.FromUtcDatetime(datetime)
}

// SubAssignDuration is `datetime -= rhs` for a std::time::Duration. It
// panics if the resulting date would be out of range.
func (dt *DateTime) SubAssignDuration(rhs time.Duration) {
	delta := expectStdDuration(rhs)
	datetime := expect(dt.datetime.CheckedSubSigned(delta), "`DateTime - TimeDelta` overflowed")
	tz := dt.Timezone()
	*dt = tz.FromUtcDatetime(datetime)
}

// SubFixedOffset is `datetime - rhs` for a FixedOffset: the instant moved by
// the offset, with the zone's offset left as it was. It panics if the
// resulting date would be out of range.
func (dt DateTime) SubFixedOffset(rhs FixedOffset) DateTime {
	dt.datetime = expect(dt.NaiveUtc().CheckedSubOffset(rhs), "`DateTime - FixedOffset` overflowed")
	return dt
}

// SubMonths is `datetime - rhs` for Months: the local date minus months at
// the same local time, clamping the day to the last day of the month. It
// panics if the result is out of range, or the local time on the resulting
// date does not exist or is ambiguous. Consider using CheckedSubMonths.
func (dt DateTime) SubMonths(rhs Months) DateTime {
	return expect(dt.CheckedSubMonths(rhs), "`DateTime - Months` out of range")
}

// SubDateTime is `datetime - rhs` for two DateTimes: the signed span
// between the two instants.
func (dt DateTime) SubDateTime(rhs DateTime) TimeDelta {
	return dt.SignedDurationSince(rhs)
}

// AddDays is `datetime + days`: the local date plus days at the same local
// time. It panics if the result is out of range, or the local time on the
// resulting date does not exist or is ambiguous.
func (dt DateTime) AddDays(days Days) DateTime {
	return expect(dt.CheckedAddDays(days), "`DateTime + Days` out of range")
}

// SubDays is `datetime - days`: the local date minus days at the same local
// time. It panics if the result is out of range, or the local time on the
// resulting date does not exist or is ambiguous.
func (dt DateTime) SubDays(days Days) DateTime {
	return expect(dt.CheckedSubDays(days), "`DateTime - Days` out of range")
}

// DebugString is the Debug form: the local date and time in ISO 8601 form
// followed, without a space, by the Debug form of the offset, such as
// "2014-11-28T21:00:09Z" (Utc), "2014-11-28T21:00:09+09:00" (FixedOffset and
// Local) or "2014-11-28T21:00:09JST" (a chronotz zone).
func (dt DateTime) DebugString() string {
	return dt.overflowingNaiveLocal().DebugString() + dt.off().DebugString()
}

// String is the Display form: the local date, the local time and the Display
// form of the offset, separated by spaces, such as "2014-11-28 21:00:09 UTC"
// (Utc), "2014-11-28 21:00:09 +09:00" (FixedOffset and Local) or "2014-11-28
// 21:00:09 JST" (a chronotz zone).
func (dt DateTime) String() string {
	return dt.overflowingNaiveLocal().String() + " " + dt.off().String()
}

// FmtDebug is `impl fmt::Debug for DateTime`. It ignores the formatter's
// width and fill.
func (dt DateTime) FmtDebug(f *rs.Formatter) error { return f.WriteStr(dt.DebugString()) }

// FmtDisplay is `impl fmt::Display for DateTime`. It ignores the
// formatter's width and fill.
func (dt DateTime) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(dt.String()) }

// DateTimeUtcFromStr is `str::parse::<DateTime<Utc>>()`: a relaxed form of
// RFC 3339 (see DateTimeFixedOffsetFromStr), converted to UTC.
//
// The error is a ParseError.
func DateTimeUtcFromStr(s string) (DateTime, error) {
	dt, err := DateTimeFixedOffsetFromStr(s)
	if err != nil {
		return DateTime{}, err
	}
	return dt.WithTimezone(Utc{}), nil
}

// DateTimeLocalFromStr is `str::parse::<DateTime<Local>>()`: a relaxed form
// of RFC 3339 (see DateTimeFixedOffsetFromStr), converted to the local zone.
//
// The error is a ParseError.
func DateTimeLocalFromStr(s string) (DateTime, error) {
	dt, err := DateTimeFixedOffsetFromStr(s)
	if err != nil {
		return DateTime{}, err
	}
	return dt.WithTimezone(Local{}), nil
}

// DateTimeUtcFromSystemTime is `DateTime::<Utc>::from(system_time)`: a
// std::time::SystemTime, which is a time.Time in Go, as a DateTime<Utc>.
//
// It panics ("No such local time") for an instant outside chrono's range of
// years, which Rust's SystemTime and Go's time.Time can both hold.
func DateTimeUtcFromSystemTime(t time.Time) DateTime {
	// Go's Unix and Nanosecond already are what the Rust impl computes for a
	// time before the epoch: the seconds rounded down and a fraction that is
	// never negative.
	sec := t.Unix()
	nsec := uint32(t.Nanosecond())
	return Utc{}.TimestampOpt(sec, nsec).Unwrap()
}

// DateTimeLocalFromSystemTime is `DateTime::<Local>::from(system_time)`: a
// std::time::SystemTime, which is a time.Time in Go, as a DateTime<Local>.
func DateTimeLocalFromSystemTime(t time.Time) DateTime {
	return DateTimeUtcFromSystemTime(t).WithTimezone(Local{})
}

// SystemTimeFromDateTime is `SystemTime::from(datetime)`: the instant as a
// std::time::SystemTime, which is a time.Time in Go. The result is in the
// UTC location and has no monotonic clock reading; a leap second becomes
// the first instant of the next second, as Rust's Duration carries it.
func SystemTimeFromDateTime(dt DateTime) time.Time {
	sec := dt.Timestamp()
	nsec := dt.TimestampSubsecNanos()
	return time.Unix(sec, int64(nsec)).UTC()
}

const unixEpochDay int64 = 719_163
