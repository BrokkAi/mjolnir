// Port of core 1.99.1 src/time.rs (impl Debug for Duration) and std 1.99.1
// src/sys/time/unix.rs, src/sys/time/windows.rs (impl Debug for SystemTime)

package rs

import (
	"strconv"
	"time"
	"unicode/utf8"
)

const (
	fmtNanosPerSec   = 1_000_000_000
	fmtNanosPerMilli = 1_000_000
	fmtNanosPerMicro = 1_000
)

// fmtDebugDuration is `impl Debug for Duration`: `1.5s`, `2ms`, `3µs`, `4ns`.
//
// A Rust Duration is never negative; a Go one can be. A negative duration
// prints as its magnitude after a `-`, which no Rust program prints.
func fmtDebugDuration(d time.Duration, f *Formatter) error {
	// Print leading '+' sign if requested
	prefix := ""
	if f.SignPlus() {
		prefix = "+"
	}

	var total uint64
	if d < 0 {
		prefix = "-"
		total = uint64(-(d + 1)) + 1
	} else {
		total = uint64(d)
	}
	return fmtDebugDurationParts(total/fmtNanosPerSec, uint32(total%fmtNanosPerSec), prefix, f)
}

// fmtDebugDurationParts is the body of Duration's Debug for the whole range
// of a Rust Duration, which a time.Duration covers only in part: secs
// seconds and nanos nanoseconds, after prefix.
func fmtDebugDurationParts(secs uint64, nanos uint32, prefix string, f *Formatter) error {
	if secs > 0 {
		return fmtDurationDecimal(f, secs, nanos, fmtNanosPerSec/10, prefix, "s")
	} else if nanos >= fmtNanosPerMilli {
		return fmtDurationDecimal(
			f,
			uint64(nanos/fmtNanosPerMilli),
			nanos%fmtNanosPerMilli,
			fmtNanosPerMilli/10,
			prefix,
			"ms",
		)
	} else if nanos >= fmtNanosPerMicro {
		return fmtDurationDecimal(
			f,
			uint64(nanos/fmtNanosPerMicro),
			nanos%fmtNanosPerMicro,
			fmtNanosPerMicro/10,
			prefix,
			"µs",
		)
	}
	return fmtDurationDecimal(f, uint64(nanos), 0, 1, prefix, "ns")
}

