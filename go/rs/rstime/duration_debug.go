// Port of core 1.99.1 src/time.rs (impl fmt::Debug for Duration)

package rstime

import (
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// DurationDebugSpec holds the parts of a format spec that Duration's Debug
// impl reads: `{:>12.3?}` is a width of 12, a precision of 3 and right
// alignment. The zero value is `{:?}`.
type DurationDebugSpec struct {
	// Precision is the number of digits after the decimal point. Without one
	// the fraction is printed in full with no trailing zeros.
	Precision rs.Option[int]
	// Width is the minimum width in characters.
	Width rs.Option[int]
	// Fill is the padding character. Zero means a space.
	Fill rune
	// Align is where the text sits in the width. Without one it is on the
	// left.
	Align rs.Option[rs.Alignment]
	// SignPlus is the `+` flag: a leading plus sign.
	SignPlus bool
}

// DurationDebug is `format!("{d:?}")`: the duration in the largest unit that
// keeps its whole part non-zero, as "1.5s", "250ms", "7.1µs" or "0ns".
func DurationDebug(d time.Duration) string {
	check(d)
	secs, nanos := toParts(d)
	return DurationDebugParts(secs, nanos, DurationDebugSpec{})
}

// DurationDebugWith is Duration's Debug output under a format spec, such as
// `format!("{d:.2?}")`.
func DurationDebugWith(d time.Duration, spec DurationDebugSpec) string {
	check(d)
	secs, nanos := toParts(d)
	return DurationDebugParts(secs, nanos, spec)
}

// DurationFmtDebug is `fmt::Debug::fmt(&d, f)`: it writes the duration to f
// under f's own width, precision, fill, alignment and sign.
func DurationFmtDebug(d time.Duration, f *rs.Formatter) error {
	check(d)
	secs, nanos := toParts(d)
	return f.WriteStr(DurationDebugParts(secs, nanos, DurationDebugSpec{
		Precision: f.Precision(),
		Width:     f.Width(),
		Fill:      f.Fill(),
		Align:     f.Align(),
		SignPlus:  f.SignPlus(),
	}))
}

// DurationDebugParts is the Debug output of the Rust Duration of secs whole
// seconds and nanos nanoseconds (below one second), over Rust's full range.
// It depends on nothing but its arguments.
func DurationDebugParts(secs uint64, nanos uint32, spec DurationDebugSpec) string {
	// Print leading '+' sign if requested
	prefix := ""
	if spec.SignPlus {
		prefix = "+"
	}

	switch {
	case secs > 0:
		return fmtDecimal(spec, secs, nanos, nanosPerSec/10, prefix, "s")
	case nanos >= nanosPerMilli:
		return fmtDecimal(spec, uint64(nanos/nanosPerMilli), nanos%nanosPerMilli, nanosPerMilli/10, prefix, "ms")
	case nanos >= nanosPerMicro:
		return fmtDecimal(spec, uint64(nanos/nanosPerMicro), nanos%nanosPerMicro, nanosPerMicro/10, prefix, "µs")
	default:
		return fmtDecimal(spec, uint64(nanos), 0, 1, prefix, "ns")
	}
}

// fmtDecimal formats a floating point number in decimal notation.
//
// The number is given as the integer part and a fractional part. The value of
// the fractional part is fractionalPart / divisor, so fractionalPart has to
// be less than 10 * divisor.
//
// A prefix and postfix may be added. The whole thing is padded to the
// formatter's width, if specified.
func fmtDecimal(spec DurationDebugSpec, integerPart uint64, fractionalPart uint32, divisor uint32, prefix string, postfix string) string {
	// Encode the fractional part into a temporary buffer. The buffer
	// only need to hold 9 elements, because `fractional_part` has to
	// be smaller than 10^9. The buffer is prefilled with '0' digits
	// to simplify the code below.
	buf := [9]byte{'0', '0', '0', '0', '0', '0', '0', '0', '0'}

	// The next digit is written at this position
	pos := 0

	// We keep writing digits into the buffer while there are non-zero
	// digits left and we haven't written enough digits yet.
	for fractionalPart > 0 && pos < spec.Precision.UnwrapOr(9) {
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
	// integerOverflow stands for Rust's `None`: the integer part is
	// u64::MAX + 1.
	integerOverflow := false
	if fractionalPart > 0 && fractionalPart >= divisor*5 {
		// For ties (fractional_part == divisor * 5), only round up if last digit is odd
		isTie := fractionalPart == divisor*5
		var lastDigitIsOdd bool
		if pos > 0 {
			lastDigitIsOdd = (buf[pos-1]-'0')%2 == 1
		} else {
			// No fractional digits - check the integer part
			lastDigitIsOdd = integerPart%2 == 1
		}

		if !(isTie && !lastDigitIsOdd) {
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
	if precision, ok := spec.Precision.Get(); ok {
		end = min(precision, 9)
	}

	// The integer part as text; u64::MAX + 1 == 18446744073709551616.
	integerText := "18446744073709551616"
	if !integerOverflow {
		integerText = strconv.FormatUint(integerPart, 10)
	}

	// emitWithoutPadding writes the formatted duration without any padding
	// (padding is calculated below).
	emitWithoutPadding := func(out *strings.Builder) {
		out.WriteString(prefix)
		out.WriteString(integerText)

		// Write the decimal point and the fractional part (if any).
		if end > 0 {
			out.WriteByte('.')
			out.Write(buf[:end])
			// If the user request a precision > 9, we pad '0's at the end.
			for written := end; written < spec.Precision.UnwrapOr(pos); written++ {
				out.WriteByte('0')
			}
		}

		out.WriteString(postfix)
	}

	var out strings.Builder
	requestedWidth, hasWidth := spec.Width.Get()
	if !hasWidth {
		// No `width` specified. There's no need to calculate the
		// length of the output in this case, just emit it.
		emitWithoutPadding(&out)
		return out.String()
	}

	// A `width` was specified. Calculate the actual width of
	// the output in order to calculate the required padding.
	// It consists of 4 parts:
	// 1. The prefix: is either "+" or "", so we can just use len().
	// 2. The postfix: can be "µs" so we have to count UTF8 characters.
	actualWidth := len(prefix) + utf8.RuneCountInString(postfix)
	// 3. The integer part:
	actualWidth += len(integerText)
	// 4. The fractional part (if any):
	if end > 0 {
		actualWidth += 1 + spec.Precision.UnwrapOr(pos)
	}

	if requestedWidth <= actualWidth {
		// Output is already longer than `width`, so don't pad.
		emitWithoutPadding(&out)
		return out.String()
	}

	// We need to add padding, as `Formatter::padding` does: the default
	// alignment is on the left.
	padding := requestedWidth - actualWidth
	fill := spec.Fill
	if fill == 0 {
		fill = ' '
	}
	paddingLeft := 0
	switch spec.Align.UnwrapOr(rs.Alignment_Left) {
	case rs.Alignment_Left:
		paddingLeft = 0
	case rs.Alignment_Right:
		paddingLeft = padding
	case rs.Alignment_Center:
		paddingLeft = padding / 2
	}
	for range paddingLeft {
		out.WriteRune(fill)
	}
	emitWithoutPadding(&out)
	for range padding - paddingLeft {
		out.WriteRune(fill)
	}
	return out.String()
}
