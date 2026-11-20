// Port of chrono 0.4.44 src/format/parse.rs

package chrono

// Portions copyright (c) 2015, John Nagle.

// Date and time parsing routines.

import (
	"math"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func setWeekdayWithNumDaysFromSunday(p *FormatParsed, v int64) error {
	var weekday Weekday
	switch v {
	case 0:
		weekday = Weekday_Sun
	case 1:
		weekday = Weekday_Mon
	case 2:
		weekday = Weekday_Tue
	case 3:
		weekday = Weekday_Wed
	case 4:
		weekday = Weekday_Thu
	case 5:
		weekday = Weekday_Fri
	case 6:
		weekday = Weekday_Sat
	default:
		return errOutOfRange
	}
	return p.SetWeekday(weekday)
}

func setWeekdayWithNumberFromMonday(p *FormatParsed, v int64) error {
	var weekday Weekday
	switch v {
	case 1:
		weekday = Weekday_Mon
	case 2:
		weekday = Weekday_Tue
	case 3:
		weekday = Weekday_Wed
	case 4:
		weekday = Weekday_Thu
	case 5:
		weekday = Weekday_Fri
	case 6:
		weekday = Weekday_Sat
	case 7:
		weekday = Weekday_Sun
	default:
		return errOutOfRange
	}
	return p.SetWeekday(weekday)
}

func parseRfc2822(parsed *FormatParsed, s string) (string, error) {
	// an adapted RFC 2822 syntax from Section 3.3 and 4.3:
	//
	// c-char      = <any char except '(', ')' and '\\'>
	// c-escape    = "\" <any char>
	// comment     = "(" *(comment / c-char / c-escape) ")" *S
	// date-time   = [ day-of-week "," ] date 1*S time *S *comment
	// day-of-week = *S day-name *S
	// day-name    = "Mon" / "Tue" / "Wed" / "Thu" / "Fri" / "Sat" / "Sun"
	// date        = day month year
	// day         = *S 1*2DIGIT *S
	// month       = 1*S month-name 1*S
	// month-name  = "Jan" / "Feb" / "Mar" / "Apr" / "May" / "Jun" /
	//               "Jul" / "Aug" / "Sep" / "Oct" / "Nov" / "Dec"
	// year        = *S 2*DIGIT *S
	// time        = time-of-day 1*S zone
	// time-of-day = hour ":" minute [ ":" second ]
	// hour        = *S 2DIGIT *S
	// minute      = *S 2DIGIT *S
	// second      = *S 2DIGIT *S
	// zone        = ( "+" / "-" ) 4DIGIT /
	//               "UT" / "GMT" /                  ; same as +0000
	//               "EST" / "CST" / "MST" / "PST" / ; same as -0500 to -0800
	//               "EDT" / "CDT" / "MDT" / "PDT" / ; same as -0400 to -0700
	//               1*(%d65-90 / %d97-122)          ; same as -0000
	//
	// some notes:
	//
	// - quoted characters can be in any mixture of lower and upper cases.
	//
	// - we do not recognize a folding white space (FWS) or comment (CFWS).
	//   for our purposes, instead, we accept any sequence of Unicode
	//   white space characters (denoted here to `S`). For comments, we accept
	//   any text within parentheses while respecting escaped parentheses.
	//   Any actual RFC 2822 parser is expected to parse FWS and/or CFWS themselves
	//   and replace it with a single SP (`%x20`); this is legitimate.
	//
	// - two-digit year < 50 should be interpreted by adding 2000.
	//   two-digit year >= 50 or three-digit year should be interpreted
	//   by adding 1900. note that four-or-more-digit years less than 1000
	//   are *never* affected by this rule.
	//
	// - mismatching day-of-week is always an error, which is consistent to
	//   Chrono's own rules.
	//
	// - zones can range from `-9959` to `+9959`, but `FixedOffset` does not
	//   support offsets larger than 24 hours. this is not *that* problematic
	//   since we do not directly go to a `DateTime` so one can recover
	//   the offset information from `Parsed` anyway.

	s = trimStart(s)

	if rest, weekday, err := scanShortWeekday(s); err == nil {
		if !strings.HasPrefix(rest, ",") {
			return "", errInvalid
		}
		s = rest[1:]
		if err := parsed.SetWeekday(weekday); err != nil {
			return "", err
		}
	}

	s = trimStart(s)
	s, day, err := scanNumber(s, 1, 2)
	if err != nil {
		return "", err
	}
	if err := parsed.SetDay(day); err != nil {
		return "", err
	}
	if s, err = scanSpace(s); err != nil { // mandatory
		return "", err
	}
	s, month0, err := scanShortMonth0(s)
	if err != nil {
		return "", err
	}
	if err := parsed.SetMonth(1 + int64(month0)); err != nil {
		return "", err
	}
	if s, err = scanSpace(s); err != nil { // mandatory
		return "", err
	}

	// distinguish two- and three-digit years from four-digit years
	prevlen := len(s)
	s, year, err := scanNumber(s, 2, math.MaxInt)
	if err != nil {
		return "", err
	}
	yearlen := prevlen - len(s)
	switch {
	case yearlen == 2 && 0 <= year && year <= 49:
		year += 2000 //   47 -> 2047,   05 -> 2005
	case yearlen == 2 && 50 <= year && year <= 99:
		year += 1900 //   79 -> 1979
	case yearlen == 3:
		year += 1900 //  112 -> 2012,  009 -> 1909
	default: // 1987 -> 1987, 0654 -> 0654
	}
	if err := parsed.SetYear(year); err != nil {
		return "", err
	}

	if s, err = scanSpace(s); err != nil { // mandatory
		return "", err
	}
	s, hour, err := scanNumber(s, 2, 2)
	if err != nil {
		return "", err
	}
	if err := parsed.SetHour(hour); err != nil {
		return "", err
	}
	if s, err = scanChar(trimStart(s), ':'); err != nil { // *S ":" *S
		return "", err
	}
	s = trimStart(s)
	s, minute, err := scanNumber(s, 2, 2)
	if err != nil {
		return "", err
	}
	if err := parsed.SetMinute(minute); err != nil {
		return "", err
	}
	if rest, err := scanChar(trimStart(s), ':'); err == nil {
		// [ ":" *S 2DIGIT ]
		var second int64
		if s, second, err = scanNumber(rest, 2, 2); err != nil {
			return "", err
		}
		if err := parsed.SetSecond(second); err != nil {
			return "", err
		}
	}

	if s, err = scanSpace(s); err != nil { // mandatory
		return "", err
	}
	s, offset, err := scanTimezoneOffset2822(s)
	if err != nil {
		return "", err
	}
	if err := parsed.SetOffset(int64(offset)); err != nil {
		return "", err
	}

	// optional comments
	for {
		sOut, err := scanComment2822(s)
		if err != nil {
			break
		}
		s = sOut
	}

	return s, nil
}

func parseRfc3339(s string) (DateTime, error) {
	// an adapted RFC 3339 syntax from Section 5.6:
	//
	// date-fullyear  = 4DIGIT
	// date-month     = 2DIGIT ; 01-12
	// date-mday      = 2DIGIT ; 01-28, 01-29, 01-30, 01-31 based on month/year
	// time-hour      = 2DIGIT ; 00-23
	// time-minute    = 2DIGIT ; 00-59
	// time-second    = 2DIGIT ; 00-58, 00-59, 00-60 based on leap second rules
	// time-secfrac   = "." 1*DIGIT
	// time-numoffset = ("+" / "-") time-hour ":" time-minute
	// time-offset    = "Z" / time-numoffset
	// partial-time   = time-hour ":" time-minute ":" time-second [time-secfrac]
	// full-date      = date-fullyear "-" date-month "-" date-mday
	// full-time      = partial-time time-offset
	// date-time      = full-date "T" full-time
	//
	// some notes:
	//
	// - quoted characters can be in any mixture of lower and upper cases.
	//
	// - it may accept any number of fractional digits for seconds.
	//   for Chrono, this means that we should skip digits past first 9 digits.
	//
	// - unlike RFC 2822, the valid offset ranges from -23:59 to +23:59.
	//   note that this restriction is unique to RFC 3339 and not ISO 8601.
	//   since this is not a typical Chrono behavior, we check it earlier.
	//
	// - For readability a full-date and a full-time may be separated by a space character.

	if len(s) < 19 {
		return DateTime{}, errTooShort
	}

	fixed := s[:19] // we just checked the length
	var d [4]uint8
	for i := range d {
		var err error
		if d[i], err = rfc3339Digit(fixed, i); err != nil {
			return DateTime{}, err
		}
	}
	year := uint16(d[0])*1000 + uint16(d[1])*100 + uint16(d[2])*10 + uint16(d[3])
	if s[4] != '-' {
		return DateTime{}, errInvalid
	}

	month, err := rfc3339TwoDigits(fixed, 5)
	if err != nil {
		return DateTime{}, err
	}
	if s[7] != '-' {
		return DateTime{}, errInvalid
	}

	day, err := rfc3339TwoDigits(fixed, 8)
	if err != nil {
		return DateTime{}, err
	}
	date, ok := NaiveDateFromYmdOpt(int32(year), uint32(month), uint32(day)).Get()
	if !ok {
		return DateTime{}, errOutOfRange
	}

	if !(s[10] == 't' || s[10] == 'T' || s[10] == ' ') {
		return DateTime{}, errInvalid
	}

	hour, err := rfc3339TwoDigits(fixed, 11)
	if err != nil {
		return DateTime{}, err
	}
	if s[13] != ':' {
		return DateTime{}, errInvalid
	}

	min, err := rfc3339TwoDigits(fixed, 14)
	if err != nil {
		return DateTime{}, err
	}
	if s[16] != ':' {
		return DateTime{}, errInvalid
	}

	sec, err := rfc3339TwoDigits(fixed, 17)
	if err != nil {
		return DateTime{}, err
	}
	extraNanos := uint32(0)
	if sec == 60 {
		sec, extraNanos = 59, 1_000_000_000 // rfc3339 allows leap seconds
	}

	var nano uint32
	if len(s) > 19 && s[19] == '.' {
		rest, nanosecond, err := scanNanosecond(s[20:])
		if err != nil {
			return DateTime{}, err
		}
		s = rest
		nano = extraNanos + nanosecond
	} else {
		s = s[19:]
		nano = extraNanos
	}

	time, ok := NaiveTimeFromHmsNanoOpt(uint32(hour), uint32(min), uint32(sec), nano).Get()
	if !ok {
		return DateTime{}, errOutOfRange
	}

	// Max for the hours field is `23`, and for the minutes field `59`.
	s, offset, err := scanTimezoneOffset(s, func(s string) (string, error) { return scanChar(s, ':') }, true, false, true)
	if err != nil {
		return DateTime{}, err
	}
	if s != "" {
		return DateTime{}, errTooLong
	}

	tz, ok := FixedOffsetEastOpt(offset).Get()
	if !ok {
		return DateTime{}, errOutOfRange
	}
	switch result := date.AndTime(time).AndLocalTimezone(tz).(type) {
	case *MappedLocalTime_Single[DateTime]:
		return result.F0, nil
	default:
		// `FixedOffset::with_ymd_and_hms` doesn't return `MappedLocalTime::Ambiguous`
		// and returns `MappedLocalTime::None` on invalid data
		panic(rs.Unreachable())
	}
}

func rfc3339Digit(bytes string, index int) (uint8, error) {
	if isAsciiDigit(bytes[index]) {
		return bytes[index] - '0', nil
	}
	return 0, errInvalid
}

// rfc3339TwoDigits is `digit(fixed, i)? * 10 + digit(fixed, i + 1)?`.
func rfc3339TwoDigits(bytes string, index int) (uint8, error) {
	tens, err := rfc3339Digit(bytes, index)
	if err != nil {
		return 0, err
	}
	ones, err := rfc3339Digit(bytes, index+1)
	if err != nil {
		return 0, err
	}
	return tens*10 + ones, nil
}

// FormatParse is `format::parse`: it tries to parse given string into
// `parsed` with given formatting items. It returns nil when the entire string
// has been parsed (otherwise `parsed` should not be used). There should be no
// trailing string after parsing; use a stray space item (*FormatItem_Space)
// to trim whitespaces.
//
// This particular date and time parser is:
//
//   - Greedy. It will consume the longest possible prefix.
//     For example, `April` is always consumed entirely when the long month name is requested;
//     it equally accepts `Apr`, but prefers the longer prefix in this case.
//
//   - Padding-agnostic (for numeric items).
//     The FormatPad field is completely ignored,
//     so one can prepend any number of whitespace then any number of zeroes before numbers.
//
//   - (Still) obeying the intrinsic parsing width. This allows, for example, parsing `HHMMSS`.
//
// The error is a ParseError.
func FormatParse(parsed *FormatParsed, s string, items []FormatItem) error {
	rest, err := parseInternal(parsed, s, items)
	if err != nil {
		return err
	}
	if rest != "" {
		return errTooLong // if there are trailing chars it is an error
	}
	return nil
}

// FormatParseAndRemainder is `format::parse_and_remainder`: it tries to parse
// given string into `parsed` with given formatting items, and returns the
// unparsed remainder.
//
// This particular date and time parser is:
//
//   - Greedy. It will consume the longest possible prefix.
//     For example, `April` is always consumed entirely when the long month name is requested;
//     it equally accepts `Apr`, but prefers the longer prefix in this case.
//
//   - Padding-agnostic (for numeric items).
//     The FormatPad field is completely ignored,
//     so one can prepend any number of zeroes before numbers.
//
//   - (Still) obeying the intrinsic parsing width. This allows, for example, parsing `HHMMSS`.
//
// The error is a ParseError.
func FormatParseAndRemainder(parsed *FormatParsed, s string, items []FormatItem) (string, error) {
	return parseInternal(parsed, s, items)
}

func parseInternal(parsed *FormatParsed, s string, items []FormatItem) (string, error) {
	for _, item := range items {
		switch item := item.(type) {
		case *FormatItem_Literal:
			prefix := item.F0
			if len(s) < len(prefix) {
				return "", errTooShort
			}
			if !strings.HasPrefix(s, prefix) {
				return "", errInvalid
			}
			s = s[len(prefix):]

		case *FormatItem_OwnedLiteral:
			prefix := item.F0
			if len(s) < len(prefix) {
				return "", errTooShort
			}
			if !strings.HasPrefix(s, prefix) {
				return "", errInvalid
			}
			s = s[len(prefix):]

		case *FormatItem_Space:
			s = trimStart(s)

		case *FormatItem_OwnedSpace:
			s = trimStart(s)

		case *FormatItem_Numeric:
			var width int
			var signed bool
			var set func(*FormatParsed, int64) error
			switch item.F0 {
			case FormatNumeric_Year:
				width, signed, set = 4, true, (*FormatParsed).SetYear
			case FormatNumeric_YearDiv100:
				width, signed, set = 2, false, (*FormatParsed).SetYearDiv100
			case FormatNumeric_YearMod100:
				width, signed, set = 2, false, (*FormatParsed).SetYearMod100
			case FormatNumeric_IsoYear:
				width, signed, set = 4, true, (*FormatParsed).SetIsoyear
			case FormatNumeric_IsoYearDiv100:
				width, signed, set = 2, false, (*FormatParsed).SetIsoyearDiv100
			case FormatNumeric_IsoYearMod100:
				width, signed, set = 2, false, (*FormatParsed).SetIsoyearMod100
			case FormatNumeric_Quarter:
				width, signed, set = 1, false, (*FormatParsed).SetQuarter
			case FormatNumeric_Month:
				width, signed, set = 2, false, (*FormatParsed).SetMonth
			case FormatNumeric_Day:
				width, signed, set = 2, false, (*FormatParsed).SetDay
			case FormatNumeric_WeekFromSun:
				width, signed, set = 2, false, (*FormatParsed).SetWeekFromSun
			case FormatNumeric_WeekFromMon:
				width, signed, set = 2, false, (*FormatParsed).SetWeekFromMon
			case FormatNumeric_IsoWeek:
				width, signed, set = 2, false, (*FormatParsed).SetIsoweek
			case FormatNumeric_NumDaysFromSun:
				width, signed, set = 1, false, setWeekdayWithNumDaysFromSunday
			case FormatNumeric_WeekdayFromMon:
				width, signed, set = 1, false, setWeekdayWithNumberFromMonday
			case FormatNumeric_Ordinal:
				width, signed, set = 3, false, (*FormatParsed).SetOrdinal
			case FormatNumeric_Hour:
				width, signed, set = 2, false, (*FormatParsed).SetHour
			case FormatNumeric_Hour12:
				width, signed, set = 2, false, (*FormatParsed).SetHour12
			case FormatNumeric_Minute:
				width, signed, set = 2, false, (*FormatParsed).SetMinute
			case FormatNumeric_Second:
				width, signed, set = 2, false, (*FormatParsed).SetSecond
			case FormatNumeric_Nanosecond:
				width, signed, set = 9, false, (*FormatParsed).SetNanosecond
			case FormatNumeric_Timestamp:
				width, signed, set = math.MaxInt, false, (*FormatParsed).SetTimestamp
			default:
				panic(rs.Unreachable(uint8(item.F0)))
			}

			s = trimStart(s)
			var v int64
			var err error
			if signed {
				if strings.HasPrefix(s, "-") {
					if s, v, err = scanNumber(s[1:], 1, math.MaxInt); err != nil {
						return "", err
					}
					var ok bool
					if v, ok = checkedSubI64(0, v); !ok {
						return "", errOutOfRange
					}
				} else if strings.HasPrefix(s, "+") {
					if s, v, err = scanNumber(s[1:], 1, math.MaxInt); err != nil {
						return "", err
					}
				} else {
					// if there is no explicit sign, we respect the original `width`
					if s, v, err = scanNumber(s, 1, width); err != nil {
						return "", err
					}
				}
			} else {
				if s, v, err = scanNumber(s, 1, width); err != nil {
					return "", err
				}
			}
			if err := set(parsed, v); err != nil {
				return "", err
			}

		case *FormatItem_Fixed:
			switch item.F0 {
			case FormatFixed_ShortMonthName:
				rest, month0, err := scanShortMonth0(s)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetMonth(int64(month0) + 1); err != nil {
					return "", err
				}

			case FormatFixed_LongMonthName:
				rest, month0, err := scanShortOrLongMonth0(s)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetMonth(int64(month0) + 1); err != nil {
					return "", err
				}

			case FormatFixed_ShortWeekdayName:
				rest, weekday, err := scanShortWeekday(s)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetWeekday(weekday); err != nil {
					return "", err
				}

			case FormatFixed_LongWeekdayName:
				rest, weekday, err := scanShortOrLongWeekday(s)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetWeekday(weekday); err != nil {
					return "", err
				}

			case FormatFixed_LowerAmPm, FormatFixed_UpperAmPm:
				if len(s) < 2 {
					return "", errTooShort
				}
				var ampm bool
				switch string([]byte{s[0] | 32, s[1] | 32}) {
				case "am":
					ampm = false
				case "pm":
					ampm = true
				default:
					return "", errInvalid
				}
				if err := parsed.SetAmpm(ampm); err != nil {
					return "", err
				}
				s = s[2:]

			case FormatFixed_Nanosecond:
				if strings.HasPrefix(s, ".") {
					rest, nano, err := scanNanosecond(s[1:])
					if err != nil {
						return "", err
					}
					s = rest
					if err := parsed.SetNanosecond(int64(nano)); err != nil {
						return "", err
					}
				}

			case FormatFixed_Nanosecond3:
				if strings.HasPrefix(s, ".") {
					rest, nano, err := scanNanosecondFixed(s[1:], 3)
					if err != nil {
						return "", err
					}
					s = rest
					if err := parsed.SetNanosecond(nano); err != nil {
						return "", err
					}
				}

			case FormatFixed_Nanosecond6:
				if strings.HasPrefix(s, ".") {
					rest, nano, err := scanNanosecondFixed(s[1:], 6)
					if err != nil {
						return "", err
					}
					s = rest
					if err := parsed.SetNanosecond(nano); err != nil {
						return "", err
					}
				}

			case FormatFixed_Nanosecond9:
				if strings.HasPrefix(s, ".") {
					rest, nano, err := scanNanosecondFixed(s[1:], 9)
					if err != nil {
						return "", err
					}
					s = rest
					if err := parsed.SetNanosecond(nano); err != nil {
						return "", err
					}
				}

			case formatFixedNanosecond3NoDot:
				if len(s) < 3 {
					return "", errTooShort
				}
				rest, nano, err := scanNanosecondFixed(s, 3)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetNanosecond(nano); err != nil {
					return "", err
				}

			case formatFixedNanosecond6NoDot:
				if len(s) < 6 {
					return "", errTooShort
				}
				rest, nano, err := scanNanosecondFixed(s, 6)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetNanosecond(nano); err != nil {
					return "", err
				}

			case formatFixedNanosecond9NoDot:
				if len(s) < 9 {
					return "", errTooShort
				}
				rest, nano, err := scanNanosecondFixed(s, 9)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetNanosecond(nano); err != nil {
					return "", err
				}

			case FormatFixed_TimezoneName:
				s = strings.TrimLeftFunc(s, func(c rune) bool { return !isWhitespace(c) })

			case FormatFixed_TimezoneOffsetColon,
				FormatFixed_TimezoneOffsetDoubleColon,
				FormatFixed_TimezoneOffsetTripleColon,
				FormatFixed_TimezoneOffset:
				rest, offset, err := scanTimezoneOffset(trimStart(s), scanColonOrSpace, false, false, true)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetOffset(int64(offset)); err != nil {
					return "", err
				}

			case FormatFixed_TimezoneOffsetColonZ, FormatFixed_TimezoneOffsetZ:
				rest, offset, err := scanTimezoneOffset(trimStart(s), scanColonOrSpace, true, false, true)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetOffset(int64(offset)); err != nil {
					return "", err
				}

			case formatFixedTimezoneOffsetPermissive:
				rest, offset, err := scanTimezoneOffset(trimStart(s), scanColonOrSpace, true, true, true)
				if err != nil {
					return "", err
				}
				s = rest
				if err := parsed.SetOffset(int64(offset)); err != nil {
					return "", err
				}

			case FormatFixed_RFC2822:
				rest, err := parseRfc2822(parsed, s)
				if err != nil {
					return "", err
				}
				s = rest

			case FormatFixed_RFC3339:
				// Used for the `%+` specifier, which has the description:
				// "Same as `%Y-%m-%dT%H:%M:%S%.f%:z` (...)
				// This format also supports having a `Z` or `UTC` in place of `%:z`."
				// Use the relaxed parser to match this description.
				rest, err := parseRfc3339Relaxed(parsed, s)
				if err != nil {
					return "", err
				}
				s = rest

			default:
				panic(rs.Unreachable(uint8(item.F0)))
			}

		case *FormatItem_Error:
			return "", errBadFormat

		default:
			panic(rs.Unreachable(item))
		}
	}
	return s, nil
}

// DateTimeFixedOffsetFromStr is `str::parse::<DateTime<FixedOffset>>()`: a
// relaxed form of RFC 3339. A space or a 'T' are accepted as the separator
// between the date and time parts. Additional spaces are allowed between
// each component.
//
// All of these examples are equivalent:
//
//	"2012-12-12T12:12:12Z"
//	"2012-12-12 12:12:12Z"
//	"2012-  12-12T12:  12:12Z"
//
// See parseRfc3339Relaxed for the differences with RFC 3339. The error is a
// ParseError.
func DateTimeFixedOffsetFromStr(s string) (DateTime, error) {
	parsed := FormatParsedNew()
	s, err := parseRfc3339Relaxed(&parsed, s)
	if err != nil {
		return DateTime{}, err
	}
	if trimStart(s) != "" {
		return DateTime{}, errTooLong
	}
	return parsed.ToDatetime()
}

var (
	rfc3339RelaxedDateItems = []FormatItem{
		&FormatItem_Numeric{F0: FormatNumeric_Year, F1: FormatPad_Zero},
		&FormatItem_Space{F0: ""},
		&FormatItem_Literal{F0: "-"},
		&FormatItem_Numeric{F0: FormatNumeric_Month, F1: FormatPad_Zero},
		&FormatItem_Space{F0: ""},
		&FormatItem_Literal{F0: "-"},
		&FormatItem_Numeric{F0: FormatNumeric_Day, F1: FormatPad_Zero},
	}
	rfc3339RelaxedTimeItems = []FormatItem{
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
)

// parseRfc3339Relaxed accepts a relaxed form of RFC3339.
//
// Differences with RFC3339:
//   - Values don't require padding to two digits.
//   - Years outside the range 0...=9999 are accepted, but they must include a sign.
//   - `UTC` is accepted as a valid timezone name/offset (for compatibility with the debug format of
//     `DateTime<Utc>`.
//   - There can be spaces between any of the components.
//   - The colon in the offset may be missing.
func parseRfc3339Relaxed(parsed *FormatParsed, s string) (string, error) {
	s, err := parseInternal(parsed, s, rfc3339RelaxedDateItems)
	if err != nil {
		return "", err
	}

	if len(s) == 0 {
		return "", errTooShort
	}
	if !(s[0] == 't' || s[0] == 'T' || s[0] == ' ') {
		return "", errInvalid
	}
	s = s[1:]

	if s, err = parseInternal(parsed, s, rfc3339RelaxedTimeItems); err != nil {
		return "", err
	}
	s = trimStart(s)
	var offset int32
	if len(s) >= 3 && equalIgnoreAsciiCase("UTC", s[:3]) {
		s, offset = s[3:], 0
	} else {
		if s, offset, err = scanTimezoneOffset(s, scanColonOrSpace, true, false, true); err != nil {
			return "", err
		}
	}
	if err := parsed.SetOffset(int64(offset)); err != nil {
		return "", err
	}
	return s, nil
}
