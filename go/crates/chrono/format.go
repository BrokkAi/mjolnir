// Port of chrono 0.4.44 src/format/mod.rs

package chrono

// Formatting (and parsing) utilities for date and time.
//
// This module provides the common types and routines to implement,
// for example, DateTime.Format or DateTimeParseFromStr methods.
// For most cases you should use these high-level interfaces.
//
// Internally the formatting and parsing shares the same abstract **formatting items**,
// which are just a sequence of the FormatItem type.
// They are generated from more readable **format strings**;
// currently Chrono supports a built-in syntax closely resembling
// C's `strftime` format. The available options can be found in format_strftime.go.

import "github.com/BrokkAi/mjolnir/go/rs"

// FormatPad is the padding characters for numeric items (`format::Pad`).
type FormatPad uint8

const (
	// FormatPad_None is no padding.
	FormatPad_None FormatPad = iota
	// FormatPad_Zero is zero (`0`) padding.
	FormatPad_Zero
	// FormatPad_Space is space padding.
	FormatPad_Space
)

var _ = rs.RegisterEnum[FormatPad]("Pad",
	rs.Const(FormatPad_None, "None"),
	rs.Const(FormatPad_Zero, "Zero"),
	rs.Const(FormatPad_Space, "Space"),
)

// DebugString is the Debug form, which is derived: the variant's name.
func (p FormatPad) DebugString() string {
	switch p {
	case FormatPad_None:
		return "None"
	case FormatPad_Zero:
		return "Zero"
	case FormatPad_Space:
		return "Space"
	default:
		panic(rs.Unreachable(uint8(p)))
	}
}

// FormatNumeric is the numeric item types (`format::Numeric`).
// They have associated formatting width (FW) and parsing width (PW).
//
// The **formatting width** is the minimal width to be formatted.
// If the number is too short, and the padding is not FormatPad_None, then
// it is left-padded. If the number is too long or (in some cases) negative,
// it is printed as is.
//
// The **parsing width** is the maximal width to be scanned.
// The parser only tries to consume from one to given number of digits
// (greedily). It also trims the preceding whitespace if any.
// It cannot parse the negative number, so some date and time cannot be
// formatted then parsed with the same formatting items.
//
// Rust's variant `Internal(InternalNumeric)` cannot be constructed and has no
// Go counterpart.
type FormatNumeric uint8

