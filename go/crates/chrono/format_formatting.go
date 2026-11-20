// Port of chrono 0.4.44 src/format/formatting.rs

package chrono

// Date and time formatting routines.

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// FormatDelayedFormat is a *temporary* object which can be used as an
// argument to `format!` or others (`format::DelayedFormat`). This is normally
// constructed via the `Format` methods of each date and time type.
//
// Nothing is formatted until the value is printed: String (Rust's
// `to_string()`) panics when the items cannot be formatted, WriteTo returns
// the error instead, and FmtDisplay passes it to the formatter, which is how
// `write!` reports it and why `format!` panics.
//
// The derived Debug form of the Rust struct is not provided.
type FormatDelayedFormat struct {
	// The date view, if any.
	date rs.Option[NaiveDate]
	// The time view, if any.
	time rs.Option[NaiveTime]
	// The name and local-to-UTC difference for the offset (timezone), if any.
	off rs.Option[rs.Tuple2[string, FixedOffset]]
	// The items to be formatted, which Rust keeps as an iterator.
	items []FormatItem
}

// FormatDelayedFormatNew is `DelayedFormat::new`: a new FormatDelayedFormat
// value out of local date and time.
func FormatDelayedFormatNew(date rs.Option[NaiveDate], time rs.Option[NaiveTime], items []FormatItem) FormatDelayedFormat {
	return FormatDelayedFormat{date: date, time: time, off: rs.None[rs.Tuple2[string, FixedOffset]](), items: items}
}

// FormatDelayedFormatNewWithOffset is `DelayedFormat::new_with_offset`: a new
// FormatDelayedFormat value out of local date and time and UTC offset.
func FormatDelayedFormatNewWithOffset(date rs.Option[NaiveDate], time rs.Option[NaiveTime], offset Offset, items []FormatItem) FormatDelayedFormat {
	nameAndDiff := rs.T2(offset.String(), offset.Fix())
	return FormatDelayedFormat{date: date, time: time, off: rs.Some(nameAndDiff), items: items}
}

// WriteTo is `DelayedFormat::write_to`: the formatted text appended to w, as
// the Display impl produces it but without a formatter's padding.
//
// It returns an error (Rust's `fmt::Error`) when an item is the error item,
// which is what an invalid format string leaves behind, or when an item
// needs a date, a time or an offset that the value does not have. What was
// formatted before the failing item stays in w.
func (d FormatDelayedFormat) WriteTo(w *strings.Builder) error {
	for _, item := range d.items {
		var err error
		switch item := item.(type) {
		case *FormatItem_Literal:
			w.WriteString(item.F0)
		case *FormatItem_Space:
			w.WriteString(item.F0)
		case *FormatItem_OwnedLiteral:
			w.WriteString(item.F0)
		case *FormatItem_OwnedSpace:
			w.WriteString(item.F0)
		case *FormatItem_Numeric:
			err = d.formatNumeric(w, item.F0, item.F1)
		case *FormatItem_Fixed:
			err = d.formatFixed(w, item.F0)
		case *FormatItem_Error:
			err = errFmt
		default:
			panic(rs.Unreachable(item))
		}
		if err != nil {
			return err
		}
	}
	return nil
}

func writeOne(w *strings.Builder, v uint8) error {
	w.WriteByte('0' + v)
	return nil
}

func writeTwo(w *strings.Builder, v uint8, pad FormatPad) error {
	ones := '0' + v%10
	tens := v / 10
	switch {
	case tens == 0 && pad == FormatPad_None:
	case tens == 0 && pad == FormatPad_Space:
		w.WriteByte(' ')
	default:
		w.WriteByte('0' + tens)
	}
	w.WriteByte(ones)
	return nil
}

func writeYear(w *strings.Builder, year int32, pad FormatPad) error {
	if 1000 <= year && year <= 9999 {
		// fast path
		if err := writeHundreds(w, uint8(year/100)); err != nil {
			return err
		}
		return writeHundreds(w, uint8(year%100))
	}
	return writeN(w, 4, int64(year), pad, !(0 <= year && year < 10_000))
}

