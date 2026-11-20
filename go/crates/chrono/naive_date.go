// Port of chrono 0.4.44 src/naive/date/mod.rs

package chrono

// ISO 8601 calendar date without timezone.
//
// The implementation is optimized for determining year, month, day and day of week.
//
// Format of NaiveDate:
// `YYYY_YYYY_YYYY_YYYY_YYYO_OOOO_OOOO_LWWW`
// `Y`: Year
// `O`: Ordinal
// `L`: leap year flag (1 = common year, 0 is leap year)
// `W`: weekday before the first day of the year
// `LWWW`: will also be referred to as the year flags (`F`)

import (
	"math"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// NaiveDate is the ISO 8601 calendar date without timezone.
// It allows every proleptic Gregorian date from Jan 1, 262143 BCE to Dec 31,
// 262142 CE. It also supports the conversion from ISO 8601 ordinal and week
// date.
//
// It is a Copy type, and comparable with == because every date has exactly
// one representation; Equal and Compare (or rs.Eq and rs.Cmp) say the same.
// The zero value is 1970-01-01, as `NaiveDate::default()` is.
type NaiveDate struct {
	// The crate's `yof` ((year << 13) | ordinal and flags), exclusive-or'ed
	// with the yof of 1970-01-01. Rust keeps a NonZeroI32, which has no
	// default; a Go value starts as zero, and zero has to be a date.
	v int32
}

// naiveDateEpochYof is the yof of 1970-01-01: a common year after a Wednesday
// (year flags D).
const naiveDateEpochYof int32 = (1970 << 13) | (1 << 4) | 0o12

// weeksFrom returns the number of whole weeks since the first `day` of the
// year, which is what `%U` and `%W` print.
func (d NaiveDate) weeksFrom(day Weekday) int32 {
	return (int32(d.Ordinal()) - int32(d.Weekday().DaysSince(day)) + 6) / 7
}

// naiveDateFromOrdinalAndFlags makes a new NaiveDate from year, ordinal and
// flags. It does not check whether the flags are correct for the provided
// year.
func naiveDateFromOrdinalAndFlags(year int32, ordinal uint32, flags yearFlags) rs.Option[NaiveDate] {
	if year < naiveDateMinYear || year > naiveDateMaxYear {
		return rs.None[NaiveDate]() // Out-of-range
	}
	if ordinal == 0 || ordinal > 366 {
		return rs.None[NaiveDate]() // Invalid
	}
	yof := (year << 13) | int32(ordinal<<4) | int32(flags)
	if yof&olMask <= naiveDateMaxOl {
		return rs.Some(naiveDateFromYof(yof))
	}
	return rs.None[NaiveDate]() // Does not exist: Ordinal 366 in a common year.
}

// naiveDateFromMdf makes a new NaiveDate from year and packed
// month-day-flags. It does not check whether the flags are correct for the
// provided year.
func naiveDateFromMdf(year int32, m mdf) rs.Option[NaiveDate] {
	if year < naiveDateMinYear || year > naiveDateMaxYear {
		return rs.None[NaiveDate]() // Out-of-range
	}
	ordinalAndFlags, ok := m.ordinalAndFlags()
	if !ok {
		return rs.None[NaiveDate]()
	}
	return rs.Some(naiveDateFromYof((year << 13) | ordinalAndFlags))
}

// NaiveDateFromYmdOpt is `NaiveDate::from_ymd_opt`: a date from the calendar
// date (year, month and day).
//
// It returns None if the specified calendar day does not exist, the value for
// month or day is invalid, or year is out of range.
func NaiveDateFromYmdOpt(year int32, month uint32, day uint32) rs.Option[NaiveDate] {
	flags := yearFlagsFromYear(year)
	if m, ok := mdfNew(month, day, flags); ok {
		return naiveDateFromMdf(year, m)
	}
	return rs.None[NaiveDate]()
}

// NaiveDateFromYoOpt is `NaiveDate::from_yo_opt`: a date from the ordinal
// date (year and day of the year).
//
// It returns None if the specified ordinal day does not exist, the value for
// ordinal is invalid, or year is out of range.
func NaiveDateFromYoOpt(year int32, ordinal uint32) rs.Option[NaiveDate] {
	flags := yearFlagsFromYear(year)
	return naiveDateFromOrdinalAndFlags(year, ordinal, flags)
}

// NaiveDateFromIsoywdOpt is `NaiveDate::from_isoywd_opt`: a date from the ISO
// week date (year, week number and day of the week). The resulting date may
// have a different year from the input year.
//
// It returns None if the specified week does not exist in that year, the
// value for week is invalid, or the resulting date is out of range.
func NaiveDateFromIsoywdOpt(year int32, week uint32, weekday Weekday) rs.Option[NaiveDate] {
	flags := yearFlagsFromYear(year)
	nweeks := flags.nisoweeks()
	if week == 0 || week > nweeks {
		return rs.None[NaiveDate]()
	}
	// ordinal = week ordinal - delta
	weekord := week*7 + uint32(weekday)
	delta := flags.isoweekDelta()
	var ordinal uint32
	if weekord <= delta {
		// ordinal < 1, previous year
		prevflags := yearFlagsFromYear(year - 1)
		year, ordinal, flags = year-1, weekord+prevflags.ndays()-delta, prevflags
	} else {
		ordinal = weekord - delta
		ndays := flags.ndays()
		if ordinal > ndays {
			// ordinal > ndays, next year
			nextflags := yearFlagsFromYear(year + 1)
			year, ordinal, flags = year+1, ordinal-ndays, nextflags
		}
	}
	return naiveDateFromOrdinalAndFlags(year, ordinal, flags)
}

// NaiveDateFromNumDaysFromCeOpt is `NaiveDate::from_num_days_from_ce_opt`: a
// date from a day's number in the proleptic Gregorian calendar, with January
// 1, 1 being day 1.
//
// It returns None if the date is out of range.
func NaiveDateFromNumDaysFromCeOpt(days int32) rs.Option[NaiveDate] {
	days, ok := checkedAddI32(days, 365) // make December 31, 1 BCE equal to day 0
	if !ok {
		return rs.None[NaiveDate]()
	}
	yearDiv400 := divEuclid(days, 146_097)
	cycle := remEuclid(days, 146_097)
	yearMod400, ordinal := cycleToYo(uint32(cycle))
	flags := yearFlagsFromYearMod400(int32(yearMod400))
	return naiveDateFromOrdinalAndFlags(yearDiv400*400+int32(yearMod400), ordinal, flags)
}

// NaiveDateFromEpochDays is `NaiveDate::from_epoch_days`: a date from the
// number of days since January 1, 1970 in UTC.
//
// It returns None if the date is out of range.
func NaiveDateFromEpochDays(days int32) rs.Option[NaiveDate] {
	ceDays, ok := checkedAddI32(days, int32(unixEpochDay))
	if !ok {
		return rs.None[NaiveDate]()
	}
	return NaiveDateFromNumDaysFromCeOpt(ceDays)
}

// NaiveDateFromWeekdayOfMonthOpt is `NaiveDate::from_weekday_of_month_opt`: a
// date by counting the number of occurrences of a particular day-of-week
// since the beginning of the given month. For instance, if you want the 2nd
// Friday of March 2017, you would use (2017, 3, Weekday_Fri, 2).
// `n` is 1-indexed.
//
// It returns None if the specified day does not exist in that month, on
// invalid values for n or month, or if year is out of range.
func NaiveDateFromWeekdayOfMonthOpt(year int32, month uint32, weekday Weekday, n uint8) rs.Option[NaiveDate] {
	if n == 0 {
		return rs.None[NaiveDate]()
	}
	firstDay, ok := NaiveDateFromYmdOpt(year, month, 1).Get()
	if !ok {
		return rs.None[NaiveDate]()
	}
	first := firstDay.Weekday()
	firstToDow := (7 + weekday.NumberFromMonday() - first.NumberFromMonday()) % 7
	day := uint32(n-1)*7 + firstToDow + 1
	return NaiveDateFromYmdOpt(year, month, day)
}

// NaiveDateParseFromStr is `NaiveDate::parse_from_str`: a string parsed with
// the specified format string into a new NaiveDate. See the strftime format
// in FormatStrftimeItemsNew for the supported sequences.
//
// The error is a ParseError.
func NaiveDateParseFromStr(s string, fmt string) (NaiveDate, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, FormatStrftimeItemsNew(fmt).Collect()); err != nil {
		return NaiveDate{}, err
	}
	return parsed.ToNaiveDate()
}

