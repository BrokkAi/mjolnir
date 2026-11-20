// Port of zmij 1.0.21 src/lib.rs (`write`, the layout of the digits): how
// serde_json writes a finite f64 or f32.

package serdejson

import (
	"math"
	"strconv"
	"strings"
)

// The digits are the shortest that read back as the value, nearest to it,
// which every correct shortest algorithm agrees on (zmij's Schubfach and
// yy, and the Ryu in strconv). What is zmij's own is where they go: fixed
// notation for a decimal exponent in -5..=15 (-6..=12 for f32), otherwise
// d.ddde±X with no padded exponent, and always a decimal point or exponent.

// formatF64 is `zmij::Buffer::new().format_finite(value)` for an f64.
func formatF64(value float64) string {
	return zmijLayout(strconv.FormatFloat(value, 'e', -1, 64), math.Signbit(value), -5, 15)
}

// formatF32 is `zmij::Buffer::new().format_finite(value)` for an f32.
func formatF32(value float32) string {
	return zmijLayout(strconv.FormatFloat(float64(value), 'e', -1, 32), math.Signbit(float64(value)), -6, 12)
}

// zmijLayout lays out the shortest digits that strconv wrote in its 'e'
// form ("-d.ddde±XX") as zmij's write does. minExp and maxExp bound the
// decimal exponents written in fixed notation.
func zmijLayout(scientific string, negative bool, minExp, maxExp int) string {
	var out strings.Builder
	out.Grow(24)
	if negative {
		out.WriteByte('-')
	}
	scientific = strings.TrimPrefix(scientific, "-")
	mantissa, exponent, _ := strings.Cut(scientific, "e")
	digits := strings.Replace(mantissa, ".", "", 1)
	decExp, _ := strconv.Atoi(exponent)
	if digits == "0" {
		out.WriteString("0.0")
		return out.String()
	}
	length := len(digits)
	if decExp >= minExp && decExp <= maxExp {
		switch {
		case length-1 <= decExp:
			// 1234e7 -> 12340000000.0
			out.WriteString(digits)
			out.WriteString(strings.Repeat("0", decExp+1-length))
			out.WriteString(".0")
		case decExp >= 0:
			// 1234e-2 -> 12.34
			out.WriteString(digits[:decExp+1])
			out.WriteByte('.')
			out.WriteString(digits[decExp+1:])
		default:
			// 1234e-6 -> 0.001234
			out.WriteString("0.")
			out.WriteString(strings.Repeat("0", -decExp-1))
			out.WriteString(digits)
		}
		return out.String()
	}
	// 1234e30 -> 1.234e33
	out.WriteByte(digits[0])
	if length > 1 {
		out.WriteByte('.')
		out.WriteString(digits[1:])
	}
	out.WriteByte('e')
	if decExp >= 0 {
		out.WriteByte('+')
	} else {
		out.WriteByte('-')
		decExp = -decExp
	}
	out.WriteString(strconv.Itoa(decExp))
	return out.String()
}

// formatF64Display is zmij's `format`, which also writes NaN and the
// infinities: how serde_json's errors print a float.
func formatF64Display(value float64) string {
	switch {
	case math.IsNaN(value):
		return "NaN"
	case math.IsInf(value, 1):
		return "inf"
	case math.IsInf(value, -1):
		return "-inf"
	}
	return formatF64(value)
}
