// Port of chrono 0.4.44 src/traits.rs

package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// Datelike is the common set of methods for the date component. T is the
// implementing type itself (Rust's `Self`), which the `With*` methods return.
// NaiveDate, NaiveDateTime and DateTime implement it.
//
// A type that relies on one of the trait's default methods calls the
// matching DatelikeDefault function.
type Datelike[T any] interface {
	// Year returns the year number in the calendar date.
	Year() int32
	// YearCe returns the absolute year number starting from 1 with a boolean
	// flag, which is false when the year predates the epoch (BCE/BC) and true
	// otherwise (CE/AD).
	YearCe() (bool, uint32)
	// Quarter returns the quarter number starting from 1 (1 through 4).
	Quarter() uint32
	// Month returns the month number starting from 1 (1 through 12).
	Month() uint32
	// Month0 returns the month number starting from 0 (0 through 11).
	Month0() uint32
	// Day returns the day of month starting from 1 (1 through 31).
	Day() uint32
	// Day0 returns the day of month starting from 0 (0 through 30).
	Day0() uint32
	// Ordinal returns the day of year starting from 1 (1 through 366).
	Ordinal() uint32
	// Ordinal0 returns the day of year starting from 0 (0 through 365).
	Ordinal0() uint32
	// Weekday returns the day of week.
	Weekday() Weekday
	// IsoWeek returns the ISO week.
	IsoWeek() IsoWeek
	// WithYear makes a new value with the year number changed, while keeping
	// the same month and day. None when the resulting date does not exist
	// (February 29 in a non-leap year) or is out of range.
	WithYear(year int32) rs.Option[T]
	// WithMonth makes a new value with the month number (starting from 1)
	// changed.
	WithMonth(month uint32) rs.Option[T]
	// WithMonth0 makes a new value with the month number (starting from 0)
	// changed.
	WithMonth0(month0 uint32) rs.Option[T]
	// WithDay makes a new value with the day of month (starting from 1)
	// changed.
	WithDay(day uint32) rs.Option[T]
	// WithDay0 makes a new value with the day of month (starting from 0)
	// changed.
	WithDay0(day0 uint32) rs.Option[T]
	// WithOrdinal makes a new value with the day of year (starting from 1)
	// changed.
	WithOrdinal(ordinal uint32) rs.Option[T]
	// WithOrdinal0 makes a new value with the day of year (starting from 0)
	// changed.
	WithOrdinal0(ordinal0 uint32) rs.Option[T]
	// NumDaysFromCe counts the days in the proleptic Gregorian calendar, with
	// January 1, Year 1 (CE) as day 1.
	NumDaysFromCe() int32
	// NumDaysInMonth returns the number of days in the month.
	NumDaysInMonth() uint8
}

// DatelikeDefaultYearCe is the default body of `Datelike::year_ce`.
func DatelikeDefaultYearCe[T any](d Datelike[T]) (bool, uint32) {
	year := d.Year()
	if year < 1 {
		return false, uint32(1 - year)
	}
	return true, uint32(year)
}

// DatelikeDefaultQuarter is the default body of `Datelike::quarter`.
func DatelikeDefaultQuarter[T any](d Datelike[T]) uint32 {
	return (d.Month()-1)/3 + 1
}

// DatelikeDefaultNumDaysFromCe is the default body of
// `Datelike::num_days_from_ce`.
func DatelikeDefaultNumDaysFromCe[T any](d Datelike[T]) int32 {
	return numDaysFromCe(d.Year(), d.Ordinal())
}

func numDaysFromCe(year int32, ordinal uint32) int32 {
	// See test_num_days_from_ce_against_alternative_impl below for a more straightforward
	// implementation.

	// we know this wouldn't overflow since year is limited to 1/2^13 of i32's full range.
	year--
	var ndays int32
	if year < 0 {
		excess := 1 + (-year)/400
		year += excess * 400
		ndays -= excess * 146_097
	}
	div100 := year / 100
	ndays += ((year * 1461) >> 2) - div100 + (div100 >> 2)
	return ndays + int32(ordinal)
}

// DatelikeDefaultNumDaysInMonth is the default body of
// `Datelike::num_days_in_month`.
func DatelikeDefaultNumDaysInMonth[T any](d Datelike[T]) uint8 {
	// The value returned from `self.month()` is guaranteed to be in the
	// range [1,12], which will never result in a `None` value here.
	month := MonthFromU32(d.Month()).Unwrap()
	// `Month::num_days` will only return `None` if the provided year is out
	// of range. Since we are passing it directly from a verified date, we
	// know it is in range, and the result will never be `None`.
	return month.NumDays(d.Year()).Unwrap()
}

// Timelike is the common set of methods for the time component. T is the
// implementing type itself (Rust's `Self`), which the `With*` methods return.
// NaiveTime, NaiveDateTime and DateTime implement it.
type Timelike[T any] interface {
	// Hour returns the hour number from 0 to 23.
	Hour() uint32
	// Hour12 returns the hour number from 1 to 12 with a boolean flag, which
	// is false for AM and true for PM.
	Hour12() (bool, uint32)
	// Minute returns the minute number from 0 to 59.
	Minute() uint32
	// Second returns the second number from 0 to 59.
	Second() uint32
	// Nanosecond returns the number of nanoseconds since the whole non-leap
	// second. The range from 1,000,000,000 to 1,999,999,999 represents the
	// leap second.
	Nanosecond() uint32
	// WithHour makes a new value with the hour number changed.
	WithHour(hour uint32) rs.Option[T]
	// WithMinute makes a new value with the minute number changed.
	WithMinute(min uint32) rs.Option[T]
	// WithSecond makes a new value with the second number changed. As with
	// the Second method, the input range is restricted to 0 through 59.
	WithSecond(sec uint32) rs.Option[T]
	// WithNanosecond makes a new value with nanoseconds since the whole
	// non-leap second changed. As with the Nanosecond method, the input range
	// can exceed 1,000,000,000 for leap seconds.
	WithNanosecond(nano uint32) rs.Option[T]
	// NumSecondsFromMidnight returns the number of non-leap seconds past the
	// last midnight.
	NumSecondsFromMidnight() uint32
}

// TimelikeDefaultHour12 is the default body of `Timelike::hour12`.
func TimelikeDefaultHour12[T any](t Timelike[T]) (bool, uint32) {
	hour := t.Hour()
	hour12 := hour % 12
	if hour12 == 0 {
		hour12 = 12
	}
	return hour >= 12, hour12
}

// TimelikeDefaultNumSecondsFromMidnight is the default body of
// `Timelike::num_seconds_from_midnight`.
func TimelikeDefaultNumSecondsFromMidnight[T any](t Timelike[T]) uint32 {
	return t.Hour()*3600 + t.Minute()*60 + t.Second()
}
