// Port of chrono 0.4.44 src/naive/datetime/mod.rs

package chrono

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// NaiveDateTime is the ISO 8601 combined date and time without timezone.
//
// It is a Copy type, comparable with ==. The zero value is
// 1970-01-01T00:00:00, as `NaiveDateTime::default()` is.
type NaiveDateTime struct {
	date NaiveDate
	time NaiveTime
}

// NaiveDateTimeNew is `NaiveDateTime::new`: a NaiveDateTime from a date and a
// time.
func NaiveDateTimeNew(date NaiveDate, time NaiveTime) NaiveDateTime {
	return NaiveDateTime{date: date, time: time}
}

// NaiveDateTimeParseFromStr is `NaiveDateTime::parse_from_str`: a string
// parsed with the specified format string into a new NaiveDateTime. See
// FormatStrftimeItemsNew for the supported sequences.
//
// An offset in the input (`%z`) is read and then ignored; years outside 0
// through 9999 must have a sign.
//
// The error is a ParseError.
func NaiveDateTimeParseFromStr(s string, fmt string) (NaiveDateTime, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, FormatStrftimeItemsNew(fmt).Collect()); err != nil {
		return NaiveDateTime{}, err
	}
	return parsed.ToNaiveDatetimeWithOffset(0) // no offset adjustment
}

// NaiveDateTimeParseAndRemainder is `NaiveDateTime::parse_and_remainder`: a
// NaiveDateTime parsed from the beginning of a string with the specified
// format string, and the remaining portion of the string.
func NaiveDateTimeParseAndRemainder(s string, fmt string) (NaiveDateTime, string, error) {
	parsed := FormatParsedNew()
	remainder, err := FormatParseAndRemainder(&parsed, s, FormatStrftimeItemsNew(fmt).Collect())
	if err != nil {
		return NaiveDateTime{}, "", err
	}
	datetime, err := parsed.ToNaiveDatetimeWithOffset(0) // no offset adjustment
	if err != nil {
		return NaiveDateTime{}, "", err
	}
	return datetime, remainder, nil
}

// Date is `NaiveDateTime::date`: the date component.
func (dt NaiveDateTime) Date() NaiveDate {
	return dt.date
}

// Time is `NaiveDateTime::time`: the time component.
func (dt NaiveDateTime) Time() NaiveTime {
	return dt.time
}

// CheckedAddSigned is `NaiveDateTime::checked_add_signed`: the datetime plus
// rhs, or None on overflow.
//
// As a part of Chrono's leap second handling, the addition assumes that
// *there is no leap second ever*, except when the NaiveDateTime itself
// represents a leap second in which case the assumption becomes that *there
// is exactly a single leap second ever*.
func (dt NaiveDateTime) CheckedAddSigned(rhs TimeDelta) rs.Option[NaiveDateTime] {
	time, remainder := dt.time.OverflowingAddSigned(rhs)
	remainderDelta, ok := TimeDeltaTrySeconds(remainder).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	date, ok := dt.date.CheckedAddSigned(remainderDelta).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: time})
}

// CheckedAddMonths is `NaiveDateTime::checked_add_months`: the datetime plus
// months, clamping the day to the last day of the month. None if the
// resulting date would be out of range.
func (dt NaiveDateTime) CheckedAddMonths(rhs Months) rs.Option[NaiveDateTime] {
	date, ok := dt.date.CheckedAddMonths(rhs).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: dt.time})
}

// CheckedAddOffset is `NaiveDateTime::checked_add_offset`: the datetime plus
// an offset, keeping a leap second. None if the result would be outside the
// valid range for NaiveDateTime.
func (dt NaiveDateTime) CheckedAddOffset(rhs FixedOffset) rs.Option[NaiveDateTime] {
	time, days := dt.time.overflowingAddOffset(rhs)
	date := dt.date
	var ok bool
	switch days {
	case -1:
		if date, ok = dt.date.PredOpt().Get(); !ok {
			return rs.None[NaiveDateTime]()
		}
	case 1:
		if date, ok = dt.date.SuccOpt().Get(); !ok {
			return rs.None[NaiveDateTime]()
		}
	}
	return rs.Some(NaiveDateTime{date: date, time: time})
}

