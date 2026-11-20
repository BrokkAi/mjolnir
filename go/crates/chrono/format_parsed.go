// Port of chrono 0.4.44 src/format/parsed.rs

package chrono

// A collection of parsed date and time items.
// They can be constructed incrementally while being checked for consistency.

import (
	"math"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// FormatParsed is a type to hold parsed fields of date and time that can
// check all fields are consistent (`format::Parsed`).
//
// There are three classes of methods:
//
//   - `Set*` methods to set fields you have available. They do a basic range
//     check, and if the same field is set more than once it is checked for
//     consistency.
//
//   - `To*` methods try to make a concrete date and time value out of set
//     fields. They fully check that all fields are consistent and whether the
//     date/datetime exists.
//
//   - Methods to inspect the parsed fields, which carry the suffix `Method`
//     because the fields themselves are exported, as they are public in Rust.
//
// FormatParsed is used internally by all parsing functions in chrono. It is
// a public type so that it can be used to write custom parsers that reuse
// the resolving algorithm, or to inspect the results of a string parsed with
// chrono without converting it to concrete types.
//
// # Resolving algorithm
//
// Resolving date/time parts is littered with lots of corner cases, which is
// why common date/time parsers do not implement it correctly.
//
// Chrono provides a complete resolution algorithm that checks all fields for
// consistency via the FormatParsed type.
//
// As an easy example, consider RFC 2822. The RFC 2822 date and time format
// has a day of the week part, which should be consistent with the other date
// parts. But a `strptime`-based parse would happily accept inconsistent
// input:
//
//	>>> import time
//	>>> time.strptime('Wed, 31 Dec 2014 04:26:40 +0000',
//	                  '%a, %d %b %Y %H:%M:%S +0000')
//	time.struct_time(tm_year=2014, tm_mon=12, tm_mday=31,
//	                 tm_hour=4, tm_min=26, tm_sec=40,
//	                 tm_wday=2, tm_yday=365, tm_isdst=-1)
//	>>> time.strptime('Thu, 31 Dec 2014 04:26:40 +0000',
//	                  '%a, %d %b %Y %H:%M:%S +0000')
//	time.struct_time(tm_year=2014, tm_mon=12, tm_mday=31,
//	                 tm_hour=4, tm_min=26, tm_sec=40,
//	                 tm_wday=3, tm_yday=365, tm_isdst=-1)
//
// The zero value has no field set, as `Parsed::new()` has.
type FormatParsed struct {
	Year          rs.Option[int32]   `rs:"year"`
	YearDiv100    rs.Option[int32]   `rs:"year_div_100"`
	YearMod100    rs.Option[int32]   `rs:"year_mod_100"`
	Isoyear       rs.Option[int32]   `rs:"isoyear"`
	IsoyearDiv100 rs.Option[int32]   `rs:"isoyear_div_100"`
	IsoyearMod100 rs.Option[int32]   `rs:"isoyear_mod_100"`
	Quarter       rs.Option[uint32]  `rs:"quarter"`
	Month         rs.Option[uint32]  `rs:"month"`
	WeekFromSun   rs.Option[uint32]  `rs:"week_from_sun"`
	WeekFromMon   rs.Option[uint32]  `rs:"week_from_mon"`
	Isoweek       rs.Option[uint32]  `rs:"isoweek"`
	Weekday       rs.Option[Weekday] `rs:"weekday"`
	Ordinal       rs.Option[uint32]  `rs:"ordinal"`
	Day           rs.Option[uint32]  `rs:"day"`
	HourDiv12     rs.Option[uint32]  `rs:"hour_div_12"`
	HourMod12     rs.Option[uint32]  `rs:"hour_mod_12"`
	Minute        rs.Option[uint32]  `rs:"minute"`
	Second        rs.Option[uint32]  `rs:"second"`
	Nanosecond    rs.Option[uint32]  `rs:"nanosecond"`
	Timestamp     rs.Option[int64]   `rs:"timestamp"`
	Offset        rs.Option[int32]   `rs:"offset"`
}

// FmtDebug is the derived `Debug`, which also shows the Rust struct's private
// unit field.
func (p FormatParsed) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Parsed").
		Field("year", p.Year).
		Field("year_div_100", p.YearDiv100).
		Field("year_mod_100", p.YearMod100).
		Field("isoyear", p.Isoyear).
		Field("isoyear_div_100", p.IsoyearDiv100).
		Field("isoyear_mod_100", p.IsoyearMod100).
		Field("quarter", p.Quarter).
		Field("month", p.Month).
		Field("week_from_sun", p.WeekFromSun).
		Field("week_from_mon", p.WeekFromMon).
		Field("isoweek", p.Isoweek).
		Field("weekday", p.Weekday).
		Field("ordinal", p.Ordinal).
		Field("day", p.Day).
		Field("hour_div_12", p.HourDiv12).
		Field("hour_mod_12", p.HourMod12).
		Field("minute", p.Minute).
		Field("second", p.Second).
		Field("nanosecond", p.Nanosecond).
		Field("timestamp", p.Timestamp).
		Field("offset", p.Offset).
		Field("_dummy", rs.Unit{}).
		Finish()
}

