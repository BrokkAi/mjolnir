// Port of core 1.99.1 src/time.rs

package rstime

import (
	"math"
	"math/bits"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const (
	nanosPerSec   = 1_000_000_000
	nanosPerMilli = 1_000_000
	nanosPerMicro = 1_000
	millisPerSec  = 1_000
	microsPerSec  = 1_000_000
	secsPerMinute = 60
	minsPerHour   = 60
)

const (
	// DurationZero is Duration::ZERO.
	DurationZero time.Duration = 0
	// DurationMax is Duration::MAX as the port holds it: the largest
	// time.Duration. See "The range of a Duration" in the package
	// documentation.
	DurationMax time.Duration = math.MaxInt64
)

// maxSecs and maxSecsNanos are DurationMax split into whole seconds and the
// nanoseconds beyond them.
const (
	maxSecs      = uint64(math.MaxInt64 / nanosPerSec)
	maxSecsNanos = uint32(math.MaxInt64 % nanosPerSec)
)

// fromParts converts whole seconds and nanoseconds (below one second) into a
// time.Duration, saturating at DurationMax.
func fromParts(secs uint64, nanos uint32) time.Duration {
	if secs > maxSecs || (secs == maxSecs && nanos > maxSecsNanos) {
		return DurationMax
	}
	return time.Duration(secs)*nanosPerSec + time.Duration(nanos)
}

// toParts splits a duration into whole seconds and the nanoseconds beyond
// them.
func toParts(d time.Duration) (secs uint64, nanos uint32) {
	return uint64(d / nanosPerSec), uint32(d % nanosPerSec)
}

// check panics when d is negative. A Rust Duration is unsigned, so a negative
// one here was produced by Go's own arithmetic where the Rust code would have
// panicked or saturated.
func check(d time.Duration) {
	if d < 0 {
		rs.PanicAt(2, "rstime: negative Duration (a Rust Duration is unsigned; subtract with rstime.DurationSub, DurationCheckedSub or DurationSaturatingSub)")
	}
}

// DurationNew is Duration::new: secs whole seconds and nanos nanoseconds,
// carrying any whole seconds in nanos.
func DurationNew(secs uint64, nanos uint32) time.Duration {
	if nanos >= nanosPerSec {
		carry := uint64(nanos / nanosPerSec)
		if secs > math.MaxUint64-carry {
			rs.PanicAt(1, "overflow in Duration::new")
		}
		secs += carry
		nanos %= nanosPerSec
	}
	return fromParts(secs, nanos)
}

// DurationFromSecs is Duration::from_secs.
func DurationFromSecs(secs uint64) time.Duration {
	return fromParts(secs, 0)
}

// DurationFromMillis is Duration::from_millis.
func DurationFromMillis(millis uint64) time.Duration {
	return fromParts(millis/millisPerSec, uint32(millis%millisPerSec)*nanosPerMilli)
}

// DurationFromMicros is Duration::from_micros.
func DurationFromMicros(micros uint64) time.Duration {
	return fromParts(micros/microsPerSec, uint32(micros%microsPerSec)*nanosPerMicro)
}

// DurationFromNanos is Duration::from_nanos.
func DurationFromNanos(nanos uint64) time.Duration {
	return fromParts(nanos/nanosPerSec, uint32(nanos%nanosPerSec))
}

// DurationFromMins is Duration::from_mins.
func DurationFromMins(mins uint64) time.Duration {
	if mins > math.MaxUint64/secsPerMinute {
		rs.PanicAt(1, "overflow in Duration::from_mins")
	}
	return DurationFromSecs(mins * secsPerMinute)
}

// DurationFromHours is Duration::from_hours.
func DurationFromHours(hours uint64) time.Duration {
	if hours > math.MaxUint64/(secsPerMinute*minsPerHour) {
		rs.PanicAt(1, "overflow in Duration::from_hours")
	}
	return DurationFromSecs(hours * minsPerHour * secsPerMinute)
}

// DurationIsZero is Duration::is_zero.
func DurationIsZero(d time.Duration) bool {
	check(d)
	return d == 0
}

// DurationAsSecs is Duration::as_secs: the whole seconds.
func DurationAsSecs(d time.Duration) uint64 {
	check(d)
	return uint64(d / nanosPerSec)
}

// DurationSubsecMillis is Duration::subsec_millis: the part below one second,
// in whole milliseconds.
func DurationSubsecMillis(d time.Duration) uint32 {
	check(d)
	return uint32(d%nanosPerSec) / nanosPerMilli
}

// DurationSubsecMicros is Duration::subsec_micros.
func DurationSubsecMicros(d time.Duration) uint32 {
	check(d)
	return uint32(d%nanosPerSec) / nanosPerMicro
}

// DurationSubsecNanos is Duration::subsec_nanos.
func DurationSubsecNanos(d time.Duration) uint32 {
	check(d)
	return uint32(d % nanosPerSec)
}

// DurationAsMillis is Duration::as_millis. Rust returns a u128; the count of
// a Go duration always fits a uint64, so `as u64` and `u64::try_from` on it
// are the identity.
func DurationAsMillis(d time.Duration) uint64 {
	check(d)
	return uint64(d / nanosPerMilli)
}

// DurationAsMicros is Duration::as_micros. See DurationAsMillis for the type.
func DurationAsMicros(d time.Duration) uint64 {
	check(d)
	return uint64(d / nanosPerMicro)
}

// DurationAsNanos is Duration::as_nanos. See DurationAsMillis for the type.
func DurationAsNanos(d time.Duration) uint64 {
	check(d)
	return uint64(d)
}

// DurationAbsDiff is Duration::abs_diff.
func DurationAbsDiff(d, other time.Duration) time.Duration {
	check(d)
	check(other)
	if d >= other {
		return d - other
	}
	return other - d
}

// DurationCheckedAdd is Duration::checked_add: None when the sum is out of
// range.
func DurationCheckedAdd(d, rhs time.Duration) rs.Option[time.Duration] {
	check(d)
	check(rhs)
	if d > DurationMax-rhs {
		return rs.None[time.Duration]()
	}
	return rs.Some(d + rhs)
}

// DurationSaturatingAdd is Duration::saturating_add.
func DurationSaturatingAdd(d, rhs time.Duration) time.Duration {
	check(d)
	check(rhs)
	if d > DurationMax-rhs {
		return DurationMax
	}
	return d + rhs
}

// DurationAdd is `d + rhs` on two durations.
func DurationAdd(d, rhs time.Duration) time.Duration {
	check(d)
	check(rhs)
	if d > DurationMax-rhs {
		rs.PanicAt(1, "overflow when adding durations")
	}
	return d + rhs
}

// DurationCheckedSub is Duration::checked_sub: None when rhs is the larger.
func DurationCheckedSub(d, rhs time.Duration) rs.Option[time.Duration] {
	check(d)
	check(rhs)
	if rhs > d {
		return rs.None[time.Duration]()
	}
	return rs.Some(d - rhs)
}

// DurationSaturatingSub is Duration::saturating_sub.
func DurationSaturatingSub(d, rhs time.Duration) time.Duration {
	check(d)
	check(rhs)
	if rhs > d {
		return 0
	}
	return d - rhs
}

// DurationSub is `d - rhs` on two durations.
func DurationSub(d, rhs time.Duration) time.Duration {
	check(d)
	check(rhs)
	if rhs > d {
		rs.PanicAt(1, "overflow when subtracting durations")
	}
	return d - rhs
}

// mulScalar multiplies a duration by an integer, reporting whether the
// product is in range.
func mulScalar(d time.Duration, rhs uint32) (time.Duration, bool) {
	high, low := bits.Mul64(uint64(d), uint64(rhs))
	if high != 0 || low > uint64(DurationMax) {
		return 0, false
	}
	return time.Duration(low), true
}

// DurationCheckedMul is Duration::checked_mul.
func DurationCheckedMul(d time.Duration, rhs uint32) rs.Option[time.Duration] {
	check(d)
	product, ok := mulScalar(d, rhs)
	return rs.OptionOf(product, ok)
}

// DurationSaturatingMul is Duration::saturating_mul.
func DurationSaturatingMul(d time.Duration, rhs uint32) time.Duration {
	check(d)
	if product, ok := mulScalar(d, rhs); ok {
		return product
	}
	return DurationMax
}

// DurationMul is `d * rhs` (and `rhs * d`) on a duration and an integer.
func DurationMul(d time.Duration, rhs uint32) time.Duration {
	check(d)
	product, ok := mulScalar(d, rhs)
	if !ok {
		rs.PanicAt(1, "overflow when multiplying duration by scalar")
	}
	return product
}

// DurationCheckedDiv is Duration::checked_div: None when rhs is zero.
func DurationCheckedDiv(d time.Duration, rhs uint32) rs.Option[time.Duration] {
	check(d)
	if rhs == 0 {
		return rs.None[time.Duration]()
	}
	return rs.Some(d / time.Duration(rhs))
}

// DurationDiv is `d / rhs` on a duration and an integer.
func DurationDiv(d time.Duration, rhs uint32) time.Duration {
	check(d)
	if rhs == 0 {
		rs.PanicAt(1, "divide by zero error when dividing duration by scalar")
	}
	return d / time.Duration(rhs)
}

// DurationSum is `iter.sum::<Duration>()`.
func DurationSum(durations []time.Duration) time.Duration {
	var total time.Duration
	for _, d := range durations {
		check(d)
		if total > DurationMax-d {
			rs.PanicAt(1, "overflow in iter::sum over durations")
		}
		total += d
	}
	return total
}

// partsAsSecsF64 is Duration::as_secs_f64 on whole seconds and nanoseconds.
// The two terms are kept apart as Rust computes them, so the result is the
// same float.
func partsAsSecsF64(secs uint64, nanos uint32) float64 {
	return float64(secs) + float64(nanos)/float64(nanosPerSec)
}

// DurationAsSecsF64 is Duration::as_secs_f64.
func DurationAsSecsF64(d time.Duration) float64 {
	check(d)
	return partsAsSecsF64(toParts(d))
}

// DurationAsSecsF32 is Duration::as_secs_f32.
func DurationAsSecsF32(d time.Duration) float32 {
	check(d)
	secs, nanos := toParts(d)
	return float32(secs) + float32(nanos)/float32(nanosPerSec)
}

// DurationAsMillisF64 is Duration::as_millis_f64.
func DurationAsMillisF64(d time.Duration) float64 {
	check(d)
	secs, nanos := toParts(d)
	return float64(secs)*float64(millisPerSec) + float64(nanos)/float64(nanosPerMilli)
}

// DurationAsMillisF32 is Duration::as_millis_f32.
func DurationAsMillisF32(d time.Duration) float32 {
	check(d)
	secs, nanos := toParts(d)
	return float32(secs)*float32(millisPerSec) + float32(nanos)/float32(nanosPerMilli)
}

// DurationDivDurationF64 is Duration::div_duration_f64.
func DurationDivDurationF64(d, rhs time.Duration) float64 {
	check(d)
	check(rhs)
	secs, nanos := toParts(d)
	rhsSecs, rhsNanos := toParts(rhs)
	selfNanos := float64(secs)*float64(nanosPerSec) + float64(nanos)
	otherNanos := float64(rhsSecs)*float64(nanosPerSec) + float64(rhsNanos)
	return selfNanos / otherNanos
}

// DurationDivDurationF32 is Duration::div_duration_f32.
func DurationDivDurationF32(d, rhs time.Duration) float32 {
	check(d)
	check(rhs)
	secs, nanos := toParts(d)
	rhsSecs, rhsNanos := toParts(rhs)
	selfNanos := float32(secs)*float32(nanosPerSec) + float32(nanos)
	otherNanos := float32(rhsSecs)*float32(nanosPerSec) + float32(rhsNanos)
	return selfNanos / otherNanos
}

// DurationFromSecsF64 is Duration::from_secs_f64. It panics, with the text of
// the TryFromFloatSecsError, when secs is negative, not finite or too large
// for a Rust Duration.
func DurationFromSecsF64(secs float64) time.Duration {
	whole, nanos, err := DurationTryFromSecsF64Parts(secs)
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, nanos)
}