// CheckedSubOffset is `NaiveDateTime::checked_sub_offset`: the datetime minus
// an offset, keeping a leap second. None if the result would be outside the
// valid range for NaiveDateTime.
func (dt NaiveDateTime) CheckedSubOffset(rhs FixedOffset) rs.Option[NaiveDateTime] {
	time, days := dt.time.overflowingSubOffset(rhs)
	date := dt.date
	var ok bool
	switch days {
	case -1:
		if date, ok = dt.date.PredOpt().Get(); !ok {
			return rs.None[NaiveDateTime]()
		}
	case 1:
		if date, ok = dt.date.SuccOpt().Get(); !ok {
			return rs.None[NaiveDateTime]()
		}
	}
	return rs.Some(NaiveDateTime{date: date, time: time})
}

// overflowingAddOffset adds an offset to the current datetime.
//
// This method is similar to CheckedAddOffset, but it will not return an
// Option. Instead it will fall back to the hidden "before min" and "after
// max" dates, which only exist so that the local time of an in-range instant
// can be printed.
func (dt NaiveDateTime) overflowingAddOffset(rhs FixedOffset) NaiveDateTime {
	time, days := dt.time.overflowingAddOffset(rhs)
	date := dt.date
	switch days {
	case -1:
		date = dt.date.PredOpt().UnwrapOr(naiveDateBeforeMin)
	case 1:
		date = dt.date.SuccOpt().UnwrapOr(naiveDateAfterMax)
	}
	return NaiveDateTime{date: date, time: time}
}

// overflowingSubOffset subtracts an offset from the current datetime.
//
// This method is similar to CheckedSubOffset, but it will not return an
// Option.
func (dt NaiveDateTime) overflowingSubOffset(rhs FixedOffset) NaiveDateTime {
	time, days := dt.time.overflowingSubOffset(rhs)
	date := dt.date
	switch days {
	case -1:
		date = dt.date.PredOpt().UnwrapOr(naiveDateBeforeMin)
	case 1:
		date = dt.date.SuccOpt().UnwrapOr(naiveDateAfterMax)
	}
	return NaiveDateTime{date: date, time: time}
}

// CheckedSubSigned is `NaiveDateTime::checked_sub_signed`: the datetime minus
// rhs, or None on overflow.
func (dt NaiveDateTime) CheckedSubSigned(rhs TimeDelta) rs.Option[NaiveDateTime] {
	time, remainder := dt.time.OverflowingSubSigned(rhs)
	remainderDelta, ok := TimeDeltaTrySeconds(remainder).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	date, ok := dt.date.CheckedSubSigned(remainderDelta).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: time})
}

// CheckedSubMonths is `NaiveDateTime::checked_sub_months`: the datetime minus
// months, clamping the day to the last day of the month. None if the
// resulting date would be out of range.
func (dt NaiveDateTime) CheckedSubMonths(rhs Months) rs.Option[NaiveDateTime] {
	date, ok := dt.date.CheckedSubMonths(rhs).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: dt.time})
}

// CheckedAddDays is `NaiveDateTime::checked_add_days`: the datetime plus a
// duration in days, or None if the resulting date would be out of range.
func (dt NaiveDateTime) CheckedAddDays(days Days) rs.Option[NaiveDateTime] {
	date, ok := dt.date.CheckedAddDays(days).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: dt.time})
}

// CheckedSubDays is `NaiveDateTime::checked_sub_days`: the datetime minus a
// duration in days, or None if the resulting date would be out of range.
func (dt NaiveDateTime) CheckedSubDays(days Days) rs.Option[NaiveDateTime] {
	date, ok := dt.date.CheckedSubDays(days).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: date, time: dt.time})
}

// SignedDurationSince is `NaiveDateTime::signed_duration_since`: the datetime
// minus rhs as a TimeDelta. This does not overflow or underflow at all.
//
// As a part of Chrono's leap second handling, the subtraction assumes that
// *there is no leap second ever*, except when any of the NaiveDateTimes
// themselves represents a leap second in which case the assumption becomes
// that *there are exactly one (or two) leap second(s) ever*.
func (dt NaiveDateTime) SignedDurationSince(rhs NaiveDateTime) TimeDelta {
	return expect(
		dt.date.SignedDurationSince(rhs.date).CheckedAdd(dt.time.SignedDurationSince(rhs.time)),
		"always in range",
	)
}