const (
	// FormatNumeric_Year is the full Gregorian year (FW=4, PW=∞). May accept
	// years before 1 BCE or after 9999 CE, given an initial sign (+/-).
	FormatNumeric_Year FormatNumeric = iota
	// FormatNumeric_YearDiv100 is the Gregorian year divided by 100 (century
	// number; FW=PW=2). Implies the non-negative year.
	FormatNumeric_YearDiv100
	// FormatNumeric_YearMod100 is the Gregorian year modulo 100 (FW=PW=2).
	// Cannot be negative.
	FormatNumeric_YearMod100
	// FormatNumeric_IsoYear is the year in the ISO week date (FW=4, PW=∞).
	// May accept years before 1 BCE or after 9999 CE, given an initial sign.
	FormatNumeric_IsoYear
	// FormatNumeric_IsoYearDiv100 is the year in the ISO week date, divided
	// by 100 (FW=PW=2). Implies the non-negative year.
	FormatNumeric_IsoYearDiv100
	// FormatNumeric_IsoYearMod100 is the year in the ISO week date, modulo
	// 100 (FW=PW=2). Cannot be negative.
	FormatNumeric_IsoYearMod100
	// FormatNumeric_Quarter is the quarter (FW=PW=1).
	FormatNumeric_Quarter
	// FormatNumeric_Month is the month (FW=PW=2).
	FormatNumeric_Month
	// FormatNumeric_Day is the day of the month (FW=PW=2).
	FormatNumeric_Day
	// FormatNumeric_WeekFromSun is the week number, where the week 1 starts
	// at the first Sunday of January (FW=PW=2).
	FormatNumeric_WeekFromSun
	// FormatNumeric_WeekFromMon is the week number, where the week 1 starts
	// at the first Monday of January (FW=PW=2).
	FormatNumeric_WeekFromMon
	// FormatNumeric_IsoWeek is the week number in the ISO week date
	// (FW=PW=2).
	FormatNumeric_IsoWeek
	// FormatNumeric_NumDaysFromSun is the day of the week, where Sunday = 0
	// and Saturday = 6 (FW=PW=1).
	FormatNumeric_NumDaysFromSun
	// FormatNumeric_WeekdayFromMon is the day of the week, where Monday = 1
	// and Sunday = 7 (FW=PW=1).
	FormatNumeric_WeekdayFromMon
	// FormatNumeric_Ordinal is the day of the year (FW=PW=3).
	FormatNumeric_Ordinal
	// FormatNumeric_Hour is the hour number in the 24-hour clocks (FW=PW=2).
	FormatNumeric_Hour
	// FormatNumeric_Hour12 is the hour number in the 12-hour clocks
	// (FW=PW=2).
	FormatNumeric_Hour12
	// FormatNumeric_Minute is the number of minutes since the last whole
	// hour (FW=PW=2).
	FormatNumeric_Minute
	// FormatNumeric_Second is the number of seconds since the last whole
	// minute (FW=PW=2).
	FormatNumeric_Second
	// FormatNumeric_Nanosecond is the number of nanoseconds since the last
	// whole second (FW=PW=9). Note that this is *not* left-aligned; see also
	// FormatFixed_Nanosecond.
	FormatNumeric_Nanosecond
	// FormatNumeric_Timestamp is the number of non-leap seconds since the
	// midnight UTC on January 1, 1970 (FW=1, PW=∞). For formatting, it
	// assumes UTC upon the absence of time zone offset.
	FormatNumeric_Timestamp
)

var formatNumericNames = [...]string{
	"Year", "YearDiv100", "YearMod100", "IsoYear", "IsoYearDiv100", "IsoYearMod100", "Quarter",
	"Month", "Day", "WeekFromSun", "WeekFromMon", "IsoWeek", "NumDaysFromSun", "WeekdayFromMon",
	"Ordinal", "Hour", "Hour12", "Minute", "Second", "Nanosecond", "Timestamp",
}

var _ = func() *rs.TypeInfo {
	variants := make([]rs.VariantSpec, 0, len(formatNumericNames))
	for index, name := range formatNumericNames {
		variants = append(variants, rs.Const(FormatNumeric(index), name))
	}
	return rs.RegisterEnum[FormatNumeric]("Numeric", variants...)
}()

// DebugString is the Debug form, which is derived: the variant's name.
func (n FormatNumeric) DebugString() string {
	if int(n) >= len(formatNumericNames) {
		panic(rs.Unreachable(uint8(n)))
	}
	return formatNumericNames[n]
}

// FormatFixed is the fixed-format item types (`format::Fixed`).
//
// They have their own rules of formatting and parsing.
// Otherwise noted, they print in the specified cases but parse
// case-insensitively.
//
// Rust's variant `Internal(InternalFixed)` holds a private value; the four
// internal items are Go values of this type that have no exported constant.
// They come out of FormatStrftimeItems for `%#z`, `%3f`, `%6f` and `%9f`.
type FormatFixed uint8