// NaiveDateParseAndRemainder is `NaiveDate::parse_and_remainder`: a
// NaiveDate parsed from the beginning of a string with the specified format
// string, and the remaining portion of the string.
func NaiveDateParseAndRemainder(s string, fmt string) (NaiveDate, string, error) {
	parsed := FormatParsedNew()
	remainder, err := FormatParseAndRemainder(&parsed, s, FormatStrftimeItemsNew(fmt).Collect())
	if err != nil {
		return NaiveDate{}, "", err
	}
	date, err := parsed.ToNaiveDate()
	if err != nil {
		return NaiveDate{}, "", err
	}
	return date, remainder, nil
}

// CheckedAddMonths is `NaiveDate::checked_add_months`: the date plus the
// given number of months, clamping the day to the last day of the resulting
// month. None if the result is out of range.
func (d NaiveDate) CheckedAddMonths(months Months) rs.Option[NaiveDate] {
	if months == 0 {
		return rs.Some(d)
	}
	if uint32(months) <= uint32(math.MaxInt32) {
		return d.diffMonths(int32(months))
	}
	return rs.None[NaiveDate]()
}

// CheckedSubMonths is `NaiveDate::checked_sub_months`: the date minus the
// given number of months, clamping the day to the last day of the resulting
// month. None if the result is out of range.
func (d NaiveDate) CheckedSubMonths(months Months) rs.Option[NaiveDate] {
	if months == 0 {
		return rs.Some(d)
	}
	if uint32(months) <= uint32(math.MaxInt32) {
		return d.diffMonths(-int32(months))
	}
	return rs.None[NaiveDate]()
}

func (d NaiveDate) diffMonths(months int32) rs.Option[NaiveDate] {
	months, ok := checkedAddI32(d.Year()*12+int32(d.Month())-1, months)
	if !ok {
		return rs.None[NaiveDate]()
	}
	year := divEuclid(months, 12)
	month := uint32(remEuclid(months, 12)) + 1

	// Clamp original day in case new month is shorter
	flags := yearFlagsFromYear(year)
	febDays := uint32(28)
	if flags.ndays() == 366 {
		febDays = 29
	}
	days := [12]uint32{31, febDays, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31}
	dayMax := days[month-1]
	day := d.Day()
	if day > dayMax {
		day = dayMax
	}

	return NaiveDateFromYmdOpt(year, month, day)
}