// FormatWithItems is `NaiveDateTime::format_with_items`: the combined date
// and time formatted with the specified formatting items. The result is
// formatted when its String method is called, which panics if an item is the
// error item or asks for an offset.
func (dt NaiveDateTime) FormatWithItems(items []FormatItem) FormatDelayedFormat {
	return FormatDelayedFormatNew(rs.Some(dt.date), rs.Some(dt.time), items)
}

// Format is `NaiveDateTime::format`: the combined date and time formatted
// with the specified format string. See FormatStrftimeItemsNew for the
// supported escape sequences.
//
// The result is formatted when its String method is called (Rust's
// `to_string()`), which panics when the format string is invalid or asks for
// an offset.
func (dt NaiveDateTime) Format(fmt string) FormatDelayedFormat {
	return dt.FormatWithItems(FormatStrftimeItemsNew(fmt).Collect())
}

// AndLocalTimezone is `NaiveDateTime::and_local_timezone`: the datetime read
// as a local time in tz. A local time can be ambiguous (the clock was turned
// back) or not exist at all (the clock was turned forward), which the result
// says.
func (dt NaiveDateTime) AndLocalTimezone(tz TimeZone) MappedLocalTime[DateTime] {
	return tz.FromLocalDatetime(dt)
}

// AndUtc is `NaiveDateTime::and_utc`: the datetime read as UTC.
func (dt NaiveDateTime) AndUtc() DateTime {
	return DateTimeFromNaiveUtcAndOffset(dt, Utc{}, Utc{})
}

// NaiveDateTimeMin is `NaiveDateTime::MIN`: the minimum possible
// NaiveDateTime.
var NaiveDateTimeMin = NaiveDateTime{date: NaiveDateMin, time: NaiveTimeMin}

// NaiveDateTimeMax is `NaiveDateTime::MAX`: the maximum possible
// NaiveDateTime.
var NaiveDateTimeMax = NaiveDateTime{date: NaiveDateMax, time: naiveTimeMax}

// NaiveDateTimeFromNaiveDate is `NaiveDateTime::from(date)`: the date at
// midnight.
func NaiveDateTimeFromNaiveDate(date NaiveDate) NaiveDateTime {
	return date.AndHmsOpt(0, 0, 0).Unwrap()
}

// Year is `Datelike::year`.
func (dt NaiveDateTime) Year() int32 { return dt.date.Year() }

// YearCe is `Datelike::year_ce`.
func (dt NaiveDateTime) YearCe() (bool, uint32) { return DatelikeDefaultYearCe[NaiveDateTime](dt) }

// Quarter is `Datelike::quarter`.
func (dt NaiveDateTime) Quarter() uint32 { return DatelikeDefaultQuarter[NaiveDateTime](dt) }

// Month is `Datelike::month`: the month number starting from 1.
func (dt NaiveDateTime) Month() uint32 { return dt.date.Month() }

// Month0 is `Datelike::month0`: the month number starting from 0.
func (dt NaiveDateTime) Month0() uint32 { return dt.date.Month0() }

// Day is `Datelike::day`: the day of month starting from 1.
func (dt NaiveDateTime) Day() uint32 { return dt.date.Day() }

// Day0 is `Datelike::day0`: the day of month starting from 0.
func (dt NaiveDateTime) Day0() uint32 { return dt.date.Day0() }

// Ordinal is `Datelike::ordinal`: the day of year starting from 1.
func (dt NaiveDateTime) Ordinal() uint32 { return dt.date.Ordinal() }

// Ordinal0 is `Datelike::ordinal0`: the day of year starting from 0.
func (dt NaiveDateTime) Ordinal0() uint32 { return dt.date.Ordinal0() }

// Weekday is `Datelike::weekday`.
func (dt NaiveDateTime) Weekday() Weekday { return dt.date.Weekday() }

// IsoWeek is `Datelike::iso_week`.
func (dt NaiveDateTime) IsoWeek() IsoWeek { return dt.date.IsoWeek() }

// WithYear is `Datelike::with_year`: a new NaiveDateTime with the year number
// changed, while keeping the same month and day. None if the resulting date
// does not exist or the year is out of range.
func (dt NaiveDateTime) WithYear(year int32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithYear(year))
}

