// Port of chrono 0.4.44 src/time_delta.rs

package chrono

import (
	"math"
	"math/big"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const (
	nanosPerMicro  int32 = 1000
	nanosPerMilli  int32 = 1_000_000
	nanosPerSec    int32 = 1_000_000_000
	microsPerSec   int64 = 1_000_000
	millisPerSec   int64 = 1000
	secsPerMinute  int64 = 60
	secsPerHour    int64 = 3600
	secsPerDay     int64 = 86_400
	secsPerWeek    int64 = 604_800
	timeDeltaMinS  int64 = -math.MaxInt64/millisPerSec - 1
	timeDeltaMinNs int32 = nanosPerSec + int32(-math.MaxInt64%millisPerSec)*nanosPerMilli
	timeDeltaMaxS  int64 = math.MaxInt64 / millisPerSec
	timeDeltaMaxNs int32 = int32(math.MaxInt64%millisPerSec) * nanosPerMilli
)

// TimeDelta is chrono's time span with nanosecond precision, which may be
// negative. Its range is that of an i64 count of milliseconds, about 292
// million years either way. The zero value is the zero span, as
// `TimeDelta::default()` is.
//
// It is a Copy type: methods take and return values. Compare two spans with
// Equal and Compare (or rs.Eq and rs.Cmp); the fields are private.
type TimeDelta struct {
	secs  int64
	nanos int32 // Always 0 <= nanos < nanosPerSec
}

// TimeDeltaMin is `TimeDelta::MIN`: the smallest (most negative) span.
var TimeDeltaMin = TimeDelta{secs: timeDeltaMinS, nanos: timeDeltaMinNs}

// TimeDeltaMax is `TimeDelta::MAX`: the largest span.
var TimeDeltaMax = TimeDelta{secs: timeDeltaMaxS, nanos: timeDeltaMaxNs}

// TimeDeltaNew is `TimeDelta::new`: a span of secs seconds plus nanos
// nanoseconds, or None when it is out of bounds or nanos >= 1,000,000,000.
func TimeDeltaNew(secs int64, nanos uint32) rs.Option[TimeDelta] {
	if secs < timeDeltaMinS ||
		secs > timeDeltaMaxS ||
		nanos >= 1_000_000_000 ||
		(secs == timeDeltaMaxS && nanos > uint32(timeDeltaMaxNs)) ||
		(secs == timeDeltaMinS && nanos < uint32(timeDeltaMinNs)) {
		return rs.None[TimeDelta]()
	}
	return rs.Some(TimeDelta{secs: secs, nanos: int32(nanos)})
}

// TimeDeltaWeeks is `TimeDelta::weeks`. It panics when out of bounds.
func TimeDeltaWeeks(weeks int64) TimeDelta {
	return expect(TimeDeltaTryWeeks(weeks), "TimeDelta::weeks out of bounds")
}

// TimeDeltaTryWeeks is `TimeDelta::try_weeks`.
func TimeDeltaTryWeeks(weeks int64) rs.Option[TimeDelta] {
	secs, ok := checkedMulI64(weeks, secsPerWeek)
	if !ok {
		return rs.None[TimeDelta]()
	}
	return TimeDeltaTrySeconds(secs)
}

// TimeDeltaDays is `TimeDelta::days`. It panics when out of bounds.
func TimeDeltaDays(days int64) TimeDelta {
	return expect(TimeDeltaTryDays(days), "TimeDelta::days out of bounds")
}

// TimeDeltaTryDays is `TimeDelta::try_days`.
func TimeDeltaTryDays(days int64) rs.Option[TimeDelta] {
	secs, ok := checkedMulI64(days, secsPerDay)
	if !ok {
		return rs.None[TimeDelta]()
	}
	return TimeDeltaTrySeconds(secs)
}

// TimeDeltaHours is `TimeDelta::hours`. It panics when out of bounds.
func TimeDeltaHours(hours int64) TimeDelta {
	return expect(TimeDeltaTryHours(hours), "TimeDelta::hours out of bounds")
}

// TimeDeltaTryHours is `TimeDelta::try_hours`.
func TimeDeltaTryHours(hours int64) rs.Option[TimeDelta] {
	secs, ok := checkedMulI64(hours, secsPerHour)
	if !ok {
		return rs.None[TimeDelta]()
	}
	return TimeDeltaTrySeconds(secs)
}

// TimeDeltaMinutes is `TimeDelta::minutes`. It panics when out of bounds.
func TimeDeltaMinutes(minutes int64) TimeDelta {
	return expect(TimeDeltaTryMinutes(minutes), "TimeDelta::minutes out of bounds")
}

// TimeDeltaTryMinutes is `TimeDelta::try_minutes`.
func TimeDeltaTryMinutes(minutes int64) rs.Option[TimeDelta] {
	secs, ok := checkedMulI64(minutes, secsPerMinute)
	if !ok {
		return rs.None[TimeDelta]()
	}
	return TimeDeltaTrySeconds(secs)
}

// TimeDeltaSeconds is `TimeDelta::seconds`. It panics when out of bounds
// (more than i64::MAX / 1000 seconds either way).
func TimeDeltaSeconds(seconds int64) TimeDelta {
	return expect(TimeDeltaTrySeconds(seconds), "TimeDelta::seconds out of bounds")
}

// TimeDeltaTrySeconds is `TimeDelta::try_seconds`.
func TimeDeltaTrySeconds(seconds int64) rs.Option[TimeDelta] {
	return TimeDeltaNew(seconds, 0)
}

// TimeDeltaMilliseconds is `TimeDelta::milliseconds`. It panics for
// i64::MIN, the one value out of bounds.
func TimeDeltaMilliseconds(milliseconds int64) TimeDelta {
	return expect(TimeDeltaTryMilliseconds(milliseconds), "TimeDelta::milliseconds out of bounds")
}

// TimeDeltaTryMilliseconds is `TimeDelta::try_milliseconds`.
func TimeDeltaTryMilliseconds(milliseconds int64) rs.Option[TimeDelta] {
	// We don't need to compare against MAX, as this function accepts an
	// i64, and MAX is aligned to i64::MAX milliseconds.
	if milliseconds < -math.MaxInt64 {
		return rs.None[TimeDelta]()
	}
	secs, millis := divModFloor64(milliseconds, millisPerSec)
	return rs.Some(TimeDelta{secs: secs, nanos: int32(millis) * nanosPerMilli})
}

// TimeDeltaMicroseconds is `TimeDelta::microseconds`. Every i64 is in bounds.
func TimeDeltaMicroseconds(microseconds int64) TimeDelta {
	secs, micros := divModFloor64(microseconds, microsPerSec)
	return TimeDelta{secs: secs, nanos: int32(micros) * nanosPerMicro}
}

// TimeDeltaNanoseconds is `TimeDelta::nanoseconds`. Every i64 is in bounds.
func TimeDeltaNanoseconds(nanos int64) TimeDelta {
	secs, nanos := divModFloor64(nanos, int64(nanosPerSec))
	return TimeDelta{secs: secs, nanos: int32(nanos)}
}

// TimeDeltaZero is `TimeDelta::zero`.
func TimeDeltaZero() TimeDelta { return TimeDelta{} }

// TimeDeltaDefault is `TimeDelta::default()`, the zero span.
func TimeDeltaDefault() TimeDelta { return TimeDelta{} }

// NumWeeks is `TimeDelta::num_weeks`: the whole weeks in the span.
func (d TimeDelta) NumWeeks() int64 { return d.NumDays() / 7 }

// NumDays is `TimeDelta::num_days`: the whole days in the span.
func (d TimeDelta) NumDays() int64 { return d.NumSeconds() / secsPerDay }

// NumHours is `TimeDelta::num_hours`: the whole hours in the span.
func (d TimeDelta) NumHours() int64 { return d.NumSeconds() / secsPerHour }

// NumMinutes is `TimeDelta::num_minutes`: the whole minutes in the span.
func (d TimeDelta) NumMinutes() int64 { return d.NumSeconds() / secsPerMinute }

// NumSeconds is `TimeDelta::num_seconds`: the whole seconds in the span,
// rounded toward zero.
func (d TimeDelta) NumSeconds() int64 {
	// If secs is negative, nanos should be subtracted from the duration.
	if d.secs < 0 && d.nanos > 0 {
		return d.secs + 1
	}
	return d.secs
}

// AsSecondsF64 is `TimeDelta::as_seconds_f64`.
func (d TimeDelta) AsSecondsF64() float64 {
	return float64(d.secs) + float64(d.nanos)/float64(nanosPerSec)
}

// AsSecondsF32 is `TimeDelta::as_seconds_f32`.
func (d TimeDelta) AsSecondsF32() float32 {
	return float32(d.secs) + float32(d.nanos)/float32(nanosPerSec)
}

// NumMilliseconds is `TimeDelta::num_milliseconds`.
func (d TimeDelta) NumMilliseconds() int64 {
	// A proper TimeDelta will not overflow, because MIN and MAX are defined such
	// that the range is within the bounds of an i64, from -i64::MAX through to
	// +i64::MAX inclusive. Notably, i64::MIN is excluded from this range.
	secsPart := d.NumSeconds() * millisPerSec
	nanosPart := d.SubsecNanos() / nanosPerMilli
	return secsPart + int64(nanosPart)
}

// SubsecMillis is `TimeDelta::subsec_millis`: the milliseconds beyond the
// whole seconds, negative for a negative span.
func (d TimeDelta) SubsecMillis() int32 { return d.SubsecNanos() / nanosPerMilli }

// NumMicroseconds is `TimeDelta::num_microseconds`, or None on overflow.
func (d TimeDelta) NumMicroseconds() rs.Option[int64] {
	secsPart, ok := checkedMulI64(d.NumSeconds(), microsPerSec)
	if !ok {
		return rs.None[int64]()
	}
	nanosPart := d.SubsecNanos() / nanosPerMicro
	return rs.OptionOf(checkedAddI64(secsPart, int64(nanosPart)))
}

// SubsecMicros is `TimeDelta::subsec_micros`.
func (d TimeDelta) SubsecMicros() int32 { return d.SubsecNanos() / nanosPerMicro }

// NumNanoseconds is `TimeDelta::num_nanoseconds`, or None on overflow (a span
// of more than about 292 years).
func (d TimeDelta) NumNanoseconds() rs.Option[int64] {
	secsPart, ok := checkedMulI64(d.NumSeconds(), int64(nanosPerSec))
	if !ok {
		return rs.None[int64]()
	}
	nanosPart := d.SubsecNanos()
	return rs.OptionOf(checkedAddI64(secsPart, int64(nanosPart)))
}

// SubsecNanos is `TimeDelta::subsec_nanos`: the nanoseconds beyond the whole
// seconds, negative for a negative span.
func (d TimeDelta) SubsecNanos() int32 {
	if d.secs < 0 && d.nanos > 0 {
		return d.nanos - nanosPerSec
	}
	return d.nanos
}

// CheckedAdd is `TimeDelta::checked_add`: the sum, or None on overflow.
func (d TimeDelta) CheckedAdd(rhs TimeDelta) rs.Option[TimeDelta] {
	// No overflow checks here because we stay comfortably within the range of an `i64`.
	// Range checks happen in `TimeDelta::new`.
	secs := d.secs + rhs.secs
	nanos := d.nanos + rhs.nanos
	if nanos >= nanosPerSec {
		nanos -= nanosPerSec
		secs++
	}
	return TimeDeltaNew(secs, uint32(nanos))
}

// CheckedSub is `TimeDelta::checked_sub`: the difference, or None on overflow.
func (d TimeDelta) CheckedSub(rhs TimeDelta) rs.Option[TimeDelta] {
	// No overflow checks here because we stay comfortably within the range of an `i64`.
	// Range checks happen in `TimeDelta::new`.
	secs := d.secs - rhs.secs
	nanos := d.nanos - rhs.nanos
	if nanos < 0 {
		nanos += nanosPerSec
		secs--
	}
	return TimeDeltaNew(secs, uint32(nanos))
}

// CheckedMul is `TimeDelta::checked_mul`: the product, or None on overflow.
func (d TimeDelta) CheckedMul(rhs int32) rs.Option[TimeDelta] {
	// Multiply nanoseconds as i64, because it cannot overflow that way.
	totalNanos := int64(d.nanos) * int64(rhs)
	extraSecs, nanos := divModFloor64(totalNanos, int64(nanosPerSec))
	// Multiply seconds as i128 to prevent overflow
	secs := new(big.Int).Mul(big.NewInt(d.secs), big.NewInt(int64(rhs)))
	secs.Add(secs, big.NewInt(extraSecs))
	if secs.Cmp(big.NewInt(math.MinInt64)) <= 0 || secs.Cmp(big.NewInt(math.MaxInt64)) >= 0 {
		return rs.None[TimeDelta]()
	}
	return rs.Some(TimeDelta{secs: secs.Int64(), nanos: int32(nanos)})
}

// CheckedDiv is `TimeDelta::checked_div`: the quotient, or None when rhs is
// zero.
func (d TimeDelta) CheckedDiv(rhs int32) rs.Option[TimeDelta] {
	if rhs == 0 {
		return rs.None[TimeDelta]()
	}
	secs := d.secs / int64(rhs)
	carry := d.secs % int64(rhs)
	extraNanos := carry * int64(nanosPerSec) / int64(rhs)
	nanos := d.nanos/rhs + int32(extraNanos)
	switch {
	case nanos < 0:
		secs, nanos = secs-1, nanos+nanosPerSec
	case nanos >= nanosPerSec:
		secs, nanos = secs+1, nanos-nanosPerSec
	}
	return rs.Some(TimeDelta{secs: secs, nanos: nanos})
}

// Abs is `TimeDelta::abs`.
func (d TimeDelta) Abs() TimeDelta {
	if d.secs < 0 && d.nanos != 0 {
		return TimeDelta{secs: absI64(d.secs + 1), nanos: nanosPerSec - d.nanos}
	}
	return TimeDelta{secs: absI64(d.secs), nanos: d.nanos}
}

// IsZero is `TimeDelta::is_zero`.
func (d TimeDelta) IsZero() bool { return d.secs == 0 && d.nanos == 0 }

// TimeDeltaFromStd is `TimeDelta::from_std`: a std::time::Duration as a
// TimeDelta.
//
// Rust's Duration is unsigned and reaches 2^64 seconds, so the Rust function
// fails for a duration beyond TimeDelta's range. Go's time.Duration is a
// signed count of nanoseconds that is always inside that range, and may be
// negative, which Rust's cannot be: a negative duration is the
// OutOfRangeError.
func TimeDeltaFromStd(duration time.Duration) (TimeDelta, error) {
	if duration < 0 {
		return TimeDelta{}, OutOfRangeError{}
	}
	secs := int64(duration / time.Second)
	nanos := uint32(duration % time.Second)
	// We need to check secs as u64 before coercing to i64
	if secs > timeDeltaMaxS {
		return TimeDelta{}, OutOfRangeError{}
	}
	delta, ok := TimeDeltaNew(secs, nanos).Get()
	if !ok {
		return TimeDelta{}, OutOfRangeError{}
	}
	return delta, nil
}

// ToStd is `TimeDelta::to_std`: the span as a std::time::Duration, or the
// OutOfRangeError when it is negative.
//
// Rust's Duration holds any non-negative TimeDelta. Go's time.Duration holds
// about 292 years, so a longer span is returned as the longest time.Duration
// (math.MaxInt64 nanoseconds) rather than as an error Rust would not report.
func (d TimeDelta) ToStd() (time.Duration, error) {
	if d.secs < 0 {
		return 0, OutOfRangeError{}
	}
	nanos, ok := checkedMulI64(d.secs, int64(nanosPerSec))
	if ok {
		nanos, ok = checkedAddI64(nanos, int64(d.nanos))
	}
	if !ok {
		return time.Duration(math.MaxInt64), nil
	}
	return time.Duration(nanos), nil
}

// Neg is `-delta` (`impl Neg for TimeDelta`).
func (d TimeDelta) Neg() TimeDelta {
	if d.nanos == 0 {
		return TimeDelta{secs: -d.secs, nanos: 0}
	}
	return TimeDelta{secs: -d.secs - 1, nanos: nanosPerSec - d.nanos}
}

// Add is `delta + rhs`. It panics on overflow.
func (d TimeDelta) Add(rhs TimeDelta) TimeDelta {
	return expect(d.CheckedAdd(rhs), "`TimeDelta + TimeDelta` overflowed")
}

// Sub is `delta - rhs`. It panics on overflow.
func (d TimeDelta) Sub(rhs TimeDelta) TimeDelta {
	return expect(d.CheckedSub(rhs), "`TimeDelta - TimeDelta` overflowed")
}

// AddAssign is `delta += rhs`. It panics on overflow.
func (d *TimeDelta) AddAssign(rhs TimeDelta) {
	*d = expect(d.CheckedAdd(rhs), "`TimeDelta + TimeDelta` overflowed")
}

// SubAssign is `delta -= rhs`. It panics on overflow.
func (d *TimeDelta) SubAssign(rhs TimeDelta) {
	*d = expect(d.CheckedSub(rhs), "`TimeDelta - TimeDelta` overflowed")
}

// Mul is `delta * rhs`. It panics on overflow.
func (d TimeDelta) Mul(rhs int32) TimeDelta {
	return expect(d.CheckedMul(rhs), "`TimeDelta * i32` overflowed")
}

// Div is `delta / rhs`. It panics when rhs is zero.
func (d TimeDelta) Div(rhs int32) TimeDelta {
	return expect(d.CheckedDiv(rhs), "`i32` is zero")
}

// TimeDeltaSum is `iter.sum::<TimeDelta>()`. It panics on overflow.
func TimeDeltaSum(deltas []TimeDelta) TimeDelta {
	total := TimeDeltaZero()
	for _, delta := range deltas {
		total = total.Add(delta)
	}
	return total
}

// Equal is `==` (derived PartialEq).
func (d TimeDelta) Equal(other TimeDelta) bool { return d == other }

// Compare is `cmp` (derived Ord): by seconds, then nanoseconds.
func (d TimeDelta) Compare(other TimeDelta) int {
	switch {
	case d.secs < other.secs:
		return -1
	case d.secs > other.secs:
		return 1
	case d.nanos < other.nanos:
		return -1
	case d.nanos > other.nanos:
		return 1
	}
	return 0
}

// String is the Display form: an ISO 8601 duration in seconds, such as
// "PT1.5S", "-PT90S" or "P0D".
func (d TimeDelta) String() string {
	// technically speaking, negative duration is not valid ISO 8601,
	// but we need to print it anyway.
	abs, sign := d, ""
	if d.secs < 0 {
		abs, sign = d.Neg(), "-"
	}
	var out strings.Builder
	out.WriteString(sign)
	out.WriteString("P")
	// Plenty of ways to encode an empty string. `P0D` is short and not too strange.
	if abs.secs == 0 && abs.nanos == 0 {
		out.WriteString("0D")
		return out.String()
	}
	out.WriteString("T")
	out.WriteString(strconv.FormatInt(abs.secs, 10))
	if abs.nanos > 0 {
		// Count the number of significant digits, while removing all trailing zero's.
		figures := 9
		fractionDigits := abs.nanos
		for {
			div := fractionDigits / 10
			lastDigit := fractionDigits % 10
			if lastDigit != 0 {
				break
			}
			fractionDigits = div
			figures--
		}
		out.WriteString(".")
		out.WriteString(formatUintZero(uint64(fractionDigits), figures))
	}
	out.WriteString("S")
	return out.String()
}

// DebugString is the Debug form, which is derived:
// "TimeDelta { secs: 1, nanos: 500000000 }".
func (d TimeDelta) DebugString() string {
	return "TimeDelta { secs: " + strconv.FormatInt(d.secs, 10) + ", nanos: " + strconv.FormatInt(int64(d.nanos), 10) + " }"
}

// FmtDisplay is `impl fmt::Display for TimeDelta`, which ignores the
// formatter's width and fill.
func (d TimeDelta) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(d.String()) }