// CheckedAddDays is `NaiveDate::checked_add_days`: the date plus a number of
// days, or None if the result is out of range.
func (d NaiveDate) CheckedAddDays(days Days) rs.Option[NaiveDate] {
	if uint64(days) <= uint64(math.MaxInt32) {
		return d.addDays(int32(days))
	}
	return rs.None[NaiveDate]()
}

// CheckedSubDays is `NaiveDate::checked_sub_days`: the date minus a number of
// days, or None if the result is out of range.
func (d NaiveDate) CheckedSubDays(days Days) rs.Option[NaiveDate] {
	if uint64(days) <= uint64(math.MaxInt32) {
		return d.addDays(-int32(days))
	}
	return rs.None[NaiveDate]()
}

// addDays adds a duration of days to the date. It returns None if the
// resulting date would be out of range.
func (d NaiveDate) addDays(days int32) rs.Option[NaiveDate] {
	// Fast path if the result is within the same year.
	// Also `DateTime::checked_(add|sub)_days` relies on this path, because if the value remains
	// within the year it doesn't do a check if the year is in range.
	// This way `DateTime:checked_(add|sub)_days(Days::new(0))` can be a no-op on dates were the
	// local datetime is beyond `NaiveDate::{MIN, MAX}.
	const ordinalMask int32 = 0b1_1111_1111_0000
	if ordinal, ok := checkedAddI32((d.yof()&ordinalMask)>>4, days); ok {
		leap := int32(0)
		if d.LeapYear() {
			leap = 1
		}
		if ordinal > 0 && ordinal <= (365+leap) {
			yearAndFlags := d.yof() &^ ordinalMask
			return rs.Some(naiveDateFromYof(yearAndFlags | (ordinal << 4)))
		}
	}
	// do the full check
	year := d.Year()
	yearDiv400, yearMod400 := divModFloor(year, 400)
	cycle, ok := checkedAddI32(int32(yoToCycle(uint32(yearMod400), d.Ordinal())), days)
	if !ok {
		return rs.None[NaiveDate]()
	}
	cycleDiv400y, cycle := divModFloor(cycle, 146_097)
	yearDiv400 += cycleDiv400y

	yearMod400u, ordinal := cycleToYo(uint32(cycle))
	flags := yearFlagsFromYearMod400(int32(yearMod400u))
	return naiveDateFromOrdinalAndFlags(yearDiv400*400+int32(yearMod400u), ordinal, flags)
}

// AndTime is `NaiveDate::and_time`: a NaiveDateTime from the date and a
// NaiveTime.
func (d NaiveDate) AndTime(time NaiveTime) NaiveDateTime {
	return NaiveDateTimeNew(d, time)
}

// AndHmsOpt is `NaiveDate::and_hms_opt`: a NaiveDateTime from the date, hour,
// minute and second. No leap second is allowed here; use AndHmsMilliOpt and
// friends to deal with a leap second.
//
// It returns None on invalid hour, minute and/or second.
func (d NaiveDate) AndHmsOpt(hour, min, sec uint32) rs.Option[NaiveDateTime] {
	time, ok := NaiveTimeFromHmsOpt(hour, min, sec).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(d.AndTime(time))
}

// AndHmsMilliOpt is `NaiveDate::and_hms_milli_opt`: a NaiveDateTime from the
// date, hour, minute, second and millisecond. The millisecond part is allowed
// to exceed 1,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or millisecond.
func (d NaiveDate) AndHmsMilliOpt(hour, min, sec, milli uint32) rs.Option[NaiveDateTime] {
	time, ok := NaiveTimeFromHmsMilliOpt(hour, min, sec, milli).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(d.AndTime(time))
}

// AndHmsMicroOpt is `NaiveDate::and_hms_micro_opt`: a NaiveDateTime from the
// date, hour, minute, second and microsecond. The microsecond part is allowed
// to exceed 1,000,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or microsecond.
func (d NaiveDate) AndHmsMicroOpt(hour, min, sec, micro uint32) rs.Option[NaiveDateTime] {
	time, ok := NaiveTimeFromHmsMicroOpt(hour, min, sec, micro).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(d.AndTime(time))
}

// AndHmsNanoOpt is `NaiveDate::and_hms_nano_opt`: a NaiveDateTime from the
// date, hour, minute, second and nanosecond. The nanosecond part is allowed
// to exceed 1,000,000,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or nanosecond.
func (d NaiveDate) AndHmsNanoOpt(hour, min, sec, nano uint32) rs.Option[NaiveDateTime] {
	time, ok := NaiveTimeFromHmsNanoOpt(hour, min, sec, nano).Get()
	if !ok {
		return rs.None[NaiveDateTime]()
	}
	return rs.Some(d.AndTime(time))
}

// mdf returns the packed month-day-flags.
func (d NaiveDate) mdf() mdf {
	return mdfFromOl((d.yof()&olMask)>>3, d.yearFlags())
}

// withMdf makes a new NaiveDate with the packed month-day-flags changed.
//
// It returns None when the resulting NaiveDate would be invalid.
func (d NaiveDate) withMdf(m mdf) rs.Option[NaiveDate] {
	ordinal, ok := m.ordinal()
	if !ok {
		return rs.None[NaiveDate]() // Non-existing date
	}
	return rs.Some(naiveDateFromYof((d.yof() &^ ordinalMask) | int32(ordinal<<4)))
}