// WithMonth is `Datelike::with_month`: a new NaiveDateTime with the month
// number (starting from 1) changed.
func (dt NaiveDateTime) WithMonth(month uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithMonth(month))
}

// WithMonth0 is `Datelike::with_month0`: a new NaiveDateTime with the month
// number (starting from 0) changed.
func (dt NaiveDateTime) WithMonth0(month0 uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithMonth0(month0))
}

// WithDay is `Datelike::with_day`: a new NaiveDateTime with the day of month
// (starting from 1) changed.
func (dt NaiveDateTime) WithDay(day uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithDay(day))
}

// WithDay0 is `Datelike::with_day0`: a new NaiveDateTime with the day of
// month (starting from 0) changed.
func (dt NaiveDateTime) WithDay0(day0 uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithDay0(day0))
}

// WithOrdinal is `Datelike::with_ordinal`: a new NaiveDateTime with the day
// of year (starting from 1) changed.
func (dt NaiveDateTime) WithOrdinal(ordinal uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithOrdinal(ordinal))
}

// WithOrdinal0 is `Datelike::with_ordinal0`: a new NaiveDateTime with the day
// of year (starting from 0) changed.
func (dt NaiveDateTime) WithOrdinal0(ordinal0 uint32) rs.Option[NaiveDateTime] {
	return dt.withDate(dt.date.WithOrdinal0(ordinal0))
}

// NumDaysFromCe is `Datelike::num_days_from_ce`.
func (dt NaiveDateTime) NumDaysFromCe() int32 {
	return DatelikeDefaultNumDaysFromCe[NaiveDateTime](dt)
}

// NumDaysInMonth is `Datelike::num_days_in_month`.
func (dt NaiveDateTime) NumDaysInMonth() uint8 {
	return DatelikeDefaultNumDaysInMonth[NaiveDateTime](dt)
}

func (dt NaiveDateTime) withDate(date rs.Option[NaiveDate]) rs.Option[NaiveDateTime] {
	d, ok := date.Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: d, time: dt.time})
}

func (dt NaiveDateTime) withTime(time rs.Option[NaiveTime]) rs.Option[NaiveDateTime] {
	t, ok := time.Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(NaiveDateTime{date: dt.date, time: t})
}

// Hour is `Timelike::hour`: the hour number from 0 to 23.
func (dt NaiveDateTime) Hour() uint32 { return dt.time.Hour() }

// Hour12 is `Timelike::hour12`.
func (dt NaiveDateTime) Hour12() (bool, uint32) { return TimelikeDefaultHour12[NaiveDateTime](dt) }

// Minute is `Timelike::minute`: the minute number from 0 to 59.
func (dt NaiveDateTime) Minute() uint32 { return dt.time.Minute() }

// Second is `Timelike::second`: the second number from 0 to 59.
func (dt NaiveDateTime) Second() uint32 { return dt.time.Second() }

// Nanosecond is `Timelike::nanosecond`: the number of nanoseconds since the
// whole non-leap second. The range from 1,000,000,000 to 1,999,999,999
// represents the leap second.
func (dt NaiveDateTime) Nanosecond() uint32 { return dt.time.Nanosecond() }

// WithHour is `Timelike::with_hour`: a new NaiveDateTime with the hour number
// changed.
func (dt NaiveDateTime) WithHour(hour uint32) rs.Option[NaiveDateTime] {
	return dt.withTime(dt.time.WithHour(hour))
}

// WithMinute is `Timelike::with_minute`: a new NaiveDateTime with the minute
// number changed.
func (dt NaiveDateTime) WithMinute(min uint32) rs.Option[NaiveDateTime] {
	return dt.withTime(dt.time.WithMinute(min))
}

// WithSecond is `Timelike::with_second`: a new NaiveDateTime with the second
// number changed.
func (dt NaiveDateTime) WithSecond(sec uint32) rs.Option[NaiveDateTime] {
	return dt.withTime(dt.time.WithSecond(sec))
}

// WithNanosecond is `Timelike::with_nanosecond`: a new NaiveDateTime with
// nanoseconds since the whole non-leap second changed.
func (dt NaiveDateTime) WithNanosecond(nano uint32) rs.Option[NaiveDateTime] {
	return dt.withTime(dt.time.WithNanosecond(nano))
}