// setIfConsistent checks if `old` is either empty or has the same value as
// `new` (i.e. "consistent"), and if it is empty, sets `old` to `new` as well.
func setIfConsistent[T comparable](old *rs.Option[T], new T) error {
	if oldValue, ok := old.Get(); ok && oldValue != new {
		return errImpossible
	}
	*old = rs.Some(new)
	return nil
}

// FormatParsedNew is `Parsed::new`: the initial value of parsed parts.
func FormatParsedNew() FormatParsed {
	return FormatParsed{}
}

// FormatParsedDefault is `Parsed::default()`: no field set.
func FormatParsedDefault() FormatParsed {
	return FormatParsed{}
}

// SetYear is `Parsed::set_year`: it sets the `year` field to the given
// value.
//
// The value can be negative, unlike the `year_div_100` and `year_mod_100`
// fields.
//
// It returns the "out of range" ParseError if value is outside the range of
// an i32, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetYear(value int64) error {
	if value < math.MinInt32 || value > math.MaxInt32 {
		return errOutOfRange
	}
	return setIfConsistent(&p.Year, int32(value))
}

// SetYearDiv100 is `Parsed::set_year_div_100`: it sets the `year_div_100`
// field to the given value.
//
// It returns the "out of range" ParseError if value is negative or if it is
// greater than i32::MAX, and the "impossible" one if this field was already
// set to a different value.
func (p *FormatParsed) SetYearDiv100(value int64) error {
	if !(0 <= value && value <= math.MaxInt32) {
		return errOutOfRange
	}
	return setIfConsistent(&p.YearDiv100, int32(value))
}

// SetYearMod100 is `Parsed::set_year_mod_100`: it sets the `year_mod_100`
// field to the given value.
//
// When set it implies that the year is not negative.
//
// If this field is set while the `year_div_100` field is missing (and the
// full `year` field is also not set), it assumes a default value for the
// `year_div_100` field. The default is 19 when `year_mod_100 >= 70` and 20
// otherwise.
//
// It returns the "out of range" ParseError if value is negative or if it is
// greater than 99, and the "impossible" one if this field was already set to
// a different value.
func (p *FormatParsed) SetYearMod100(value int64) error {
	if !(0 <= value && value < 100) {
		return errOutOfRange
	}
	return setIfConsistent(&p.YearMod100, int32(value))
}

// SetIsoyear is `Parsed::set_isoyear`: it sets the `isoyear` field, that is
// part of an ISO 8601 week date, to the given value.
//
// The value can be negative, unlike the `isoyear_div_100` and
// `isoyear_mod_100` fields.
//
// It returns the "out of range" ParseError if value is outside the range of
// an i32, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetIsoyear(value int64) error {
	if value < math.MinInt32 || value > math.MaxInt32 {
		return errOutOfRange
	}
	return setIfConsistent(&p.Isoyear, int32(value))
}

// SetIsoyearDiv100 is `Parsed::set_isoyear_div_100`: it sets the
// `isoyear_div_100` field, that is part of an ISO 8601 week date, to the
// given value.
//
// It returns the "out of range" ParseError if value is negative or if it is
// greater than i32::MAX, and the "impossible" one if this field was already
// set to a different value.
func (p *FormatParsed) SetIsoyearDiv100(value int64) error {
	if !(0 <= value && value <= math.MaxInt32) {
		return errOutOfRange
	}
	return setIfConsistent(&p.IsoyearDiv100, int32(value))
}

// SetIsoyearMod100 is `Parsed::set_isoyear_mod_100`: it sets the
// `isoyear_mod_100` field, that is part of an ISO 8601 week date, to the
// given value.
//
// When set it implies that the year is not negative.
//
// If this field is set while the `isoyear_div_100` field is missing (and the
// full `isoyear` field is also not set), it assumes a default value for the
// `isoyear_div_100` field. The default is 19 when `year_mod_100 >= 70` and
// 20 otherwise.
//
// It returns the "out of range" ParseError if value is negative or if it is
// greater than 99, and the "impossible" one if this field was already set to
// a different value.
func (p *FormatParsed) SetIsoyearMod100(value int64) error {
	if !(0 <= value && value < 100) {
		return errOutOfRange
	}
	return setIfConsistent(&p.IsoyearMod100, int32(value))
}

// SetQuarter is `Parsed::set_quarter`: it sets the `quarter` field to the
// given value.
//
// Quarter 1 starts in January.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-4, and the "impossible" one if this field was already set to a different
// value.
func (p *FormatParsed) SetQuarter(value int64) error {
	if !(1 <= value && value <= 4) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Quarter, uint32(value))
}

// SetMonth is `Parsed::set_month`: it sets the `month` field to the given
// value.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-12, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetMonth(value int64) error {
	if !(1 <= value && value <= 12) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Month, uint32(value))
}

// SetWeekFromSun is `Parsed::set_week_from_sun`: it sets the `week_from_sun`
// week number field to the given value.
//
// Week 1 starts at the first Sunday of January.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-53, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetWeekFromSun(value int64) error {
	if !(0 <= value && value <= 53) {
		return errOutOfRange
	}
	return setIfConsistent(&p.WeekFromSun, uint32(value))
}