// SuccOpt is `NaiveDate::succ_opt`: the next calendar date, or None when the
// date is the last representable date.
func (d NaiveDate) SuccOpt() rs.Option[NaiveDate] {
	newOl := (d.yof() & olMask) + (1 << 4)
	if newOl <= naiveDateMaxOl {
		return rs.Some(naiveDateFromYof(d.yof()&^olMask | newOl))
	}
	return NaiveDateFromYoOpt(d.Year()+1, 1)
}

// PredOpt is `NaiveDate::pred_opt`: the previous calendar date, or None when
// the date is the first representable date.
func (d NaiveDate) PredOpt() rs.Option[NaiveDate] {
	newShiftedOrdinal := (d.yof() & ordinalMask) - (1 << 4)
	if newShiftedOrdinal > 0 {
		return rs.Some(naiveDateFromYof(d.yof()&^ordinalMask | newShiftedOrdinal))
	}
	return NaiveDateFromYmdOpt(d.Year()-1, 12, 31)
}

// CheckedAddSigned is `NaiveDate::checked_add_signed`: the date plus the
// days in rhs (its sub-day part is dropped), or None if the result is out of
// range.
func (d NaiveDate) CheckedAddSigned(rhs TimeDelta) rs.Option[NaiveDate] {
	days := rhs.NumDays()
	if days < math.MinInt32 || days > math.MaxInt32 {
		return rs.None[NaiveDate]()
	}
	return d.addDays(int32(days))
}

// CheckedSubSigned is `NaiveDate::checked_sub_signed`: the date minus the
// days in rhs (its sub-day part is dropped), or None if the result is out of
// range.
func (d NaiveDate) CheckedSubSigned(rhs TimeDelta) rs.Option[NaiveDate] {
	days := -rhs.NumDays()
	if days < math.MinInt32 || days > math.MaxInt32 {
		return rs.None[NaiveDate]()
	}
	return d.addDays(int32(days))
}

// SignedDurationSince is `NaiveDate::signed_duration_since`: the date minus
// rhs as a TimeDelta. This does not overflow or underflow at all, as all
// possible output fits in the range of TimeDelta.
func (d NaiveDate) SignedDurationSince(rhs NaiveDate) TimeDelta {
	year1 := d.Year()
	year2 := rhs.Year()
	year1Div400, year1Mod400 := divModFloor(year1, 400)
	year2Div400, year2Mod400 := divModFloor(year2, 400)
	cycle1 := int64(yoToCycle(uint32(year1Mod400), d.Ordinal()))
	cycle2 := int64(yoToCycle(uint32(year2Mod400), rhs.Ordinal()))
	days := (int64(year1Div400)-int64(year2Div400))*146_097 + (cycle1 - cycle2)
	// The range of `TimeDelta` is ca. 585 million years, the range of `NaiveDate` ca. 525.000
	// years.
	return expect(TimeDeltaTryDays(days), "always in range")
}

// AbsDiff is `NaiveDate::abs_diff`: the absolute difference between two
// dates measured as the number of days.
func (d NaiveDate) AbsDiff(rhs NaiveDate) Days {
	a, b := int64(d.NumDaysFromCe()), int64(rhs.NumDaysFromCe())
	if a < b {
		a, b = b, a
	}
	return DaysNew(uint64(a - b))
}

// YearsSince is `NaiveDate::years_since`: the number of whole years from the
// given base until the date, or None if base > d.
func (d NaiveDate) YearsSince(base NaiveDate) rs.Option[uint32] {
	years := d.Year() - base.Year()
	// Comparing tuples is not (yet) possible in const context. Instead we combine month and
	// day into one `u32` for easy comparison.
	if ((d.Month() << 5) | d.Day()) < ((base.Month() << 5) | base.Day()) {
		years--
	}
	if years >= 0 {
		return rs.Some(uint32(years))
	}
	return rs.None[uint32]()
}

// FormatWithItems is `NaiveDate::format_with_items`: the date formatted with
// the specified formatting items. The result is formatted when its String
// method is called, which panics if an item is the error item or asks for a
// time or an offset.
func (d NaiveDate) FormatWithItems(items []FormatItem) FormatDelayedFormat {
	return FormatDelayedFormatNew(rs.Some(d), rs.None[NaiveTime](), items)
}

// Format is `NaiveDate::format`: the date formatted with the specified
// format string. See FormatStrftimeItemsNew for the supported escape
// sequences.
//
// The result is formatted when its String method is called (Rust's
// `to_string()`), which panics when the format string is invalid or asks for
// a time or an offset.
func (d NaiveDate) Format(fmt string) FormatDelayedFormat {
	return d.FormatWithItems(FormatStrftimeItemsNew(fmt).Collect())
}

// IterDays is `NaiveDate::iter_days`: an iterator that steps by days across
// all representable dates.
func (d NaiveDate) IterDays() *NaiveDateDaysIterator {
	return &NaiveDateDaysIterator{value: d}
}

// IterWeeks is `NaiveDate::iter_weeks`: an iterator that steps by weeks
// across all representable dates.
func (d NaiveDate) IterWeeks() *NaiveDateWeeksIterator {
	return &NaiveDateWeeksIterator{value: d}
}

// Week is `NaiveDate::week`: the NaiveWeek that the date belongs to, starting
// with the Weekday specified.
func (d NaiveDate) Week(start Weekday) NaiveWeek {
	return naiveWeekNew(d, start)
}