// DurationFromSecsF32 is Duration::from_secs_f32.
func DurationFromSecsF32(secs float32) time.Duration {
	whole, nanos, err := DurationTryFromSecsF32Parts(secs)
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, nanos)
}

// DurationTryFromSecsF64 is Duration::try_from_secs_f64. The error is a
// *TryFromFloatSecsError.
func DurationTryFromSecsF64(secs float64) (time.Duration, error) {
	whole, nanos, err := DurationTryFromSecsF64Parts(secs)
	if err != nil {
		return 0, err
	}
	return fromParts(whole, nanos), nil
}

// DurationTryFromSecsF32 is Duration::try_from_secs_f32.
func DurationTryFromSecsF32(secs float32) (time.Duration, error) {
	whole, nanos, err := DurationTryFromSecsF32Parts(secs)
	if err != nil {
		return 0, err
	}
	return fromParts(whole, nanos), nil
}

// mulF64 is Duration::mul_f64 on whole seconds and nanoseconds.
func mulF64(secs uint64, nanos uint32, rhs float64) (uint64, uint32, error) {
	return DurationTryFromSecsF64Parts(rhs * partsAsSecsF64(secs, nanos))
}

// divF64 is Duration::div_f64 on whole seconds and nanoseconds.
func divF64(secs uint64, nanos uint32, rhs float64) (uint64, uint32, error) {
	return DurationTryFromSecsF64Parts(partsAsSecsF64(secs, nanos) / rhs)
}

