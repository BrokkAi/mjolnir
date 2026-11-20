// Port of chrono 0.4.44 src/weekday.rs

package chrono

import "github.com/BrokkAi/mjolnir/go/rs"

// Weekday is the day of the week.
//
// The order of the days of week depends on the context.
// (This is why this type does not implement PartialOrd or Ord.)
// One should prefer `*_from_monday` or `*_from_sunday` methods to get the correct result.
type Weekday uint8

const (
	// Weekday_Mon is Monday.
	Weekday_Mon Weekday = 0
	// Weekday_Tue is Tuesday.
	Weekday_Tue Weekday = 1
	// Weekday_Wed is Wednesday.
	Weekday_Wed Weekday = 2
	// Weekday_Thu is Thursday.
	Weekday_Thu Weekday = 3
	// Weekday_Fri is Friday.
	Weekday_Fri Weekday = 4
	// Weekday_Sat is Saturday.
	Weekday_Sat Weekday = 5
	// Weekday_Sun is Sunday.
	Weekday_Sun Weekday = 6
)

var _ = rs.RegisterEnum[Weekday]("Weekday",
	rs.Const(Weekday_Mon, "Mon"),
	rs.Const(Weekday_Tue, "Tue"),
	rs.Const(Weekday_Wed, "Wed"),
	rs.Const(Weekday_Thu, "Thu"),
	rs.Const(Weekday_Fri, "Fri"),
	rs.Const(Weekday_Sat, "Sat"),
	rs.Const(Weekday_Sun, "Sun"),
)

// Succ is `Weekday::succ`: the next day in the week.
func (w Weekday) Succ() Weekday {
	switch w {
	case Weekday_Mon:
		return Weekday_Tue
	case Weekday_Tue:
		return Weekday_Wed
	case Weekday_Wed:
		return Weekday_Thu
	case Weekday_Thu:
		return Weekday_Fri
	case Weekday_Fri:
		return Weekday_Sat
	case Weekday_Sat:
		return Weekday_Sun
	case Weekday_Sun:
		return Weekday_Mon
	default:
		panic(rs.Unreachable(uint8(w)))
	}
}

// Pred is `Weekday::pred`: the previous day in the week.
func (w Weekday) Pred() Weekday {
	switch w {
	case Weekday_Mon:
		return Weekday_Sun
	case Weekday_Tue:
		return Weekday_Mon
	case Weekday_Wed:
		return Weekday_Tue
	case Weekday_Thu:
		return Weekday_Wed
	case Weekday_Fri:
		return Weekday_Thu
	case Weekday_Sat:
		return Weekday_Fri
	case Weekday_Sun:
		return Weekday_Sat
	default:
		panic(rs.Unreachable(uint8(w)))
	}
}

// NumberFromMonday is `Weekday::number_from_monday`: a day-of-week number
// starting from Monday = 1. (ISO 8601 weekday number)
func (w Weekday) NumberFromMonday() uint32 { return w.DaysSince(Weekday_Mon) + 1 }

// NumberFromSunday is `Weekday::number_from_sunday`: a day-of-week number
// starting from Sunday = 1.
func (w Weekday) NumberFromSunday() uint32 { return w.DaysSince(Weekday_Sun) + 1 }

// NumDaysFromMonday is `Weekday::num_days_from_monday`: a day-of-week number
// starting from Monday = 0.
func (w Weekday) NumDaysFromMonday() uint32 { return w.DaysSince(Weekday_Mon) }

// NumDaysFromSunday is `Weekday::num_days_from_sunday`: a day-of-week number
// starting from Sunday = 0.
func (w Weekday) NumDaysFromSunday() uint32 { return w.DaysSince(Weekday_Sun) }

// DaysSince is `Weekday::days_since`: the number of days from other to w,
// going forward (0 through 6).
func (w Weekday) DaysSince(other Weekday) uint32 {
	lhs := uint32(w)
	rhs := uint32(other)
	if lhs < rhs {
		return 7 + lhs - rhs
	}
	return lhs - rhs
}

// String is the Display form: the three-letter English name. The Debug form
// is the same text.
func (w Weekday) String() string {
	switch w {
	case Weekday_Mon:
		return "Mon"
	case Weekday_Tue:
		return "Tue"
	case Weekday_Wed:
		return "Wed"
	case Weekday_Thu:
		return "Thu"
	case Weekday_Fri:
		return "Fri"
	case Weekday_Sat:
		return "Sat"
	case Weekday_Sun:
		return "Sun"
	default:
		panic(rs.Unreachable(uint8(w)))
	}
}

// DebugString is the Debug form, which is derived: the variant's name.
func (w Weekday) DebugString() string { return w.String() }

// FmtDisplay is `impl fmt::Display for Weekday`, which honours width, fill
// and alignment.
func (w Weekday) FmtDisplay(f *rs.Formatter) error { return f.Pad(w.String()) }

// WeekdayTryFromU8 is `Weekday::try_from(u8)`: Monday is 0 and Sunday 6. Any
// other value is the OutOfRange error.
func WeekdayTryFromU8(value uint8) (Weekday, error) {
	if value <= 6 {
		return Weekday(value), nil
	}
	return 0, outOfRangeNew()
}

// WeekdayFromI64 is num_traits' `Weekday::from_i64`: Monday is 0.
func WeekdayFromI64(n int64) rs.Option[Weekday] {
	if n >= 0 && n <= 6 {
		return rs.Some(Weekday(n))
	}
	return rs.None[Weekday]()
}

// WeekdayFromU64 is num_traits' `Weekday::from_u64`: Monday is 0.
func WeekdayFromU64(n uint64) rs.Option[Weekday] {
	if n <= 6 {
		return rs.Some(Weekday(n))
	}
	return rs.None[Weekday]()
}

// WeekdayFromU32 is num_traits' `Weekday::from_u32`: Monday is 0.
func WeekdayFromU32(n uint32) rs.Option[Weekday] { return WeekdayFromU64(uint64(n)) }

// ParseWeekdayError is the error of WeekdayFromStr.
type ParseWeekdayError struct{}

// String is the Display form, which is the Debug form.
func (ParseWeekdayError) String() string { return "ParseWeekdayError { .. }" }

// Error makes ParseWeekdayError a Go error; the text is the Display form.
func (e ParseWeekdayError) Error() string { return e.String() }

// DebugString is the Debug form.
func (ParseWeekdayError) DebugString() string { return "ParseWeekdayError { .. }" }

// FmtDisplay is `impl fmt::Display for ParseWeekdayError`.
func (e ParseWeekdayError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is `impl fmt::Debug for ParseWeekdayError`.
func (e ParseWeekdayError) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.DebugString()) }

// the actual `FromStr` implementation is in the `format` module to leverage the existing code