// LeapYear is `NaiveDate::leap_year`: whether the date's year is a leap year.
func (d NaiveDate) LeapYear() bool {
	return d.yof()&0b1000 == 0
}

// Year is `Datelike::year`: the year number in the calendar date.
func (d NaiveDate) Year() int32 {
	return d.yof() >> 13
}

// Ordinal is `Datelike::ordinal`: the day of year starting from 1 (1 through
// 366).
func (d NaiveDate) Ordinal() uint32 {
	return uint32((d.yof() & ordinalMask) >> 4)
}

// Month is `Datelike::month`: the month number starting from 1 (1 through
// 12).
func (d NaiveDate) Month() uint32 {
	return d.mdf().month()
}

// Day is `Datelike::day`: the day of month starting from 1 (1 through 31).
func (d NaiveDate) Day() uint32 {
	return d.mdf().day()
}

// Weekday is `Datelike::weekday`: the day of week.
func (d NaiveDate) Weekday() Weekday {
	switch (((d.yof() & ordinalMask) >> 4) + (d.yof() & weekdayFlagsMask)) % 7 {
	case 0:
		return Weekday_Mon
	case 1:
		return Weekday_Tue
	case 2:
		return Weekday_Wed
	case 3:
		return Weekday_Thu
	case 4:
		return Weekday_Fri
	case 5:
		return Weekday_Sat
	default:
		return Weekday_Sun
	}
}

func (d NaiveDate) yearFlags() yearFlags {
	return yearFlags(d.yof() & yearFlagsMask)
}

// NumDaysFromCe is `Datelike::num_days_from_ce`: the days in the proleptic
// Gregorian calendar, with January 1, Year 1 (CE) as day 1.
func (d NaiveDate) NumDaysFromCe() int32 {
	return numDaysFromCe(d.Year(), d.Ordinal())
}

// ToEpochDays is `NaiveDate::to_epoch_days`: the number of days since January
// 1, 1970 in UTC.
func (d NaiveDate) ToEpochDays() int32 {
	return d.NumDaysFromCe() - int32(unixEpochDay)
}

// naiveDateFromYof creates a new NaiveDate. This does not check the validity
// of the ordinal or flags.
func naiveDateFromYof(yof int32) NaiveDate {
	// The following are the invariants our ordinal and flags should uphold for a valid
	// `NaiveDate`.
	rs.DebugAssert(((yof & olMask) >> 3) > 1)
	rs.DebugAssert(((yof & olMask) >> 3) <= naiveDateMaxOl)
	rs.DebugAssert((yof & 0b111) != 0)
	return NaiveDate{v: yof ^ naiveDateEpochYof}
}

// yof gets the raw year-ordinal-flags i32.
func (d NaiveDate) yof() int32 {
	return d.v ^ naiveDateEpochYof
}

// NaiveDateMin is `NaiveDate::MIN`: the minimum possible NaiveDate (January
// 1, 262144 BCE).
var NaiveDateMin = naiveDateFromYof((naiveDateMinYear << 13) | (1 << 4) | 0o12 /* D */)

// NaiveDateMax is `NaiveDate::MAX`: the maximum possible NaiveDate (December
// 31, 262142 CE).
var NaiveDateMax = naiveDateFromYof((naiveDateMaxYear << 13) | (365 << 4) | 0o16 /* G */)

// naiveDateBeforeMin is one day before NaiveDateMin.
//
// Only for internal use, and to be used with great care. Methods should not
// return this value to users.
var naiveDateBeforeMin = naiveDateFromYof(((naiveDateMinYear - 1) << 13) | (366 << 4) | 0o07 /* FE */)

// naiveDateAfterMax is one day after NaiveDateMax.
//
// Only for internal use, and to be used with great care. Methods should not
// return this value to users.
var naiveDateAfterMax = naiveDateFromYof(((naiveDateMaxYear + 1) << 13) | (1 << 4) | 0o17 /* F */)

// YearCe is `Datelike::year_ce`.
func (d NaiveDate) YearCe() (bool, uint32) { return DatelikeDefaultYearCe[NaiveDate](d) }

// Quarter is `Datelike::quarter`: the quarter number starting from 1.
func (d NaiveDate) Quarter() uint32 { return DatelikeDefaultQuarter[NaiveDate](d) }

// Month0 is `Datelike::month0`: the month number starting from 0 (0 through
// 11).
func (d NaiveDate) Month0() uint32 {
	return d.Month() - 1
}

// Day0 is `Datelike::day0`: the day of month starting from 0 (0 through 30).
func (d NaiveDate) Day0() uint32 {
	return d.mdf().day() - 1
}

// Ordinal0 is `Datelike::ordinal0`: the day of year starting from 0 (0
// through 365).
func (d NaiveDate) Ordinal0() uint32 {
	return d.Ordinal() - 1
}

// IsoWeek is `Datelike::iso_week`.
func (d NaiveDate) IsoWeek() IsoWeek {
	return isoWeekFromYof(d.Year(), d.Ordinal(), d.yearFlags())
}

// WithYear is `Datelike::with_year`: a new NaiveDate with the year number
// changed, while keeping the same month and day.
//
// It returns None if the resulting date does not exist (February 29 in a
// non-leap year), or if the year is out of range.
func (d NaiveDate) WithYear(year int32) rs.Option[NaiveDate] {
	// we need to operate with `mdf` since we should keep the month and day number as is
	m := d.mdf()

	// adjust the flags as needed
	flags := yearFlagsFromYear(year)
	m = m.withFlags(flags)

	return naiveDateFromMdf(year, m)
}

