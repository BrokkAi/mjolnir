// Port of chrono 0.4.44 src/naive/internals.rs

package chrono

import "strconv"

// yearFlags is the dominical letter of a year.
//
// yearFlags are used as the last four bits of NaiveDate, mdf and IsoWeek.
//
// There are 14 possible classes of year in the Gregorian calendar:
// common and leap years starting with Monday through Sunday.
//
// The yearFlags stores this information into 4 bits `LWWW`. `L` is the leap year flag, with `1`
// for the common year (this simplifies validating an ordinal in NaiveDate). `WWW` is a non-zero
// Weekday of the last day in the preceding year.
type yearFlags uint8

// Weekday of the last day in the preceding year.
// Allows for quick day of week calculation from the 1-based ordinal.
const (
	yearStartsAfterMonday    uint8 = 7 // non-zero to allow use with `NonZero*`.
	yearStartsAfterThuesday  uint8 = 1
	yearStartsAfterWednesday uint8 = 2
	yearStartsAfterThursday  uint8 = 3
	yearStartsAfterFriday    uint8 = 4
	yearStartsAfterSaturday  uint8 = 5
	yearStartsAfterSunday    uint8 = 6

	commonYear uint8 = 1 << 3
	leapYear   uint8 = 0 << 3
)

const (
	yearFlagsA  = yearFlags(commonYear | yearStartsAfterSaturday)
	yearFlagsAG = yearFlags(leapYear | yearStartsAfterSaturday)
	yearFlagsB  = yearFlags(commonYear | yearStartsAfterFriday)
	yearFlagsBA = yearFlags(leapYear | yearStartsAfterFriday)
	yearFlagsC  = yearFlags(commonYear | yearStartsAfterThursday)
	yearFlagsCB = yearFlags(leapYear | yearStartsAfterThursday)
	yearFlagsD  = yearFlags(commonYear | yearStartsAfterWednesday)
	yearFlagsDC = yearFlags(leapYear | yearStartsAfterWednesday)
	yearFlagsE  = yearFlags(commonYear | yearStartsAfterThuesday)
	yearFlagsED = yearFlags(leapYear | yearStartsAfterThuesday)
	yearFlagsF  = yearFlags(commonYear | yearStartsAfterMonday)
	yearFlagsFE = yearFlags(leapYear | yearStartsAfterMonday)
	yearFlagsG  = yearFlags(commonYear | yearStartsAfterSunday)
	yearFlagsGF = yearFlags(leapYear | yearStartsAfterSunday)
)

var yearToFlags = [400]yearFlags{
	yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG,
	yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF,
	yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE,
	yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD,
	yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC,
	yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG,
	yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF,
	yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE,
	yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD,
	yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC,
	yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG,
	yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF,
	yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE,
	yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD,
	yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC,
	yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG,
	yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF,
	yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC, yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE,
	yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB, yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD,
	yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA, yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC,
	yearFlagsBA, yearFlagsG, yearFlagsF, yearFlagsE, yearFlagsDC, yearFlagsB, yearFlagsA, yearFlagsG, yearFlagsFE, yearFlagsD, yearFlagsC, yearFlagsB,
	yearFlagsAG, yearFlagsF, yearFlagsE, yearFlagsD, yearFlagsCB, yearFlagsA, yearFlagsG, yearFlagsF, yearFlagsED, yearFlagsC, yearFlagsB, yearFlagsA,
	yearFlagsGF, yearFlagsE, yearFlagsD, yearFlagsC,
}

func yearFlagsFromYear(year int32) yearFlags {
	year = remEuclid(year, 400)
	return yearFlagsFromYearMod400(year)
}

func yearFlagsFromYearMod400(year int32) yearFlags {
	return yearToFlags[year]
}

func (flags yearFlags) ndays() uint32 {
	return 366 - uint32(flags>>3)
}

func (flags yearFlags) isoweekDelta() uint32 {
	delta := uint32(flags & 0b0111)
	if delta < 3 {
		delta += 7
	}
	return delta
}