// SetWeekFromMon is `Parsed::set_week_from_mon`: it sets the `week_from_mon`
// week number field to the given value. Set the `year` field first.
//
// Week 1 starts at the first Monday of January.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-53, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetWeekFromMon(value int64) error {
	if !(0 <= value && value <= 53) {
		return errOutOfRange
	}
	return setIfConsistent(&p.WeekFromMon, uint32(value))
}

// SetIsoweek is `Parsed::set_isoweek`: it sets the `isoweek` field for an
// ISO 8601 week date to the given value.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-53, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetIsoweek(value int64) error {
	if !(1 <= value && value <= 53) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Isoweek, uint32(value))
}

// SetWeekday is `Parsed::set_weekday`: it sets the `weekday` field to the
// given value.
//
// It returns the "impossible" ParseError if this field was already set to a
// different value.
func (p *FormatParsed) SetWeekday(value Weekday) error {
	return setIfConsistent(&p.Weekday, value)
}

// SetOrdinal is `Parsed::set_ordinal`: it sets the `ordinal` (day of the
// year) field to the given value.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-366, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetOrdinal(value int64) error {
	if !(1 <= value && value <= 366) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Ordinal, uint32(value))
}

// SetDay is `Parsed::set_day`: it sets the `day` of the month field to the
// given value.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-31, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetDay(value int64) error {
	if !(1 <= value && value <= 31) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Day, uint32(value))
}

// SetAmpm is `Parsed::set_ampm`: it sets the `hour_div_12` am/pm field to
// the given value.
//
// false indicates AM and true indicates PM.
//
// It returns the "impossible" ParseError if this field was already set to a
// different value.
func (p *FormatParsed) SetAmpm(value bool) error {
	hourDiv12 := uint32(0)
	if value {
		hourDiv12 = 1
	}
	return setIfConsistent(&p.HourDiv12, hourDiv12)
}

// SetHour12 is `Parsed::set_hour12`: it sets the `hour_mod_12` field, for
// the hour number in 12-hour clocks, to the given value.
//
// value must be the canonical value of the hour on the clock, so 12 for
// noon and midnight, not 0.
//
// It returns the "out of range" ParseError if value is not in the range
// 1-12, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetHour12(value int64) error {
	if !(1 <= value && value <= 12) {
		return errOutOfRange
	}
	if value == 12 {
		value = 0
	}
	return setIfConsistent(&p.HourMod12, uint32(value))
}

// SetHour is `Parsed::set_hour`: it sets the `hour_div_12` and `hour_mod_12`
// fields to the given value, derived from an hour number in 24-hour clocks.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-23, and the "impossible" one if one of the fields was already set to a
// different value.
func (p *FormatParsed) SetHour(value int64) error {
	var hourDiv12, hourMod12 uint32
	switch {
	case 0 <= value && value <= 11:
		hourDiv12, hourMod12 = 0, uint32(value)
	case 12 <= value && value <= 23:
		hourDiv12, hourMod12 = 1, uint32(value)-12
	default:
		return errOutOfRange
	}
	if err := setIfConsistent(&p.HourDiv12, hourDiv12); err != nil {
		return err
	}
	return setIfConsistent(&p.HourMod12, hourMod12)
}

// SetMinute is `Parsed::set_minute`: it sets the `minute` field to the given
// value.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-59, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetMinute(value int64) error {
	if !(0 <= value && value <= 59) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Minute, uint32(value))
}

// SetSecond is `Parsed::set_second`: it sets the `second` field to the given
// value.
//
// The value can be 60 in the case of a leap second.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-60, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetSecond(value int64) error {
	if !(0 <= value && value <= 60) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Second, uint32(value))
}

// SetNanosecond is `Parsed::set_nanosecond`: it sets the `nanosecond` field
// to the given value.
//
// This is the number of nanoseconds since the whole second.
//
// It returns the "out of range" ParseError if value is not in the range
// 0-999,999,999, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetNanosecond(value int64) error {
	if !(0 <= value && value <= 999_999_999) {
		return errOutOfRange
	}
	return setIfConsistent(&p.Nanosecond, uint32(value))
}

// SetTimestamp is `Parsed::set_timestamp`: it sets the `timestamp` field to
// the given value.
//
// A Unix timestamp is defined as the number of non-leap seconds since
// midnight UTC on January 1, 1970.
//
// It returns the "impossible" ParseError if this field was already set to a
// different value.
func (p *FormatParsed) SetTimestamp(value int64) error {
	return setIfConsistent(&p.Timestamp, value)
}

// SetOffset is `Parsed::set_offset`: it sets the `offset` field to the given
// value.
//
// The offset is in seconds from local time to UTC.
//
// It returns the "out of range" ParseError if value is outside the range of
// an i32, and the "impossible" one if this field was already set to a
// different value.
func (p *FormatParsed) SetOffset(value int64) error {
	if value < math.MinInt32 || value > math.MaxInt32 {
		return errOutOfRange
	}
	return setIfConsistent(&p.Offset, int32(value))
}