// WithMonth is `Datelike::with_month`: a new NaiveDate with the month number
// (starting from 1) changed.
//
// It returns None if the resulting date does not exist (for example month(4)
// when day of the month is 31), or if the value for month is invalid.
func (d NaiveDate) WithMonth(month uint32) rs.Option[NaiveDate] {
	m, ok := d.mdf().withMonth(month)
	if !ok {
		return rs.None[NaiveDate]()
	}
	return d.withMdf(m)
}

// WithMonth0 is `Datelike::with_month0`: a new NaiveDate with the month
// number (starting from 0) changed.
func (d NaiveDate) WithMonth0(month0 uint32) rs.Option[NaiveDate] {
	if month0 == math.MaxUint32 {
		return rs.None[NaiveDate]()
	}
	m, ok := d.mdf().withMonth(month0 + 1)
	if !ok {
		return rs.None[NaiveDate]()
	}
	return d.withMdf(m)
}

// WithDay is `Datelike::with_day`: a new NaiveDate with the day of month
// (starting from 1) changed.
//
// It returns None if the resulting date does not exist (for example day(31)
// in April), or if the value for day is invalid.
func (d NaiveDate) WithDay(day uint32) rs.Option[NaiveDate] {
	m, ok := d.mdf().withDay(day)
	if !ok {
		return rs.None[NaiveDate]()
	}
	return d.withMdf(m)
}

// WithDay0 is `Datelike::with_day0`: a new NaiveDate with the day of month
// (starting from 0) changed.
func (d NaiveDate) WithDay0(day0 uint32) rs.Option[NaiveDate] {
	if day0 == math.MaxUint32 {
		return rs.None[NaiveDate]()
	}
	m, ok := d.mdf().withDay(day0 + 1)
	if !ok {
		return rs.None[NaiveDate]()
	}
	return d.withMdf(m)
}

// WithOrdinal is `Datelike::with_ordinal`: a new NaiveDate with the day of
// year (starting from 1) changed.
//
// It returns None if the resulting date does not exist (with_ordinal(366) in
// a non-leap year), or if the value for ordinal is invalid.
func (d NaiveDate) WithOrdinal(ordinal uint32) rs.Option[NaiveDate] {
	if ordinal == 0 || ordinal > 366 {
		return rs.None[NaiveDate]()
	}
	yof := (d.yof() &^ ordinalMask) | int32(ordinal<<4)
	if yof&olMask <= naiveDateMaxOl {
		return rs.Some(naiveDateFromYof(yof))
	}
	return rs.None[NaiveDate]() // Does not exist: Ordinal 366 in a common year.
}

// WithOrdinal0 is `Datelike::with_ordinal0`: a new NaiveDate with the day of
// year (starting from 0) changed.
func (d NaiveDate) WithOrdinal0(ordinal0 uint32) rs.Option[NaiveDate] {
	if ordinal0 == math.MaxUint32 {
		return rs.None[NaiveDate]()
	}
	return d.WithOrdinal(ordinal0 + 1)
}

// NumDaysInMonth is `Datelike::num_days_in_month`.
func (d NaiveDate) NumDaysInMonth() uint8 { return DatelikeDefaultNumDaysInMonth[NaiveDate](d) }

// Add is `date + rhs` for a TimeDelta: the date plus the days in rhs. It
// panics if the resulting date would be out of range; use CheckedAddSigned
// to get an Option instead.
func (d NaiveDate) Add(rhs TimeDelta) NaiveDate {
	return expect(d.CheckedAddSigned(rhs), "`NaiveDate + TimeDelta` overflowed")
}

// AddAssign is `date += rhs` for a TimeDelta. It panics on overflow.
func (d *NaiveDate) AddAssign(rhs TimeDelta) {
	*d = expect(d.CheckedAddSigned(rhs), "`NaiveDate + TimeDelta` overflowed")
}

// AddMonths is `date + months`. It panics if the resulting date would be out
// of range; use CheckedAddMonths to get an Option instead.
func (d NaiveDate) AddMonths(months Months) NaiveDate {
	return expect(d.CheckedAddMonths(months), "`NaiveDate + Months` out of range")
}

// SubMonths is `date - months`. It panics if the resulting date would be out
// of range; use CheckedSubMonths to get an Option instead.
func (d NaiveDate) SubMonths(months Months) NaiveDate {
	return expect(d.CheckedSubMonths(months), "`NaiveDate - Months` out of range")
}

// AddDays is `date + days`. It panics if the resulting date would be out of
// range; use CheckedAddDays to get an Option instead.
func (d NaiveDate) AddDays(days Days) NaiveDate {
	return expect(d.CheckedAddDays(days), "`NaiveDate + Days` out of range")
}

// SubDays is `date - days`. It panics if the resulting date would be out of
// range; use CheckedSubDays to get an Option instead.
func (d NaiveDate) SubDays(days Days) NaiveDate {
	return expect(d.CheckedSubDays(days), "`NaiveDate - Days` out of range")
}

// Sub is `date - rhs` for a TimeDelta: the date minus the days in rhs. It
// panics if the resulting date would be out of range; use CheckedSubSigned
// to get an Option instead.
func (d NaiveDate) Sub(rhs TimeDelta) NaiveDate {
	return expect(d.CheckedSubSigned(rhs), "`NaiveDate - TimeDelta` overflowed")
}

