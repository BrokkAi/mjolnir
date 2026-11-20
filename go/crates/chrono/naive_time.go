// Port of chrono 0.4.44 src/naive/time/mod.rs

package chrono

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// NaiveTime is the ISO 8601 time without timezone.
// It allows for the nanosecond precision and optional leap second
// representation.
//
// # Leap Second Handling
//
// Since 1960s, the manmade atomic clock has been so accurate that it is much
// more accurate than Earth's own motion. It became desirable to define the
// civil time in terms of the atomic clock, but that risks the
// desynchronization of the civil time from Earth. To account for this, the
// designers of the Coordinated Universal Time (UTC) made that the UTC should
// be kept within 0.9 seconds of the observed Earth-bound time. When the mean
// solar day is longer than the ideal (86,400 seconds), the error slowly
// accumulates and it is necessary to add a **leap second** to slow the UTC
// down a bit.
//
// Chrono does not try to accurately implement leap seconds; it is impossible.
// Rather, it allows for leap seconds but behaves as if there are *no other*
// leap seconds. A leap second is the second 59 of a minute with a fraction of
// 1,000,000,000 nanoseconds or more: 23:59:60 is `secs = 86399` with
// `frac = 1_000_000_000`, and prints as "23:59:60".
//
// It is a Copy type, comparable with ==. The zero value is midnight, as
// `NaiveTime::default()` is.
type NaiveTime struct {
	secs uint32
	frac uint32
}

// NaiveTimeFromHmsOpt is `NaiveTime::from_hms_opt`: a time from hour, minute
// and second. The millisecond part is allowed to exceed 1,000,000,000 in
// order to represent a leap second only through the other constructors.
//
// It returns None on invalid hour, minute and/or second.
func NaiveTimeFromHmsOpt(hour, min, sec uint32) rs.Option[NaiveTime] {
	return NaiveTimeFromHmsNanoOpt(hour, min, sec, 0)
}

// NaiveTimeFromHmsMilliOpt is `NaiveTime::from_hms_milli_opt`: a time from
// hour, minute, second and millisecond. The millisecond part is allowed to
// exceed 1,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or millisecond.
func NaiveTimeFromHmsMilliOpt(hour, min, sec, milli uint32) rs.Option[NaiveTime] {
	nano, ok := checkedMulU32(milli, 1_000_000)
	if !ok {
		return rs.None[NaiveTime]()
	}
	return NaiveTimeFromHmsNanoOpt(hour, min, sec, nano)
}

// NaiveTimeFromHmsMicroOpt is `NaiveTime::from_hms_micro_opt`: a time from
// hour, minute, second and microsecond. The microsecond part is allowed to
// exceed 1,000,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or microsecond.
func NaiveTimeFromHmsMicroOpt(hour, min, sec, micro uint32) rs.Option[NaiveTime] {
	nano, ok := checkedMulU32(micro, 1_000)
	if !ok {
		return rs.None[NaiveTime]()
	}
	return NaiveTimeFromHmsNanoOpt(hour, min, sec, nano)
}

// NaiveTimeFromHmsNanoOpt is `NaiveTime::from_hms_nano_opt`: a time from
// hour, minute, second and nanosecond. The nanosecond part is allowed to
// exceed 1,000,000,000 in order to represent a leap second, but only when
// `sec == 59`.
//
// It returns None on invalid hour, minute, second and/or nanosecond.
func NaiveTimeFromHmsNanoOpt(hour, min, sec, nano uint32) rs.Option[NaiveTime] {
	if (hour >= 24 || min >= 60 || sec >= 60) ||
		(nano >= 1_000_000_000 && sec != 59) ||
		nano >= 2_000_000_000 {
		return rs.None[NaiveTime]()
	}
	secs := hour*3600 + min*60 + sec
	return rs.Some(NaiveTime{secs: secs, frac: nano})
}