func resolveYear(y rs.Option[int32], q rs.Option[int32], r rs.Option[int32]) (rs.Option[int32], error) {
	yValue, yOk := y.Get()
	qValue, qOk := q.Get()
	rValue, rOk := r.Get()
	rIsMod := rOk && 0 <= rValue && rValue <= 99
	switch {
	// if there is no further information, simply return the given full year.
	// this is a common case, so let's avoid division here.
	case !qOk && !rOk:
		return y, nil

	// if there is a full year *and* also quotient and/or modulo,
	// check if present quotient and/or modulo is consistent to the full year.
	// since the presence of those fields means a positive full year,
	// we should filter a negative full year first.
	case yOk && (rIsMod || !rOk):
		if yValue < 0 {
			return rs.None[int32](), errImpossible
		}
		q_ := yValue / 100
		r_ := yValue % 100
		if q.UnwrapOr(q_) == q_ && r.UnwrapOr(r_) == r_ {
			return rs.Some(yValue), nil
		}
		return rs.None[int32](), errImpossible

	// the full year is missing but we have quotient and modulo.
	// reconstruct the full year. make sure that the result is always positive.
	case !yOk && qOk && rIsMod:
		if qValue < 0 {
			return rs.None[int32](), errImpossible
		}
		year, ok := checkedMulI32(qValue, 100)
		if ok {
			year, ok = checkedAddI32(year, rValue)
		}
		if !ok {
			return rs.None[int32](), errOutOfRange
		}
		return rs.Some(year), nil

	// we only have modulo. try to interpret a modulo as a conventional two-digit year.
	// note: we are affected by Rust issue #18060. avoid multiple range patterns.
	case !yOk && !qOk && rIsMod:
		if rValue < 70 {
			return rs.Some(rValue + 2000), nil
		}
		return rs.Some(rValue + 1900), nil

	// otherwise it is an out-of-bound or insufficient condition.
	case !yOk && qOk && !rOk:
		return rs.None[int32](), errNotEnough
	default:
		return rs.None[int32](), errOutOfRange
	}
}

// ToNaiveDate is `Parsed::to_naive_date`: a parsed naive date out of given
// fields.
//
// This method is able to determine the date from given subset of fields:
//
//   - Year, month, day.
//   - Year, day of the year (ordinal).
//   - Year, week number counted from Sunday or Monday, day of the week.
//   - ISO week date.
//
// Gregorian year and ISO week date year can have their century number
// (`*_div_100`) omitted, the two-digit year is used to guess the century
// number then.
//
// It delegates to the chosen date and checks every other field against it.
//
// It returns the "impossible" ParseError if any of the date fields conflict,
// the "not enough" one if there are not enough fields set to create a date,
// and the "out of range" one if any of the date fields is set to a value
// beyond their acceptable range, if the value would be outside the range of
// a NaiveDate, or if the date does not exist.
func (p *FormatParsed) ToNaiveDate() (NaiveDate, error) {
	givenYear, err := resolveYear(p.Year, p.YearDiv100, p.YearMod100)
	if err != nil {
		return NaiveDate{}, err
	}
	givenIsoyear, err := resolveYear(p.Isoyear, p.IsoyearDiv100, p.IsoyearMod100)
	if err != nil {
		return NaiveDate{}, err
	}

	// verify the normal year-month-day date.
	verifyYmd := func(date NaiveDate) bool {
		year := date.Year()
		yearDiv100, yearMod100 := rs.None[int32](), rs.None[int32]() // they should be empty to be consistent
		if year >= 0 {
			yearDiv100, yearMod100 = rs.Some(year/100), rs.Some(year%100)
		}
		month := date.Month()
		day := date.Day()
		return p.Year.UnwrapOr(year) == year &&
			p.YearDiv100.Or(yearDiv100) == yearDiv100 &&
			p.YearMod100.Or(yearMod100) == yearMod100 &&
			p.Month.UnwrapOr(month) == month &&
			p.Day.UnwrapOr(day) == day
	}

	// verify the ISO week date.
	verifyIsoweekdate := func(date NaiveDate) bool {
		week := date.IsoWeek()
		isoyear := week.Year()
		isoweek := week.Week()
		weekday := date.Weekday()
		isoyearDiv100, isoyearMod100 := rs.None[int32](), rs.None[int32]() // they should be empty to be consistent
		if isoyear >= 0 {
			isoyearDiv100, isoyearMod100 = rs.Some(isoyear/100), rs.Some(isoyear%100)
		}
		return p.Isoyear.UnwrapOr(isoyear) == isoyear &&
			p.IsoyearDiv100.Or(isoyearDiv100) == isoyearDiv100 &&
			p.IsoyearMod100.Or(isoyearMod100) == isoyearMod100 &&
			p.Isoweek.UnwrapOr(isoweek) == isoweek &&
			p.Weekday.UnwrapOr(weekday) == weekday
	}

	// verify the ordinal and other (non-ISO) week dates.
	verifyOrdinal := func(date NaiveDate) bool {
		ordinal := date.Ordinal()
		weekFromSun := date.weeksFrom(Weekday_Sun)
		weekFromMon := date.weeksFrom(Weekday_Mon)
		parsedWeekFromSun, parsedWeekFromMon := weekFromSun, weekFromMon
		if v, ok := p.WeekFromSun.Get(); ok {
			parsedWeekFromSun = int32(v)
		}
		if v, ok := p.WeekFromMon.Get(); ok {
			parsedWeekFromMon = int32(v)
		}
		return p.Ordinal.UnwrapOr(ordinal) == ordinal &&
			parsedWeekFromSun == weekFromSun &&
			parsedWeekFromMon == weekFromMon
	}

	// test several possibilities.
	// tries to construct a full `NaiveDate` as much as possible, then verifies that
	// it is consistent with other given fields.
	var verified bool
	var parsedDate NaiveDate
	year, hasYear := givenYear.Get()
	isoyear, hasIsoyear := givenIsoyear.Get()
	month, hasMonth := p.Month.Get()
	day, hasDay := p.Day.Get()
	ordinal, hasOrdinal := p.Ordinal.Get()
	weekFromSun, hasWeekFromSun := p.WeekFromSun.Get()
	weekFromMon, hasWeekFromMon := p.WeekFromMon.Get()
	isoweek, hasIsoweek := p.Isoweek.Get()
	weekday, hasWeekday := p.Weekday.Get()
	switch {
	case hasYear && hasMonth && hasDay:
		// year, month, day
		date, ok := NaiveDateFromYmdOpt(year, month, day).Get()
		if !ok {
			return NaiveDate{}, errOutOfRange
		}
		verified, parsedDate = verifyIsoweekdate(date) && verifyOrdinal(date), date

	case hasYear && hasOrdinal:
		// year, day of the year
		date, ok := NaiveDateFromYoOpt(year, ordinal).Get()
		if !ok {
			return NaiveDate{}, errOutOfRange
		}
		verified, parsedDate = verifyYmd(date) && verifyIsoweekdate(date) && verifyOrdinal(date), date

	case hasYear && hasWeekFromSun && hasWeekday:
		// year, week (starting at 1st Sunday), day of the week
		date, err := resolveWeekDate(year, weekFromSun, weekday, Weekday_Sun)
		if err != nil {
			return NaiveDate{}, err
		}
		verified, parsedDate = verifyYmd(date) && verifyIsoweekdate(date) && verifyOrdinal(date), date

	case hasYear && hasWeekFromMon && hasWeekday:
		// year, week (starting at 1st Monday), day of the week
		date, err := resolveWeekDate(year, weekFromMon, weekday, Weekday_Mon)
		if err != nil {
			return NaiveDate{}, err
		}
		verified, parsedDate = verifyYmd(date) && verifyIsoweekdate(date) && verifyOrdinal(date), date

	case hasIsoyear && hasIsoweek && hasWeekday:
		// ISO year, week, day of the week
		date, ok := NaiveDateFromIsoywdOpt(isoyear, isoweek, weekday).Get()
		if !ok {
			return NaiveDate{}, errOutOfRange
		}
		verified, parsedDate = verifyYmd(date) && verifyOrdinal(date), date

	default:
		return NaiveDate{}, errNotEnough
	}

	if !verified {
		return NaiveDate{}, errImpossible
	} else if parsed, ok := p.Quarter.Get(); ok {
		if parsed != parsedDate.Quarter() {
			return NaiveDate{}, errImpossible
		}
	}

	return parsedDate, nil
}