const (
	// FormatFixed_ShortMonthName is the abbreviated month names.
	//
	// Prints a three-letter-long name in the title case, reads the same name
	// in any case.
	FormatFixed_ShortMonthName FormatFixed = iota
	// FormatFixed_LongMonthName is the full month names.
	//
	// Prints a full name in the title case, reads either a short or full
	// name in any case.
	FormatFixed_LongMonthName
	// FormatFixed_ShortWeekdayName is the abbreviated day of the week names.
	//
	// Prints a three-letter-long name in the title case, reads the same name
	// in any case.
	FormatFixed_ShortWeekdayName
	// FormatFixed_LongWeekdayName is the full day of the week names.
	//
	// Prints a full name in the title case, reads either a short or full
	// name in any case.
	FormatFixed_LongWeekdayName
	// FormatFixed_LowerAmPm is AM/PM.
	//
	// Prints in lower case, reads in any case.
	FormatFixed_LowerAmPm
	// FormatFixed_UpperAmPm is AM/PM.
	//
	// Prints in upper case, reads in any case.
	FormatFixed_UpperAmPm
	// FormatFixed_Nanosecond is an optional dot plus one or more digits for
	// left-aligned nanoseconds. May print nothing, 3, 6 or 9 digits according
	// to the available accuracy. See also FormatNumeric_Nanosecond.
	FormatFixed_Nanosecond
	// FormatFixed_Nanosecond3 is the same as FormatFixed_Nanosecond but the
	// accuracy is fixed to 3.
	FormatFixed_Nanosecond3
	// FormatFixed_Nanosecond6 is the same as FormatFixed_Nanosecond but the
	// accuracy is fixed to 6.
	FormatFixed_Nanosecond6
	// FormatFixed_Nanosecond9 is the same as FormatFixed_Nanosecond but the
	// accuracy is fixed to 9.
	FormatFixed_Nanosecond9
	// FormatFixed_TimezoneName is the timezone name.
	//
	// It does not support parsing, its use in the parser is an immediate
	// failure.
	FormatFixed_TimezoneName
	// FormatFixed_TimezoneOffsetColon is the offset from the local time to
	// UTC (`+09:00` or `-04:00` or `+00:00`).
	//
	// In the parser, the colon can be omitted and/or surrounded with any
	// amount of whitespace. The offset is limited from `-24:00` to `+24:00`,
	// which is the same as FixedOffset's range.
	FormatFixed_TimezoneOffsetColon
	// FormatFixed_TimezoneOffsetDoubleColon is the offset from the local
	// time to UTC with seconds (`+09:00:00` or `-04:00:00` or `+00:00:00`).
	//
	// In the parser, the colon can be omitted and/or surrounded with any
	// amount of whitespace. The offset is limited from `-24:00:00` to
	// `+24:00:00`, which is the same as FixedOffset's range.
	FormatFixed_TimezoneOffsetDoubleColon
	// FormatFixed_TimezoneOffsetTripleColon is the offset from the local
	// time to UTC without minutes (`+09` or `-04` or `+00`).
	//
	// In the parser, the colon can be omitted and/or surrounded with any
	// amount of whitespace. The offset is limited from `-24` to `+24`, which
	// is the same as FixedOffset's range.
	FormatFixed_TimezoneOffsetTripleColon
	// FormatFixed_TimezoneOffsetColonZ is the offset from the local time to
	// UTC (`+09:00` or `-04:00` or `Z`).
	//
	// In the parser, the colon can be omitted and/or surrounded with any
	// amount of whitespace, and `Z` can be either in upper case or in lower
	// case. The offset is limited from `-24:00` to `+24:00`, which is the
	// same as FixedOffset's range.
	FormatFixed_TimezoneOffsetColonZ
	// FormatFixed_TimezoneOffset is the same as
	// FormatFixed_TimezoneOffsetColon but prints no colon (e.g. `+0900`).
	// Parsing allows an optional colon.
	FormatFixed_TimezoneOffset
	// FormatFixed_TimezoneOffsetZ is the same as
	// FormatFixed_TimezoneOffsetColonZ but prints no colon (e.g. `+0900`).
	// Parsing allows an optional colon.
	FormatFixed_TimezoneOffsetZ
	// FormatFixed_RFC2822 is RFC 2822 date and time syntax. Commonly used for
	// email and MIME date and time.
	FormatFixed_RFC2822
	// FormatFixed_RFC3339 is RFC 3339 & ISO 8601 date and time syntax.
	FormatFixed_RFC3339

	// formatFixedTimezoneOffsetPermissive is the same as
	// FormatFixed_TimezoneOffsetColonZ, but allows missing minutes on parsing.
	//
	// It's a hidden internal item in Rust:
	// `Internal(InternalFixed { val: TimezoneOffsetPermissive })`.
	formatFixedTimezoneOffsetPermissive
	// formatFixedNanosecond3NoDot is the same as FormatFixed_Nanosecond but
	// the accuracy is fixed to 3 and there is no leading dot.
	formatFixedNanosecond3NoDot
	// formatFixedNanosecond6NoDot is the same as FormatFixed_Nanosecond but
	// the accuracy is fixed to 6 and there is no leading dot.
	formatFixedNanosecond6NoDot
	// formatFixedNanosecond9NoDot is the same as FormatFixed_Nanosecond but
	// the accuracy is fixed to 9 and there is no leading dot.
	formatFixedNanosecond9NoDot
)