// FmtDebug is the derived `Debug`.
func (d TimeDelta) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("TimeDelta").Field("secs", d.secs).Field("nanos", d.nanos).Finish()
}

// OutOfRangeError is the error of TimeDeltaFromStd and TimeDelta.ToStd: the
// duration does not fit the target type.
type OutOfRangeError struct{}

// String is the Display form.
func (OutOfRangeError) String() string {
	return "Source duration value is out of range for the target type"
}

// Error makes OutOfRangeError a Go error; the text is the Display form.
func (e OutOfRangeError) Error() string { return e.String() }

// DebugString is the Debug form, which is derived.
func (OutOfRangeError) DebugString() string { return "OutOfRangeError(())" }

// FmtDisplay is `impl fmt::Display for OutOfRangeError`.
func (e OutOfRangeError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is the derived `Debug`.
func (e OutOfRangeError) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("OutOfRangeError").Field(rs.Unit{}).Finish()
}

func divModFloor64(this, other int64) (int64, int64) {
	return divEuclid(this, other), remEuclid(this, other)
}

func absI64(v int64) int64 {
	if v < 0 {
		return -v
	}
	return v
}

// DurationNew and the functions below are the names Rust code reaches
// through the alias `chrono::Duration`.
func DurationNew(secs int64, nanos uint32) rs.Option[TimeDelta] { return TimeDeltaNew(secs, nanos) }