// SubAssign is `date -= rhs` for a TimeDelta. It panics on overflow.
func (d *NaiveDate) SubAssign(rhs TimeDelta) {
	*d = expect(d.CheckedSubSigned(rhs), "`NaiveDate - TimeDelta` overflowed")
}

// SubNaiveDate is `date - rhs` for two dates: the signed span between them.
// This does not overflow or underflow at all, as all possible output fits in
// the range of TimeDelta.
func (d NaiveDate) SubNaiveDate(rhs NaiveDate) TimeDelta {
	return d.SignedDurationSince(rhs)
}

// NaiveDateFromNaiveDateTime is `NaiveDate::from(naive_datetime)`: the date
// part.
func NaiveDateFromNaiveDateTime(naiveDatetime NaiveDateTime) NaiveDate {
	return naiveDatetime.Date()
}

// Equal is `==` (derived PartialEq).
func (d NaiveDate) Equal(other NaiveDate) bool { return d == other }

// Compare is `cmp` (derived Ord): calendar order.
func (d NaiveDate) Compare(other NaiveDate) int {
	a, b := d.yof(), other.yof()
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	}
	return 0
}

// NaiveDateDaysIterator is the iterator over NaiveDate with a step size of
// one day, which NaiveDate.IterDays returns.
type NaiveDateDaysIterator struct {
	value NaiveDate
}

// Next is `Iterator::next`: the current date, stepping to the day after. It
// has no way to return NaiveDateMax.
func (it *NaiveDateDaysIterator) Next() rs.Option[NaiveDate] {
	// We return the current value, and have no way to return `NaiveDate::MAX`.
	current := it.value
	// This can't panic because current is < NaiveDate::MAX:
	next, ok := current.SuccOpt().Get()
	if !ok {
		return rs.None[NaiveDate]()
	}
	it.value = next
	return rs.Some(current)
}

// SizeHint is `Iterator::size_hint`: the exact number of dates left.
func (it *NaiveDateDaysIterator) SizeHint() (int, rs.Option[int]) {
	exactSize := NaiveDateMax.SignedDurationSince(it.value).NumDays()
	return int(exactSize), rs.Some(int(exactSize))
}

// NextBack is `DoubleEndedIterator::next_back`: the current date, stepping to
// the day before. It has no way to return NaiveDateMin.
func (it *NaiveDateDaysIterator) NextBack() rs.Option[NaiveDate] {
	// We return the current value, and have no way to return `NaiveDate::MIN`.
	current := it.value
	previous, ok := current.PredOpt().Get()
	if !ok {
		return rs.None[NaiveDate]()
	}
	it.value = previous
	return rs.Some(current)
}

// NaiveDateWeeksIterator is the iterator over NaiveDate with a step size of
// one week, which NaiveDate.IterWeeks returns.
type NaiveDateWeeksIterator struct {
	value NaiveDate
}

// Next is `Iterator::next`: the current date, stepping to a week later.
func (it *NaiveDateWeeksIterator) Next() rs.Option[NaiveDate] {
	current := it.value
	next, ok := current.CheckedAddDays(DaysNew(7)).Get()
	if !ok {
		return rs.None[NaiveDate]()
	}
	it.value = next
	return rs.Some(current)
}

// SizeHint is `Iterator::size_hint`: the exact number of dates left.
func (it *NaiveDateWeeksIterator) SizeHint() (int, rs.Option[int]) {
	exactSize := NaiveDateMax.SignedDurationSince(it.value).NumWeeks()
	return int(exactSize), rs.Some(int(exactSize))
}

// NextBack is `DoubleEndedIterator::next_back`: the current date, stepping to
// a week earlier.
func (it *NaiveDateWeeksIterator) NextBack() rs.Option[NaiveDate] {
	current := it.value
	previous, ok := current.CheckedSubDays(DaysNew(7)).Get()
	if !ok {
		return rs.None[NaiveDate]()
	}
	it.value = previous
	return rs.Some(current)
}

// DebugString is the Debug form: the ISO 8601 date, such as "2015-09-05".
// A year outside 0 through 9999 has an explicit sign and at least four
// digits ("+12345-06-07", "-0001-12-31").
func (d NaiveDate) DebugString() string {
	year := d.Year()
	m := d.mdf()
	var out []byte
	if 0 <= year && year <= 9999 {
		out = appendHundreds(out, uint8(year/100))
		out = appendHundreds(out, uint8(year%100))
	} else {
		// ISO 8601 requires the explicit sign for out-of-range years
		out = append(out, formatInt(int64(year), 5, true, true)...)
	}

	out = append(out, '-')
	out = appendHundreds(out, uint8(m.month()))
	out = append(out, '-')
	out = appendHundreds(out, uint8(m.day()))
	return string(out)
}

// String is the Display form, which is the Debug form.
func (d NaiveDate) String() string { return d.DebugString() }

// FmtDebug is `impl fmt::Debug for NaiveDate`. It ignores the formatter's
// width and fill.
func (d NaiveDate) FmtDebug(f *rs.Formatter) error { return f.WriteStr(d.DebugString()) }

// FmtDisplay is `impl fmt::Display for NaiveDate`. It ignores the
// formatter's width and fill.
func (d NaiveDate) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(d.String()) }