var formatFixedNames = [...]string{
	"ShortMonthName", "LongMonthName", "ShortWeekdayName", "LongWeekdayName", "LowerAmPm",
	"UpperAmPm", "Nanosecond", "Nanosecond3", "Nanosecond6", "Nanosecond9", "TimezoneName",
	"TimezoneOffsetColon", "TimezoneOffsetDoubleColon", "TimezoneOffsetTripleColon",
	"TimezoneOffsetColonZ", "TimezoneOffset", "TimezoneOffsetZ", "RFC2822", "RFC3339",
	"Internal(InternalFixed { val: TimezoneOffsetPermissive })",
	"Internal(InternalFixed { val: Nanosecond3NoDot })",
	"Internal(InternalFixed { val: Nanosecond6NoDot })",
	"Internal(InternalFixed { val: Nanosecond9NoDot })",
}

// DebugString is the Debug form, which is derived: the variant's name, or
// for an internal item `Internal(InternalFixed { val: ... })`.
func (x FormatFixed) DebugString() string {
	if int(x) >= len(formatFixedNames) {
		panic(rs.Unreachable(uint8(x)))
	}
	return formatFixedNames[x]
}

// FmtDebug is the derived `Debug`.
func (x FormatFixed) FmtDebug(f *rs.Formatter) error {
	if x < formatFixedTimezoneOffsetPermissive {
		return f.WriteStr(x.DebugString())
	}
	// The derived form of the nested structs also has a pretty-printed
	// layout, which the builders produce.
	val := [...]string{"TimezoneOffsetPermissive", "Nanosecond3NoDot", "Nanosecond6NoDot", "Nanosecond9NoDot"}[x-formatFixedTimezoneOffsetPermissive]
	return f.DebugTuple("Internal").Field(formatInternalFixed{val: formatInternalInternal(val)}).Finish()
}

// formatInternalFixed is the opaque `InternalFixed`, which only prints.
type formatInternalFixed struct{ val formatInternalInternal }

func (x formatInternalFixed) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("InternalFixed").Field("val", x.val).Finish()
}

type formatInternalInternal string

func (x formatInternalInternal) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(x)) }

// FormatOffsetFormat is the type for specifying the format of UTC offsets
// (`format::OffsetFormat`).
type FormatOffsetFormat struct {
	// Precision is the level of detail (hours, minutes, seconds) of the
	// offset.
	Precision FormatOffsetPrecision `rs:"precision"`
	// Colons is the separator between hours, minutes and seconds.
	Colons FormatColons `rs:"colons"`
	// AllowZulu represents `+00:00` as `Z`.
	AllowZulu bool `rs:"allow_zulu"`
	// Padding pads the hour value to two digits.
	Padding FormatPad `rs:"padding"`
}

var _ = rs.RegisterStruct[FormatOffsetFormat]("OffsetFormat")

// FormatOffsetPrecision is the precision of an offset from UTC formatting
// item (`format::OffsetPrecision`).
type FormatOffsetPrecision uint8