// DurationMulF64 is Duration::mul_f64. It panics as DurationFromSecsF64 does.
func DurationMulF64(d time.Duration, rhs float64) time.Duration {
	check(d)
	secs, nanos := toParts(d)
	whole, subsec, err := mulF64(secs, nanos, rhs)
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, subsec)
}

// DurationMulF32 is Duration::mul_f32.
func DurationMulF32(d time.Duration, rhs float32) time.Duration {
	check(d)
	secs, nanos := toParts(d)
	whole, subsec, err := mulF64(secs, nanos, float64(rhs))
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, subsec)
}

// DurationDivF64 is Duration::div_f64. It panics as DurationFromSecsF64 does.
func DurationDivF64(d time.Duration, rhs float64) time.Duration {
	check(d)
	secs, nanos := toParts(d)
	whole, subsec, err := divF64(secs, nanos, rhs)
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, subsec)
}

// DurationDivF32 is Duration::div_f32.
func DurationDivF32(d time.Duration, rhs float32) time.Duration {
	check(d)
	secs, nanos := toParts(d)
	whole, subsec, err := divF64(secs, nanos, float64(rhs))
	if err != nil {
		rs.PanicAt(1, err.Error())
	}
	return fromParts(whole, subsec)
}

// tryFromFloatSecsErrorKind is TryFromFloatSecsErrorKind.
type tryFromFloatSecsErrorKind uint8