// NaiveDateFromStr is `str::parse::<NaiveDate>()`: a date in the form
// "2015-09-05". It is the form of the Display and Debug output, read more
// leniently: white space is allowed before each number and before each "-",
// numbers need no zero padding, and a year with a sign may have any number of
// digits.
//
// The error is a ParseError.
func NaiveDateFromStr(s string) (NaiveDate, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, naiveDateFromStrItems); err != nil {
		return NaiveDate{}, err
	}
	return parsed.ToNaiveDate()
}

var naiveDateFromStrItems = []FormatItem{
	&FormatItem_Numeric{F0: FormatNumeric_Year, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: "-"},
	&FormatItem_Numeric{F0: FormatNumeric_Month, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
	&FormatItem_Literal{F0: "-"},
	&FormatItem_Numeric{F0: FormatNumeric_Day, F1: FormatPad_Zero},
	&FormatItem_Space{F0: ""},
}

// NaiveDateDefault is `NaiveDate::default()`: 1970-01-01, the Go zero value.
func NaiveDateDefault() NaiveDate {
	return NaiveDateFromYmdOpt(1970, 1, 1).Unwrap()
}

func cycleToYo(cycle uint32) (uint32, uint32) {
	yearMod400 := cycle / 365
	ordinal0 := cycle % 365
	delta := uint32(yearDeltas[yearMod400])
	if ordinal0 < delta {
		yearMod400--
		ordinal0 += 365 - uint32(yearDeltas[yearMod400])
	} else {
		ordinal0 -= delta
	}
	return yearMod400, ordinal0 + 1
}

func yoToCycle(yearMod400 uint32, ordinal uint32) uint32 {
	return yearMod400*365 + uint32(yearDeltas[yearMod400]) + ordinal - 1
}

func divModFloor(val int32, div int32) (int32, int32) {
	return divEuclid(val, div), remEuclid(val, div)
}

const (
	// naiveDateMaxYear is the maximum year NaiveDate supports.
	naiveDateMaxYear int32 = (math.MaxInt32 >> 13) - 1
	// naiveDateMinYear is the minimum year NaiveDate supports.
	naiveDateMinYear int32 = (math.MinInt32 >> 13) + 1

	ordinalMask  int32 = 0b1_1111_1111_0000
	leapYearMask int32 = 0b1000

	// OL: ordinal and leap year flag.
	// With only these parts of the date an ordinal 366 in a common year would be encoded as
	// `((366 << 1) | 1) << 3`, and in a leap year as `((366 << 1) | 0) << 3`, which is less.
	// This allows for efficiently checking the ordinal exists depending on whether this is a leap year.
	olMask         int32 = ordinalMask | leapYearMask
	naiveDateMaxOl int32 = 366 << 4

	// Weekday of the last day in the preceding year.
	// Allows for quick day of week calculation from the 1-based ordinal.
	weekdayFlagsMask int32 = 0b111

	yearFlagsMask int32 = leapYearMask | weekdayFlagsMask
)

var yearDeltas = [401]uint8{
	0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6,
	6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12,
	12, 13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18,
	18, 19, 19, 19, 19, 20, 20, 20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 23, 23, 23, 23, 24, 24, 24,
	24, 25, 25, 25, 25, 25, 25, 25, 25, 26, 26, 26, 26, 27, 27, 27, 27, 28, 28, 28, 28, 29, 29, 29,
	29, 30, 30, 30, 30, 31, 31, 31, 31, 32, 32, 32, 32, 33, 33, 33, 33, 34, 34, 34, 34, 35, 35, 35,
	35, 36, 36, 36, 36, 37, 37, 37, 37, 38, 38, 38, 38, 39, 39, 39, 39, 40, 40, 40, 40, 41, 41, 41,
	41, 42, 42, 42, 42, 43, 43, 43, 43, 44, 44, 44, 44, 45, 45, 45, 45, 46, 46, 46, 46, 47, 47, 47,
	47, 48, 48, 48, 48, 49, 49, 49, 49, 49, 49, 49, 49, 50, 50, 50, 50, 51, 51, 51, 51, 52, 52, 52,
	52, 53, 53, 53, 53, 54, 54, 54, 54, 55, 55, 55, 55, 56, 56, 56, 56, 57, 57, 57, 57, 58, 58, 58,
	58, 59, 59, 59, 59, 60, 60, 60, 60, 61, 61, 61, 61, 62, 62, 62, 62, 63, 63, 63, 63, 64, 64, 64,
	64, 65, 65, 65, 65, 66, 66, 66, 66, 67, 67, 67, 67, 68, 68, 68, 68, 69, 69, 69, 69, 70, 70, 70,
	70, 71, 71, 71, 71, 72, 72, 72, 72, 73, 73, 73, 73, 73, 73, 73, 73, 74, 74, 74, 74, 75, 75, 75,
	75, 76, 76, 76, 76, 77, 77, 77, 77, 78, 78, 78, 78, 79, 79, 79, 79, 80, 80, 80, 80, 81, 81, 81,
	81, 82, 82, 82, 82, 83, 83, 83, 83, 84, 84, 84, 84, 85, 85, 85, 85, 86, 86, 86, 86, 87, 87, 87,
	87, 88, 88, 88, 88, 89, 89, 89, 89, 90, 90, 90, 90, 91, 91, 91, 91, 92, 92, 92, 92, 93, 93, 93,
	93, 94, 94, 94, 94, 95, 95, 95, 95, 96, 96, 96, 96, 97, 97, 97, 97,
}