const (
	// FormatOffsetPrecision_Hours formats offset from UTC as only hours. Not
	// recommended, it is not uncommon for timezones to have an offset of 30
	// minutes, 15 minutes, etc. Any minutes and seconds get truncated.
	FormatOffsetPrecision_Hours FormatOffsetPrecision = iota
	// FormatOffsetPrecision_Minutes formats offset from UTC as hours and
	// minutes. Any seconds will be rounded to the nearest minute.
	FormatOffsetPrecision_Minutes
	// FormatOffsetPrecision_Seconds formats offset from UTC as hours, minutes
	// and seconds.
	FormatOffsetPrecision_Seconds
	// FormatOffsetPrecision_OptionalMinutes formats offset from UTC as hours,
	// and optionally with minutes. Any seconds will be rounded to the nearest
	// minute.
	FormatOffsetPrecision_OptionalMinutes
	// FormatOffsetPrecision_OptionalSeconds formats offset from UTC as hours
	// and minutes, and optionally seconds.
	FormatOffsetPrecision_OptionalSeconds
	// FormatOffsetPrecision_OptionalMinutesAndSeconds formats offset from UTC
	// as hours and optionally minutes and seconds.
	FormatOffsetPrecision_OptionalMinutesAndSeconds
)

var _ = rs.RegisterEnum[FormatOffsetPrecision]("OffsetPrecision",
	rs.Const(FormatOffsetPrecision_Hours, "Hours"),
	rs.Const(FormatOffsetPrecision_Minutes, "Minutes"),
	rs.Const(FormatOffsetPrecision_Seconds, "Seconds"),
	rs.Const(FormatOffsetPrecision_OptionalMinutes, "OptionalMinutes"),
	rs.Const(FormatOffsetPrecision_OptionalSeconds, "OptionalSeconds"),
	rs.Const(FormatOffsetPrecision_OptionalMinutesAndSeconds, "OptionalMinutesAndSeconds"),
)

// FormatColons is the separator between hours and minutes in an offset
// (`format::Colons`).
type FormatColons uint8

const (
	// FormatColons_None is no separator.
	FormatColons_None FormatColons = iota
	// FormatColons_Colon is colon (`:`) as separator.
	FormatColons_Colon
	// FormatColons_Maybe is no separator when formatting, colon allowed when
	// parsing.
	FormatColons_Maybe
)

var _ = rs.RegisterEnum[FormatColons]("Colons",
	rs.Const(FormatColons_None, "None"),
	rs.Const(FormatColons_Colon, "Colon"),
	rs.Const(FormatColons_Maybe, "Maybe"),
)

// FormatItem is a single formatting item (`format::Item`). This is used for
// both formatting and parsing.
//
// A value is always a pointer to one of the variant structs. Rust's borrowed
// and owned variants hold the same Go string.
type FormatItem interface {
	isFormatItem()
	// ToOwned is `Item::to_owned`: the item with a borrowed literal or space
	// made the owned variant.
	ToOwned() FormatItem
}

// FormatItem_Literal is `Item::Literal`: a literally printed and parsed
// text.
type FormatItem_Literal struct{ F0 string }

// FormatItem_OwnedLiteral is `Item::OwnedLiteral`: the same as
// FormatItem_Literal but owned.
type FormatItem_OwnedLiteral struct{ F0 string }

// FormatItem_Space is `Item::Space`: whitespace. Prints literally but
// reads zero or more whitespace.
type FormatItem_Space struct{ F0 string }

// FormatItem_OwnedSpace is `Item::OwnedSpace`: the same as FormatItem_Space
// but owned.
type FormatItem_OwnedSpace struct{ F0 string }

// FormatItem_Numeric is `Item::Numeric`: a numeric item. Can be optionally
// padded to the maximal length (if any) when formatting; the parser simply
// ignores any padded whitespace and zeroes.
type FormatItem_Numeric struct {
	F0 FormatNumeric
	F1 FormatPad
}