// ToNaiveTime is `Parsed::to_naive_time`: a parsed naive time out of given
// fields.
//
// This method is able to determine the time from given subset of fields:
//
//   - Hour, minute. (second and nanosecond assumed to be 0)
//   - Hour, minute, second. (nanosecond assumed to be 0)
//   - Hour, minute, second, nanosecond.
//
// It is able to handle leap seconds when given second is 60.
//
// It returns the "out of range" ParseError if any of the time fields is set
// to a value beyond their acceptable range, and the "not enough" one if an
// hour field is missing, if AM/PM is missing in a 12-hour clock, if minutes
// are missing, or if seconds are missing while the nanosecond field is
// present.
func (p *FormatParsed) ToNaiveTime() (NaiveTime, error) {
	hourDiv12, ok := p.HourDiv12.Get()
	if !ok {
		return NaiveTime{}, errNotEnough
	}
	if hourDiv12 > 1 {
		return NaiveTime{}, errOutOfRange
	}
	hourMod12, ok := p.HourMod12.Get()
	if !ok {
		return NaiveTime{}, errNotEnough
	}
	if hourMod12 > 11 {
		return NaiveTime{}, errOutOfRange
	}
	hour := hourDiv12*12 + hourMod12

	minute, ok := p.Minute.Get()
	if !ok {
		return NaiveTime{}, errNotEnough
	}
	if minute > 59 {
		return NaiveTime{}, errOutOfRange
	}

	// we allow omitting seconds or nanoseconds, but they should be in the range.
	var second, nano uint32
	switch v := p.Second.UnwrapOr(0); {
	case v <= 59:
		second, nano = v, 0
	case v == 60:
		second, nano = 59, 1_000_000_000
	default:
		return NaiveTime{}, errOutOfRange
	}
	if v, ok := p.Nanosecond.Get(); ok {
		switch {
		case v <= 999_999_999 && p.Second.IsSome():
			nano += v
		case v <= 999_999_999:
			return NaiveTime{}, errNotEnough // second is missing
		default:
			return NaiveTime{}, errOutOfRange
		}
	}

	time, ok := NaiveTimeFromHmsNanoOpt(hour, minute, second, nano).Get()
	if !ok {
		return NaiveTime{}, errOutOfRange
	}
	return time, nil
}