// NaiveTimeFromNumSecondsFromMidnightOpt is
// `NaiveTime::from_num_seconds_from_midnight_opt`: a time from the number of
// seconds since midnight and nanosecond. The nanosecond part is allowed to
// exceed 1,000,000,000 in order to represent a leap second, but only when
// `secs % 60 == 59`.
//
// It returns None on invalid number of seconds and/or nanosecond.
func NaiveTimeFromNumSecondsFromMidnightOpt(secs uint32, nano uint32) rs.Option[NaiveTime] {
	if secs >= 86_400 || nano >= 2_000_000_000 || (nano >= 1_000_000_000 && secs%60 != 59) {
		return rs.None[NaiveTime]()
	}
	return rs.Some(NaiveTime{secs: secs, frac: nano})
}

// NaiveTimeParseFromStr is `NaiveTime::parse_from_str`: a string parsed with
// the specified format string into a new NaiveTime. See
// FormatStrftimeItemsNew for the supported sequences.
//
// The error is a ParseError.
func NaiveTimeParseFromStr(s string, fmt string) (NaiveTime, error) {
	parsed := FormatParsedNew()
	if err := FormatParse(&parsed, s, FormatStrftimeItemsNew(fmt).Collect()); err != nil {
		return NaiveTime{}, err
	}
	return parsed.ToNaiveTime()
}

// NaiveTimeParseAndRemainder is `NaiveTime::parse_and_remainder`: a
// NaiveTime parsed from the beginning of a string with the specified format
// string, and the remaining portion of the string.
func NaiveTimeParseAndRemainder(s string, fmt string) (NaiveTime, string, error) {
	parsed := FormatParsedNew()
	remainder, err := FormatParseAndRemainder(&parsed, s, FormatStrftimeItemsNew(fmt).Collect())
	if err != nil {
		return NaiveTime{}, "", err
	}
	time, err := parsed.ToNaiveTime()
	if err != nil {
		return NaiveTime{}, "", err
	}
	return time, remainder, nil
}

// OverflowingAddSigned is `NaiveTime::overflowing_add_signed`: the time plus
// rhs, wrapping around midnight, and the number of *seconds* by which it
// wrapped (in whole days).
func (t NaiveTime) OverflowingAddSigned(rhs TimeDelta) (NaiveTime, int64) {
	secs := int64(t.secs)
	frac := int32(t.frac)
	secsToAdd := rhs.NumSeconds()
	fracToAdd := rhs.SubsecNanos()

	// Check if `self` is a leap second and adding `rhs` would escape that leap second.
	// If that is the case, update `frac` and `secs` to involve no leap second.
	// If it stays within the leap second or the second before, and only adds a fractional
	// second, just do that and return (this way the rest of the code can ignore leap seconds).
	if frac >= 1_000_000_000 {
		// check below is adjusted to not overflow an i32: `frac + frac_to_add >= 2_000_000_000`
		if secsToAdd > 0 || (fracToAdd > 0 && frac >= 2_000_000_000-fracToAdd) {
			frac -= 1_000_000_000
		} else if secsToAdd < 0 {
			frac -= 1_000_000_000
			secs++
		} else {
			return NaiveTime{secs: t.secs, frac: uint32(frac + fracToAdd)}, 0
		}
	}

	secs += secsToAdd
	frac += fracToAdd

	if frac < 0 {
		frac += 1_000_000_000
		secs--
	} else if frac >= 1_000_000_000 {
		frac -= 1_000_000_000
		secs++
	}

	secsInDay := remEuclid(secs, 86_400)
	remaining := secs - secsInDay
	return NaiveTime{secs: uint32(secsInDay), frac: uint32(frac)}, remaining
}

// OverflowingSubSigned is `NaiveTime::overflowing_sub_signed`: the time minus
// rhs, wrapping around midnight, and the number of *seconds* by which it
// wrapped (in whole days).
func (t NaiveTime) OverflowingSubSigned(rhs TimeDelta) (NaiveTime, int64) {
	time, wrapped := t.OverflowingAddSigned(rhs.Neg())
	return time, -wrapped // safe to negate, rhs is within +/- (2^63 / 1000)
}

