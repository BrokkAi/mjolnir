// Port of chrono 0.4.44 src/format/strftime.rs

package chrono

/*
`strftime`/`strptime`-inspired date and time formatting syntax.

## Specifiers

The following specifiers are available both to formatting and parsing.

| Spec. | Example  | Description                                                                |
|-------|----------|----------------------------------------------------------------------------|
|       |          | **DATE SPECIFIERS:**                                                       |
| `%Y`  | `2001`   | The full proleptic Gregorian year, zero-padded to 4 digits. chrono supports years from -262144 to 262143. Note: years before 1 BCE or after 9999 CE, require an initial sign (+/-).|
| `%C`  | `20`     | The proleptic Gregorian year divided by 100, zero-padded to 2 digits. [^1] |
| `%y`  | `01`     | The proleptic Gregorian year modulo 100, zero-padded to 2 digits. [^1]     |
|       |          |                                                                            |
| `%q`  | `1`      | Quarter of year (1-4)                                                      |
| `%m`  | `07`     | Month number (01--12), zero-padded to 2 digits.                            |
| `%b`  | `Jul`    | Abbreviated month name. Always 3 letters.                                  |
| `%B`  | `July`   | Full month name. Also accepts corresponding abbreviation in parsing.       |
| `%h`  | `Jul`    | Same as `%b`.                                                              |
|       |          |                                                                            |
| `%d`  | `08`     | Day number (01--31), zero-padded to 2 digits.                              |
| `%e`  | ` 8`     | Same as `%d` but space-padded. Same as `%_d`.                              |
|       |          |                                                                            |
| `%a`  | `Sun`    | Abbreviated weekday name. Always 3 letters.                                |
| `%A`  | `Sunday` | Full weekday name. Also accepts corresponding abbreviation in parsing.     |
| `%w`  | `0`      | Sunday = 0, Monday = 1, ..., Saturday = 6.                                 |
| `%u`  | `7`      | Monday = 1, Tuesday = 2, ..., Sunday = 7. (ISO 8601)                       |
|       |          |                                                                            |
| `%U`  | `28`     | Week number starting with Sunday (00--53), zero-padded to 2 digits. [^2]   |
| `%W`  | `27`     | Same as `%U`, but week 1 starts with the first Monday in that year instead.|
|       |          |                                                                            |
| `%G`  | `2001`   | Same as `%Y` but uses the year number in ISO 8601 week date. [^3]          |
| `%g`  | `01`     | Same as `%y` but uses the year number in ISO 8601 week date. [^3]          |
| `%V`  | `27`     | Same as `%U` but uses the week number in ISO 8601 week date (01--53). [^3] |
|       |          |                                                                            |
| `%j`  | `189`    | Day of the year (001--366), zero-padded to 3 digits.                       |
|       |          |                                                                            |
| `%D`  | `07/08/01`    | Month-day-year format. Same as `%m/%d/%y`.                            |
| `%x`  | `07/08/01`    | Locale's date representation (e.g., 12/31/99).                        |
| `%F`  | `2001-07-08`  | Year-month-day format (ISO 8601). Same as `%Y-%m-%d`.                 |
| `%v`  | ` 8-Jul-2001` | Day-month-year format. Same as `%e-%b-%Y`.                            |
|       |          |                                                                            |
|       |          | **TIME SPECIFIERS:**                                                       |
| `%H`  | `00`     | Hour number (00--23), zero-padded to 2 digits.                             |
| `%k`  | ` 0`     | Same as `%H` but space-padded. Same as `%_H`.                              |
| `%I`  | `12`     | Hour number in 12-hour clocks (01--12), zero-padded to 2 digits.           |
| `%l`  | `12`     | Same as `%I` but space-padded. Same as `%_I`.                              |
|       |          |                                                                            |
| `%P`  | `am`     | `am` or `pm` in 12-hour clocks.                                            |
| `%p`  | `AM`     | `AM` or `PM` in 12-hour clocks.                                            |
|       |          |                                                                            |
| `%M`  | `34`     | Minute number (00--59), zero-padded to 2 digits.                           |
| `%S`  | `60`     | Second number (00--60), zero-padded to 2 digits. [^4]                      |
| `%f`  | `26490000`    | Number of nanoseconds since last whole second. [^7]                   |
| `%.f` | `.026490`| Decimal fraction of a second. Consumes the leading dot. [^7]               |
| `%.3f`| `.026`        | Decimal fraction of a second with a fixed length of 3.                |
| `%.6f`| `.026490`     | Decimal fraction of a second with a fixed length of 6.                |
| `%.9f`| `.026490000`  | Decimal fraction of a second with a fixed length of 9.                |
| `%3f` | `026`         | Decimal fraction of a second like `%.3f` but without the leading dot. |
| `%6f` | `026490`      | Decimal fraction of a second like `%.6f` but without the leading dot. |
| `%9f` | `026490000`   | Decimal fraction of a second like `%.9f` but without the leading dot. |
|       |               |                                                                       |
| `%R`  | `00:34`       | Hour-minute format. Same as `%H:%M`.                                  |
| `%T`  | `00:34:60`    | Hour-minute-second format. Same as `%H:%M:%S`.                        |
| `%X`  | `00:34:60`    | Locale's time representation (e.g., 23:13:48).                        |
| `%r`  | `12:34:60 AM` | Locale's 12 hour clock time. (e.g., 11:11:04 PM). Falls back to `%X` if the locale does not have a 12 hour clock format. |
|       |          |                                                                            |
|       |          | **TIME ZONE SPECIFIERS:**                                                  |
| `%Z`  | `ACST`   | Local time zone name. Skips all non-whitespace characters during parsing. Identical to `%:z` when formatting. [^8] |
| `%z`  | `+0930`  | Offset from the local time to UTC (with UTC being `+0000`).                |
| `%:z` | `+09:30` | Same as `%z` but with a colon.                                             |
|`%::z`|`+09:30:00`| Offset from the local time to UTC with seconds.                            |
|`%:::z`| `+09`    | Offset from the local time to UTC without minutes.                         |
| `%#z` | `+09`    | *Parsing only:* Same as `%z` but allows minutes to be missing or present.  |
|       |          |                                                                            |
|       |          | **DATE & TIME SPECIFIERS:**                                                |
|`%c`|`Sun Jul  8 00:34:60 2001`|Locale's date and time (e.g., Thu Mar  3 23:05:25 2005).       |
| `%+`  | `2001-07-08T00:34:60.026490+09:30` | ISO 8601 / RFC 3339 date & time format. [^5]     |
|       |               |                                                                       |
| `%s`  | `994518299`   | UNIX timestamp, the number of seconds since 1970-01-01 00:00 UTC. [^6]|
|       |          |                                                                            |
|       |          | **SPECIAL SPECIFIERS:**                                                    |
| `%t`  |          | Literal tab (`\t`).                                                        |
| `%n`  |          | Literal newline (`\n`).                                                    |
| `%%`  |          | Literal percent sign.                                                      |

It is possible to override the default padding behavior of numeric specifiers `%?`.
This is not allowed for other specifiers and will result in the `BAD_FORMAT` error.

Modifier | Description
-------- | -----------
`%-?`    | Suppresses any padding including spaces and zeroes. (e.g. `%j` = `012`, `%-j` = `12`)
`%_?`    | Uses spaces as a padding. (e.g. `%j` = `012`, `%_j` = ` 12`)
`%0?`    | Uses zeroes as a padding. (e.g. `%e` = ` 9`, `%0e` = `09`)

Notes:

[^1]: `%C`, `%y`:
   This is floor division, so 100 BCE (year number -99) will print `-1` and `99` respectively.
   For `%y`, values greater or equal to 70 are interpreted as being in the 20th century,
   values smaller than 70 in the 21st century.

[^2]: `%U`:
   Week 1 starts with the first Sunday in that year.
   It is possible to have week 0 for days before the first Sunday.

[^3]: `%G`, `%g`, `%V`:
   Week 1 is the first week with at least 4 days in that year.
   Week 0 does not exist, so this should be used with `%G` or `%g`.

[^4]: `%S`:
   It accounts for leap seconds, so `60` is possible.

[^5]: `%+`: Same as `%Y-%m-%dT%H:%M:%S%.f%:z`, i.e. 0, 3, 6 or 9 fractional
   digits for seconds and colons in the time zone offset.

   This format also supports having a `Z` or `UTC` in place of `%:z`. They
   are equivalent to `+00:00`.

   Note that all `T`, `Z`, and `UTC` are parsed case-insensitively.

   The typical `strftime` implementations have different (and locale-dependent)
   formats for this specifier. While Chrono's format for `%+` is far more
   stable, it is best to avoid this specifier if you want to control the exact
   output.

[^6]: `%s`:
   This is not padded and can be negative.
   For the purpose of Chrono, it only accounts for non-leap seconds
   so it slightly differs from ISO C `strftime` behavior.

[^7]: `%f`, `%.f`:
   `%f` and `%.f` are notably different formatting specifiers.
   `%f` counts the number of nanoseconds since the last whole second, while `%.f` is a fraction of a
   second.
   Example: 7μs is formatted as `7000` with `%f`, and formatted as `.000007` with `%.f`.

[^8]: `%Z`:
   Since `chrono` is not aware of timezones beyond their offsets, this specifier
   **only prints the offset** when used for formatting. The timezone abbreviation
   will NOT be printed. See [this issue](https://github.com/chronotope/chrono/issues/960)
   for more information.

   Offset will not be populated from the parsed data, nor will it be validated.
   Timezone is completely ignored. Similar to the glibc `strptime` treatment of
   this format code.

   It is not possible to reliably convert from an abbreviation to an offset,
   for example CDT can mean either Central Daylight Time (North America) or
   China Daylight Time.
*/