// ToNaiveDatetimeWithOffset is `Parsed::to_naive_datetime_with_offset`: a
// parsed naive date and time out of given fields, except for the offset
// field.
//
// The offset is assumed to have a given value. It is not compared against
// the offset field set in the FormatParsed type, so it is allowed to be
// inconsistent.
//
// This method is able to determine the combined date and time from date and
// time fields or from a single timestamp field. It checks all fields are
// consistent with each other.
//
// It returns the "impossible" ParseError if any of the date fields conflict,
// or if a timestamp conflicts with any of the other fields, the "not enough"
// one if there are not enough fields set to create a datetime, and the "out
// of range" one if any of the fields is set to a value beyond their
// acceptable range, if the value would be outside the range of a
// NaiveDateTime, or if the date does not exist.
func (p *FormatParsed) ToNaiveDatetimeWithOffset(offset int32) (NaiveDateTime, error) {
	date, dateErr := p.ToNaiveDate()
	time, timeErr := p.ToNaiveTime()
	if dateErr == nil && timeErr == nil {
		datetime := date.AndTime(time)

		// verify the timestamp field if any
		// the following is safe, `timestamp` is very limited in range
		timestamp := datetime.AndUtc().Timestamp() - int64(offset)
		if givenTimestamp, ok := p.Timestamp.Get(); ok {
			// if `datetime` represents a leap second, it might be off by one second.
			if givenTimestamp != timestamp &&
				!(datetime.Nanosecond() >= 1_000_000_000 && givenTimestamp == timestamp+1) {
				return NaiveDateTime{}, errImpossible
			}
		}

		return datetime, nil
	} else if timestamp, ok := p.Timestamp.Get(); ok {
		// if date and time is problematic already, there is no point proceeding.
		// we at least try to give a correct error though.
		switch {
		case dateErr == errOutOfRange || timeErr == errOutOfRange:
			return NaiveDateTime{}, errOutOfRange
		case dateErr == errImpossible || timeErr == errImpossible:
			return NaiveDateTime{}, errImpossible
		default: // one of them is insufficient
		}

		// reconstruct date and time fields from timestamp
		ts, ok := checkedAddI64(timestamp, int64(offset))
		if !ok {
			return NaiveDateTime{}, errOutOfRange
		}
		fromTimestamp, ok := DateTimeFromTimestampSecs(ts).Get()
		if !ok {
			return NaiveDateTime{}, errOutOfRange
		}
		datetime := fromTimestamp.NaiveUtc()

		// fill year, ordinal, hour, minute and second fields from timestamp.
		// if existing fields are consistent, this will allow the full date/time reconstruction.
		parsed := *p
		if parsed.Second == rs.Some[uint32](60) {
			// `datetime.second()` cannot be 60, so this is the only case for a leap second.
			switch datetime.Second() {
			// it's okay, just do not try to overwrite the existing field.
			case 59:
			// `datetime` is known to be off by one second.
			case 0:
				datetime.SubAssign(TimeDeltaTrySeconds(1).Unwrap())
			// otherwise it is impossible.
			default:
				return NaiveDateTime{}, errImpossible
			}
			// ...and we have the correct candidates for other fields.
		} else {
			if err := parsed.SetSecond(int64(datetime.Second())); err != nil {
				return NaiveDateTime{}, err
			}
		}
		if err := parsed.SetYear(int64(datetime.Year())); err != nil {
			return NaiveDateTime{}, err
		}
		if err := parsed.SetOrdinal(int64(datetime.Ordinal())); err != nil { // more efficient than ymd
			return NaiveDateTime{}, err
		}
		if err := parsed.SetHour(int64(datetime.Hour())); err != nil {
			return NaiveDateTime{}, err
		}
		if err := parsed.SetMinute(int64(datetime.Minute())); err != nil {
			return NaiveDateTime{}, err
		}

		// validate other fields (e.g. week) and return
		date, err := parsed.ToNaiveDate()
		if err != nil {
			return NaiveDateTime{}, err
		}
		time, err := parsed.ToNaiveTime()
		if err != nil {
			return NaiveDateTime{}, err
		}
		return date.AndTime(time), nil
	}
	// reproduce the previous error(s)
	if dateErr != nil {
		return NaiveDateTime{}, dateErr
	}
	if timeErr != nil {
		return NaiveDateTime{}, timeErr
	}
	panic(rs.Unreachable())
}

// ToFixedOffset is `Parsed::to_fixed_offset`: a parsed fixed time zone
// offset out of given fields.
//
// It returns the "out of range" ParseError if the offset is out of range for
// a FixedOffset, and the "not enough" one if the offset field is not set.
func (p *FormatParsed) ToFixedOffset() (FixedOffset, error) {
	offset, ok := p.Offset.Get()
	if !ok {
		return FixedOffset{}, errNotEnough
	}
	fixed, ok := FixedOffsetEastOpt(offset).Get()
	if !ok {
		return FixedOffset{}, errOutOfRange
	}
	return fixed, nil
}