// FormatItem_Fixed is `Item::Fixed`: a fixed-format item.
type FormatItem_Fixed struct{ F0 FormatFixed }

// FormatItem_Error is `Item::Error`: issues a formatting error. Used to
// signal an invalid format string.
type FormatItem_Error struct{}

var _ = rs.RegisterEnum[FormatItem]("Item",
	rs.Variant[FormatItem_Literal]("Literal", rs.ShapeTuple),
	rs.Variant[FormatItem_OwnedLiteral]("OwnedLiteral", rs.ShapeTuple),
	rs.Variant[FormatItem_Space]("Space", rs.ShapeTuple),
	rs.Variant[FormatItem_OwnedSpace]("OwnedSpace", rs.ShapeTuple),
	rs.Variant[FormatItem_Numeric]("Numeric", rs.ShapeTuple),
	rs.Variant[FormatItem_Fixed]("Fixed", rs.ShapeTuple),
	rs.Variant[FormatItem_Error]("Error", rs.ShapeUnit),
)

func (*FormatItem_Literal) isFormatItem()      {}
func (*FormatItem_OwnedLiteral) isFormatItem() {}
func (*FormatItem_Space) isFormatItem()        {}
func (*FormatItem_OwnedSpace) isFormatItem()   {}
func (*FormatItem_Numeric) isFormatItem()      {}
func (*FormatItem_Fixed) isFormatItem()        {}
func (*FormatItem_Error) isFormatItem()        {}

func formatNum(numeric FormatNumeric) FormatItem {
	return &FormatItem_Numeric{F0: numeric, F1: FormatPad_None}
}

func formatNum0(numeric FormatNumeric) FormatItem {
	return &FormatItem_Numeric{F0: numeric, F1: FormatPad_Zero}
}

func formatNums(numeric FormatNumeric) FormatItem {
	return &FormatItem_Numeric{F0: numeric, F1: FormatPad_Space}
}

func formatFixedItem(fixed FormatFixed) FormatItem {
	return &FormatItem_Fixed{F0: fixed}
}

// ToOwned is `Item::to_owned`.
func (item *FormatItem_Literal) ToOwned() FormatItem { return &FormatItem_OwnedLiteral{F0: item.F0} }

// ToOwned is `Item::to_owned`.
func (item *FormatItem_Space) ToOwned() FormatItem { return &FormatItem_OwnedSpace{F0: item.F0} }

// ToOwned is `Item::to_owned`.
func (item *FormatItem_Numeric) ToOwned() FormatItem {
	return &FormatItem_Numeric{F0: item.F0, F1: item.F1}
}

// ToOwned is `Item::to_owned`.
func (item *FormatItem_Fixed) ToOwned() FormatItem { return &FormatItem_Fixed{F0: item.F0} }

// ToOwned is `Item::to_owned`.
func (item *FormatItem_OwnedLiteral) ToOwned() FormatItem {
	return &FormatItem_OwnedLiteral{F0: item.F0}
}

// ToOwned is `Item::to_owned`.
func (item *FormatItem_OwnedSpace) ToOwned() FormatItem { return &FormatItem_OwnedSpace{F0: item.F0} }

// ToOwned is `Item::to_owned`.
func (item *FormatItem_Error) ToOwned() FormatItem { return &FormatItem_Error{} }

// formatItemClone is `item.clone()`: the queued items of a composite
// specifier are shared tables, and every caller gets its own copy.
func formatItemClone(item FormatItem) FormatItem {
	switch item := item.(type) {
	case *FormatItem_Literal:
		return &FormatItem_Literal{F0: item.F0}
	case *FormatItem_OwnedLiteral:
		return &FormatItem_OwnedLiteral{F0: item.F0}
	case *FormatItem_Space:
		return &FormatItem_Space{F0: item.F0}
	case *FormatItem_OwnedSpace:
		return &FormatItem_OwnedSpace{F0: item.F0}
	case *FormatItem_Numeric:
		return &FormatItem_Numeric{F0: item.F0, F1: item.F1}
	case *FormatItem_Fixed:
		return &FormatItem_Fixed{F0: item.F0}
	case *FormatItem_Error:
		return &FormatItem_Error{}
	default:
		panic(rs.Unreachable(item))
	}
}