func writeN(w *strings.Builder, n int, v int64, pad FormatPad, alwaysSign bool) error {
	if alwaysSign {
		switch pad {
		case FormatPad_None:
			w.WriteString(formatInt(v, 0, true, false))
		case FormatPad_Zero:
			w.WriteString(formatInt(v, n+1, true, true))
		case FormatPad_Space:
			w.WriteString(formatInt(v, n+1, true, false))
		}
	} else {
		switch pad {
		case FormatPad_None:
			w.WriteString(formatInt(v, 0, false, false))
		case FormatPad_Zero:
			w.WriteString(formatInt(v, n, false, true))
		case FormatPad_Space:
			w.WriteString(formatInt(v, n, false, false))
		}
	}
	return nil
}

func (d FormatDelayedFormat) formatNumeric(w *strings.Builder, spec FormatNumeric, pad FormatPad) error {
	date, hasDate := d.date.Get()
	time, hasTime := d.time.Get()
	switch {
	case spec == FormatNumeric_Year && hasDate:
		return writeYear(w, date.Year(), pad)
	case spec == FormatNumeric_YearDiv100 && hasDate:
		return writeTwo(w, uint8(divEuclid(date.Year(), 100)), pad)
	case spec == FormatNumeric_YearMod100 && hasDate:
		return writeTwo(w, uint8(remEuclid(date.Year(), 100)), pad)
	case spec == FormatNumeric_IsoYear && hasDate:
		return writeYear(w, date.IsoWeek().Year(), pad)
	case spec == FormatNumeric_IsoYearDiv100 && hasDate:
		return writeTwo(w, uint8(divEuclid(date.IsoWeek().Year(), 100)), pad)
	case spec == FormatNumeric_IsoYearMod100 && hasDate:
		return writeTwo(w, uint8(remEuclid(date.IsoWeek().Year(), 100)), pad)
	case spec == FormatNumeric_Quarter && hasDate:
		return writeOne(w, uint8(date.Quarter()))
	case spec == FormatNumeric_Month && hasDate:
		return writeTwo(w, uint8(date.Month()), pad)
	case spec == FormatNumeric_Day && hasDate:
		return writeTwo(w, uint8(date.Day()), pad)
	case spec == FormatNumeric_WeekFromSun && hasDate:
		return writeTwo(w, uint8(date.weeksFrom(Weekday_Sun)), pad)
	case spec == FormatNumeric_WeekFromMon && hasDate:
		return writeTwo(w, uint8(date.weeksFrom(Weekday_Mon)), pad)
	case spec == FormatNumeric_IsoWeek && hasDate:
		return writeTwo(w, uint8(date.IsoWeek().Week()), pad)
	case spec == FormatNumeric_NumDaysFromSun && hasDate:
		return writeOne(w, uint8(date.Weekday().NumDaysFromSunday()))
	case spec == FormatNumeric_WeekdayFromMon && hasDate:
		return writeOne(w, uint8(date.Weekday().NumberFromMonday()))
	case spec == FormatNumeric_Ordinal && hasDate:
		return writeN(w, 3, int64(date.Ordinal()), pad, false)
	case spec == FormatNumeric_Hour && hasTime:
		return writeTwo(w, uint8(time.Hour()), pad)
	case spec == FormatNumeric_Hour12 && hasTime:
		_, hour12 := time.Hour12()
		return writeTwo(w, uint8(hour12), pad)
	case spec == FormatNumeric_Minute && hasTime:
		return writeTwo(w, uint8(time.Minute()), pad)
	case spec == FormatNumeric_Second && hasTime:
		return writeTwo(w, uint8(time.Second()+time.Nanosecond()/1_000_000_000), pad)
	case spec == FormatNumeric_Nanosecond && hasTime:
		return writeN(w, 9, int64(time.Nanosecond()%1_000_000_000), pad, false)
	case spec == FormatNumeric_Timestamp && hasDate && hasTime:
		offset := int64(0)
		if off, ok := d.off.Get(); ok {
			offset = int64(off.F1.LocalMinusUtc())
		}
		timestamp := date.AndTime(time).AndUtc().Timestamp() - offset
		return writeN(w, 9, timestamp, pad, false)
	default:
		return errFmt // insufficient arguments for given format
	}
}