// DurationWeeks is `Duration::weeks`.
func DurationWeeks(weeks int64) TimeDelta {
	return expect(TimeDeltaTryWeeks(weeks), "TimeDelta::weeks out of bounds")
}

// DurationTryWeeks is `Duration::try_weeks`.
func DurationTryWeeks(weeks int64) rs.Option[TimeDelta] { return TimeDeltaTryWeeks(weeks) }

// DurationDays is `Duration::days`.
func DurationDays(days int64) TimeDelta {
	return expect(TimeDeltaTryDays(days), "TimeDelta::days out of bounds")
}

// DurationTryDays is `Duration::try_days`.
func DurationTryDays(days int64) rs.Option[TimeDelta] { return TimeDeltaTryDays(days) }

// DurationHours is `Duration::hours`.
func DurationHours(hours int64) TimeDelta {
	return expect(TimeDeltaTryHours(hours), "TimeDelta::hours out of bounds")
}

// DurationTryHours is `Duration::try_hours`.
func DurationTryHours(hours int64) rs.Option[TimeDelta] { return TimeDeltaTryHours(hours) }

// DurationMinutes is `Duration::minutes`.
func DurationMinutes(minutes int64) TimeDelta {
	return expect(TimeDeltaTryMinutes(minutes), "TimeDelta::minutes out of bounds")
}