func (flags yearFlags) nisoweeks() uint32 {
	return 52 + ((0b0000_0100_0000_0110 >> uint(flags)) & 1)
}

// DebugString is the Debug form: the dominical letters.
func (flags yearFlags) DebugString() string {
	switch flags {
	case 0o15:
		return "A"
	case 0o05:
		return "AG"
	case 0o14:
		return "B"
	case 0o04:
		return "BA"
	case 0o13:
		return "C"
	case 0o03:
		return "CB"
	case 0o12:
		return "D"
	case 0o02:
		return "DC"
	case 0o11:
		return "E"
	case 0o01:
		return "ED"
	case 0o10:
		return "F?"
	case 0o00:
		return "FE?" // non-canonical
	case 0o17:
		return "F"
	case 0o07:
		return "FE"
	case 0o16:
		return "G"
	case 0o06:
		return "GF"
	default:
		return "YearFlags(" + strconv.Itoa(int(flags)) + ")"
	}
}

// OL: (ordinal << 1) | leap year flag
const (
	maxOl  uint32 = 366 << 1 // `(366 << 1) | 1` would be day 366 in a non-leap year
	maxMdl uint32 = (12 << 6) | (31 << 1) | 1
)

// The next table are adjustment values to convert a date encoded as month-day-leapyear to
// ordinal-leapyear. OL = MDL - adjustment.
// Dates that do not exist are encoded as 0 (`XX`).
var mdlToOl = [maxMdl + 1]int8{
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 64, 64, 64, 64, 64,
	64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
	64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
	64, 64, 64, 64, 64, 64, 64, 64, 0, 0, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66,
	66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66,
	66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 0, 0, 0, 0, 0,
	0, 0, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74,
	72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74,
	72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 0, 0, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 76, 74, 76, 0, 0, 0, 0, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80,
	78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80,
	78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80,
	0, 0, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82,
	80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82,
	80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 0, 0, 0, 0, 84, 86, 84, 86, 84, 86,
	84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86,
	84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86,
	84, 86, 84, 86, 84, 86, 84, 86, 0, 0, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88,
	86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88,
	86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88,
	0, 0, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90,
	88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90,
	88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 0, 0, 0, 0, 92, 94, 92, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 0, 0, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96,
	94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96,
	94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 0, 0,
	0, 0, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100,
	98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100,
	98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100,
}

var olToMdl = [maxOl + 1]uint8{
	0, 0, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
	64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
	64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 66, 66, 66, 66, 66, 66, 66, 66,
	66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66,
	66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66,
	66, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74,
	72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74,
	72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 74, 72, 76, 74, 76, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76, 74, 76,
	74, 76, 74, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80,
	78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80,
	78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 80, 78, 82, 80, 82, 80, 82, 80, 82,
	80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82,
	80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82, 80, 82,
	80, 82, 80, 82, 80, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86,
	84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86,
	84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 86, 84, 88, 86, 88, 86, 88,
	86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88,
	86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88, 86, 88,
	86, 88, 86, 88, 86, 88, 86, 88, 86, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90,
	88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90,
	88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 90, 88, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 94,
	92, 94, 92, 94, 92, 94, 92, 94, 92, 94, 92, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96,
	94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96,
	94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 96, 94, 100,
	98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100,
	98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100,
	98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98, 100, 98,
}

// mdf is month, day of month and year flags: `(month << 9) | (day << 4) | flags`
// `M_MMMD_DDDD_LFFF`
//
// The whole bits except for the least 3 bits are referred as `Mdl` (month, day of month, and leap
// year flag), which is an index to the mdlToOl lookup table.
//
// The conversion between the packed calendar date (mdf) and the ordinal date (NaiveDate) is
// based on the moderately-sized lookup table (~1.5KB) and the packed representation is chosen for
// efficient lookup.
//
// The methods of mdf validate their inputs as late as possible. Dates that can't exist, like
// February 30, can still be represented. This allows the validation to be combined with the final
// table lookup, which is good for performance.
type mdf uint32