func (d FormatDelayedFormat) formatFixed(w *strings.Builder, spec FormatFixed) error {
	date, hasDate := d.date.Get()
	time, hasTime := d.time.Get()
	off, hasOff := d.off.Get()
	switch {
	case spec == FormatFixed_ShortMonthName && hasDate:
		w.WriteString(shortMonths[date.Month0()])
		return nil
	case spec == FormatFixed_LongMonthName && hasDate:
		w.WriteString(longMonths[date.Month0()])
		return nil
	case spec == FormatFixed_ShortWeekdayName && hasDate:
		w.WriteString(shortWeekdays[date.Weekday().NumDaysFromSunday()])
		return nil
	case spec == FormatFixed_LongWeekdayName && hasDate:
		w.WriteString(longWeekdays[date.Weekday().NumDaysFromSunday()])
		return nil
	case spec == FormatFixed_LowerAmPm && hasTime:
		ampm := amPm[0]
		if pm, _ := time.Hour12(); pm {
			ampm = amPm[1]
		}
		w.WriteString(strings.ToLower(ampm))
		return nil
	case spec == FormatFixed_UpperAmPm && hasTime:
		ampm := amPm[0]
		if pm, _ := time.Hour12(); pm {
			ampm = amPm[1]
		}
		w.WriteString(ampm)
		return nil
	case spec == FormatFixed_Nanosecond && hasTime:
		nano := time.Nanosecond() % 1_000_000_000
		if nano == 0 {
			return nil
		}
		w.WriteString(decimalPoint)
		if nano%1_000_000 == 0 {
			w.WriteString(formatUintZero(uint64(nano/1_000_000), 3))
		} else if nano%1_000 == 0 {
			w.WriteString(formatUintZero(uint64(nano/1_000), 6))
		} else {
			w.WriteString(formatUintZero(uint64(nano), 9))
		}
		return nil
	case spec == FormatFixed_Nanosecond3 && hasTime:
		w.WriteString(decimalPoint)
		w.WriteString(formatUintZero(uint64(time.Nanosecond()/1_000_000%1000), 3))
		return nil
	case spec == FormatFixed_Nanosecond6 && hasTime:
		w.WriteString(decimalPoint)
		w.WriteString(formatUintZero(uint64(time.Nanosecond()/1_000%1_000_000), 6))
		return nil
	case spec == FormatFixed_Nanosecond9 && hasTime:
		w.WriteString(decimalPoint)
		w.WriteString(formatUintZero(uint64(time.Nanosecond()%1_000_000_000), 9))
		return nil
	case spec == formatFixedNanosecond3NoDot && hasTime:
		w.WriteString(formatUintZero(uint64(time.Nanosecond()/1_000_000%1_000), 3))
		return nil
	case spec == formatFixedNanosecond6NoDot && hasTime:
		w.WriteString(formatUintZero(uint64(time.Nanosecond()/1_000%1_000_000), 6))
		return nil
	case spec == formatFixedNanosecond9NoDot && hasTime:
		w.WriteString(formatUintZero(uint64(time.Nanosecond()%1_000_000_000), 9))
		return nil
	case spec == FormatFixed_TimezoneName && hasOff:
		w.WriteString(off.F0)
		return nil
	case (spec == FormatFixed_TimezoneOffset || spec == FormatFixed_TimezoneOffsetZ) && hasOff:
		offsetFormat := FormatOffsetFormat{
			Precision: FormatOffsetPrecision_Minutes,
			Colons:    FormatColons_Maybe,
			AllowZulu: spec == FormatFixed_TimezoneOffsetZ,
			Padding:   FormatPad_Zero,
		}
		return offsetFormat.format(w, off.F1)
	case (spec == FormatFixed_TimezoneOffsetColon || spec == FormatFixed_TimezoneOffsetColonZ) && hasOff:
		offsetFormat := FormatOffsetFormat{
			Precision: FormatOffsetPrecision_Minutes,
			Colons:    FormatColons_Colon,
			AllowZulu: spec == FormatFixed_TimezoneOffsetColonZ,
			Padding:   FormatPad_Zero,
		}
		return offsetFormat.format(w, off.F1)
	case spec == FormatFixed_TimezoneOffsetDoubleColon && hasOff:
		offsetFormat := FormatOffsetFormat{
			Precision: FormatOffsetPrecision_Seconds,
			Colons:    FormatColons_Colon,
			AllowZulu: false,
			Padding:   FormatPad_Zero,
		}
		return offsetFormat.format(w, off.F1)
	case spec == FormatFixed_TimezoneOffsetTripleColon && hasOff:
		offsetFormat := FormatOffsetFormat{
			Precision: FormatOffsetPrecision_Hours,
			Colons:    FormatColons_None,
			AllowZulu: false,
			Padding:   FormatPad_Zero,
		}
		return offsetFormat.format(w, off.F1)
	case spec == FormatFixed_RFC2822 && hasDate && hasTime && hasOff:
		return writeRfc2822(w, NaiveDateTimeNew(date, time), off.F1)
	case spec == FormatFixed_RFC3339 && hasDate && hasTime && hasOff:
		return writeRfc3339(w, NaiveDateTimeNew(date, time), off.F1, SecondsFormat_AutoSi, false)
	default:
		return errFmt // insufficient arguments for given format
	}
}

