// Port of chrono 0.4.44 src/naive/mod.rs

package chrono

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// NaiveWeek is a week represented by a NaiveDate and a Weekday which is the
// first day of the week.
type NaiveWeek struct {
	date  NaiveDate
	start Weekday
}

// naiveWeekNew creates a new NaiveWeek.
func naiveWeekNew(date NaiveDate, start Weekday) NaiveWeek {
	return NaiveWeek{date: date, start: start}
}

// FirstDay is `NaiveWeek::first_day`: a date representing the first day of
// the week. It panics if that day is before NaiveDateMin.
func (w NaiveWeek) FirstDay() NaiveDate {
	return expect(w.CheckedFirstDay(), "first weekday out of range for `NaiveDate`")
}

// CheckedFirstDay is `NaiveWeek::checked_first_day`: the first day of the
// week, or None if it is before NaiveDateMin.
func (w NaiveWeek) CheckedFirstDay() rs.Option[NaiveDate] {
	start := int32(w.start.NumDaysFromMonday())
	refDay := int32(w.date.Weekday().NumDaysFromMonday())
	// Calculate the number of days to subtract from `self.date`.
	// Do not construct an intermediate date beyond `self.date`, because that may be out of
	// range if `date` is close to `NaiveDate::MAX`.
	days := start - refDay
	if start > refDay {
		days -= 7
	}
	return w.date.addDays(days)
}

// LastDay is `NaiveWeek::last_day`: a date representing the last day of the
// week. It panics if that day is after NaiveDateMax.
func (w NaiveWeek) LastDay() NaiveDate {
	return expect(w.CheckedLastDay(), "last weekday out of range for `NaiveDate`")
}

// CheckedLastDay is `NaiveWeek::checked_last_day`: the last day of the week,
// or None if it is after NaiveDateMax.
func (w NaiveWeek) CheckedLastDay() rs.Option[NaiveDate] {
	end := int32(w.start.Pred().NumDaysFromMonday())
	refDay := int32(w.date.Weekday().NumDaysFromMonday())
	// Calculate the number of days to add to `self.date`.
	// Do not construct an intermediate date before `self.date` (like with `first_day()`),
	// because that may be out of range if `date` is close to `NaiveDate::MIN`.
	days := end - refDay
	if end < refDay {
		days += 7
	}
	return w.date.addDays(days)
}

// Days is `NaiveWeek::days`: the first and the last day of the week, which
// Rust returns as the inclusive range `first..=last`. It panics if either is
// out of range.
func (w NaiveWeek) Days() (NaiveDate, NaiveDate) {
	// `expect` doesn't work because `RangeInclusive` is not `Copy`
	days, ok := w.CheckedDays().Get()
	if !ok {
		rs.PanicAt(1, "first or last weekday is out of range for `NaiveDate`")
	}
	return days.F0, days.F1
}

// CheckedDays is `NaiveWeek::checked_days`: the first and the last day of the
// week (Rust's `first..=last`), or None if either is out of range.
func (w NaiveWeek) CheckedDays() rs.Option[rs.Tuple2[NaiveDate, NaiveDate]] {
	first, firstOk := w.CheckedFirstDay().Get()
	last, lastOk := w.CheckedLastDay().Get()
	if firstOk && lastOk {
		return rs.Some(rs.T2(first, last))
	}
	return rs.None[rs.Tuple2[NaiveDate, NaiveDate]]()
}

// Equal is `impl PartialEq for NaiveWeek`: two weeks are equal when they
// start on the same day. Like the Rust impl, it panics when a first day is
// out of range.
func (w NaiveWeek) Equal(other NaiveWeek) bool {
	return w.FirstDay() == other.FirstDay()
}

// HashKey is `impl Hash for NaiveWeek`: the week's first day.
func (w NaiveWeek) HashKey() string { return w.FirstDay().String() }

// DebugString is the Debug form, which is derived.
func (w NaiveWeek) DebugString() string {
	return "NaiveWeek { date: " + w.date.DebugString() + ", start: " + w.start.DebugString() + " }"
}

// FmtDebug is the derived `Debug`.
func (w NaiveWeek) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("NaiveWeek").Field("date", w.date).Field("start", w.start).Finish()
}

// Days is a duration in calendar days.
//
// This is useful because when using TimeDelta it is possible that adding
// `TimeDelta::days(1)` doesn't increment the day value as expected due to it
// being a fixed number of seconds. This difference applies only when dealing
// with DateTime<TimeZone> data types and in other cases `TimeDelta::days(n)`
// and `Days::new(n)` are equivalent.
type Days uint64

var _ = rs.RegisterNewtype[Days]("Days")

// DaysNew is `Days::new`: a Days from a number of days.
func DaysNew(num uint64) Days { return Days(num) }

// DebugString is the Debug form, which is derived: "Days(3)".
func (d Days) DebugString() string { return "Days(" + strconv.FormatUint(uint64(d), 10) + ")" }