// SignedDurationSince is `NaiveTime::signed_duration_since`: the time minus
// rhs as a TimeDelta, always within a day either way.
//
// As a part of Chrono's leap second handling, the subtraction assumes that
// *there is no leap second ever*, except when any of the times themselves
// represents a leap second in which case the assumption becomes that *there
// are exactly one (or two) leap second(s) ever*.
func (t NaiveTime) SignedDurationSince(rhs NaiveTime) TimeDelta {
	//     |    |    :leap|    |    |    |    |    |    |    :leap|    |
	//     |    |    :    |    |    |    |    |    |    |    :    |    |
	// ----+----+-----*---+----+----+----+----+----+----+-------*-+----+----
	//          |   `rhs` |                             |    `self`
	//          |======================================>|       |
	//          |     |  `self.secs - rhs.secs`         |`self.frac`
	//          |====>|   |                             |======>|
	//      `rhs.frac`|========================================>|
	//          |     |   |        `self - rhs`         |       |

	secs := int64(t.secs) - int64(rhs.secs)
	frac := int64(t.frac) - int64(rhs.frac)

	// `secs` may contain a leap second yet to be counted
	if t.secs > rhs.secs && rhs.frac >= 1_000_000_000 {
		secs++
	} else if t.secs < rhs.secs && t.frac >= 1_000_000_000 {
		secs--
	}

	secsFromFrac := divEuclid(frac, 1_000_000_000)
	nanos := uint32(remEuclid(frac, 1_000_000_000))

	return expect(TimeDeltaNew(secs+secsFromFrac, nanos), "must be in range")
}

// overflowingAddOffset adds a given FixedOffset to the current time, and
// returns the number of days that should be added to a date as a result of
// the offset (either -1, 0, or 1 because the offset is always less than 24h).
//
// This method is similar to OverflowingAddSigned, but preserves leap seconds.
func (t NaiveTime) overflowingAddOffset(offset FixedOffset) (NaiveTime, int32) {
	secs := int32(t.secs) + offset.LocalMinusUtc()
	days := divEuclid(secs, 86_400)
	secs = remEuclid(secs, 86_400)
	return NaiveTime{secs: uint32(secs), frac: t.frac}, days
}

// overflowingSubOffset subtracts a given FixedOffset from the current time,
// and returns the number of days that should be added to a date as a result
// of the offset (either -1, 0, or 1 because the offset is always less than
// 24h).
//
// This method is similar to OverflowingSubSigned, but preserves leap seconds.
func (t NaiveTime) overflowingSubOffset(offset FixedOffset) (NaiveTime, int32) {
	secs := int32(t.secs) - offset.LocalMinusUtc()
	days := divEuclid(secs, 86_400)
	secs = remEuclid(secs, 86_400)
	return NaiveTime{secs: uint32(secs), frac: t.frac}, days
}

// FormatWithItems is `NaiveTime::format_with_items`: the time formatted with
// the specified formatting items. The result is formatted when its String
// method is called, which panics if an item is the error item or asks for a
// date or an offset.
func (t NaiveTime) FormatWithItems(items []FormatItem) FormatDelayedFormat {
	return FormatDelayedFormatNew(rs.None[NaiveDate](), rs.Some(t), items)
}

// Format is `NaiveTime::format`: the time formatted with the specified format
// string. See FormatStrftimeItemsNew for the supported escape sequences.
//
// The result is formatted when its String method is called (Rust's
// `to_string()`), which panics when the format string is invalid or asks for
// a date or an offset.
func (t NaiveTime) Format(fmt string) FormatDelayedFormat {
	return t.FormatWithItems(FormatStrftimeItemsNew(fmt).Collect())
}

// hms returns a triple of the hour, minute and second numbers.
func (t NaiveTime) hms() (uint32, uint32, uint32) {
	sec := t.secs % 60
	mins := t.secs / 60
	min := mins % 60
	hour := mins / 60
	return hour, min, sec
}