// formatItemEqual is `==` on two items (derived PartialEq): the same variant
// holding equal values.
func formatItemEqual(a, b FormatItem) bool {
	switch a := a.(type) {
	case *FormatItem_Literal:
		b, ok := b.(*FormatItem_Literal)
		return ok && a.F0 == b.F0
	case *FormatItem_OwnedLiteral:
		b, ok := b.(*FormatItem_OwnedLiteral)
		return ok && a.F0 == b.F0
	case *FormatItem_Space:
		b, ok := b.(*FormatItem_Space)
		return ok && a.F0 == b.F0
	case *FormatItem_OwnedSpace:
		b, ok := b.(*FormatItem_OwnedSpace)
		return ok && a.F0 == b.F0
	case *FormatItem_Numeric:
		b, ok := b.(*FormatItem_Numeric)
		return ok && *a == *b
	case *FormatItem_Fixed:
		b, ok := b.(*FormatItem_Fixed)
		return ok && a.F0 == b.F0
	case *FormatItem_Error:
		_, ok := b.(*FormatItem_Error)
		return ok
	default:
		panic(rs.Unreachable(a))
	}
}

// ParseError is an error from the `parse` function and from every parsing
// function of this package (`format::ParseError`, also `chrono::ParseError`).
//
// It is a Copy type, comparable with ==. Kind says which error it is, and
// the Display text (String, Error) is one fixed sentence for each kind:
//
//	OutOfRange   input is out of range
//	Impossible   no possible date and time matching input
//	NotEnough    input is not enough for unique date and time
//	Invalid      input contains invalid characters
//	TooShort     premature end of input
//	TooLong      trailing input
//	BadFormat    bad or unsupported format string
type ParseError struct {
	kind FormatParseErrorKind
}

// Kind is `ParseError::kind`: the category of the parse error.
func (e ParseError) Kind() FormatParseErrorKind {
	return e.kind
}

// FormatParseErrorKind is the category of parse error
// (`format::ParseErrorKind`).
type FormatParseErrorKind uint8

const (
	// FormatParseErrorKind_OutOfRange is: given field is out of permitted
	// range.
	FormatParseErrorKind_OutOfRange FormatParseErrorKind = iota

	// FormatParseErrorKind_Impossible is: there is no possible date and time
	// value with given set of fields.
	//
	// This does not include the out-of-range conditions, which are trivially
	// invalid. It includes the case that there are one or more fields that
	// are inconsistent to each other.
	FormatParseErrorKind_Impossible

	// FormatParseErrorKind_NotEnough is: given set of fields is not enough to
	// make a requested date and time value.
	//
	// Note that there *may* be a case that given fields constrain the
	// possible values so much that there is a unique possible value. Chrono
	// only tries to be correct for most useful sets of fields however, as
	// such constraint solving can be expensive.
	FormatParseErrorKind_NotEnough

	// FormatParseErrorKind_Invalid is: the input string has some invalid
	// character sequence for given formatting items.
	FormatParseErrorKind_Invalid

	// FormatParseErrorKind_TooShort is: the input string has been prematurely
	// ended.
	FormatParseErrorKind_TooShort

	// FormatParseErrorKind_TooLong is: all formatting items have been
	// consumed but the input string remains.
	FormatParseErrorKind_TooLong

	// FormatParseErrorKind_BadFormat is: there was an error on the formatting
	// string, or there were non-supported formatting items.
	FormatParseErrorKind_BadFormat
)