// NumSecondsFromMidnight is `Timelike::num_seconds_from_midnight`.
func (dt NaiveDateTime) NumSecondsFromMidnight() uint32 {
	return TimelikeDefaultNumSecondsFromMidnight[NaiveDateTime](dt)
}

// Add is `datetime + rhs` for a TimeDelta. It panics if the resulting date
// would be out of range; use CheckedAddSigned to get an Option instead.
func (dt NaiveDateTime) Add(rhs TimeDelta) NaiveDateTime {
	return expect(dt.CheckedAddSigned(rhs), "`NaiveDateTime + TimeDelta` overflowed")
}

// AddDuration is `datetime + rhs` for a std::time::Duration. It panics if
// the resulting date would be out of range.
func (dt NaiveDateTime) AddDuration(rhs time.Duration) NaiveDateTime {
	delta := expectStdDuration(rhs)
	return expect(dt.CheckedAddSigned(delta), "`NaiveDateTime + TimeDelta` overflowed")
}

// AddAssign is `datetime += rhs` for a TimeDelta. It panics on overflow.
func (dt *NaiveDateTime) AddAssign(rhs TimeDelta) {
	*dt = expect(dt.CheckedAddSigned(rhs), "`NaiveDateTime + TimeDelta` overflowed")
}

// AddAssignDuration is `datetime += rhs` for a std::time::Duration. It
// panics on overflow.
func (dt *NaiveDateTime) AddAssignDuration(rhs time.Duration) {
	delta := expectStdDuration(rhs)
	*dt = expect(dt.CheckedAddSigned(delta), "`NaiveDateTime + TimeDelta` overflowed")
}

// AddFixedOffset is `datetime + rhs` for a FixedOffset, keeping a leap
// second. It panics if the resulting date would be out of range.
func (dt NaiveDateTime) AddFixedOffset(rhs FixedOffset) NaiveDateTime {
	return expect(dt.CheckedAddOffset(rhs), "`NaiveDateTime + FixedOffset` out of range")
}

// AddMonths is `datetime + rhs` for Months, clamping the day to the last day
// of the month. It panics if the resulting date would be out of range.
func (dt NaiveDateTime) AddMonths(rhs Months) NaiveDateTime {
	return expect(dt.CheckedAddMonths(rhs), "`NaiveDateTime + Months` out of range")
}

// Sub is `datetime - rhs` for a TimeDelta. It panics if the resulting date
// would be out of range; use CheckedSubSigned to get an Option instead.
func (dt NaiveDateTime) Sub(rhs TimeDelta) NaiveDateTime {
	return expect(dt.CheckedSubSigned(rhs), "`NaiveDateTime - TimeDelta` overflowed")
}

// SubDuration is `datetime - rhs` for a std::time::Duration. It panics if
// the resulting date would be out of range.
func (dt NaiveDateTime) SubDuration(rhs time.Duration) NaiveDateTime {
	delta := expectStdDuration(rhs)
	return expect(dt.CheckedSubSigned(delta), "`NaiveDateTime - TimeDelta` overflowed")
}

// SubAssign is `datetime -= rhs` for a TimeDelta. It panics on overflow.
func (dt *NaiveDateTime) SubAssign(rhs TimeDelta) {
	*dt = expect(dt.CheckedSubSigned(rhs), "`NaiveDateTime - TimeDelta` overflowed")
}

// SubAssignDuration is `datetime -= rhs` for a std::time::Duration. It
// panics on overflow.
func (dt *NaiveDateTime) SubAssignDuration(rhs time.Duration) {
	delta := expectStdDuration(rhs)
	*dt = expect(dt.CheckedSubSigned(delta), "`NaiveDateTime - TimeDelta` overflowed")
}

// SubFixedOffset is `datetime - rhs` for a FixedOffset, keeping a leap
// second. It panics if the resulting date would be out of range.
func (dt NaiveDateTime) SubFixedOffset(rhs FixedOffset) NaiveDateTime {
	return expect(dt.CheckedSubOffset(rhs), "`NaiveDateTime - FixedOffset` out of range")
}

// SubMonths is `datetime - rhs` for Months, clamping the day to the last day
// of the month. It panics if the resulting date would be out of range.
func (dt NaiveDateTime) SubMonths(rhs Months) NaiveDateTime {
	return expect(dt.CheckedSubMonths(rhs), "`NaiveDateTime - Months` out of range")
}