// ToDatetime is `Parsed::to_datetime`: the parsed date and time with time
// zone out of given fields, as a DateTime<FixedOffset>.
//
// This method is able to determine the combined date and time from date,
// time and offset fields, and/or from a single timestamp field. It checks
// all fields are consistent with each other.
//
// It returns the "impossible" ParseError if any of the date fields conflict,
// or if a timestamp conflicts with any of the other fields, the "not enough"
// one if there are not enough fields set to create a datetime, or if there
// is no offset, and the "out of range" one if any of the fields is set to a
// value beyond their acceptable range, if the value would be outside the
// range of a NaiveDateTime or FixedOffset, or if the date does not exist.
func (p *FormatParsed) ToDatetime() (DateTime, error) {
	// If there is no explicit offset, consider a timestamp value as indication of a UTC value.
	var offset int32
	if off, ok := p.Offset.Get(); ok {
		offset = off
	} else if p.Timestamp.IsSome() {
		offset = 0 // UNIX timestamp may assume 0 offset
	} else {
		return DateTime{}, errNotEnough
	}
	datetime, err := p.ToNaiveDatetimeWithOffset(offset)
	if err != nil {
		return DateTime{}, err
	}
	fixed, ok := FixedOffsetEastOpt(offset).Get()
	if !ok {
		return DateTime{}, errOutOfRange
	}

	switch result := fixed.FromLocalDatetime(datetime).(type) {
	case *MappedLocalTime_None[DateTime]:
		return DateTime{}, errImpossible
	case *MappedLocalTime_Single[DateTime]:
		return result.F0, nil
	case *MappedLocalTime_Ambiguous[DateTime]:
		return DateTime{}, errNotEnough
	default:
		panic(rs.Unreachable(result))
	}
}

// ToDatetimeWithTimezone is `Parsed::to_datetime_with_timezone`: the parsed
// date and time with time zone out of given fields, with an additional
// TimeZone used to interpret and validate the local date.
//
// This method is able to determine the combined date and time from date and
// time, and/or from a single timestamp field. It checks all fields are
// consistent with each other.
//
// If the parsed fields include an UTC offset, it also has to be consistent
// with the offset in the provided tz time zone for that datetime.
//
// It returns the "impossible" ParseError if any of the date fields conflict,
// if a timestamp conflicts with any of the other fields, if the offset field
// is set but differs from the offset at that time in the tz time zone, or if
// the local datetime does not exist in the provided time zone (because it
// falls in a transition due to for example DST); the "not enough" one if
// there are not enough fields set to create a datetime, or if the local time
// in the provided time zone is ambiguous (because it falls in a transition
// due to for example DST) while there is no offset field or timestamp field
// set; and the "out of range" one if the value would be outside the range of
// a NaiveDateTime or FixedOffset, if any of the fields is set to a value
// beyond their acceptable range, or if the date does not exist.
func (p *FormatParsed) ToDatetimeWithTimezone(tz TimeZone) (DateTime, error) {
	// if we have `timestamp` specified, guess an offset from that.
	guessedOffset := int32(0)
	if timestamp, ok := p.Timestamp.Get(); ok {
		// make a naive `DateTime` from given timestamp and (if any) nanosecond.
		// an empty `nanosecond` is always equal to zero, so missing nanosecond is fine.
		nanosecond := p.Nanosecond.UnwrapOr(0)
		fromTimestamp, ok := DateTimeFromTimestamp(timestamp, nanosecond).Get()
		if !ok {
			return DateTime{}, errOutOfRange
		}
		dt := fromTimestamp.NaiveUtc()
		guessedOffset = tz.OffsetFromUtcDatetime(dt).Fix().LocalMinusUtc()
	}

	// checks if the given `DateTime` has a consistent `Offset` with given `self.offset`.
	checkOffset := func(dt DateTime) bool {
		if offset, ok := p.Offset.Get(); ok {
			return dt.Offset().Fix().LocalMinusUtc() == offset
		}
		return true
	}

	// `guessed_offset` should be correct when `self.timestamp` is given.
	// it will be 0 otherwise, but this is fine as the algorithm ignores offset for that case.
	datetime, err := p.ToNaiveDatetimeWithOffset(guessedOffset)
	if err != nil {
		return DateTime{}, err
	}
	switch result := tz.FromLocalDatetime(datetime).(type) {
	case *MappedLocalTime_None[DateTime]:
		return DateTime{}, errImpossible
	case *MappedLocalTime_Single[DateTime]:
		if checkOffset(result.F0) {
			return result.F0, nil
		}
		return DateTime{}, errImpossible
	case *MappedLocalTime_Ambiguous[DateTime]:
		// try to disambiguate two possible local dates by offset.
		min, max := result.F0, result.F1
		switch minOk, maxOk := checkOffset(min), checkOffset(max); {
		case !minOk && !maxOk:
			return DateTime{}, errImpossible
		case !minOk && maxOk:
			return max, nil
		case minOk && !maxOk:
			return min, nil
		default:
			return DateTime{}, errNotEnough
		}
	default:
		panic(rs.Unreachable(result))
	}
}

// YearMethod is `Parsed::year`: the `year` field if set.
func (p *FormatParsed) YearMethod() rs.Option[int32] { return p.Year }

// YearDiv100Method is `Parsed::year_div_100`: the `year_div_100` field if
// set.
func (p *FormatParsed) YearDiv100Method() rs.Option[int32] { return p.YearDiv100 }