const (
	// Value is negative.
	tryFromFloatSecsErrorKind_Negative tryFromFloatSecsErrorKind = iota
	// Value is either too big to be represented as `Duration` or `NaN`.
	tryFromFloatSecsErrorKind_OverflowOrNan
)

// TryFromFloatSecsError is core::time::TryFromFloatSecsError: why a float
// could not become a Duration.
type TryFromFloatSecsError struct {
	kind tryFromFloatSecsErrorKind
}

// String is the error's Display text.
func (e *TryFromFloatSecsError) String() string {
	switch e.kind {
	case tryFromFloatSecsErrorKind_Negative:
		return "cannot convert float seconds to Duration: value is negative"
	default:
		return "cannot convert float seconds to Duration: value is either too big or NaN"
	}
}

// Error makes the value a Go error with its Display text.
func (e *TryFromFloatSecsError) Error() string { return e.String() }

// FmtDisplay is the Display impl.
func (e *TryFromFloatSecsError) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(e.String())
}

// FmtDebug is the derived Debug impl.
func (e *TryFromFloatSecsError) FmtDebug(f *rs.Formatter) error {
	kind := "OverflowOrNan"
	if e.kind == tryFromFloatSecsErrorKind_Negative {
		kind = "Negative"
	}
	return f.DebugStruct("TryFromFloatSecsError").Field("kind", rawDebug(kind)).Finish()
}

// Equal is the derived PartialEq.
func (e *TryFromFloatSecsError) Equal(other *TryFromFloatSecsError) bool {
	return e.kind == other.kind
}

// rawDebug is a value whose Debug output is its text as it stands, the way a
// unit variant's name or `format_args!("...")` prints.
type rawDebug string

func (r rawDebug) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(r)) }