import (
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// FormatStrftimeItems is the parsing iterator for `strftime`-like format
// strings (`format::StrftimeItems`).
//
// See the table at the top of format_strftime.go for the supported
// formatting specifiers.
//
// FormatStrftimeItems is used in combination with more low-level methods
// such as FormatParse or FormatWithItems.
//
// If formatting or parsing date and time values is not performance-critical,
// the functions `ParseFromStr` and the methods `Format` on types such as
// DateTime are easier to use.
type FormatStrftimeItems struct {
	// Remaining portion of the string.
	remainder string
	// If the current specifier is composed of multiple formatting items (e.g. `%+`),
	// `queue` stores a slice of `Item`s that have to be returned one by one.
	queue   []FormatItem
	lenient bool
}

// FormatStrftimeItemsNew is `StrftimeItems::new`: a new parsing iterator
// from a `strftime`-like format string.
//
// While iterating, the error item (*FormatItem_Error) will be returned if the
// format string contains an invalid or unrecognized formatting specifier.
func FormatStrftimeItemsNew(s string) *FormatStrftimeItems {
	return &FormatStrftimeItems{remainder: s, queue: nil, lenient: false}
}

// FormatStrftimeItemsNewLenient is `StrftimeItems::new_lenient`: the same as
// FormatStrftimeItemsNew, but returns a literal item (the specifier's own
// text) instead of the error item.
//
// Useful for formatting according to potentially invalid format strings.
func FormatStrftimeItemsNewLenient(s string) *FormatStrftimeItems {
	return &FormatStrftimeItems{remainder: s, queue: nil, lenient: true}
}

// Parse is `StrftimeItems::parse`: the format string as a slice of
// formatting items.
//
// If you need to format or parse multiple values with the same format
// string, it is more efficient to convert it to a slice of formatting items
// than to re-parse the format string on every use.
//
// It returns the ParseError "bad or unsupported format string" if the format
// string contains an invalid or unrecognized formatting specifier and the
// FormatStrftimeItems wasn't constructed with FormatStrftimeItemsNewLenient.
func (items *FormatStrftimeItems) Parse() ([]FormatItem, error) {
	out := []FormatItem{}
	for {
		item, ok := items.Next().Get()
		if !ok {
			return out, nil
		}
		if _, isError := item.(*FormatItem_Error); isError {
			return nil, errBadFormat
		}
		out = append(out, item)
	}
}

// ParseToOwned is `StrftimeItems::parse_to_owned`: the format string as a
// slice of items that contain no references to slices of the format string.
// In Go the items of Parse do not borrow either; this differs from it only
// in returning the owned variants of the literal and space items.
func (items *FormatStrftimeItems) ParseToOwned() ([]FormatItem, error) {
	out := []FormatItem{}
	for {
		item, ok := items.Next().Get()
		if !ok {
			return out, nil
		}
		if _, isError := item.(*FormatItem_Error); isError {
			return nil, errBadFormat
		}
		out = append(out, item.ToOwned())
	}
}

// Collect is `items.collect::<Vec<_>>()`: every remaining item, with the
// error item in place of each invalid specifier. It is what the `Format` and
// `ParseFromStr` functions hand to the formatter and the parser.
func (items *FormatStrftimeItems) Collect() []FormatItem {
	out := []FormatItem{}
	for {
		item, ok := items.Next().Get()
		if !ok {
			return out
		}
		out = append(out, item)
	}
}

func (items *FormatStrftimeItems) parseNextItem(remainder string) (string, FormatItem, bool) {
	if remainder == "" {
		return "", nil, false
	}
	first, _ := utf8.DecodeRuneInString(remainder)
	var original string
	switch {
	// the next item is a specifier
	case first == '%':
		original, remainder = remainder, remainder[1:]

	// the next item is space
	case isWhitespace(first):
		// `%` is not a whitespace, so `c != '%'` is redundant
		nextspec := strings.IndexFunc(remainder, func(c rune) bool { return !isWhitespace(c) })
		if nextspec < 0 {
			nextspec = len(remainder)
		}
		rs.Assert(nextspec > 0)
		item := &FormatItem_Space{F0: remainder[:nextspec]}
		remainder = remainder[nextspec:]
		return remainder, item, true

	// the next item is literal
	default:
		nextspec := strings.IndexFunc(remainder, func(c rune) bool { return isWhitespace(c) || c == '%' })
		if nextspec < 0 {
			nextspec = len(remainder)
		}
		rs.Assert(nextspec > 0)
		item := &FormatItem_Literal{F0: remainder[:nextspec]}
		remainder = remainder[nextspec:]
		return remainder, item, true
	}

	// next takes the next character of the specifier; ok is false at the
	// premature end of string.
	next := func() (rune, bool) {
		if remainder == "" {
			return 0, false
		}
		x, size := utf8.DecodeRuneInString(remainder)
		remainder = remainder[size:]
		return x, true
	}

	spec, ok := next()
	if !ok {
		return remainder, items.error(original, remainder), true
	}
	padOverride, hasPadOverride := FormatPad_None, true
	switch spec {
	case '-':
		padOverride = FormatPad_None
	case '0':
		padOverride = FormatPad_Zero
	case '_':
		padOverride = FormatPad_Space
	default:
		hasPadOverride = false
	}

	isAlternate := spec == '#'
	if hasPadOverride || isAlternate {
		if spec, ok = next(); !ok {
			return remainder, items.error(original, remainder), true
		}
	}
	if isAlternate && !strings.ContainsRune(haveAlternates, spec) {
		return remainder, items.error(original, remainder), true
	}

	// queue returns the first item and leaves the others to be returned one
	// by one.
	queue := func(slice []FormatItem) FormatItem {
		items.queue = slice[1:]
		return formatItemClone(slice[0])
	}

	var item FormatItem
	switch spec {
	case 'A':
		item = formatFixedItem(FormatFixed_LongWeekdayName)
	case 'B':
		item = formatFixedItem(FormatFixed_LongMonthName)
	case 'C':
		item = formatNum0(FormatNumeric_YearDiv100)
	case 'D':
		item = queue(strftimeUpperD)
	case 'F':
		item = queue(strftimeUpperF)
	case 'G':
		item = formatNum0(FormatNumeric_IsoYear)
	case 'H':
		item = formatNum0(FormatNumeric_Hour)
	case 'I':
		item = formatNum0(FormatNumeric_Hour12)
	case 'M':
		item = formatNum0(FormatNumeric_Minute)
	case 'P':
		item = formatFixedItem(FormatFixed_LowerAmPm)
	case 'R':
		item = queue(strftimeUpperR)
	case 'S':
		item = formatNum0(FormatNumeric_Second)
	case 'T':
		item = queue(strftimeUpperT)
	case 'U':
		item = formatNum0(FormatNumeric_WeekFromSun)
	case 'V':
		item = formatNum0(FormatNumeric_IsoWeek)
	case 'W':
		item = formatNum0(FormatNumeric_WeekFromMon)
	case 'X':
		item = queue(strftimeTFmt)
	case 'Y':
		item = formatNum0(FormatNumeric_Year)
	case 'Z':
		item = formatFixedItem(FormatFixed_TimezoneName)
	case 'a':
		item = formatFixedItem(FormatFixed_ShortWeekdayName)
	case 'b', 'h':
		item = formatFixedItem(FormatFixed_ShortMonthName)
	case 'c':
		item = queue(strftimeDTFmt)
	case 'd':
		item = formatNum0(FormatNumeric_Day)
	case 'e':
		item = formatNums(FormatNumeric_Day)
	case 'f':
		item = formatNum0(FormatNumeric_Nanosecond)
	case 'g':
		item = formatNum0(FormatNumeric_IsoYearMod100)
	case 'j':
		item = formatNum0(FormatNumeric_Ordinal)
	case 'k':
		item = formatNums(FormatNumeric_Hour)
	case 'l':
		item = formatNums(FormatNumeric_Hour12)
	case 'm':
		item = formatNum0(FormatNumeric_Month)
	case 'n':
		item = &FormatItem_Space{F0: "\n"}
	case 'p':
		item = formatFixedItem(FormatFixed_UpperAmPm)
	case 'q':
		item = formatNum(FormatNumeric_Quarter)
	case 'r':
		item = queue(strftimeTFmtAmpm)
	case 's':
		item = formatNum(FormatNumeric_Timestamp)
	case 't':
		item = &FormatItem_Space{F0: "\t"}
	case 'u':
		item = formatNum(FormatNumeric_WeekdayFromMon)
	case 'v':
		item = queue(strftimeLowerV)
	case 'w':
		item = formatNum(FormatNumeric_NumDaysFromSun)
	case 'x':
		item = queue(strftimeDFmt)
	case 'y':
		item = formatNum0(FormatNumeric_YearMod100)
	case 'z':
		if isAlternate {
			item = formatFixedItem(formatFixedTimezoneOffsetPermissive)
		} else {
			item = formatFixedItem(FormatFixed_TimezoneOffset)
		}
	case '+':
		item = formatFixedItem(FormatFixed_RFC3339)
	case ':':
		if strings.HasPrefix(remainder, "::z") {
			remainder = remainder[3:]
			item = formatFixedItem(FormatFixed_TimezoneOffsetTripleColon)
		} else if strings.HasPrefix(remainder, ":z") {
			remainder = remainder[2:]
			item = formatFixedItem(FormatFixed_TimezoneOffsetDoubleColon)
		} else if strings.HasPrefix(remainder, "z") {
			remainder = remainder[1:]
			item = formatFixedItem(FormatFixed_TimezoneOffsetColon)
		} else {
			item = items.error(original, remainder)
		}
	case '.':
		digit, ok := next()
		if !ok {
			return remainder, items.error(original, remainder), true
		}
		switch digit {
		case '3', '6', '9':
			f, ok := next()
			if !ok {
				return remainder, items.error(original, remainder), true
			}
			if f != 'f' {
				item = items.error(original, remainder)
				break
			}
			switch digit {
			case '3':
				item = formatFixedItem(FormatFixed_Nanosecond3)
			case '6':
				item = formatFixedItem(FormatFixed_Nanosecond6)
			default:
				item = formatFixedItem(FormatFixed_Nanosecond9)
			}
		case 'f':
			item = formatFixedItem(FormatFixed_Nanosecond)
		default:
			item = items.error(original, remainder)
		}
	case '3', '6', '9':
		f, ok := next()
		if !ok {
			return remainder, items.error(original, remainder), true
		}
		if f != 'f' {
			item = items.error(original, remainder)
			break
		}
		switch spec {
		case '3':
			item = formatFixedItem(formatFixedNanosecond3NoDot)
		case '6':
			item = formatFixedItem(formatFixedNanosecond6NoDot)
		default:
			item = formatFixedItem(formatFixedNanosecond9NoDot)
		}
	case '%':
		item = &FormatItem_Literal{F0: "%"}
	default:
		item = items.error(original, remainder)
	}

	// Adjust `item` if we have any padding modifier.
	// Not allowed on non-numeric items or on specifiers composed out of multiple
	// formatting items.
	if hasPadOverride {
		if numeric, ok := item.(*FormatItem_Numeric); ok && len(items.queue) == 0 {
			return remainder, &FormatItem_Numeric{F0: numeric.F0, F1: padOverride}, true
		}
		return remainder, items.error(original, remainder), true
	}
	return remainder, item, true
}

func (items *FormatStrftimeItems) error(original string, remainder string) FormatItem {
	if items.lenient {
		return &FormatItem_Literal{F0: original[:len(original)-len(remainder)]}
	}
	return &FormatItem_Error{}
}

// Next is `Iterator::next`: the next formatting item of the format string.
func (items *FormatStrftimeItems) Next() rs.Option[FormatItem] {
	// We have items queued to return from a specifier composed of multiple formatting items.
	if len(items.queue) > 0 {
		item := items.queue[0]
		items.queue = items.queue[1:]
		return rs.Some(formatItemClone(item))
	}

	// Normal: we are parsing the formatting string.
	remainder, item, ok := items.parseNextItem(items.remainder)
	if !ok {
		return rs.None[FormatItem]()
	}
	items.remainder = remainder
	return rs.Some(item)
}

var (
	strftimeUpperD = []FormatItem{
		formatNum0(FormatNumeric_Month),
		&FormatItem_Literal{F0: "/"},
		formatNum0(FormatNumeric_Day),
		&FormatItem_Literal{F0: "/"},
		formatNum0(FormatNumeric_YearMod100),
	}
	strftimeUpperF = []FormatItem{
		formatNum0(FormatNumeric_Year),
		&FormatItem_Literal{F0: "-"},
		formatNum0(FormatNumeric_Month),
		&FormatItem_Literal{F0: "-"},
		formatNum0(FormatNumeric_Day),
	}
	strftimeUpperR = []FormatItem{
		formatNum0(FormatNumeric_Hour),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Minute),
	}
	strftimeUpperT = []FormatItem{
		formatNum0(FormatNumeric_Hour),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Minute),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Second),
	}
	strftimeLowerV = []FormatItem{
		formatNums(FormatNumeric_Day),
		&FormatItem_Literal{F0: "-"},
		formatFixedItem(FormatFixed_ShortMonthName),
		&FormatItem_Literal{F0: "-"},
		formatNum0(FormatNumeric_Year),
	}

	strftimeDFmt = []FormatItem{
		formatNum0(FormatNumeric_Month),
		&FormatItem_Literal{F0: "/"},
		formatNum0(FormatNumeric_Day),
		&FormatItem_Literal{F0: "/"},
		formatNum0(FormatNumeric_YearMod100),
	}
	strftimeDTFmt = []FormatItem{
		formatFixedItem(FormatFixed_ShortWeekdayName),
		&FormatItem_Space{F0: " "},
		formatFixedItem(FormatFixed_ShortMonthName),
		&FormatItem_Space{F0: " "},
		formatNums(FormatNumeric_Day),
		&FormatItem_Space{F0: " "},
		formatNum0(FormatNumeric_Hour),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Minute),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Second),
		&FormatItem_Space{F0: " "},
		formatNum0(FormatNumeric_Year),
	}
	strftimeTFmt = []FormatItem{
		formatNum0(FormatNumeric_Hour),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Minute),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Second),
	}
	strftimeTFmtAmpm = []FormatItem{
		formatNum0(FormatNumeric_Hour12),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Minute),
		&FormatItem_Literal{F0: ":"},
		formatNum0(FormatNumeric_Second),
		&FormatItem_Space{F0: " "},
		formatFixedItem(FormatFixed_UpperAmPm),
	}
)

const haveAlternates = "z"