// format is the private `DelayedFormat::format`: the text, or the
// formatting error.
func (d FormatDelayedFormat) format() (string, error) {
	var result strings.Builder
	if err := d.WriteTo(&result); err != nil {
		return "", err
	}
	return result.String(), nil
}

// String is `to_string()`: the formatted text.
//
// Like Rust's `to_string()` on a Display impl that fails, it panics with "a
// Display implementation returned an error unexpectedly: Error" when the
// format string is invalid or asks for a field the value does not have. Use
// WriteTo to get the error instead.
func (d FormatDelayedFormat) String() string {
	result, err := d.format()
	if err != nil {
		rs.PanicAt(1, "a Display implementation returned an error unexpectedly: Error")
	}
	return result
}

// FmtDisplay is `impl Display for DelayedFormat`: the formatted text, padded
// as the formatter asks (`{:<10}`), or the formatting error.
func (d FormatDelayedFormat) FmtDisplay(f *rs.Formatter) error {
	result, err := d.format()
	if err != nil {
		return err
	}
	return f.Pad(result)
}

func (o FormatOffsetFormat) format(w *strings.Builder, offset FixedOffset) error {
	off := offset.LocalMinusUtc()
	if o.AllowZulu && off == 0 {
		w.WriteByte('Z')
		return nil
	}
	sign := byte('+')
	if off < 0 {
		sign, off = '-', -off
	}

	var hours uint8
	var mins uint8
	var secs uint8
	var precision FormatOffsetPrecision
	switch o.Precision {
	case FormatOffsetPrecision_Hours:
		// Minutes and seconds are simply truncated
		hours = uint8(off / 3600)
		precision = FormatOffsetPrecision_Hours
	case FormatOffsetPrecision_Minutes, FormatOffsetPrecision_OptionalMinutes:
		// Round seconds to the nearest minute.
		minutes := (off + 30) / 60
		mins = uint8(minutes % 60)
		hours = uint8(minutes / 60)
		if o.Precision == FormatOffsetPrecision_OptionalMinutes && mins == 0 {
			precision = FormatOffsetPrecision_Hours
		} else {
			precision = FormatOffsetPrecision_Minutes
		}
	case FormatOffsetPrecision_Seconds,
		FormatOffsetPrecision_OptionalSeconds,
		FormatOffsetPrecision_OptionalMinutesAndSeconds:
		minutes := off / 60
		secs = uint8(off % 60)
		mins = uint8(minutes % 60)
		hours = uint8(minutes / 60)
		if o.Precision != FormatOffsetPrecision_Seconds && secs == 0 {
			if o.Precision == FormatOffsetPrecision_OptionalMinutesAndSeconds && mins == 0 {
				precision = FormatOffsetPrecision_Hours
			} else {
				precision = FormatOffsetPrecision_Minutes
			}
		} else {
			precision = FormatOffsetPrecision_Seconds
		}
	default:
		panic(rs.Unreachable(uint8(o.Precision)))
	}
	colons := o.Colons == FormatColons_Colon

	if hours < 10 {
		if o.Padding == FormatPad_Space {
			w.WriteByte(' ')
		}
		w.WriteByte(sign)
		if o.Padding == FormatPad_Zero {
			w.WriteByte('0')
		}
		w.WriteByte('0' + hours)
	} else {
		w.WriteByte(sign)
		if err := writeHundreds(w, hours); err != nil {
			return err
		}
	}
	if precision == FormatOffsetPrecision_Minutes || precision == FormatOffsetPrecision_Seconds {
		if colons {
			w.WriteByte(':')
		}
		if err := writeHundreds(w, mins); err != nil {
			return err
		}
	}
	if precision == FormatOffsetPrecision_Seconds {
		if colons {
			w.WriteByte(':')
		}
		if err := writeHundreds(w, secs); err != nil {
			return err
		}
	}
	return nil
}

