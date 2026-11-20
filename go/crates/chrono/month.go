// Port of chrono 0.4.44 src/month.rs

package chrono

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Month is the month of the year.
//
// This enum is just a convenience implementation.
// The month in dates created by DateLike objects does not return this enum.
//
// The discriminants are zero-indexed, as in chrono; the API (MonthTryFromU8,
// NumberFromMonth) is one-indexed.
type Month uint8

const (
	// Month_January is January.
	Month_January Month = 0
	// Month_February is February.
	Month_February Month = 1
	// Month_March is March.
	Month_March Month = 2
	// Month_April is April.
	Month_April Month = 3
	// Month_May is May.
	Month_May Month = 4
	// Month_June is June.
	Month_June Month = 5
	// Month_July is July.
	Month_July Month = 6
	// Month_August is August.
	Month_August Month = 7
	// Month_September is September.
	Month_September Month = 8
	// Month_October is October.
	Month_October Month = 9
	// Month_November is November.
	Month_November Month = 10
	// Month_December is December.
	Month_December Month = 11
)

var _ = rs.RegisterEnum[Month]("Month",
	rs.Const(Month_January, "January"),
	rs.Const(Month_February, "February"),
	rs.Const(Month_March, "March"),
	rs.Const(Month_April, "April"),
	rs.Const(Month_May, "May"),
	rs.Const(Month_June, "June"),
	rs.Const(Month_July, "July"),
	rs.Const(Month_August, "August"),
	rs.Const(Month_September, "September"),
	rs.Const(Month_October, "October"),
	rs.Const(Month_November, "November"),
	rs.Const(Month_December, "December"),
)

// Succ is `Month::succ`: the next month, December wrapping to January.
func (m Month) Succ() Month {
	if m > Month_December {
		panic(rs.Unreachable(uint8(m)))
	}
	if m == Month_December {
		return Month_January
	}
	return m + 1
}

// Pred is `Month::pred`: the previous month, January wrapping to December.
func (m Month) Pred() Month {
	if m > Month_December {
		panic(rs.Unreachable(uint8(m)))
	}
	if m == Month_January {
		return Month_December
	}
	return m - 1
}

// NumberFromMonth is `Month::number_from_month`: a month-of-year number
// starting from January = 1.
func (m Month) NumberFromMonth() uint32 {
	if m > Month_December {
		panic(rs.Unreachable(uint8(m)))
	}
	return uint32(m) + 1
}

// Name is `Month::name`: the English name of the month.
func (m Month) Name() string {
	switch m {
	case Month_January:
		return "January"
	case Month_February:
		return "February"
	case Month_March:
		return "March"
	case Month_April:
		return "April"
	case Month_May:
		return "May"
	case Month_June:
		return "June"
	case Month_July:
		return "July"
	case Month_August:
		return "August"
	case Month_September:
		return "September"
	case Month_October:
		return "October"
	case Month_November:
		return "November"
	case Month_December:
		return "December"
	default:
		panic(rs.Unreachable(uint8(m)))
	}
}

// DebugString is the Debug form, which is derived: the variant's name.
func (m Month) DebugString() string { return m.Name() }

// NumDays is `Month::num_days`: the number of days of the month in year, or
// None when the year is out of chrono's range and the month is February.
func (m Month) NumDays(year int32) rs.Option[uint8] {
	switch m {
	case Month_January:
		return rs.Some[uint8](31)
	case Month_February:
		date, ok := NaiveDateFromYmdOpt(year, 2, 1).Get()
		if !ok {
			return rs.None[uint8]()
		}
		if date.LeapYear() {
			return rs.Some[uint8](29)
		}
		return rs.Some[uint8](28)
	case Month_March:
		return rs.Some[uint8](31)
	case Month_April:
		return rs.Some[uint8](30)
	case Month_May:
		return rs.Some[uint8](31)
	case Month_June:
		return rs.Some[uint8](30)
	case Month_July:
		return rs.Some[uint8](31)
	case Month_August:
		return rs.Some[uint8](31)
	case Month_September:
		return rs.Some[uint8](30)
	case Month_October:
		return rs.Some[uint8](31)
	case Month_November:
		return rs.Some[uint8](30)
	case Month_December:
		return rs.Some[uint8](31)
	default:
		panic(rs.Unreachable(uint8(m)))
	}
}

// Compare is `cmp` (derived Ord): by position in the year.
func (m Month) Compare(other Month) int {
	switch {
	case m < other:
		return -1
	case m > other:
		return 1
	}
	return 0
}

// MonthTryFromU8 is `Month::try_from(u8)`: January is 1 and December 12. Any
// other value is the OutOfRange error.
func MonthTryFromU8(value uint8) (Month, error) {
	if value >= 1 && value <= 12 {
		return Month(value - 1), nil
	}
	return 0, outOfRangeNew()
}

// MonthFromU64 is num_traits' `Month::from_u64`: January is 1. Like the
// crate, it looks only at the low 32 bits.
func MonthFromU64(n uint64) rs.Option[Month] { return MonthFromU32(uint32(n)) }

// MonthFromI64 is num_traits' `Month::from_i64`: January is 1. Like the
// crate, it looks only at the low 32 bits.
func MonthFromI64(n int64) rs.Option[Month] { return MonthFromU32(uint32(n)) }

// MonthFromU32 is num_traits' `Month::from_u32`: January is 1.
func MonthFromU32(n uint32) rs.Option[Month] {
	if n >= 1 && n <= 12 {
		return rs.Some(Month(n - 1))
	}
	return rs.None[Month]()
}

// Months is a duration in calendar months, for the CheckedAddMonths and
// CheckedSubMonths methods.
type Months uint32

var _ = rs.RegisterNewtype[Months]("Months")

// MonthsNew is `Months::new`.
func MonthsNew(num uint32) Months { return Months(num) }

// AsU32 is `Months::as_u32`: the number of months.
func (m Months) AsU32() uint32 { return uint32(m) }

// DebugString is the Debug form, which is derived: "Months(3)".
func (m Months) DebugString() string { return "Months(" + strconv.FormatUint(uint64(m), 10) + ")" }

// ParseMonthError is the error of MonthFromStr.
type ParseMonthError struct{}

// String is the Display form.
func (ParseMonthError) String() string { return "ParseMonthError { .. }" }

// Error makes ParseMonthError a Go error; the text is the Display form.
func (e ParseMonthError) Error() string { return e.String() }

// DebugString is the Debug form.
func (ParseMonthError) DebugString() string { return "ParseMonthError { .. }" }

// FmtDisplay is `impl fmt::Display for ParseMonthError`.
func (e ParseMonthError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is `impl fmt::Debug for ParseMonthError`.
func (e ParseMonthError) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.DebugString()) }