// DurationTryFromSecsF64Parts is Duration::try_from_secs_f64 over Rust's full
// range: the whole seconds and nanoseconds of the result. It rounds to the
// nearest nanosecond, ties to even, exactly as Rust does.
func DurationTryFromSecsF64Parts(value float64) (secs uint64, nanos uint32, err error) {
	const (
		mantBits = 52
		expBits  = 11
		offset   = 44
		minExp   = 1 - (1<<expBits)/2
		mantMask = uint64(1)<<mantBits - 1
		expMask  = uint64(1)<<expBits - 1
	)
	if value < 0 {
		return 0, 0, &TryFromFloatSecsError{kind: tryFromFloatSecsErrorKind_Negative}
	}
	raw := math.Float64bits(value)
	mant := raw&mantMask | (mantMask + 1)
	exp := int((raw>>mantBits)&expMask) + minExp

	switch {
	case exp < -31:
		// the input represents less than 1ns and can not be rounded to it
		return 0, 0, nil
	case exp < 0:
		// the input is less than 1 second
		// t = mant << (offset + exp), a 128-bit value
		shift := uint(offset + exp)
		tHigh := mant >> (64 - shift)
		tLow := mant << shift
		// nanos_tmp = NANOS_PER_SEC * t, shifted right by mantBits + offset = 96
		carry, low := bits.Mul64(tLow, nanosPerSec)
		high := tHigh*nanosPerSec + carry
		nanos = uint32(high >> 32)
		remHigh := high & (1<<32 - 1)
		isTie := remHigh == 1<<31 && low == 0
		isEven := nanos&1 == 0
		remMsbClear := remHigh&(1<<31) == 0
		if !(remMsbClear || (isEven && isTie)) {
			nanos++
		}
		if nanos != nanosPerSec {
			return 0, nanos, nil
		}
		return 1, 0, nil
	case exp < mantBits:
		secs = mant >> uint(mantBits-exp)
		t := (mant << uint(exp)) & mantMask
		// nanos_tmp = NANOS_PER_SEC * t, shifted right by mantBits
		high, low := bits.Mul64(t, nanosPerSec)
		nanos = uint32(high<<(64-mantBits) | low>>mantBits)
		rem := low & mantMask
		isTie := rem == 1<<(mantBits-1)
		isEven := nanos&1 == 0
		remMsbClear := rem&(1<<(mantBits-1)) == 0
		if !(remMsbClear || (isEven && isTie)) {
			nanos++
		}
		if nanos != nanosPerSec {
			return secs, nanos, nil
		}
		return secs + 1, 0, nil
	case exp < 64:
		// the input has no fractional part
		return mant << uint(exp-mantBits), 0, nil
	default:
		return 0, 0, &TryFromFloatSecsError{kind: tryFromFloatSecsErrorKind_OverflowOrNan}
	}
}

// DurationTryFromSecsF32Parts is Duration::try_from_secs_f32 over Rust's full
// range.
func DurationTryFromSecsF32Parts(value float32) (secs uint64, nanos uint32, err error) {
	const (
		mantBits = 23
		expBits  = 8
		offset   = 41
		minExp   = 1 - (1<<expBits)/2
		mantMask = uint32(1)<<mantBits - 1
		expMask  = uint32(1)<<expBits - 1
	)
	if value < 0 {
		return 0, 0, &TryFromFloatSecsError{kind: tryFromFloatSecsErrorKind_Negative}
	}
	raw := math.Float32bits(value)
	mant := raw&mantMask | (mantMask + 1)
	exp := int((raw>>mantBits)&expMask) + minExp

	switch {
	case exp < -31:
		// the input represents less than 1ns and can not be rounded to it
		return 0, 0, nil
	case exp < 0:
		// the input is less than 1 second
		t := uint64(mant) << uint(offset+exp)
		// nanos_tmp = NANOS_PER_SEC * t, shifted right by mantBits + offset = 64
		high, low := bits.Mul64(t, nanosPerSec)
		nanos = uint32(high)
		isTie := low == 1<<63
		isEven := nanos&1 == 0
		remMsbClear := low&(1<<63) == 0
		if !(remMsbClear || (isEven && isTie)) {
			nanos++
		}
		// f32 does not have enough precision to reach a whole second here;
		// Duration::new carries if it ever does.
		return carryNanos(0, nanos)
	case exp < mantBits:
		secs = uint64(mant >> uint(mantBits-exp))
		t := uint64((mant << uint(exp)) & mantMask)
		nanosTmp := nanosPerSec * t
		nanos = uint32(nanosTmp >> mantBits)
		rem := nanosTmp & uint64(mantMask)
		isTie := rem == 1<<(mantBits-1)
		isEven := nanos&1 == 0
		remMsbClear := rem&(1<<(mantBits-1)) == 0
		if !(remMsbClear || (isEven && isTie)) {
			nanos++
		}
		return carryNanos(secs, nanos)
	case exp < 64:
		// the input has no fractional part
		return uint64(mant) << uint(exp-mantBits), 0, nil
	default:
		return 0, 0, &TryFromFloatSecsError{kind: tryFromFloatSecsErrorKind_OverflowOrNan}
	}
}

// carryNanos is Duration::new on parts: a whole second in nanos moves to
// secs.
func carryNanos(secs uint64, nanos uint32) (uint64, uint32, error) {
	if nanos >= nanosPerSec {
		return secs + uint64(nanos/nanosPerSec), nanos % nanosPerSec, nil
	}
	return secs, nanos, nil
}