// SecondsFormat specifies the format of the seconds in DateTime.ToRfc3339Opts
// (`chrono::SecondsFormat`, also `format::SecondsFormat`).
type SecondsFormat uint8

const (
	// SecondsFormat_Secs formats whole seconds only, with no decimal point
	// nor subseconds.
	SecondsFormat_Secs SecondsFormat = iota

	// SecondsFormat_Millis uses fixed 3 subsecond digits. This corresponds to
	// FormatFixed_Nanosecond3.
	SecondsFormat_Millis

	// SecondsFormat_Micros uses fixed 6 subsecond digits. This corresponds to
	// FormatFixed_Nanosecond6.
	SecondsFormat_Micros

	// SecondsFormat_Nanos uses fixed 9 subsecond digits. This corresponds to
	// FormatFixed_Nanosecond9.
	SecondsFormat_Nanos

	// SecondsFormat_AutoSi automatically selects one of Secs, Millis, Micros,
	// or Nanos to display all available non-zero sub-second digits. This
	// corresponds to FormatFixed_Nanosecond.
	SecondsFormat_AutoSi
)

var _ = rs.RegisterEnum[SecondsFormat]("SecondsFormat",
	rs.Const(SecondsFormat_Secs, "Secs"),
	rs.Const(SecondsFormat_Millis, "Millis"),
	rs.Const(SecondsFormat_Micros, "Micros"),
	rs.Const(SecondsFormat_Nanos, "Nanos"),
	rs.Const(SecondsFormat_AutoSi, "AutoSi"),
)