// NaiveTimeMin is `NaiveTime::MIN`: the earliest possible NaiveTime,
// midnight.
var NaiveTimeMin = NaiveTime{secs: 0, frac: 0}

var naiveTimeMax = NaiveTime{secs: 23*3600 + 59*60 + 59, frac: 999_999_999}

// Hour is `Timelike::hour`: the hour number from 0 to 23.
func (t NaiveTime) Hour() uint32 {
	hour, _, _ := t.hms()
	return hour
}

// Hour12 is `Timelike::hour12`: the hour number from 1 to 12 with a boolean
// flag, which is false for AM and true for PM.
func (t NaiveTime) Hour12() (bool, uint32) { return TimelikeDefaultHour12[NaiveTime](t) }

// Minute is `Timelike::minute`: the minute number from 0 to 59.
func (t NaiveTime) Minute() uint32 {
	_, min, _ := t.hms()
	return min
}

// Second is `Timelike::second`: the second number from 0 to 59. It does not
// include the leap second; use Nanosecond for that.
func (t NaiveTime) Second() uint32 {
	_, _, sec := t.hms()
	return sec
}

// Nanosecond is `Timelike::nanosecond`: the number of nanoseconds since the
// whole non-leap second. The range from 1,000,000,000 to 1,999,999,999
// represents the leap second.
func (t NaiveTime) Nanosecond() uint32 {
	return t.frac
}

// WithHour is `Timelike::with_hour`: a new NaiveTime with the hour number
// changed.
//
// It returns None if the value for hour is invalid.
func (t NaiveTime) WithHour(hour uint32) rs.Option[NaiveTime] {
	if hour >= 24 {
		return rs.None[NaiveTime]()
	}
	secs := hour*3600 + t.secs%3600
	return rs.Some(NaiveTime{secs: secs, frac: t.frac})
}

// WithMinute is `Timelike::with_minute`: a new NaiveTime with the minute
// number changed.
//
// It returns None if the value for minute is invalid.
func (t NaiveTime) WithMinute(min uint32) rs.Option[NaiveTime] {
	if min >= 60 {
		return rs.None[NaiveTime]()
	}
	secs := t.secs/3600*3600 + min*60 + t.secs%60
	return rs.Some(NaiveTime{secs: secs, frac: t.frac})
}

// WithSecond is `Timelike::with_second`: a new NaiveTime with the second
// number changed.
//
// As with the Second method, the input range is restricted to 0 through 59.
// It returns None if the value for second is invalid.
func (t NaiveTime) WithSecond(sec uint32) rs.Option[NaiveTime] {
	if sec >= 60 {
		return rs.None[NaiveTime]()
	}
	secs := t.secs/60*60 + sec
	return rs.Some(NaiveTime{secs: secs, frac: t.frac})
}

// WithNanosecond is `Timelike::with_nanosecond`: a new NaiveTime with
// nanoseconds since the whole non-leap second changed.
//
// As with the Nanosecond method, the input range can exceed 1,000,000,000
// for leap seconds. It returns None if `nanosecond >= 2,000,000,000`.
func (t NaiveTime) WithNanosecond(nano uint32) rs.Option[NaiveTime] {
	if nano >= 2_000_000_000 {
		return rs.None[NaiveTime]()
	}
	return rs.Some(NaiveTime{secs: t.secs, frac: nano})
}

// NumSecondsFromMidnight is `Timelike::num_seconds_from_midnight`: the number
// of non-leap seconds past the last midnight.
func (t NaiveTime) NumSecondsFromMidnight() uint32 {
	return t.secs // do not repeat the calculation!
}

// Add is `time + rhs` for a TimeDelta, wrapping around midnight and ignoring
// the integral number of days in rhs.
func (t NaiveTime) Add(rhs TimeDelta) NaiveTime {
	time, _ := t.OverflowingAddSigned(rhs)
	return time
}