// mdfNew makes a new mdf value from month, day and yearFlags.
//
// This method doesn't fully validate the range of the month and day parameters, only as
// much as what can't be deferred until later. The year flags are trusted to be correct.
//
// It returns false if `month > 12` or `day > 31`.
func mdfNew(month, day uint32, flags yearFlags) (mdf, bool) {
	if month <= 12 && day <= 31 {
		return mdf((month << 9) | (day << 4) | uint32(flags)), true
	}
	return 0, false
}

// mdfFromOl makes a new mdf value from an i32 with an ordinal and a leap year flag, and year
// flags.
//
// The ol is trusted to be valid, and the flags are trusted to match it.
func mdfFromOl(ol int32, flags yearFlags) mdf {
	// Array is indexed from `[2..=MAX_OL]`, with a `0` index having a meaningless value.
	return mdf(((uint32(ol) + uint32(olToMdl[ol])) << 3) | uint32(flags))
}

// month returns the month of this mdf.
func (m mdf) month() uint32 { return uint32(m) >> 9 }

// withMonth replaces the month of this mdf, keeping the day and flags.
// It returns false if `month > 12`.
func (m mdf) withMonth(month uint32) (mdf, bool) {
	if month > 12 {
		return 0, false
	}
	return mdf((uint32(m) & 0b1_1111_1111) | (month << 9)), true
}

// day returns the day of this mdf.
func (m mdf) day() uint32 { return (uint32(m) >> 4) & 0b1_1111 }

// withDay replaces the day of this mdf, keeping the month and flags.
// It returns false if `day > 31`.
func (m mdf) withDay(day uint32) (mdf, bool) {
	if day > 31 {
		return 0, false
	}
	return mdf((uint32(m) &^ 0b1_1111_0000) | (day << 4)), true
}

// withFlags replaces the flags of this mdf, keeping the month and day.
func (m mdf) withFlags(flags yearFlags) mdf {
	return mdf((uint32(m) &^ 0b1111) | uint32(flags))
}

// ordinal returns the ordinal that corresponds to this mdf.
//
// This does a table lookup to calculate the corresponding ordinal. It returns false if
// `month == 0` or `day == 0`, or if the given day does not exist in the given month.
func (m mdf) ordinal() (uint32, bool) {
	mdl := uint32(m) >> 3
	v := mdlToOl[mdl]
	if v == 0 {
		return 0, false
	}
	return (mdl - uint32(uint8(v))) >> 1, true
}

// yearFlags returns the year flags of this mdf.
func (m mdf) yearFlags() yearFlags { return yearFlags(uint32(m) & 0b1111) }

// ordinalAndFlags returns the ordinal that corresponds to this mdf, encoded as a value including
// year flags.
//
// This does a table lookup to calculate the corresponding ordinal. It returns false if
// `month == 0` or `day == 0`, or if the given day does not exist in the given month.
func (m mdf) ordinalAndFlags() (int32, bool) {
	mdl := uint32(m) >> 3
	v := mdlToOl[mdl]
	if v == 0 {
		return 0, false
	}
	return int32(m) - (int32(v) << 3), true
}

// valid reports whether the mdf is a date that exists. It is the Rust tests'
// helper.
func (m mdf) valid() bool {
	mdl := uint32(m) >> 3
	return mdlToOl[mdl] > 0
}

// DebugString is the Debug form.
func (m mdf) DebugString() string {
	flags := uint32(m) & 0b1111
	octal := strconv.FormatUint(uint64(flags), 8)
	for len(octal) < 2 {
		octal = "0" + octal
	}
	return "Mdf((" + strconv.FormatUint(uint64(uint32(m)>>9), 10) + " << 9) | (" +
		strconv.FormatUint(uint64((uint32(m)>>4)&0b1_1111), 10) + " << 4) | 0o" + octal +
		" /*" + yearFlags(flags).DebugString() + "*/)"
}