// YearMod100Method is `Parsed::year_mod_100`: the `year_mod_100` field if
// set.
func (p *FormatParsed) YearMod100Method() rs.Option[int32] { return p.YearMod100 }

// IsoyearMethod is `Parsed::isoyear`: the `isoyear` field that is part of an
// ISO 8601 week date if set.
func (p *FormatParsed) IsoyearMethod() rs.Option[int32] { return p.Isoyear }

// IsoyearDiv100Method is `Parsed::isoyear_div_100`: the `isoyear_div_100`
// field that is part of an ISO 8601 week date if set.
func (p *FormatParsed) IsoyearDiv100Method() rs.Option[int32] { return p.IsoyearDiv100 }

// IsoyearMod100Method is `Parsed::isoyear_mod_100`: the `isoyear_mod_100`
// field that is part of an ISO 8601 week date if set.
func (p *FormatParsed) IsoyearMod100Method() rs.Option[int32] { return p.IsoyearMod100 }

// QuarterMethod is `Parsed::quarter`: the `quarter` field if set.
func (p *FormatParsed) QuarterMethod() rs.Option[uint32] { return p.Quarter }

// MonthMethod is `Parsed::month`: the `month` field if set.
func (p *FormatParsed) MonthMethod() rs.Option[uint32] { return p.Month }

// WeekFromSunMethod is `Parsed::week_from_sun`: the `week_from_sun` field if
// set.
func (p *FormatParsed) WeekFromSunMethod() rs.Option[uint32] { return p.WeekFromSun }

// WeekFromMonMethod is `Parsed::week_from_mon`: the `week_from_mon` field if
// set.
func (p *FormatParsed) WeekFromMonMethod() rs.Option[uint32] { return p.WeekFromMon }

// IsoweekMethod is `Parsed::isoweek`: the `isoweek` field that is part of an
// ISO 8601 week date if set.
func (p *FormatParsed) IsoweekMethod() rs.Option[uint32] { return p.Isoweek }

// WeekdayMethod is `Parsed::weekday`: the `weekday` field if set.
func (p *FormatParsed) WeekdayMethod() rs.Option[Weekday] { return p.Weekday }

// OrdinalMethod is `Parsed::ordinal`: the `ordinal` (day of the year) field
// if set.
func (p *FormatParsed) OrdinalMethod() rs.Option[uint32] { return p.Ordinal }

// DayMethod is `Parsed::day`: the `day` of the month field if set.
func (p *FormatParsed) DayMethod() rs.Option[uint32] { return p.Day }

// HourDiv12Method is `Parsed::hour_div_12`: the `hour_div_12` field (am/pm)
// if set.
//
// 0 indicates AM and 1 indicates PM.
func (p *FormatParsed) HourDiv12Method() rs.Option[uint32] { return p.HourDiv12 }

// HourMod12Method is `Parsed::hour_mod_12`: the `hour_mod_12` field if set.
func (p *FormatParsed) HourMod12Method() rs.Option[uint32] { return p.HourMod12 }

// MinuteMethod is `Parsed::minute`: the `minute` field if set.
func (p *FormatParsed) MinuteMethod() rs.Option[uint32] { return p.Minute }

// SecondMethod is `Parsed::second`: the `second` field if set.
//
// The value can be 60 in the case of a leap second.
func (p *FormatParsed) SecondMethod() rs.Option[uint32] { return p.Second }

// NanosecondMethod is `Parsed::nanosecond`: the `nanosecond` field if set.
func (p *FormatParsed) NanosecondMethod() rs.Option[uint32] { return p.Nanosecond }

// TimestampMethod is `Parsed::timestamp`: the `timestamp` field if set.
func (p *FormatParsed) TimestampMethod() rs.Option[int64] { return p.Timestamp }

// OffsetMethod is `Parsed::offset`: the `offset` field if set.
func (p *FormatParsed) OffsetMethod() rs.Option[int32] { return p.Offset }

// resolveWeekDate creates a NaiveDate when given a year, week, weekday, and
// the definition at which day of the week a week starts.
//
// Returns the "impossible" ParseError if the resulting date falls in a
// different year, or the "out of range" one if any of the fields is out of
// range.
func resolveWeekDate(year int32, week uint32, weekday Weekday, weekStartDay Weekday) (NaiveDate, error) {
	if week > 53 {
		return NaiveDate{}, errOutOfRange
	}

	firstDayOfYear, ok := NaiveDateFromYoOpt(year, 1).Get()
	if !ok {
		return NaiveDate{}, errOutOfRange
	}
	// Ordinal of the day at which week 1 starts.
	firstWeekStart := 1 + int32(weekStartDay.DaysSince(firstDayOfYear.Weekday()))
	// Number of the `weekday`, which is 0 for the first day of the week.
	weekdayNumber := int32(weekday.DaysSince(weekStartDay))
	ordinal := firstWeekStart + (int32(week)-1)*7 + weekdayNumber
	if ordinal <= 0 {
		return NaiveDate{}, errImpossible
	}
	date, ok := firstDayOfYear.WithOrdinal(uint32(ordinal)).Get()
	if !ok {
		return NaiveDate{}, errImpossible
	}
	return date, nil
}