// AddAssign is `time += rhs` for a TimeDelta.
func (t *NaiveTime) AddAssign(rhs TimeDelta) {
	*t = t.Add(rhs)
}

// stdDurationParts splits a std::time::Duration into Rust's `as_secs()` and
// `subsec_nanos()`. Rust's Duration is unsigned; a negative Go duration has
// no Rust counterpart and panics.
func stdDurationParts(duration time.Duration) (uint64, uint32) {
	if duration < 0 {
		rs.PanicAt(2, "a negative time.Duration stands for no std::time::Duration")
	}
	return uint64(duration / time.Second), uint32(duration % time.Second)
}

// AddDuration is `time + rhs` for a std::time::Duration, wrapping around
// midnight.
func (t NaiveTime) AddDuration(rhs time.Duration) NaiveTime {
	// We don't care about values beyond `24 * 60 * 60`, so we can take a modulus and avoid
	// overflow during the conversion to `TimeDelta`.
	// But we limit to double that just in case `self` is a leap-second.
	asSecs, subsecNanos := stdDurationParts(rhs)
	secs := asSecs % (2 * 24 * 60 * 60)
	d := TimeDeltaNew(int64(secs), subsecNanos).Unwrap()
	time, _ := t.OverflowingAddSigned(d)
	return time
}

// AddAssignDuration is `time += rhs` for a std::time::Duration.
func (t *NaiveTime) AddAssignDuration(rhs time.Duration) {
	*t = t.AddDuration(rhs)
}

// AddFixedOffset is `time + rhs` for a FixedOffset, wrapping around midnight
// and keeping a leap second.
func (t NaiveTime) AddFixedOffset(rhs FixedOffset) NaiveTime {
	time, _ := t.overflowingAddOffset(rhs)
	return time
}

// Sub is `time - rhs` for a TimeDelta, wrapping around midnight and ignoring
// the integral number of days in rhs.
func (t NaiveTime) Sub(rhs TimeDelta) NaiveTime {
	time, _ := t.OverflowingSubSigned(rhs)
	return time
}

// SubAssign is `time -= rhs` for a TimeDelta.
func (t *NaiveTime) SubAssign(rhs TimeDelta) {
	*t = t.Sub(rhs)
}

// SubDuration is `time - rhs` for a std::time::Duration, wrapping around
// midnight.
func (t NaiveTime) SubDuration(rhs time.Duration) NaiveTime {
	// We don't care about values beyond `24 * 60 * 60`, so we can take a modulus and avoid
	// overflow during the conversion to `TimeDelta`.
	// But we limit to double that just in case `self` is a leap-second.
	asSecs, subsecNanos := stdDurationParts(rhs)
	secs := asSecs % (2 * 24 * 60 * 60)
	d := TimeDeltaNew(int64(secs), subsecNanos).Unwrap()
	time, _ := t.OverflowingSubSigned(d)
	return time
}

// SubAssignDuration is `time -= rhs` for a std::time::Duration.
func (t *NaiveTime) SubAssignDuration(rhs time.Duration) {
	*t = t.SubDuration(rhs)
}

// SubFixedOffset is `time - rhs` for a FixedOffset, wrapping around midnight
// and keeping a leap second.
func (t NaiveTime) SubFixedOffset(rhs FixedOffset) NaiveTime {
	time, _ := t.overflowingSubOffset(rhs)
	return time
}

// SubNaiveTime is `time - rhs` for two times: the signed span between them,
// always within a day either way.
func (t NaiveTime) SubNaiveTime(rhs NaiveTime) TimeDelta {
	return t.SignedDurationSince(rhs)
}

// Equal is `==` (derived PartialEq).
func (t NaiveTime) Equal(other NaiveTime) bool { return t == other }