// writeRfc3339 writes the date, time and offset to the string. same as
// `%Y-%m-%dT%H:%M:%S%.f%:z`
func writeRfc3339(w *strings.Builder, dt NaiveDateTime, off FixedOffset, secform SecondsFormat, useZ bool) error {
	year := dt.Date().Year()
	if 0 <= year && year <= 9999 {
		if err := writeHundreds(w, uint8(year/100)); err != nil {
			return err
		}
		if err := writeHundreds(w, uint8(year%100)); err != nil {
			return err
		}
	} else {
		// ISO 8601 requires the explicit sign for out-of-range years
		w.WriteString(formatInt(int64(year), 5, true, true))
	}
	w.WriteByte('-')
	if err := writeHundreds(w, uint8(dt.Date().Month())); err != nil {
		return err
	}
	w.WriteByte('-')
	if err := writeHundreds(w, uint8(dt.Date().Day())); err != nil {
		return err
	}

	w.WriteByte('T')

	hour, min, sec := dt.Time().hms()
	nano := dt.Nanosecond()
	if nano >= 1_000_000_000 {
		sec++
		nano -= 1_000_000_000
	}
	if err := writeHundreds(w, uint8(hour)); err != nil {
		return err
	}
	w.WriteByte(':')
	if err := writeHundreds(w, uint8(min)); err != nil {
		return err
	}
	w.WriteByte(':')
	if err := writeHundreds(w, uint8(sec)); err != nil {
		return err
	}

	switch secform {
	case SecondsFormat_Secs:
	case SecondsFormat_Millis:
		w.WriteByte('.')
		w.WriteString(formatUintZero(uint64(nano/1_000_000), 3))
	case SecondsFormat_Micros:
		w.WriteByte('.')
		w.WriteString(formatUintZero(uint64(nano/1000), 6))
	case SecondsFormat_Nanos:
		w.WriteByte('.')
		w.WriteString(formatUintZero(uint64(nano), 9))
	case SecondsFormat_AutoSi:
		if nano == 0 {
		} else if nano%1_000_000 == 0 {
			w.WriteByte('.')
			w.WriteString(formatUintZero(uint64(nano/1_000_000), 3))
		} else if nano%1_000 == 0 {
			w.WriteByte('.')
			w.WriteString(formatUintZero(uint64(nano/1_000), 6))
		} else {
			w.WriteByte('.')
			w.WriteString(formatUintZero(uint64(nano), 9))
		}
	default:
		panic(rs.Unreachable())
	}

	return FormatOffsetFormat{
		Precision: FormatOffsetPrecision_Minutes,
		Colons:    FormatColons_Colon,
		AllowZulu: useZ,
		Padding:   FormatPad_Zero,
	}.format(w, off)
}

// writeRfc2822 writes datetimes like `Tue, 1 Jul 2003 10:52:37 +0200`, same
// as `%a, %d %b %Y %H:%M:%S %z`
func writeRfc2822(w *strings.Builder, dt NaiveDateTime, off FixedOffset) error {
	year := dt.Year()
	// RFC2822 is only defined on years 0 through 9999
	if !(0 <= year && year <= 9999) {
		return errFmt
	}

	w.WriteString(shortWeekdays[dt.Weekday().NumDaysFromSunday()])
	w.WriteString(", ")
	day := dt.Day()
	if day < 10 {
		w.WriteByte('0' + uint8(day))
	} else if err := writeHundreds(w, uint8(day)); err != nil {
		return err
	}
	w.WriteByte(' ')
	w.WriteString(shortMonths[dt.Month0()])
	w.WriteByte(' ')
	if err := writeHundreds(w, uint8(year/100)); err != nil {
		return err
	}
	if err := writeHundreds(w, uint8(year%100)); err != nil {
		return err
	}
	w.WriteByte(' ')

	hour, min, sec := dt.Time().hms()
	if err := writeHundreds(w, uint8(hour)); err != nil {
		return err
	}
	w.WriteByte(':')
	if err := writeHundreds(w, uint8(min)); err != nil {
		return err
	}
	w.WriteByte(':')
	sec = sec + dt.Nanosecond()/1_000_000_000
	if err := writeHundreds(w, uint8(sec)); err != nil {
		return err
	}
	w.WriteByte(' ')
	return FormatOffsetFormat{
		Precision: FormatOffsetPrecision_Minutes,
		Colons:    FormatColons_None,
		AllowZulu: false,
		Padding:   FormatPad_Zero,
	}.format(w, off)
}

// writeHundreds writes two decimal digits, or fails for a value over 99.
func writeHundreds(w *strings.Builder, n uint8) error {
	if n >= 100 {
		return errFmt
	}

	tens := '0' + n/10
	ones := '0' + n%10
	w.WriteByte(tens)
	w.WriteByte(ones)
	return nil
}

// appendHundreds is writeHundreds for the Debug forms, whose values are
// always under 100.
func appendHundreds(out []byte, n uint8) []byte {
	return append(out, '0'+n/10, '0'+n%10)
}
