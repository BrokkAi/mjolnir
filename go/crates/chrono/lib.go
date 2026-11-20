// Port of chrono 0.4.44 src/lib.rs

package chrono

import (
	"math"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Duration is chrono's alias for TimeDelta (`pub type Duration = TimeDelta`).
// Rust's std::time::Duration is Go's time.Duration; see TimeDeltaFromStd.
type Duration = TimeDelta

// OutOfRange is the error of a conversion whose input is out of range, as
// returned by WeekdayTryFromU8 and MonthTryFromU8.
type OutOfRange struct{}

func outOfRangeNew() OutOfRange { return OutOfRange{} }

// String is the Display form.
func (OutOfRange) String() string { return "out of range" }

// Error makes OutOfRange a Go error; the text is the Display form.
func (e OutOfRange) Error() string { return e.String() }

// DebugString is the Debug form, which chrono writes as the Display form.
func (OutOfRange) DebugString() string { return "out of range" }

// FmtDisplay is `impl fmt::Display for OutOfRange`.
func (e OutOfRange) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is `impl fmt::Debug for OutOfRange`.
func (e OutOfRange) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.DebugString()) }

// expect is chrono's `expect`: the value of opt, or a panic with msg.
func expect[T any](opt rs.Option[T], msg string) T {
	value, ok := opt.Get()
	if !ok {
		rs.PanicAt(2, msg)
	}
	return value
}

// fmtError is core::fmt::Error, the unit error a formatting method returns.
// Its Display text is Rust's, and its Debug text is "Error".
type fmtError struct{}

func (fmtError) Error() string { return "an error occurred when formatting" }

// errFmt is `Err(fmt::Error)`.
var errFmt error = fmtError{}

// debugStringer is implemented by the types of this package (and of chronotz)
// whose Rust Debug impl is written by hand.
type debugStringer interface {
	DebugString() string
}

// debugString is `format!("{:?}", value)` for a value that carries its own
// Debug text, and rs.DebugString for anything else.
func debugString(value any) string {
	if value, ok := value.(debugStringer); ok {
		return value.DebugString()
	}
	return rs.DebugString(value)
}

type signed interface {
	~int | ~int8 | ~int16 | ~int32 | ~int64
}

// divEuclid is Rust's `div_euclid` for a positive divisor.
func divEuclid[T signed](a, b T) T {
	q := a / b
	if a%b < 0 {
		q--
	}
	return q
}

// remEuclid is Rust's `rem_euclid` for a positive divisor.
func remEuclid[T signed](a, b T) T {
	r := a % b
	if r < 0 {
		r += b
	}
	return r
}

// checkedAddI32 is `i32::checked_add`.
func checkedAddI32(a, b int32) (int32, bool) {
	sum := int64(a) + int64(b)
	if sum < math.MinInt32 || sum > math.MaxInt32 {
		return 0, false
	}
	return int32(sum), true
}

// checkedMulI32 is `i32::checked_mul`.
func checkedMulI32(a, b int32) (int32, bool) {
	product := int64(a) * int64(b)
	if product < math.MinInt32 || product > math.MaxInt32 {
		return 0, false
	}
	return int32(product), true
}

// checkedAddI64 is `i64::checked_add`.
func checkedAddI64(a, b int64) (int64, bool) {
	sum := a + b
	if (b > 0 && sum < a) || (b < 0 && sum > a) {
		return 0, false
	}
	return sum, true
}

// checkedSubI64 is `i64::checked_sub`.
func checkedSubI64(a, b int64) (int64, bool) {
	difference := a - b
	if (b > 0 && difference > a) || (b < 0 && difference < a) {
		return 0, false
	}
	return difference, true
}

// checkedMulI64 is `i64::checked_mul`.
func checkedMulI64(a, b int64) (int64, bool) {
	if a == 0 || b == 0 {
		return 0, true
	}
	if (a == -1 && b == math.MinInt64) || (b == -1 && a == math.MinInt64) {
		return 0, false
	}
	product := a * b
	if product/b != a {
		return 0, false
	}
	return product, true
}

// checkedMulU32 is `u32::checked_mul`.
func checkedMulU32(a, b uint32) (uint32, bool) {
	product := uint64(a) * uint64(b)
	if product > math.MaxUint32 {
		return 0, false
	}
	return uint32(product), true
}

// formatInt is Rust's `{}`, `{:+}`, `{:0width$}`, `{:+0width$}`, `{:width$}`
// and `{:+width$}` for an integer: plus asks for the sign of a non-negative
// number, zero pads with zeroes after the sign, and otherwise the number is
// right-aligned with spaces, as Rust aligns numbers.
func formatInt(v int64, width int, plus bool, zero bool) string {
	var digits string
	sign := ""
	if v < 0 {
		sign = "-"
		digits = strconv.FormatUint(uint64(-(v+1))+1, 10)
	} else {
		if plus {
			sign = "+"
		}
		digits = strconv.FormatInt(v, 10)
	}
	padding := width - len(sign) - len(digits)
	if padding <= 0 {
		return sign + digits
	}
	if zero {
		return sign + strings.Repeat("0", padding) + digits
	}
	return strings.Repeat(" ", padding) + sign + digits
}

// formatUintZero is Rust's `{:0width$}` for an unsigned integer.
func formatUintZero(v uint64, width int) string {
	digits := strconv.FormatUint(v, 10)
	if len(digits) >= width {
		return digits
	}
	return strings.Repeat("0", width-len(digits)) + digits
}