// Compare is `cmp` (derived Ord): by seconds since midnight, then by the
// fraction, so a leap second sorts after the 59th second it extends.
func (t NaiveTime) Compare(other NaiveTime) int {
	switch {
	case t.secs < other.secs:
		return -1
	case t.secs > other.secs:
		return 1
	case t.frac < other.frac:
		return -1
	case t.frac > other.frac:
		return 1
	}
	return 0
}

// DebugString is the Debug form: "23:56:04", with 3, 6 or 9 fractional
// digits when the fraction is not zero ("23:56:04.012"). A leap second prints
// as second 60.
func (t NaiveTime) DebugString() string {
	hour, min, sec := t.hms()
	nano := t.frac
	if t.frac >= 1_000_000_000 {
		sec, nano = sec+1, t.frac-1_000_000_000
	}

	var out []byte
	out = appendHundreds(out, uint8(hour))
	out = append(out, ':')
	out = appendHundreds(out, uint8(min))
	out = append(out, ':')
	out = appendHundreds(out, uint8(sec))

	switch {
	case nano == 0:
	case nano%1_000_000 == 0:
		out = append(out, '.')
		out = append(out, formatUintZero(uint64(nano/1_000_000), 3)...)
	case nano%1_000 == 0:
		out = append(out, '.')
		out = append(out, formatUintZero(uint64(nano/1_000), 6)...)
	default:
		out = append(out, '.')
		out = append(out, formatUintZero(uint64(nano), 9)...)
	}
	return string(out)
}

// String is the Display form, which is the Debug form.
func (t NaiveTime) String() string { return t.DebugString() }

// FmtDebug is `impl fmt::Debug for NaiveTime`. It ignores the formatter's
// width and fill.
func (t NaiveTime) FmtDebug(f *rs.Formatter) error { return f.WriteStr(t.DebugString()) }

// FmtDisplay is `impl fmt::Display for NaiveTime`. It ignores the
// formatter's width and fill.
func (t NaiveTime) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(t.String()) }

// NaiveTimeFromStr is `str::parse::<NaiveTime>()`: a time in the form
// "23:56:04" or "23:56:04.012345". It is the form of the Display and Debug
// output, read more leniently: the seconds are optional, numbers need no zero
// padding, and white space is allowed before each number and each ":".
//
// The error is a ParseError.
func NaiveTimeFromStr(s string) (NaiveTime, error) {
	parsed := FormatParsedNew()
	s, err := FormatParseAndRemainder(&parsed, s, naiveTimeHourAndMinute)
	if err != nil {
		return NaiveTime{}, err
	}
	// Seconds are optional, don't fail if parsing them doesn't succeed.
	if rest, err := FormatParseAndRemainder(&parsed, s, naiveTimeSecondAndNanos); err == nil {
		s = rest
	}
	if err := FormatParse(&parsed, s, naiveTimeTrailingWhitespace); err != nil {
		return NaiveTime{}, err
	}
	return parsed.ToNaiveTime()
}

var (
	naiveTimeHourAndMinute = []FormatItem{
		&FormatItem_Numeric{F0: FormatNumeric_Hour, F1: FormatPad_Zero},
		&FormatItem_Space{F0: ""},
		&FormatItem_Literal{F0: ":"},
		&FormatItem_Numeric{F0: FormatNumeric_Minute, F1: FormatPad_Zero},
	}
	naiveTimeSecondAndNanos = []FormatItem{
		&FormatItem_Space{F0: ""},
		&FormatItem_Literal{F0: ":"},
		&FormatItem_Numeric{F0: FormatNumeric_Second, F1: FormatPad_Zero},
		&FormatItem_Fixed{F0: FormatFixed_Nanosecond},
		&FormatItem_Space{F0: ""},
	}
	naiveTimeTrailingWhitespace = []FormatItem{&FormatItem_Space{F0: ""}}
)

// NaiveTimeDefault is `NaiveTime::default()`: midnight, the Go zero value.
func NaiveTimeDefault() NaiveTime {
	return NaiveTimeFromHmsOpt(0, 0, 0).Unwrap()
}
