// Port of toml_edit 0.20.2 src/parser/datetime.rs

package tomledit

import (
	"math"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ;; Date and Time (as defined in RFC 3339)

// date-time = offset-date-time / local-date-time / local-date / local-time
// offset-date-time = full-date time-delim full-time
// local-date-time = full-date time-delim partial-time
// local-date = full-date
// local-time = partial-time
// full-time = partial-time time-offset
func dateTime(in *input) (Datetime, parseErr) {
	// alt((
	//     (full_date, opt((time_delim, partial_time, opt(time_offset))))
	//         .map(..)
	//         .context(StrContext::Label("date-time")),
	//     partial_time
	//         .map(|t| t.into())
	//         .context(StrContext::Label("time")),
	// ))
	start := in.pos
	parsed, e := func() (Datetime, parseErr) {
		date, e := fullDate(in)
		if e.failed() {
			return Datetime{}, e
		}
		// opt((time_delim, partial_time, opt(time_offset)))
		beforeTime := in.pos
		time, offset, e := func() (Time, rs.Option[Offset], parseErr) {
			if !timeDelim(in) {
				return Time{}, rs.None[Offset](), backtrack()
			}
			time, e := partialTime(in)
			if e.failed() {
				return Time{}, rs.None[Offset](), e
			}
			beforeOffset := in.pos
			offset, e := timeOffset(in)
			switch e.mode {
			case errBacktrack:
				in.pos = beforeOffset
				return time, rs.None[Offset](), parseErr{}
			case errCut:
				return Time{}, rs.None[Offset](), e
			}
			return time, rs.Some(offset), parseErr{}
		}()
		switch e.mode {
		case errBacktrack:
			in.pos = beforeTime
			// Local Date
			return Datetime{Date: rs.Some(date), Time: rs.None[Time](), Offset: rs.None[Offset]()}, parseErr{}
		case errCut:
			return Datetime{}, e
		}
		// Offset Date-Time
		return Datetime{Date: rs.Some(date), Time: rs.Some(time), Offset: offset}, parseErr{}
	}()
	switch e.mode {
	case errNone:
		return parsed, parseErr{}
	case errCut:
		return Datetime{}, e.add(label("date-time"))
	}
	in.pos = start
	time, e := partialTime(in)
	if e.failed() {
		return Datetime{}, e.add(label("time"))
	}
	return DatetimeFromTime(time), parseErr{}
}

// full-date      = date-fullyear "-" date-month "-" date-mday
func fullDate(in *input) (Date, parseErr) {
	// (date_fullyear, b'-', cut_err((date_month, b'-', date_mday)))
	year, e := dateFullyear(in)
	if e.failed() {
		return Date{}, e
	}
	if !in.eat('-') {
		return Date{}, backtrack()
	}
	month, e := dateMonth(in)
	if e.failed() {
		return Date{}, e.cut()
	}
	if !in.eat('-') {
		return Date{}, cutEmpty()
	}
	day, e := dateMday(in)
	if e.failed() {
		return Date{}, e.cut()
	}
	return Date{Year: year, Month: month, Day: day}, parseErr{}
}

// partial-time   = time-hour ":" time-minute ":" time-second [time-secfrac]
func partialTime(in *input) (Time, parseErr) {
	// (
	//     time_hour,
	//     b':',
	//     cut_err((time_minute, b':', time_second, opt(time_secfrac))),
	// )
	hour, e := timeHour(in)
	if e.failed() {
		return Time{}, e
	}
	if !in.eat(':') {
		return Time{}, backtrack()
	}
	minute, e := timeMinute(in)
	if e.failed() {
		return Time{}, e.cut()
	}
	if !in.eat(':') {
		return Time{}, cutEmpty()
	}
	second, e := timeSecond(in)
	if e.failed() {
		return Time{}, e.cut()
	}
	beforeSecfrac := in.pos
	nanosecond, e := timeSecfrac(in)
	switch e.mode {
	case errBacktrack:
		in.pos = beforeSecfrac
		// nanosecond.unwrap_or_default()
		nanosecond = 0
	case errCut:
		return Time{}, e
	}
	return Time{Hour: hour, Minute: minute, Second: second, Nanosecond: nanosecond}, parseErr{}
}

// time-offset    = "Z" / time-numoffset
// time-numoffset = ( "+" / "-" ) time-hour ":" time-minute
func timeOffset(in *input) (Offset, parseErr) {
	offset, e := func() (Offset, parseErr) {
		// alt((
		//     one_of((b'Z', b'z')).value(Offset::Z),
		//     (one_of((b'+', b'-')), cut_err((time_hour, b':', time_minute)))
		//         .map(..)
		//         .verify(|minutes| ((-24 * 60)..=(24 * 60)).contains(minutes))
		//         .map(|minutes| Offset::Custom { minutes }),
		// ))
		if in.oneOf(func(b byte) bool { return b == 'Z' || b == 'z' }) {
			return &Offset_Z{}, parseErr{}
		}
		start := in.pos
		signByte, ok := in.peek()
		if !ok || !isSign(signByte) {
			return nil, backtrack()
		}
		in.pos++
		hours, e := timeHour(in)
		if e.failed() {
			return nil, e.cut()
		}
		if !in.eat(':') {
			return nil, cutEmpty()
		}
		minutes, e := timeMinute(in)
		if e.failed() {
			return nil, e.cut()
		}
		sign := 1
		if signByte == '-' {
			sign = -1
		}
		total := sign * (int(hours)*60 + int(minutes))
		if total < -24*60 || total > 24*60 {
			in.pos = start
			return nil, backtrack()
		}
		return &Offset_Custom{Minutes: int16(total)}, parseErr{}
	}()
	if e.failed() {
		return nil, e.add(label("time offset"))
	}
	return offset, parseErr{}
}

// date-fullyear  = 4DIGIT
func dateFullyear(in *input) (uint16, parseErr) {
	s, e := unsignedDigits(in, 4, 4)
	if e.failed() {
		return 0, e
	}
	// s.parse::<u16>().expect("4DIGIT should match u8")
	parsed, _ := strconv.ParseUint(s, 10, 16)
	return uint16(parsed), parseErr{}
}

// twoDigits is the shape of every two-digit field:
//
//	unsigned_digits::<2, 2>
//	    .try_map(|s: &str| {
//	        let d = s.parse::<u8>().expect("2DIGIT should match u8");
//	        if (low..=high).contains(&d) {
//	            Ok(d)
//	        } else {
//	            Err(CustomError::OutOfRange)
//	        }
//	    })
func twoDigits(in *input, low, high uint8) (uint8, parseErr) {
	start := in.pos
	s, e := unsignedDigits(in, 2, 2)
	if e.failed() {
		return 0, e
	}
	d := (s[0]-'0')*10 + (s[1] - '0')
	if d < low || d > high {
		in.pos = start
		return 0, external(&customError_OutOfRange{})
	}
	return d, parseErr{}
}

// date-month     = 2DIGIT  ; 01-12
func dateMonth(in *input) (uint8, parseErr) {
	return twoDigits(in, 1, 12)
}

// date-mday      = 2DIGIT  ; 01-28, 01-29, 01-30, 01-31 based on month/year
func dateMday(in *input) (uint8, parseErr) {
	return twoDigits(in, 1, 31)
}

// time-delim     = "T" / %x20 ; T, t, or space
func timeDelim(in *input) bool {
	return in.oneOf(func(b byte) bool { return b == 'T' || b == 't' || b == ' ' })
}

// time-hour      = 2DIGIT  ; 00-23
func timeHour(in *input) (uint8, parseErr) {
	return twoDigits(in, 0, 23)
}

// time-minute    = 2DIGIT  ; 00-59
func timeMinute(in *input) (uint8, parseErr) {
	return twoDigits(in, 0, 59)
}

// time-second    = 2DIGIT  ; 00-58, 00-59, 00-60 based on leap second rules
func timeSecond(in *input) (uint8, parseErr) {
	return twoDigits(in, 0, 60)
}

// time-secfrac   = "." 1*DIGIT
func timeSecfrac(in *input) (uint32, parseErr) {
	scale := [10]uint32{
		0,
		100_000_000,
		10_000_000,
		1_000_000,
		100_000,
		10_000,
		1_000,
		100,
		10,
		1,
	}
	// preceded(b'.', unsigned_digits::<1, INF>).try_map(..)
	start := in.pos
	if !in.eat('.') {
		return 0, backtrack()
	}
	repr, e := unsignedDigits(in, 1, math.MaxInt)
	if e.failed() {
		return 0, e
	}
	maxDigits := len(scale) - 1
	if maxDigits < len(repr) {
		// Millisecond precision is required. Further precision of fractional seconds is
		// implementation-specific. If the value contains greater precision than the
		// implementation can support, the additional precision must be truncated, not rounded.
		repr = repr[0:maxDigits]
	}

	v, err := strconv.ParseUint(repr, 10, 32)
	if err != nil {
		in.pos = start
		return 0, external(&customError_OutOfRange{})
	}
	numDigits := len(repr)

	// scale the number accordingly.
	if numDigits >= len(scale) {
		in.pos = start
		return 0, external(&customError_OutOfRange{})
	}
	scaled := v * uint64(scale[numDigits])
	if scaled > math.MaxUint32 {
		in.pos = start
		return 0, external(&customError_OutOfRange{})
	}
	return uint32(scaled), parseErr{}
}

func unsignedDigits(in *input, low, high int) (string, parseErr) {
	s, ok := in.takeWhileMN(low, high, isDigit)
	if !ok {
		return "", backtrack()
	}
	return s, parseErr{}
}
