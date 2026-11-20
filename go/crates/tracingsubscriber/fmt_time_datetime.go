// Port of tracing-subscriber 0.3.23 src/fmt/time/datetime.rs
//
// The conversion is musl's __secs_to_tm, as the crate has it; musl's
// licence (MIT) is reproduced in the crate's file.

package tracingsubscriber

import (
	"strconv"
	"time"
)

// dateTime is the crate's DateTime: a UTC date and time with nanoseconds.
type dateTime struct {
	year   int64
	month  uint8
	day    uint8
	hour   uint8
	minute uint8
	second uint8
	nanos  uint32
}

// String is `impl Display for DateTime`: RFC 3339 with microseconds.
func (d dateTime) String() string {
	var out []byte
	switch {
	case d.year > 9999:
		out = append(out, '+')
		out = strconv.AppendInt(out, d.year, 10)
	case d.year < 0:
		// "{:05}": the sign counts towards the width.
		digits := strconv.FormatInt(-d.year, 10)
		out = append(out, '-')
		for pad := len(digits) + 1; pad < 5; pad++ {
			out = append(out, '0')
		}
		out = append(out, digits...)
	default:
		out = appendPadded(out, uint64(d.year), 4)
	}
	out = append(out, '-')
	out = appendPadded(out, uint64(d.month), 2)
	out = append(out, '-')
	out = appendPadded(out, uint64(d.day), 2)
	out = append(out, 'T')
	out = appendPadded(out, uint64(d.hour), 2)
	out = append(out, ':')
	out = appendPadded(out, uint64(d.minute), 2)
	out = append(out, ':')
	out = appendPadded(out, uint64(d.second), 2)
	out = append(out, '.')
	out = appendPadded(out, uint64(d.nanos/1_000), 6)
	out = append(out, 'Z')
	return string(out)
}

func appendPadded(out []byte, value uint64, width int) []byte {
	digits := strconv.FormatUint(value, 10)
	for pad := len(digits); pad < width; pad++ {
		out = append(out, '0')
	}
	return append(out, digits...)
}

// dateTimeFromSystemTime is `impl From<SystemTime> for DateTime`.
func dateTimeFromSystemTime(timestamp time.Time) dateTime {
	secs := timestamp.Unix()
	nanos := uint32(timestamp.Nanosecond())
	return dateTimeFromUnix(secs, nanos)
}

// dateTimeFromUnix is the conversion of a time given as seconds since the
// Unix epoch (negative before it) and the nanoseconds after that second.
func dateTimeFromUnix(t int64, nanos uint32) dateTime {
	// 2000-03-01 (mod 400 year, immediately after feb29
	const leapoch int64 = 946_684_800 + 86400*(31+29)
	const daysPer400y int32 = 365*400 + 97
	const daysPer100y int32 = 365*100 + 24
	const daysPer4y int32 = 365*4 + 1
	daysInMonth := [12]int8{31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 31, 29}

	// Note(dcb): this bit is rearranged slightly to avoid integer overflow.
	days := (t / 86_400) - (leapoch / 86_400)
	remsecs := int32(t % 86_400)
	if remsecs < 0 {
		remsecs += 86_400
		days--
	}

	qcCycles := int32(days / int64(daysPer400y))
	remdays := int32(days % int64(daysPer400y))
	if remdays < 0 {
		remdays += daysPer400y
		qcCycles--
	}

	cCycles := remdays / daysPer100y
	if cCycles == 4 {
		cCycles--
	}
	remdays -= cCycles * daysPer100y

	qCycles := remdays / daysPer4y
	if qCycles == 25 {
		qCycles--
	}
	remdays -= qCycles * daysPer4y

	remyears := remdays / 365
	if remyears == 4 {
		remyears--
	}
	remdays -= remyears * 365

	years := int64(remyears) + 4*int64(qCycles) + 100*int64(cCycles) + 400*int64(qcCycles)

	months := int32(0)
	for int32(daysInMonth[months]) <= remdays {
		remdays -= int32(daysInMonth[months])
		months++
	}

	if months >= 10 {
		months -= 12
		years++
	}

	return dateTime{
		year:   years + 2000,
		month:  uint8(months + 3),
		day:    uint8(remdays + 1),
		hour:   uint8(remsecs / 3600),
		minute: uint8(remsecs / 60 % 60),
		second: uint8(remsecs % 60),
		nanos:  nanos,
	}
}
