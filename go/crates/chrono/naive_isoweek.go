// Port of chrono 0.4.44 src/naive/isoweek.rs

package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// IsoWeek is the ISO 8601 week.
//
// This type, combined with Weekday, constitutes the ISO 8601 week date.
// One can retrieve this type from the existing Datelike types
// via the IsoWeek method.
type IsoWeek struct {
	// Note that this allows for larger year range than NaiveDate.
	// This is crucial because we have an edge case for the first and last week supported,
	// which year number might not match the calendar year number.
	ywf int32 // (year << 10) | (week << 4) | flag
}

// isoWeekFromYof returns the corresponding IsoWeek from the year and the
// ordinal with year flags.
//
// Internal use only. We don't expose the public constructor for IsoWeek for now
// because the year range for the week date and the calendar date do not match, and
// it is confusing to have a date that is out of range in one and not in another.
// Currently we sidestep this issue by making IsoWeek fully dependent of Datelike.
func isoWeekFromYof(year int32, ordinal uint32, flags yearFlags) IsoWeek {
	rawweek := (ordinal + flags.isoweekDelta()) / 7
	var week uint32
	if rawweek < 1 {
		// previous year
		prevlastweek := yearFlagsFromYear(year - 1).nisoweeks()
		year, week = year-1, prevlastweek
	} else {
		lastweek := flags.nisoweeks()
		if rawweek > lastweek {
			// next year
			year, week = year+1, 1
		} else {
			week = rawweek
		}
	}
	flags = yearFlagsFromYear(year)
	return IsoWeek{ywf: (year << 10) | int32(week<<4) | int32(flags)}
}

// Year is `IsoWeek::year`: the year number for this ISO week.
func (w IsoWeek) Year() int32 { return w.ywf >> 10 }

// Week is `IsoWeek::week`: the ISO week number starting from 1 (1 through
// 53).
func (w IsoWeek) Week() uint32 { return uint32((w.ywf >> 4) & 0x3f) }

// Week0 is `IsoWeek::week0`: the ISO week number starting from 0 (0 through
// 52).
func (w IsoWeek) Week0() uint32 { return uint32((w.ywf>>4)&0x3f) - 1 }

// Equal is `==` (derived PartialEq).
func (w IsoWeek) Equal(other IsoWeek) bool { return w == other }

// Compare is `cmp` (derived Ord).
func (w IsoWeek) Compare(other IsoWeek) int {
	switch {
	case w.ywf < other.ywf:
		return -1
	case w.ywf > other.ywf:
		return 1
	}
	return 0
}

// DebugString is the Debug form: the ISO 8601 week, such as "2015-W36".
func (w IsoWeek) DebugString() string {
	year := w.Year()
	week := w.Week()
	if 0 <= year && year <= 9999 {
		return formatInt(int64(year), 4, false, true) + "-W" + formatUintZero(uint64(week), 2)
	}
	// ISO 8601 requires the explicit sign for out-of-range years
	return formatInt(int64(year), 5, true, true) + "-W" + formatUintZero(uint64(week), 2)
}

// FmtDebug is `impl fmt::Debug for IsoWeek`.
func (w IsoWeek) FmtDebug(f *rs.Formatter) error { return f.WriteStr(w.DebugString()) }