// SubNaiveDateTime is `datetime - rhs` for two datetimes: the signed span
// between them. This does not overflow or underflow at all.
func (dt NaiveDateTime) SubNaiveDateTime(rhs NaiveDateTime) TimeDelta {
	return dt.SignedDurationSince(rhs)
}

// AddDays is `datetime + days`. It panics if the resulting date would be out
// of range.
func (dt NaiveDateTime) AddDays(days Days) NaiveDateTime {
	return expect(dt.CheckedAddDays(days), "`NaiveDateTime + Days` out of range")
}

// SubDays is `datetime - days`. It panics if the resulting date would be out
// of range.
func (dt NaiveDateTime) SubDays(days Days) NaiveDateTime {
	return expect(dt.CheckedSubDays(days), "`NaiveDateTime - Days` out of range")
}

// expectStdDuration is `TimeDelta::from_std(rhs).expect(..)` as the
// arithmetic with a std::time::Duration does it.
func expectStdDuration(rhs time.Duration) TimeDelta {
	delta, err := TimeDeltaFromStd(rhs)
	if err != nil {
		rs.PanicAt(2, "overflow converting from core::time::Duration to TimeDelta: "+debugString(err))
	}
	return delta
}

// Equal is `==` (derived PartialEq).
func (dt NaiveDateTime) Equal(other NaiveDateTime) bool { return dt == other }

// Compare is `cmp` (derived Ord): by date, then time.
func (dt NaiveDateTime) Compare(other NaiveDateTime) int {
	if order := dt.date.Compare(other.date); order != 0 {
		return order
	}
	return dt.time.Compare(other.time)
}

// DebugString is the Debug form: the ISO 8601 form with a "T" between date
// and time, such as "2016-11-15T07:39:24.500".
func (dt NaiveDateTime) DebugString() string {
	return dt.date.DebugString() + "T" + dt.time.DebugString()
}

// String is the Display form: the date, a space, and the time, such as
// "2016-11-15 07:39:24.500".
func (dt NaiveDateTime) String() string {
	return dt.date.String() + " " + dt.time.String()
}

// FmtDebug is `impl fmt::Debug for NaiveDateTime`. It ignores the
// formatter's width and fill.
func (dt NaiveDateTime) FmtDebug(f *rs.Formatter) error { return f.WriteStr(dt.DebugString()) }

// FmtDisplay is `impl fmt::Display for NaiveDateTime`. It ignores the
// formatter's width and fill.
func (dt NaiveDateTime) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(dt.String()) }

// NaiveDateTimeFromStr is `str::parse::<NaiveDateTime>()`: a datetime in the
// form "2015-09-18T23:56:04" (the Debug form), read leniently: numbers need
// no zero padding, white space is allowed before each number and separator
// other than the "T", and a year with a sign may have any number of digits.
// The "T" is required, and must be upper case.
//
// The error is a ParseError.
func NaiveDateTimeFromStr(s string) (NaiveDateTime, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, naiveDateTimeFromStrItems); err != nil {
		return NaiveDateTime{}, err
	}
	return parsed.ToNaiveDatetimeWithOffset(0)
}

var naiveDateTimeFromStrItems = []FormatItem{
	&FormatItem_Numeric{F0: FormatNumeric_Year, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: "-"},
	&FormatItem_Numeric{F0: FormatNumeric_Month, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: "-"},
	&FormatItem_Numeric{F0: FormatNumeric_Day, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: "T"}, // XXX shouldn't this be case-insensitive?
	&FormatItem_Numeric{F0: FormatNumeric_Hour, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: ":"},
	&FormatItem_Numeric{F0: FormatNumeric_Minute, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: ":"},
	&FormatItem_Numeric{F0: FormatNumeric_Second, F1: FormatPad_Zero},
	&FormatItem_Fixed{F0: FormatFixed_Nanosecond},
	&FormatItem_Space{F0: ""},
}

// NaiveDateTimeDefault is `NaiveDateTime::default()`: the Unix epoch,
// 1970-01-01T00:00:00, which is the Go zero value.
func NaiveDateTimeDefault() NaiveDateTime {
	return DateTimeUnixEpoch.NaiveLocal()
}