// fmtDurationDecimal is the fmt_decimal function inside Duration's Debug: it
// formats a floating point number in decimal notation.
//
// The number is given as the `integer_part` and a fractional part.
// The value of the fractional part is `fractional_part / divisor`. So
// `integer_part` = 3, `fractional_part` = 12 and `divisor` = 100
// represents the number `3.012`. Trailing zeros are omitted.
//
// `divisor` must not be above 100_000_000. It also should be a power
// of 10, everything else doesn't make sense. `fractional_part` has
// to be less than `10 * divisor`!
//
// A prefix and postfix may be added. The whole thing is padded
// to the formatter's `width`, if specified.
func fmtDurationDecimal(f *Formatter, integerPart uint64, fractionalPart uint32, divisor uint32, prefix string, postfix string) error {
	// Encode the fractional part into a temporary buffer. The buffer
	// only need to hold 9 elements, because `fractional_part` has to
	// be smaller than 10^9. The buffer is prefilled with '0' digits
	// to simplify the code below.
	buf := [9]byte{'0', '0', '0', '0', '0', '0', '0', '0', '0'}

	precision, hasPrecision := f.options.getPrecision()

	// The next digit is written at this position
	pos := 0

	// We keep writing digits into the buffer while there are non-zero
	// digits left and we haven't written enough digits yet.
	maxDigits := 9
	if hasPrecision {
		maxDigits = int(precision)
	}
	for fractionalPart > 0 && pos < maxDigits {
		// Write new digit into the buffer
		buf[pos] = '0' + byte(fractionalPart/divisor)

		fractionalPart %= divisor
		divisor /= 10
		pos++
	}

	// If a precision < 9 was specified, there may be some non-zero
	// digits left that weren't written into the buffer. In that case we
	// need to perform rounding to match the semantics of printing
	// normal floating point numbers. However, we only need to do work
	// when rounding up. This happens if the first digit of the
	// remaining ones is >= 5. When the first digit is exactly 5, rounding
	// follows IEEE-754 round-ties-to-even semantics: we only round up
	// if the last written digit is odd.
	//
	// integerOverflow stands for Rust's `None`: an integer part of
	// `u64::MAX + 1`.
	integerOverflow := false
	if fractionalPart > 0 && fractionalPart >= divisor*5 {
		// For ties (fractional_part == divisor * 5), only round up if last digit is odd
		isTie := fractionalPart == divisor*5
		var lastDigitIsOdd bool
		if pos > 0 {
			lastDigitIsOdd = (buf[pos-1]-'0')%2 == 1
		} else {
			// No fractional digits - check the integer part
			lastDigitIsOdd = (integerPart % 2) == 1
		}

		if isTie && !lastDigitIsOdd {
			// keep integer_part
		} else {
			// Round up the number contained in the buffer. We go through
			// the buffer backwards and keep track of the carry.
			revPos := pos
			carry := true
			for carry && revPos > 0 {
				revPos--

				// If the digit in the buffer is not '9', we just need to
				// increment it and can stop then (since we don't have a
				// carry anymore). Otherwise, we set it to '0' (overflow)
				// and continue.
				if buf[revPos] < '9' {
					buf[revPos]++
					carry = false
				} else {
					buf[revPos] = '0'
				}
			}

			// If we still have the carry bit set, that means that we set
			// the whole buffer to '0's and need to increment the integer
			// part.
			if carry {
				// If `integer_part == u64::MAX` and precision < 9, any
				// carry of the overflow during rounding of the
				// `fractional_part` into the `integer_part` will cause the
				// `integer_part` itself to overflow. Avoid this by using an
				// `Option<u64>`, with `None` representing `u64::MAX + 1`.
				if integerPart == ^uint64(0) {
					integerOverflow = true
				} else {
					integerPart++
				}
			}
		}
	}

	// Determine the end of the buffer: if precision is set, we just
	// use as many digits from the buffer (capped to 9). If it isn't
	// set, we only use all digits up to the last non-zero one.
	end := pos
	if hasPrecision {
		end = min(int(precision), 9)
	}

	// The fractional part is written `{:0<width$}`: w is that width.
	w := pos
	if hasPrecision {
		w = int(precision)
	}

	// This closure emits the formatted duration without emitting any
	// padding (padding is calculated below).
	emitWithoutPadding := func(f *Formatter) error {
		if err := f.buf.writeStr(prefix); err != nil {
			return err
		}
		if !integerOverflow {
			var digits [20]byte
			if err := f.buf.writeStr(string(strconv.AppendUint(digits[:0], integerPart, 10))); err != nil {
				return err
			}
		} else {
			// u64::MAX + 1 == 18446744073709551616
			if err := f.buf.writeStr("18446744073709551616"); err != nil {
				return err
			}
		}

		// Write the decimal point and the fractional part (if any).
		if end > 0 {
			if err := f.buf.writeStr("."); err != nil {
				return err
			}
			// If the user request a precision > 9, we pad '0's at the end.
			fraction := fmtFormatterNew(f.buf, fmtFormattingOptionsNew())
			fraction.options.setFill('0')
			fraction.options.setAlign(Alignment_Left)
			fraction.options.setWidth(uint16(w))
			if err := fraction.Pad(string(buf[:end])); err != nil {
				return err
			}
		}

		return f.buf.writeStr(postfix)
	}

	requestedW, hasWidth := f.options.getWidth()
	if !hasWidth {
		// No `width` specified. There's no need to calculate the
		// length of the output in this case, just emit it.
		return emitWithoutPadding(f)
	}
	// A `width` was specified. Calculate the actual width of
	// the output in order to calculate the required padding.
	// It consists of 4 parts:
	// 1. The prefix: is either "+" or "", so we can just use len().
	// 2. The postfix: can be "µs" so we have to count UTF8 characters.
	actualW := len(prefix) + utf8.RuneCountInString(postfix)
	// 3. The integer part:
	if !integerOverflow {
		if integerPart > 0 {
			// integer_part is > 0, so has length log10(x)+1
			actualW += 1 + fmtIlog10U64(integerPart)
		} else {
			// integer_part is 0, so has length 1.
			actualW++
		}
	} else {
		// integer_part is u64::MAX + 1, so has length 20
		actualW += 20
	}
	// 4. The fractional part (if any):
	if end > 0 {
		actualW += 1 + w
	}

	if int(requestedW) <= actualW {
		// Output is already longer than `width`, so don't pad.
		return emitWithoutPadding(f)
	}
	// We need to add padding. Use the `Formatter::padding` helper function.
	defaultAlign := Alignment_Left
	postPadding, err := f.padding(requestedW-uint16(actualW), defaultAlign)
	if err != nil {
		return err
	}
	if err := emitWithoutPadding(f); err != nil {
		return err
	}
	return postPadding.write(f)
}

// fmtDebugSystemTime is `impl Debug for SystemTime`, which Go code holds as
// a time.Time. Rust prints the operating system's own representation:
// seconds and nanoseconds since the Unix epoch on Unix, and the count of
// 100-nanosecond intervals since 1601 on Windows.
func fmtDebugSystemTime(t time.Time, f *Formatter) error {
	if CfgWindows {
		// FILETIME: intervals of 100ns since 1601-01-01.
		const intervalsPerSec = 10_000_000
		const intervalsToUnixEpoch = 11_644_473_600 * intervalsPerSec
		intervals := t.Unix()*intervalsPerSec + int64(t.Nanosecond()/100) + intervalsToUnixEpoch
		return f.DebugStruct("SystemTime").Field("intervals", intervals).Finish()
	}
	return f.DebugStruct("SystemTime").
		Field("tv_sec", t.Unix()).
		Field("tv_nsec", uint32(t.Nanosecond())).
		Finish()
}