var _ = rs.RegisterEnum[FormatParseErrorKind]("ParseErrorKind",
	rs.Const(FormatParseErrorKind_OutOfRange, "OutOfRange"),
	rs.Const(FormatParseErrorKind_Impossible, "Impossible"),
	rs.Const(FormatParseErrorKind_NotEnough, "NotEnough"),
	rs.Const(FormatParseErrorKind_Invalid, "Invalid"),
	rs.Const(FormatParseErrorKind_TooShort, "TooShort"),
	rs.Const(FormatParseErrorKind_TooLong, "TooLong"),
	rs.Const(FormatParseErrorKind_BadFormat, "BadFormat"),
)

// DebugString is the Debug form, which is derived: the variant's name.
func (k FormatParseErrorKind) DebugString() string {
	switch k {
	case FormatParseErrorKind_OutOfRange:
		return "OutOfRange"
	case FormatParseErrorKind_Impossible:
		return "Impossible"
	case FormatParseErrorKind_NotEnough:
		return "NotEnough"
	case FormatParseErrorKind_Invalid:
		return "Invalid"
	case FormatParseErrorKind_TooShort:
		return "TooShort"
	case FormatParseErrorKind_TooLong:
		return "TooLong"
	case FormatParseErrorKind_BadFormat:
		return "BadFormat"
	default:
		panic(rs.Unreachable(uint8(k)))
	}
}

// String is the Display form.
func (e ParseError) String() string {
	switch e.kind {
	case FormatParseErrorKind_OutOfRange:
		return "input is out of range"
	case FormatParseErrorKind_Impossible:
		return "no possible date and time matching input"
	case FormatParseErrorKind_NotEnough:
		return "input is not enough for unique date and time"
	case FormatParseErrorKind_Invalid:
		return "input contains invalid characters"
	case FormatParseErrorKind_TooShort:
		return "premature end of input"
	case FormatParseErrorKind_TooLong:
		return "trailing input"
	case FormatParseErrorKind_BadFormat:
		return "bad or unsupported format string"
	default:
		panic(rs.Unreachable())
	}
}

// Error makes ParseError a Go error; the text is the Display form.
func (e ParseError) Error() string { return e.String() }

// DebugString is the Debug form, which is derived: "ParseError(Invalid)".
func (e ParseError) DebugString() string { return "ParseError(" + e.kind.DebugString() + ")" }

// FmtDisplay is `impl fmt::Display for ParseError`.
func (e ParseError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is the derived `Debug`.
func (e ParseError) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("ParseError").Field(e.kind).Finish()
}

// to be used in this module and submodules
var (
	errOutOfRange error = ParseError{kind: FormatParseErrorKind_OutOfRange}
	errImpossible error = ParseError{kind: FormatParseErrorKind_Impossible}
	errNotEnough  error = ParseError{kind: FormatParseErrorKind_NotEnough}
	errInvalid    error = ParseError{kind: FormatParseErrorKind_Invalid}
	errTooShort   error = ParseError{kind: FormatParseErrorKind_TooShort}
	errTooLong    error = ParseError{kind: FormatParseErrorKind_TooLong}
	errBadFormat  error = ParseError{kind: FormatParseErrorKind_BadFormat}
)

// WeekdayFromStr is `str::parse::<Weekday>()`: a weekday from its
// three-letter or full English name, in any case. For example "mon",
// "Monday" and "MONDAY" are Monday.
//
// The error is a ParseWeekdayError.
//
// this implementation is here only because we need some private code from `scan`
func WeekdayFromStr(s string) (Weekday, error) {
	if rest, w, err := scanShortOrLongWeekday(s); err == nil && rest == "" {
		return w, nil
	}
	return 0, ParseWeekdayError{}
}

// MonthFromStr is `str::parse::<Month>()`: a month from its three-letter or
// full English name, in any case. For example "jan", "January" and "JANUARY"
// are January.
//
// The error is a ParseMonthError.
func MonthFromStr(s string) (Month, error) {
	if rest, w, err := scanShortOrLongMonth0(s); err == nil && rest == "" {
		if w <= 11 {
			return Month(w), nil
		}
		return 0, ParseMonthError{}
	}
	return 0, ParseMonthError{}
}