// DurationTryMinutes is `Duration::try_minutes`.
func DurationTryMinutes(minutes int64) rs.Option[TimeDelta] { return TimeDeltaTryMinutes(minutes) }

// DurationSeconds is `Duration::seconds`.
func DurationSeconds(seconds int64) TimeDelta {
	return expect(TimeDeltaTrySeconds(seconds), "TimeDelta::seconds out of bounds")
}

// DurationTrySeconds is `Duration::try_seconds`.
func DurationTrySeconds(seconds int64) rs.Option[TimeDelta] { return TimeDeltaTrySeconds(seconds) }

// DurationMilliseconds is `Duration::milliseconds`.
func DurationMilliseconds(milliseconds int64) TimeDelta {
	return expect(TimeDeltaTryMilliseconds(milliseconds), "TimeDelta::milliseconds out of bounds")
}

// DurationTryMilliseconds is `Duration::try_milliseconds`.
func DurationTryMilliseconds(milliseconds int64) rs.Option[TimeDelta] {
	return TimeDeltaTryMilliseconds(milliseconds)
}

// DurationMicroseconds is `Duration::microseconds`.
func DurationMicroseconds(microseconds int64) TimeDelta { return TimeDeltaMicroseconds(microseconds) }

// DurationNanoseconds is `Duration::nanoseconds`.
func DurationNanoseconds(nanos int64) TimeDelta { return TimeDeltaNanoseconds(nanos) }

// DurationZero is `Duration::zero`.
func DurationZero() TimeDelta { return TimeDelta{} }

// DurationFromStd is `Duration::from_std`.
func DurationFromStd(duration time.Duration) (TimeDelta, error) { return TimeDeltaFromStd(duration) }

// DurationMin is `Duration::MIN`.
var DurationMin = TimeDeltaMin

// DurationMax is `Duration::MAX`.
var DurationMax = TimeDeltaMax
