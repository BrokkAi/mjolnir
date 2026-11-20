// Port of coretests 1.99.1 tests/num/floats.rs (the f32 and f64 tests of
// the methods num_float.go provides), generated from the Rust file
//
// Each `float_test!` case runs for float32 and float64. Left out: the f16
// and f128 forms; the tests of methods the port does not have or that Go's
// conversions already are (classify, is_normal, minimum, maximum, midpoint,
// next_up, next_down, the from/to conversions and constants, to_algebraic,
// asinh, acosh, atanh, gamma, ln_gamma); exp, exp2, ln, log, log2, log10 and
// powf, which go to the C library in Rust and are held against it, within
// stated units in the last place, by TestNumLibmAgainstRust; and the
// `cfg!(miri)` branches. Every statement of the cases below is ported.

package rs_test

import (
	"math"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func numStdIs32[F float32 | float64]() bool {
	var zero F
	_, is32 := any(zero).(float32)
	return is32
}

// numStdK is the constant `Float::<name>` of TestableFloat, f32 or f64.
func numStdK[F float32 | float64](name string) F {
	if numStdIs32[F]() {
		switch name {
		case "NAN":
			return F(math.Float32frombits(0x7fc00000))
		case "SNAN":
			return F(math.Float32frombits(0x7fc00000 - 1))
		case "INFINITY":
			return F(float32(math.Inf(1)))
		case "NEG_INFINITY":
			return F(float32(math.Inf(-1)))
		case "MAX":
			return F(float32(math.MaxFloat32))
		case "MIN":
			return F(float32(-math.MaxFloat32))
		case "MIN_POSITIVE", "MIN_POSITIVE_NORMAL":
			return F(math.Float32frombits(0x00800000))
		case "MAX_SUBNORMAL":
			return F(math.Float32frombits(0x007fffff))
		case "TINY":
			return F(math.Float32frombits(0x1))
		case "EPSILON":
			return F(math.Float32frombits(0x34000000))
		case "MUL_ADD_RESULT":
			return F(float32(62.05))
		case "NEG_MUL_ADD_RESULT":
			return F(float32(48.65))
		case "MAX_RECIP":
			return F(float32(2.938736e-39))
		case "PI":
			return F(float32(math.Pi))
		}
	} else {
		switch name {
		case "NAN":
			return F(math.Float64frombits(0x7ff8000000000000))
		case "SNAN":
			return F(math.Float64frombits(0x7ff8000000000000 - 1))
		case "INFINITY":
			return F(math.Inf(1))
		case "NEG_INFINITY":
			return F(math.Inf(-1))
		case "MAX":
			return F(numStdF64(math.MaxFloat64))
		case "MIN":
			return F(numStdF64(-math.MaxFloat64))
		case "MIN_POSITIVE", "MIN_POSITIVE_NORMAL":
			return F(math.Float64frombits(0x0010000000000000))
		case "MAX_SUBNORMAL":
			return F(math.Float64frombits(0x000fffffffffffff))
		case "TINY":
			return F(math.Float64frombits(0x1))
		case "EPSILON":
			return F(math.Float64frombits(0x3cb0000000000000))
		case "MUL_ADD_RESULT":
			return F(62.050000000000004)
		case "NEG_MUL_ADD_RESULT":
			return F(48.650000000000006)
		case "MAX_RECIP":
			return F(5.562684646268003e-309)
		case "PI":
			return F(math.Pi)
		}
	}
	switch name {
	case "ZERO":
		return 0
	case "ONE":
		return 1
	case "APPROX", "POWI_APPROX", "PI_TO_DEGREES_APPROX", "_180_TO_RADIANS_APPROX":
		return F(1e-6)
	}
	panic("no constant " + name)
}

// numStdF64 makes a float64 constant a value, so that F() of it compiles
// for float32 too.
func numStdF64(x float64) float64 { return x }

// numStdQNaN is total_cmp's q_nan(): NAN with the quiet bit set, which NAN
// already has.
func numStdQNaN[F float32 | float64]() F { return numStdK[F]("NAN") }

func numStdCall[F float32 | float64](method string, args ...F) F {
	if numStdIs32[F]() {
		a := make([]float32, len(args))
		for i, x := range args {
			a[i] = float32(x)
		}
		return F(numStdCall32(method, a))
	}
	a := make([]float64, len(args))
	for i, x := range args {
		a[i] = float64(x)
	}
	return F(numStdCall64(method, a))
}

func numStdCall32(method string, a []float32) float32 {
	switch method {
	case "signum":
		return rs.F32Signum(a[0])
	case "fract":
		return rs.F32Fract(a[0])
	case "recip":
		return rs.F32Recip(a[0])
	case "to_degrees":
		return rs.F32ToDegrees(a[0])
	case "to_radians":
		return rs.F32ToRadians(a[0])
	case "round":
		return rs.F32Round(a[0])
	case "round_ties_even":
		return rs.F32RoundTiesEven(a[0])
	case "floor":
		return rs.F32Floor(a[0])
	case "ceil":
		return rs.F32Ceil(a[0])
	case "trunc":
		return rs.F32Trunc(a[0])
	case "abs":
		return rs.F32Abs(a[0])
	case "sqrt":
		return rs.F32Sqrt(a[0])
	case "min":
		return rs.F32Min(a[0], a[1])
	case "max":
		return rs.F32Max(a[0], a[1])
	case "copysign":
		return rs.F32Copysign(a[0], a[1])
	case "rem_euclid":
		return rs.F32RemEuclid(a[0], a[1])
	case "div_euclid":
		return rs.F32DivEuclid(a[0], a[1])
	case "clamp":
		return rs.F32Clamp(a[0], a[1], a[2])
	case "mul_add":
		return rs.F32MulAdd(a[0], a[1], a[2])
	}
	panic("no method " + method)
}

func numStdCall64(method string, a []float64) float64 {
	switch method {
	case "signum":
		return rs.F64Signum(a[0])
	case "fract":
		return rs.F64Fract(a[0])
	case "recip":
		return rs.F64Recip(a[0])
	case "to_degrees":
		return rs.F64ToDegrees(a[0])
	case "to_radians":
		return rs.F64ToRadians(a[0])
	case "round":
		return rs.F64Round(a[0])
	case "round_ties_even":
		return rs.F64RoundTiesEven(a[0])
	case "floor":
		return rs.F64Floor(a[0])
	case "ceil":
		return rs.F64Ceil(a[0])
	case "trunc":
		return rs.F64Trunc(a[0])
	case "abs":
		return rs.F64Abs(a[0])
	case "sqrt":
		return rs.F64Sqrt(a[0])
	case "min":
		return rs.F64Min(a[0], a[1])
	case "max":
		return rs.F64Max(a[0], a[1])
	case "copysign":
		return rs.F64Copysign(a[0], a[1])
	case "rem_euclid":
		return rs.F64RemEuclid(a[0], a[1])
	case "div_euclid":
		return rs.F64DivEuclid(a[0], a[1])
	case "clamp":
		return rs.F64Clamp(a[0], a[1], a[2])
	case "mul_add":
		return rs.F64MulAdd(a[0], a[1], a[2])
	}
	panic("no method " + method)
}

func numStdPred[F float32 | float64](method string, x F) bool {
	if numStdIs32[F]() {
		switch method {
		case "is_nan":
			return rs.F32IsNan(float32(x))
		case "is_infinite":
			return rs.F32IsInfinite(float32(x))
		case "is_finite":
			return rs.F32IsFinite(float32(x))
		case "is_sign_positive":
			return rs.F32IsSignPositive(float32(x))
		case "is_sign_negative":
			return rs.F32IsSignNegative(float32(x))
		}
	} else {
		switch method {
		case "is_nan":
			return rs.F64IsNan(float64(x))
		case "is_infinite":
			return rs.F64IsInfinite(float64(x))
		case "is_finite":
			return rs.F64IsFinite(float64(x))
		case "is_sign_positive":
			return rs.F64IsSignPositive(float64(x))
		case "is_sign_negative":
			return rs.F64IsSignNegative(float64(x))
		}
	}
	panic("no predicate " + method)
}

func numStdPowi[F float32 | float64](x F, n int32) F {
	if numStdIs32[F]() {
		return F(rs.F32Powi(float32(x), n))
	}
	return F(rs.F64Powi(float64(x), n))
}

func numStdTotalCmp[F float32 | float64](a, b F) rs.Ordering {
	if numStdIs32[F]() {
		return rs.F32TotalCmp(float32(a), float32(b))
	}
	return rs.F64TotalCmp(float64(a), float64(b))
}

func numStdBitsOf[F float32 | float64](x F) uint64 {
	if numStdIs32[F]() {
		return uint64(math.Float32bits(float32(x)))
	}
	return math.Float64bits(float64(x))
}

// numStdBiteq is assert_biteq!: the two are the same bits.
func numStdBiteq[F float32 | float64](t *testing.T, what string, got, want F) {
	t.Helper()
	if numStdBitsOf(got) != numStdBitsOf(want) {
		t.Errorf("%s: got %v (%#x), want %v (%#x)", what, got, numStdBitsOf(got), want, numStdBitsOf(want))
	}
}

// numStdApprox is assert_approx_eq!: the difference is within lim.
func numStdApprox[F float32 | float64](t *testing.T, what string, got, want, lim F) {
	t.Helper()
	diff := got - want
	if diff < 0 {
		diff = -diff
	}
	if !(diff <= lim) {
		t.Errorf("%s: %v is not approximately equal to %v (threshold %v, difference %v)", what, got, want, lim, diff)
	}
}

func numStdAssert(t *testing.T, what string, ok bool) {
	t.Helper()
	if !ok {
		t.Errorf("assertion failed: %s", what)
	}
}

// numStdShouldPanic is #[should_panic].
func numStdShouldPanic(t *testing.T, f func()) {
	t.Helper()
	defer func() {
		if recover() == nil {
			t.Errorf("no panic")
		}
	}()
	f()
}

func numStdFloats_min[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(0.0).min(0.0), 0.0);", numStdCall("min", F(0.0), F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).min(-0.0), -0.0);", numStdCall("min", -F(0.0), -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).min(9.0), 9.0);", numStdCall("min", F(9.0), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).min(0.0), -9.0);", numStdCall("min", -F(9.0), F(0.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).min(9.0), 0.0);", numStdCall("min", F(0.0), F(9.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).min(9.0), -0.0);", numStdCall("min", -F(0.0), F(9.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).min(-9.0), -9.0);", numStdCall("min", -F(0.0), -F(9.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.min(9.0), 9.0);", numStdCall("min", numStdK[F]("INFINITY"), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).min(Float::INFINITY), 9.0);", numStdCall("min", F(9.0), numStdK[F]("INFINITY")), F(9.0))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.min(-9.0), -9.0);", numStdCall("min", numStdK[F]("INFINITY"), -F(9.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).min(Float::INFINITY), -9.0);", numStdCall("min", -F(9.0), numStdK[F]("INFINITY")), -F(9.0))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.min(9.0), Float::NEG_INFINITY);", numStdCall("min", numStdK[F]("NEG_INFINITY"), F(9.0)), numStdK[F]("NEG_INFINITY"))
	numStdBiteq(t, "assert_biteq!(flt(9.0).min(Float::NEG_INFINITY), Float::NEG_INFINITY);", numStdCall("min", F(9.0), numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.min(-9.0), Float::NEG_INFINITY);", numStdCall("min", numStdK[F]("NEG_INFINITY"), -F(9.0)), numStdK[F]("NEG_INFINITY"))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).min(Float::NEG_INFINITY), Float::NEG_INFINITY);", numStdCall("min", -F(9.0), numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NAN.min(black_box(9.0)), 9.0);", numStdCall("min", numStdK[F]("NAN"), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(black_box(Float::NAN).min(-9.0), -9.0);", numStdCall("min", numStdK[F]("NAN"), -F(9.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).min(black_box(Float::NAN)), 9.0);", numStdCall("min", F(9.0), numStdK[F]("NAN")), F(9.0))
	numStdBiteq(t, "assert_biteq!(black_box(flt(-9.0)).min(Float::NAN), -9.0);", numStdCall("min", -F(9.0), numStdK[F]("NAN")), -F(9.0))
	numStdAssert(t, "assert!(Float::NAN.min(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("min", numStdK[F]("NAN"), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::SNAN.min(Float::SNAN).is_nan());", numStdPred("is_nan", numStdCall("min", numStdK[F]("SNAN"), numStdK[F]("SNAN"))))
}

func numStdFloats_max[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(0.0).max(0.0), 0.0);", numStdCall("max", F(0.0), F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).max(-0.0), -0.0);", numStdCall("max", -F(0.0), -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).max(9.0), 9.0);", numStdCall("max", F(9.0), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).max(0.0), 0.0);", numStdCall("max", -F(9.0), F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).max(-0.0), -0.0);", numStdCall("max", -F(9.0), -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).max(9.0), 9.0);", numStdCall("max", F(0.0), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).max(-9.0), 0.0);", numStdCall("max", F(0.0), -F(9.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).max(-9.0), -0.0);", numStdCall("max", -F(0.0), -F(9.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.max(9.0), Float::INFINITY);", numStdCall("max", numStdK[F]("INFINITY"), F(9.0)), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(flt(9.0).max(Float::INFINITY), Float::INFINITY);", numStdCall("max", F(9.0), numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.max(-9.0), Float::INFINITY);", numStdCall("max", numStdK[F]("INFINITY"), -F(9.0)), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).max(Float::INFINITY), Float::INFINITY);", numStdCall("max", -F(9.0), numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.max(9.0), 9.0);", numStdCall("max", numStdK[F]("NEG_INFINITY"), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).max(Float::NEG_INFINITY), 9.0);", numStdCall("max", F(9.0), numStdK[F]("NEG_INFINITY")), F(9.0))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.max(-9.0), -9.0);", numStdCall("max", numStdK[F]("NEG_INFINITY"), -F(9.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(-9.0).max(Float::NEG_INFINITY), -9.0);", numStdCall("max", -F(9.0), numStdK[F]("NEG_INFINITY")), -F(9.0))
	numStdBiteq(t, "assert_biteq!(Float::NAN.max(black_box(9.0)), 9.0);", numStdCall("max", numStdK[F]("NAN"), F(9.0)), F(9.0))
	numStdBiteq(t, "assert_biteq!(black_box(Float::NAN).max(-9.0), -9.0);", numStdCall("max", numStdK[F]("NAN"), -F(9.0)), -F(9.0))
	numStdBiteq(t, "assert_biteq!(flt(9.0).max(black_box(Float::NAN)), 9.0);", numStdCall("max", F(9.0), numStdK[F]("NAN")), F(9.0))
	numStdBiteq(t, "assert_biteq!(black_box(flt(-9.0)).max(Float::NAN), -9.0);", numStdCall("max", -F(9.0), numStdK[F]("NAN")), -F(9.0))
	numStdAssert(t, "assert!(Float::NAN.max(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("max", numStdK[F]("NAN"), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::SNAN.max(Float::SNAN).is_nan());", numStdPred("is_nan", numStdCall("max", numStdK[F]("SNAN"), numStdK[F]("SNAN"))))
}

func numStdFloats_abs[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.abs(), Float::INFINITY);", numStdCall("abs", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::ONE.abs(), Float::ONE);", numStdCall("abs", numStdK[F]("ONE")), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::ZERO.abs(), Float::ZERO);", numStdCall("abs", numStdK[F]("ZERO")), numStdK[F]("ZERO"))
	numStdBiteq(t, "assert_biteq!((-Float::ZERO).abs(), Float::ZERO);", numStdCall("abs", (-numStdK[F]("ZERO"))), numStdK[F]("ZERO"))
	numStdBiteq(t, "assert_biteq!((-Float::ONE).abs(), Float::ONE);", numStdCall("abs", (-numStdK[F]("ONE"))), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.abs(), Float::INFINITY);", numStdCall("abs", numStdK[F]("NEG_INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!((Float::ONE / Float::NEG_INFINITY).abs(), Float::ZERO);", numStdCall("abs", (numStdK[F]("ONE")/numStdK[F]("NEG_INFINITY"))), numStdK[F]("ZERO"))
	numStdAssert(t, "assert!(Float::NAN.abs().is_nan());", numStdPred("is_nan", numStdCall("abs", numStdK[F]("NAN"))))
}

func numStdFloats_copysign[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(1.0).copysign(-2.0), -1.0);", numStdCall("copysign", F(1.0), -F(2.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).copysign(2.0), 1.0);", numStdCall("copysign", -F(1.0), F(2.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.copysign(-0.0), Float::NEG_INFINITY);", numStdCall("copysign", numStdK[F]("INFINITY"), -F(0.0)), numStdK[F]("NEG_INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.copysign(0.0), Float::INFINITY);", numStdCall("copysign", numStdK[F]("NEG_INFINITY"), F(0.0)), numStdK[F]("INFINITY"))
}

func numStdFloats_rem_euclid[F float32 | float64](t *testing.T) {
	numStdAssert(t, "assert!(Float::INFINITY.rem_euclid(42.0).is_nan());", numStdPred("is_nan", numStdCall("rem_euclid", numStdK[F]("INFINITY"), F(42.0))))
	numStdBiteq(t, "assert_biteq!(flt(42.0).rem_euclid(Float::INFINITY), 42.0);", numStdCall("rem_euclid", F(42.0), numStdK[F]("INFINITY")), F(42.0))
	numStdAssert(t, "assert!(flt(42.0).rem_euclid(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("rem_euclid", F(42.0), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::INFINITY.rem_euclid(Float::INFINITY).is_nan());", numStdPred("is_nan", numStdCall("rem_euclid", numStdK[F]("INFINITY"), numStdK[F]("INFINITY"))))
	numStdAssert(t, "assert!(Float::INFINITY.rem_euclid(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("rem_euclid", numStdK[F]("INFINITY"), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::NAN.rem_euclid(Float::INFINITY).is_nan());", numStdPred("is_nan", numStdCall("rem_euclid", numStdK[F]("NAN"), numStdK[F]("INFINITY"))))
}

func numStdFloats_div_euclid[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(42.0).div_euclid(Float::INFINITY), 0.0);", numStdCall("div_euclid", F(42.0), numStdK[F]("INFINITY")), F(0.0))
	numStdAssert(t, "assert!(flt(42.0).div_euclid(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("div_euclid", F(42.0), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::INFINITY.div_euclid(Float::INFINITY).is_nan());", numStdPred("is_nan", numStdCall("div_euclid", numStdK[F]("INFINITY"), numStdK[F]("INFINITY"))))
	numStdAssert(t, "assert!(Float::INFINITY.div_euclid(Float::NAN).is_nan());", numStdPred("is_nan", numStdCall("div_euclid", numStdK[F]("INFINITY"), numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::NAN.div_euclid(Float::INFINITY).is_nan());", numStdPred("is_nan", numStdCall("div_euclid", numStdK[F]("NAN"), numStdK[F]("INFINITY"))))
}

func numStdFloats_floor[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(1.0).floor(), 1.0);", numStdCall("floor", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.3).floor(), 1.0);", numStdCall("floor", F(1.3)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.5).floor(), 1.0);", numStdCall("floor", F(1.5)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.7).floor(), 1.0);", numStdCall("floor", F(1.7)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(0.5).floor(), 0.0);", numStdCall("floor", F(0.5)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).floor(), 0.0);", numStdCall("floor", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).floor(), -0.0);", numStdCall("floor", -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).floor(), -1.0);", numStdCall("floor", -F(0.5)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).floor(), -1.0);", numStdCall("floor", -F(1.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.3).floor(), -2.0);", numStdCall("floor", -F(1.3)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).floor(), -2.0);", numStdCall("floor", -F(1.5)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.7).floor(), -2.0);", numStdCall("floor", -F(1.7)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(Float::MAX.floor(), Float::MAX);", numStdCall("floor", numStdK[F]("MAX")), numStdK[F]("MAX"))
	numStdBiteq(t, "assert_biteq!(Float::MIN.floor(), Float::MIN);", numStdCall("floor", numStdK[F]("MIN")), numStdK[F]("MIN"))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.floor(), 0.0);", numStdCall("floor", numStdK[F]("MIN_POSITIVE")), F(0.0))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).floor(), -1.0);", numStdCall("floor", (-numStdK[F]("MIN_POSITIVE"))), -F(1.0))
	numStdAssert(t, "assert!(Float::NAN.floor().is_nan());", numStdPred("is_nan", numStdCall("floor", numStdK[F]("NAN"))))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.floor(), Float::INFINITY);", numStdCall("floor", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.floor(), Float::NEG_INFINITY);", numStdCall("floor", numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
}

func numStdFloats_ceil[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(1.0).ceil(), 1.0);", numStdCall("ceil", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.3).ceil(), 2.0);", numStdCall("ceil", F(1.3)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(1.5).ceil(), 2.0);", numStdCall("ceil", F(1.5)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(1.7).ceil(), 2.0);", numStdCall("ceil", F(1.7)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(0.5).ceil(), 1.0);", numStdCall("ceil", F(0.5)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).ceil(), 0.0);", numStdCall("ceil", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).ceil(), -0.0);", numStdCall("ceil", -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).ceil(), -0.0);", numStdCall("ceil", -F(0.5)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).ceil(), -1.0);", numStdCall("ceil", -F(1.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.3).ceil(), -1.0);", numStdCall("ceil", -F(1.3)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).ceil(), -1.0);", numStdCall("ceil", -F(1.5)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.7).ceil(), -1.0);", numStdCall("ceil", -F(1.7)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(Float::MAX.ceil(), Float::MAX);", numStdCall("ceil", numStdK[F]("MAX")), numStdK[F]("MAX"))
	numStdBiteq(t, "assert_biteq!(Float::MIN.ceil(), Float::MIN);", numStdCall("ceil", numStdK[F]("MIN")), numStdK[F]("MIN"))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.ceil(), 1.0);", numStdCall("ceil", numStdK[F]("MIN_POSITIVE")), F(1.0))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).ceil(), -0.0);", numStdCall("ceil", (-numStdK[F]("MIN_POSITIVE"))), -F(0.0))
	numStdAssert(t, "assert!(Float::NAN.ceil().is_nan());", numStdPred("is_nan", numStdCall("ceil", numStdK[F]("NAN"))))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.ceil(), Float::INFINITY);", numStdCall("ceil", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.ceil(), Float::NEG_INFINITY);", numStdCall("ceil", numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
}

func numStdFloats_round[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(2.5).round(), 3.0);", numStdCall("round", F(2.5)), F(3.0))
	numStdBiteq(t, "assert_biteq!(flt(1.0).round(), 1.0);", numStdCall("round", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.3).round(), 1.0);", numStdCall("round", F(1.3)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.5).round(), 2.0);", numStdCall("round", F(1.5)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(1.7).round(), 2.0);", numStdCall("round", F(1.7)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(0.5).round(), 1.0);", numStdCall("round", F(0.5)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).round(), 0.0);", numStdCall("round", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).round(), -0.0);", numStdCall("round", -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).round(), -1.0);", numStdCall("round", -F(0.5)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).round(), -1.0);", numStdCall("round", -F(1.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.3).round(), -1.0);", numStdCall("round", -F(1.3)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).round(), -2.0);", numStdCall("round", -F(1.5)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.7).round(), -2.0);", numStdCall("round", -F(1.7)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(Float::MAX.round(), Float::MAX);", numStdCall("round", numStdK[F]("MAX")), numStdK[F]("MAX"))
	numStdBiteq(t, "assert_biteq!(Float::MIN.round(), Float::MIN);", numStdCall("round", numStdK[F]("MIN")), numStdK[F]("MIN"))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.round(), 0.0);", numStdCall("round", numStdK[F]("MIN_POSITIVE")), F(0.0))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).round(), -0.0);", numStdCall("round", (-numStdK[F]("MIN_POSITIVE"))), -F(0.0))
	numStdAssert(t, "assert!(Float::NAN.round().is_nan());", numStdPred("is_nan", numStdCall("round", numStdK[F]("NAN"))))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.round(), Float::INFINITY);", numStdCall("round", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.round(), Float::NEG_INFINITY);", numStdCall("round", numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
}

func numStdFloats_round_ties_even[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(2.5).round_ties_even(), 2.0);", numStdCall("round_ties_even", F(2.5)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(1.0).round_ties_even(), 1.0);", numStdCall("round_ties_even", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.3).round_ties_even(), 1.0);", numStdCall("round_ties_even", F(1.3)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.5).round_ties_even(), 2.0);", numStdCall("round_ties_even", F(1.5)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(1.7).round_ties_even(), 2.0);", numStdCall("round_ties_even", F(1.7)), F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(0.5).round_ties_even(), 0.0);", numStdCall("round_ties_even", F(0.5)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).round_ties_even(), 0.0);", numStdCall("round_ties_even", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).round_ties_even(), -0.0);", numStdCall("round_ties_even", -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).round_ties_even(), -0.0);", numStdCall("round_ties_even", -F(0.5)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).round_ties_even(), -1.0);", numStdCall("round_ties_even", -F(1.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.3).round_ties_even(), -1.0);", numStdCall("round_ties_even", -F(1.3)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).round_ties_even(), -2.0);", numStdCall("round_ties_even", -F(1.5)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.7).round_ties_even(), -2.0);", numStdCall("round_ties_even", -F(1.7)), -F(2.0))
	numStdBiteq(t, "assert_biteq!(Float::MAX.round_ties_even(), Float::MAX);", numStdCall("round_ties_even", numStdK[F]("MAX")), numStdK[F]("MAX"))
	numStdBiteq(t, "assert_biteq!(Float::MIN.round_ties_even(), Float::MIN);", numStdCall("round_ties_even", numStdK[F]("MIN")), numStdK[F]("MIN"))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.round_ties_even(), 0.0);", numStdCall("round_ties_even", numStdK[F]("MIN_POSITIVE")), F(0.0))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).round_ties_even(), -0.0);", numStdCall("round_ties_even", (-numStdK[F]("MIN_POSITIVE"))), -F(0.0))
	numStdAssert(t, "assert!(Float::NAN.round_ties_even().is_nan());", numStdPred("is_nan", numStdCall("round_ties_even", numStdK[F]("NAN"))))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.round_ties_even(), Float::INFINITY);", numStdCall("round_ties_even", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.round_ties_even(), Float::NEG_INFINITY);", numStdCall("round_ties_even", numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
}

func numStdFloats_trunc[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(1.0).trunc(), 1.0);", numStdCall("trunc", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.3).trunc(), 1.0);", numStdCall("trunc", F(1.3)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.5).trunc(), 1.0);", numStdCall("trunc", F(1.5)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(1.7).trunc(), 1.0);", numStdCall("trunc", F(1.7)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(0.5).trunc(), 0.0);", numStdCall("trunc", F(0.5)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(0.0).trunc(), 0.0);", numStdCall("trunc", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).trunc(), -0.0);", numStdCall("trunc", -F(0.0)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).trunc(), -0.0);", numStdCall("trunc", -F(0.5)), -F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).trunc(), -1.0);", numStdCall("trunc", -F(1.0)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.3).trunc(), -1.0);", numStdCall("trunc", -F(1.3)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).trunc(), -1.0);", numStdCall("trunc", -F(1.5)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(-1.7).trunc(), -1.0);", numStdCall("trunc", -F(1.7)), -F(1.0))
	numStdBiteq(t, "assert_biteq!(Float::MAX.trunc(), Float::MAX);", numStdCall("trunc", numStdK[F]("MAX")), numStdK[F]("MAX"))
	numStdBiteq(t, "assert_biteq!(Float::MIN.trunc(), Float::MIN);", numStdCall("trunc", numStdK[F]("MIN")), numStdK[F]("MIN"))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.trunc(), 0.0);", numStdCall("trunc", numStdK[F]("MIN_POSITIVE")), F(0.0))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).trunc(), -0.0);", numStdCall("trunc", (-numStdK[F]("MIN_POSITIVE"))), -F(0.0))
	numStdAssert(t, "assert!(Float::NAN.trunc().is_nan());", numStdPred("is_nan", numStdCall("trunc", numStdK[F]("NAN"))))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.trunc(), Float::INFINITY);", numStdCall("trunc", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.trunc(), Float::NEG_INFINITY);", numStdCall("trunc", numStdK[F]("NEG_INFINITY")), numStdK[F]("NEG_INFINITY"))
}

func numStdFloats_fract[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(flt(1.0).fract(), 0.0);", numStdCall("fract", F(1.0)), F(0.0))
	numStdApprox(t, "assert_approx_eq!(flt(1.3).fract(), 0.3);", numStdCall("fract", F(1.3)), F(0.3), numStdK[F]("APPROX"))
	numStdBiteq(t, "assert_biteq!(flt(1.5).fract(), 0.5);", numStdCall("fract", F(1.5)), F(0.5))
	numStdApprox(t, "assert_approx_eq!(flt(1.7).fract(), 0.7);", numStdCall("fract", F(1.7)), F(0.7), numStdK[F]("APPROX"))
	numStdBiteq(t, "assert_biteq!(flt(0.5).fract(), 0.5);", numStdCall("fract", F(0.5)), F(0.5))
	numStdBiteq(t, "assert_biteq!(flt(0.0).fract(), 0.0);", numStdCall("fract", F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.0).fract(), 0.0);", numStdCall("fract", -F(0.0)), F(0.0))
	numStdBiteq(t, "assert_biteq!(flt(-0.5).fract(), -0.5);", numStdCall("fract", -F(0.5)), -F(0.5))
	numStdBiteq(t, "assert_biteq!(flt(-1.0).fract(), 0.0);", numStdCall("fract", -F(1.0)), F(0.0))
	numStdApprox(t, "assert_approx_eq!(flt(-1.3).fract(), -0.3);", numStdCall("fract", -F(1.3)), -F(0.3), numStdK[F]("APPROX"))
	numStdBiteq(t, "assert_biteq!(flt(-1.5).fract(), -0.5);", numStdCall("fract", -F(1.5)), -F(0.5))
	numStdApprox(t, "assert_approx_eq!(flt(-1.7).fract(), -0.7);", numStdCall("fract", -F(1.7)), -F(0.7), numStdK[F]("APPROX"))
	numStdBiteq(t, "assert_biteq!(Float::MAX.fract(), 0.0);", numStdCall("fract", numStdK[F]("MAX")), F(0.0))
	numStdBiteq(t, "assert_biteq!(Float::MIN.fract(), 0.0);", numStdCall("fract", numStdK[F]("MIN")), F(0.0))
	numStdBiteq(t, "assert_biteq!(Float::MIN_POSITIVE.fract(), Float::MIN_POSITIVE);", numStdCall("fract", numStdK[F]("MIN_POSITIVE")), numStdK[F]("MIN_POSITIVE"))
	numStdAssert(t, "assert!(Float::MIN_POSITIVE.fract().is_sign_positive());", numStdPred("is_sign_positive", numStdCall("fract", numStdK[F]("MIN_POSITIVE"))))
	numStdBiteq(t, "assert_biteq!((-Float::MIN_POSITIVE).fract(), -Float::MIN_POSITIVE);", numStdCall("fract", (-numStdK[F]("MIN_POSITIVE"))), -numStdK[F]("MIN_POSITIVE"))
	numStdAssert(t, "assert!((-Float::MIN_POSITIVE).fract().is_sign_negative());", numStdPred("is_sign_negative", numStdCall("fract", (-numStdK[F]("MIN_POSITIVE")))))
	numStdAssert(t, "assert!(Float::NAN.fract().is_nan());", numStdPred("is_nan", numStdCall("fract", numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::INFINITY.fract().is_nan());", numStdPred("is_nan", numStdCall("fract", numStdK[F]("INFINITY"))))
	numStdAssert(t, "assert!(Float::NEG_INFINITY.fract().is_nan());", numStdPred("is_nan", numStdCall("fract", numStdK[F]("NEG_INFINITY"))))
}

func numStdFloats_signum[F float32 | float64](t *testing.T) {
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.signum(), Float::ONE);", numStdCall("signum", numStdK[F]("INFINITY")), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::ONE.signum(), Float::ONE);", numStdCall("signum", numStdK[F]("ONE")), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::ZERO.signum(), Float::ONE);", numStdCall("signum", numStdK[F]("ZERO")), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!((-Float::ZERO).signum(), -Float::ONE);", numStdCall("signum", (-numStdK[F]("ZERO"))), -numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!((-Float::ONE).signum(), -Float::ONE);", numStdCall("signum", (-numStdK[F]("ONE"))), -numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::NEG_INFINITY.signum(), -Float::ONE);", numStdCall("signum", numStdK[F]("NEG_INFINITY")), -numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!((Float::ONE / Float::NEG_INFINITY).signum(), -Float::ONE);", numStdCall("signum", (numStdK[F]("ONE")/numStdK[F]("NEG_INFINITY"))), -numStdK[F]("ONE"))
	numStdAssert(t, "assert!(Float::NAN.signum().is_nan());", numStdPred("is_nan", numStdCall("signum", numStdK[F]("NAN"))))
}

func numStdFloats_is_sign_positive[F float32 | float64](t *testing.T) {
	numStdAssert(t, "assert!(Float::INFINITY.is_sign_positive());", numStdPred("is_sign_positive", numStdK[F]("INFINITY")))
	numStdAssert(t, "assert!(Float::ONE.is_sign_positive());", numStdPred("is_sign_positive", numStdK[F]("ONE")))
	numStdAssert(t, "assert!(Float::ZERO.is_sign_positive());", numStdPred("is_sign_positive", numStdK[F]("ZERO")))
	numStdAssert(t, "assert!(!(-Float::ZERO).is_sign_positive());", !numStdPred("is_sign_positive", (-numStdK[F]("ZERO"))))
	numStdAssert(t, "assert!(!(-Float::ONE).is_sign_positive());", !numStdPred("is_sign_positive", (-numStdK[F]("ONE"))))
	numStdAssert(t, "assert!(!Float::NEG_INFINITY.is_sign_positive());", !numStdPred("is_sign_positive", numStdK[F]("NEG_INFINITY")))
	numStdAssert(t, "assert!(!(Float::ONE / Float::NEG_INFINITY).is_sign_positive());", !numStdPred("is_sign_positive", (numStdK[F]("ONE")/numStdK[F]("NEG_INFINITY"))))
	numStdAssert(t, "assert!(Float::NAN.is_sign_positive());", numStdPred("is_sign_positive", numStdK[F]("NAN")))
	numStdAssert(t, "assert!(!(-Float::NAN).is_sign_positive());", !numStdPred("is_sign_positive", (-numStdK[F]("NAN"))))
}

func numStdFloats_is_sign_negative[F float32 | float64](t *testing.T) {
	numStdAssert(t, "assert!(!Float::INFINITY.is_sign_negative());", !numStdPred("is_sign_negative", numStdK[F]("INFINITY")))
	numStdAssert(t, "assert!(!Float::ONE.is_sign_negative());", !numStdPred("is_sign_negative", numStdK[F]("ONE")))
	numStdAssert(t, "assert!(!Float::ZERO.is_sign_negative());", !numStdPred("is_sign_negative", numStdK[F]("ZERO")))
	numStdAssert(t, "assert!((-Float::ZERO).is_sign_negative());", numStdPred("is_sign_negative", (-numStdK[F]("ZERO"))))
	numStdAssert(t, "assert!((-Float::ONE).is_sign_negative());", numStdPred("is_sign_negative", (-numStdK[F]("ONE"))))
	numStdAssert(t, "assert!(Float::NEG_INFINITY.is_sign_negative());", numStdPred("is_sign_negative", numStdK[F]("NEG_INFINITY")))
	numStdAssert(t, "assert!((Float::ONE / Float::NEG_INFINITY).is_sign_negative());", numStdPred("is_sign_negative", (numStdK[F]("ONE")/numStdK[F]("NEG_INFINITY"))))
	numStdAssert(t, "assert!(!Float::NAN.is_sign_negative());", !numStdPred("is_sign_negative", numStdK[F]("NAN")))
	numStdAssert(t, "assert!((-Float::NAN).is_sign_negative());", numStdPred("is_sign_negative", (-numStdK[F]("NAN"))))
}

func numStdFloats_clamp_min_greater_than_max[F float32 | float64](t *testing.T) {
	numStdShouldPanic(t, func() { _ = numStdCall("clamp", numStdK[F]("ONE"), F(3.0), F(1.0)) })
}

func numStdFloats_clamp_min_is_nan[F float32 | float64](t *testing.T) {
	numStdShouldPanic(t, func() { _ = numStdCall("clamp", numStdK[F]("ONE"), numStdK[F]("NAN"), F(1.0)) })
}

func numStdFloats_clamp_max_is_nan[F float32 | float64](t *testing.T) {
	numStdShouldPanic(t, func() { _ = numStdCall("clamp", numStdK[F]("ONE"), F(3.0), numStdK[F]("NAN")) })
}

func numStdFloats_total_cmp[F float32 | float64](t *testing.T) {
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-q_nan()), Ordering::Equal));", numStdTotalCmp(-numStdQNaN[F](), -numStdQNaN[F]()) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::INFINITY, &-Float::INFINITY), Ordering::Equal));", numStdTotalCmp(-numStdK[F]("INFINITY"), -numStdK[F]("INFINITY")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX, &-Float::MAX), Ordering::Equal));", numStdTotalCmp(-numStdK[F]("MAX"), -numStdK[F]("MAX")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-2.5, &-2.5), Ordering::Equal));", numStdTotalCmp(-F(2.5), -F(2.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.0, &-1.0), Ordering::Equal));", numStdTotalCmp(-F(1.0), -F(1.0)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.5, &-1.5), Ordering::Equal));", numStdTotalCmp(-F(1.5), -F(1.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.5, &-0.5), Ordering::Equal));", numStdTotalCmp(-F(0.5), -F(0.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MIN_POSITIVE, &-Float::MIN_POSITIVE), Ordering::Equal));", numStdTotalCmp(-numStdK[F]("MIN_POSITIVE"), -numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX_SUBNORMAL, &-Float::MAX_SUBNORMAL), Ordering::Equal));", numStdTotalCmp(-numStdK[F]("MAX_SUBNORMAL"), -numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::TINY, &-Float::TINY), Ordering::Equal));", numStdTotalCmp(-numStdK[F]("TINY"), -numStdK[F]("TINY")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.0, &-0.0), Ordering::Equal));", numStdTotalCmp(-F(0.0), -F(0.0)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.0, &0.0), Ordering::Equal));", numStdTotalCmp(F(0.0), F(0.0)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::TINY, &Float::TINY), Ordering::Equal));", numStdTotalCmp(numStdK[F]("TINY"), numStdK[F]("TINY")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX_SUBNORMAL, &Float::MAX_SUBNORMAL), Ordering::Equal));", numStdTotalCmp(numStdK[F]("MAX_SUBNORMAL"), numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MIN_POSITIVE, &Float::MIN_POSITIVE), Ordering::Equal));", numStdTotalCmp(numStdK[F]("MIN_POSITIVE"), numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.5, &0.5), Ordering::Equal));", numStdTotalCmp(F(0.5), F(0.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.0, &1.0), Ordering::Equal));", numStdTotalCmp(F(1.0), F(1.0)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.5, &1.5), Ordering::Equal));", numStdTotalCmp(F(1.5), F(1.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&2.5, &2.5), Ordering::Equal));", numStdTotalCmp(F(2.5), F(2.5)) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX, &Float::MAX), Ordering::Equal));", numStdTotalCmp(numStdK[F]("MAX"), numStdK[F]("MAX")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::INFINITY, &Float::INFINITY), Ordering::Equal));", numStdTotalCmp(numStdK[F]("INFINITY"), numStdK[F]("INFINITY")) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&q_nan(), &q_nan()), Ordering::Equal));", numStdTotalCmp(numStdQNaN[F](), numStdQNaN[F]()) == rs.Ordering_Equal)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::INFINITY, &-Float::MAX), Ordering::Less));", numStdTotalCmp(-numStdK[F]("INFINITY"), -numStdK[F]("MAX")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX, &-2.5), Ordering::Less));", numStdTotalCmp(-numStdK[F]("MAX"), -F(2.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-2.5, &-1.5), Ordering::Less));", numStdTotalCmp(-F(2.5), -F(1.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.5, &-1.0), Ordering::Less));", numStdTotalCmp(-F(1.5), -F(1.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.0, &-0.5), Ordering::Less));", numStdTotalCmp(-F(1.0), -F(0.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.5, &-Float::MIN_POSITIVE), Ordering::Less));", numStdTotalCmp(-F(0.5), -numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MIN_POSITIVE, &-Float::MAX_SUBNORMAL), Ordering::Less));", numStdTotalCmp(-numStdK[F]("MIN_POSITIVE"), -numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX_SUBNORMAL, &-Float::TINY), Ordering::Less));", numStdTotalCmp(-numStdK[F]("MAX_SUBNORMAL"), -numStdK[F]("TINY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::TINY, &-0.0), Ordering::Less));", numStdTotalCmp(-numStdK[F]("TINY"), -F(0.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.0, &0.0), Ordering::Less));", numStdTotalCmp(-F(0.0), F(0.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.0, &Float::TINY), Ordering::Less));", numStdTotalCmp(F(0.0), numStdK[F]("TINY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::TINY, &Float::MAX_SUBNORMAL), Ordering::Less));", numStdTotalCmp(numStdK[F]("TINY"), numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX_SUBNORMAL, &Float::MIN_POSITIVE), Ordering::Less));", numStdTotalCmp(numStdK[F]("MAX_SUBNORMAL"), numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MIN_POSITIVE, &0.5), Ordering::Less));", numStdTotalCmp(numStdK[F]("MIN_POSITIVE"), F(0.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.5, &1.0), Ordering::Less));", numStdTotalCmp(F(0.5), F(1.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.0, &1.5), Ordering::Less));", numStdTotalCmp(F(1.0), F(1.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.5, &2.5), Ordering::Less));", numStdTotalCmp(F(1.5), F(2.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&2.5, &Float::MAX), Ordering::Less));", numStdTotalCmp(F(2.5), numStdK[F]("MAX")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX, &Float::INFINITY), Ordering::Less));", numStdTotalCmp(numStdK[F]("MAX"), numStdK[F]("INFINITY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX, &-Float::INFINITY), Ordering::Greater));", numStdTotalCmp(-numStdK[F]("MAX"), -numStdK[F]("INFINITY")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-2.5, &-Float::MAX), Ordering::Greater));", numStdTotalCmp(-F(2.5), -numStdK[F]("MAX")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.5, &-2.5), Ordering::Greater));", numStdTotalCmp(-F(1.5), -F(2.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-1.0, &-1.5), Ordering::Greater));", numStdTotalCmp(-F(1.0), -F(1.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.5, &-1.0), Ordering::Greater));", numStdTotalCmp(-F(0.5), -F(1.0)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MIN_POSITIVE, &-0.5), Ordering::Greater));", numStdTotalCmp(-numStdK[F]("MIN_POSITIVE"), -F(0.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::MAX_SUBNORMAL, &-Float::MIN_POSITIVE), Ordering::Greater));", numStdTotalCmp(-numStdK[F]("MAX_SUBNORMAL"), -numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-Float::TINY, &-Float::MAX_SUBNORMAL), Ordering::Greater));", numStdTotalCmp(-numStdK[F]("TINY"), -numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-0.0, &-Float::TINY), Ordering::Greater));", numStdTotalCmp(-F(0.0), -numStdK[F]("TINY")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.0, &-0.0), Ordering::Greater));", numStdTotalCmp(F(0.0), -F(0.0)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::TINY, &0.0), Ordering::Greater));", numStdTotalCmp(numStdK[F]("TINY"), F(0.0)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX_SUBNORMAL, &Float::TINY), Ordering::Greater));", numStdTotalCmp(numStdK[F]("MAX_SUBNORMAL"), numStdK[F]("TINY")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MIN_POSITIVE, &Float::MAX_SUBNORMAL), Ordering::Greater));", numStdTotalCmp(numStdK[F]("MIN_POSITIVE"), numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&0.5, &Float::MIN_POSITIVE), Ordering::Greater));", numStdTotalCmp(F(0.5), numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.0, &0.5), Ordering::Greater));", numStdTotalCmp(F(1.0), F(0.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&1.5, &1.0), Ordering::Greater));", numStdTotalCmp(F(1.5), F(1.0)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&2.5, &1.5), Ordering::Greater));", numStdTotalCmp(F(2.5), F(1.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::MAX, &2.5), Ordering::Greater));", numStdTotalCmp(numStdK[F]("MAX"), F(2.5)) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&Float::INFINITY, &Float::MAX), Ordering::Greater));", numStdTotalCmp(numStdK[F]("INFINITY"), numStdK[F]("MAX")) == rs.Ordering_Greater)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-Float::INFINITY), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -numStdK[F]("INFINITY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-Float::MAX), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -numStdK[F]("MAX")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-2.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -F(2.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-1.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -F(1.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-1.0), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -F(1.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-0.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -F(0.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-Float::MIN_POSITIVE), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-Float::MAX_SUBNORMAL), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-Float::TINY), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -numStdK[F]("TINY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &-0.0), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), -F(0.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &0.0), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), F(0.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &Float::TINY), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), numStdK[F]("TINY")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &Float::MAX_SUBNORMAL), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), numStdK[F]("MAX_SUBNORMAL")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &Float::MIN_POSITIVE), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), numStdK[F]("MIN_POSITIVE")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &0.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), F(0.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &1.0), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), F(1.0)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &1.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), F(1.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &2.5), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), F(2.5)) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &Float::MAX), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), numStdK[F]("MAX")) == rs.Ordering_Less)
	numStdAssert(t, "assert!(matches!(Float::total_cmp(&-q_nan(), &Float::INFINITY), Ordering::Less));", numStdTotalCmp(-numStdQNaN[F](), numStdK[F]("INFINITY")) == rs.Ordering_Less)
}

func numStdFloats_recip[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	max := numStdK[F]("MAX")
	_ = max
	numStdBiteq(t, "assert_biteq!(flt(1.0).recip(), 1.0);", numStdCall("recip", F(1.0)), F(1.0))
	numStdBiteq(t, "assert_biteq!(flt(2.0).recip(), 0.5);", numStdCall("recip", F(2.0)), F(0.5))
	numStdBiteq(t, "assert_biteq!(flt(-0.4).recip(), -2.5);", numStdCall("recip", -F(0.4)), -F(2.5))
	numStdBiteq(t, "assert_biteq!(flt(0.0).recip(), inf);", numStdCall("recip", F(0.0)), inf)
	numStdAssert(t, "assert!(nan.recip().is_nan());", numStdPred("is_nan", numStdCall("recip", nan)))
	numStdBiteq(t, "assert_biteq!(inf.recip(), 0.0);", numStdCall("recip", inf), F(0.0))
	numStdBiteq(t, "assert_biteq!(neg_inf.recip(), -0.0);", numStdCall("recip", neg_inf), -F(0.0))
	numStdBiteq(t, "assert_biteq!(max.recip(), Float::MAX_RECIP);", numStdCall("recip", max), numStdK[F]("MAX_RECIP"))
}

func numStdFloats_powi[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	numStdApprox(t, "assert_approx_eq!(Float::ONE.powi(1), Float::ONE);", numStdPowi(numStdK[F]("ONE"), 1), numStdK[F]("ONE"), numStdK[F]("APPROX"))
	numStdApprox(t, "assert_approx_eq!(flt(-3.1).powi(2), 9.6100000000000005506706202140776519387, Float::POWI_APPROX);", numStdPowi(-F(3.1), 2), F(9.6100000000000005506706202140776519387), numStdK[F]("POWI_APPROX"))
	numStdApprox(t, "assert_approx_eq!(flt(5.9).powi(-2), 0.028727377190462507313100483690639638451);", numStdPowi(F(5.9), -2), F(0.028727377190462507313100483690639638451), numStdK[F]("APPROX"))
	numStdBiteq(t, "assert_biteq!(flt(8.3).powi(0), Float::ONE);", numStdPowi(F(8.3), 0), numStdK[F]("ONE"))
	numStdAssert(t, "assert!(nan.powi(2).is_nan());", numStdPred("is_nan", numStdPowi(nan, 2)))
	numStdBiteq(t, "assert_biteq!(inf.powi(3), inf);", numStdPowi(inf, 3), inf)
	numStdBiteq(t, "assert_biteq!(neg_inf.powi(2), inf);", numStdPowi(neg_inf, 2), inf)
}

func numStdFloats_to_degrees[F float32 | float64](t *testing.T) {
	pi := numStdK[F]("PI")
	_ = pi
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	numStdBiteq(t, "assert_biteq!(flt(0.0).to_degrees(), 0.0);", numStdCall("to_degrees", F(0.0)), F(0.0))
	numStdApprox(t, "assert_approx_eq!(flt(-5.8).to_degrees(), -332.31552117587745090765431723855668471);", numStdCall("to_degrees", -F(5.8)), -F(332.31552117587745090765431723855668471), numStdK[F]("APPROX"))
	numStdApprox(t, "assert_approx_eq!(pi.to_degrees(), 180.0, Float::PI_TO_DEGREES_APPROX);", numStdCall("to_degrees", pi), F(180.0), numStdK[F]("PI_TO_DEGREES_APPROX"))
	numStdAssert(t, "assert!(nan.to_degrees().is_nan());", numStdPred("is_nan", numStdCall("to_degrees", nan)))
	numStdBiteq(t, "assert_biteq!(inf.to_degrees(), inf);", numStdCall("to_degrees", inf), inf)
	numStdBiteq(t, "assert_biteq!(neg_inf.to_degrees(), neg_inf);", numStdCall("to_degrees", neg_inf), neg_inf)
	numStdBiteq(t, "assert_biteq!(flt(1.0).to_degrees(), 57.2957795130823208767981548141051703);", numStdCall("to_degrees", F(1.0)), F(57.2957795130823208767981548141051703))
}

func numStdFloats_to_radians[F float32 | float64](t *testing.T) {
	pi := numStdK[F]("PI")
	_ = pi
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	numStdBiteq(t, "assert_biteq!(flt(0.0).to_radians(), 0.0);", numStdCall("to_radians", F(0.0)), F(0.0))
	numStdApprox(t, "assert_approx_eq!(flt(154.6).to_radians(), 2.6982790235832334267135442069489767804);", numStdCall("to_radians", F(154.6)), F(2.6982790235832334267135442069489767804), numStdK[F]("APPROX"))
	numStdApprox(t, "assert_approx_eq!(flt(-332.31).to_radians(), -5.7999036373023566567593094812182763013);", numStdCall("to_radians", -F(332.31)), -F(5.7999036373023566567593094812182763013), numStdK[F]("APPROX"))
	numStdApprox(t, "assert_approx_eq!(flt(180.0).to_radians(), pi, Float::_180_TO_RADIANS_APPROX);", numStdCall("to_radians", F(180.0)), pi, numStdK[F]("_180_TO_RADIANS_APPROX"))
	numStdAssert(t, "assert!(nan.to_radians().is_nan());", numStdPred("is_nan", numStdCall("to_radians", nan)))
	numStdBiteq(t, "assert_biteq!(inf.to_radians(), inf);", numStdCall("to_radians", inf), inf)
	numStdBiteq(t, "assert_biteq!(neg_inf.to_radians(), neg_inf);", numStdCall("to_radians", neg_inf), neg_inf)
}

func numStdFloats_mul_add[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	numStdBiteq(t, "assert_biteq!(flt(12.3).mul_add(flt(4.5), flt(6.7)), Float::MUL_ADD_RESULT);", numStdCall("mul_add", F(12.3), F(4.5), F(6.7)), numStdK[F]("MUL_ADD_RESULT"))
	numStdBiteq(t, "assert_biteq!((flt(-12.3)).mul_add(flt(-4.5), flt(-6.7)), Float::NEG_MUL_ADD_RESULT);", numStdCall("mul_add", (-F(12.3)), -F(4.5), -F(6.7)), numStdK[F]("NEG_MUL_ADD_RESULT"))
	numStdBiteq(t, "assert_biteq!(flt(0.0).mul_add(8.9, 1.2), 1.2);", numStdCall("mul_add", F(0.0), F(8.9), F(1.2)), F(1.2))
	numStdBiteq(t, "assert_biteq!(flt(3.4).mul_add(-0.0, 5.6), 5.6);", numStdCall("mul_add", F(3.4), -F(0.0), F(5.6)), F(5.6))
	numStdAssert(t, "assert!(nan.mul_add(7.8, 9.0).is_nan());", numStdPred("is_nan", numStdCall("mul_add", nan, F(7.8), F(9.0))))
	numStdBiteq(t, "assert_biteq!(inf.mul_add(7.8, 9.0), inf);", numStdCall("mul_add", inf, F(7.8), F(9.0)), inf)
	numStdBiteq(t, "assert_biteq!(neg_inf.mul_add(7.8, 9.0), neg_inf);", numStdCall("mul_add", neg_inf, F(7.8), F(9.0)), neg_inf)
	numStdBiteq(t, "assert_biteq!(flt(8.9).mul_add(inf, 3.2), inf);", numStdCall("mul_add", F(8.9), inf, F(3.2)), inf)
	numStdBiteq(t, "assert_biteq!((flt(-3.2)).mul_add(2.4, neg_inf), neg_inf);", numStdCall("mul_add", (-F(3.2)), F(2.4), neg_inf), neg_inf)
}

func numStdFloats_is_nan[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	pos := F(5.3)
	_ = pos
	neg := -F(10.732)
	_ = neg
	numStdAssert(t, "assert!(nan.is_nan());", numStdPred("is_nan", nan))
	numStdAssert(t, "assert!(!Float::ZERO.is_nan());", !numStdPred("is_nan", numStdK[F]("ZERO")))
	numStdAssert(t, "assert!(!pos.is_nan());", !numStdPred("is_nan", pos))
	numStdAssert(t, "assert!(!neg.is_nan());", !numStdPred("is_nan", neg))
	numStdAssert(t, "assert!(!inf.is_nan());", !numStdPred("is_nan", inf))
	numStdAssert(t, "assert!(!neg_inf.is_nan());", !numStdPred("is_nan", neg_inf))
}

func numStdFloats_is_infinite[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	pos := F(42.8)
	_ = pos
	neg := -F(109.2)
	_ = neg
	numStdAssert(t, "assert!(!nan.is_infinite());", !numStdPred("is_infinite", nan))
	numStdAssert(t, "assert!(inf.is_infinite());", numStdPred("is_infinite", inf))
	numStdAssert(t, "assert!(neg_inf.is_infinite());", numStdPred("is_infinite", neg_inf))
	numStdAssert(t, "assert!(!Float::ZERO.is_infinite());", !numStdPred("is_infinite", numStdK[F]("ZERO")))
	numStdAssert(t, "assert!(!pos.is_infinite());", !numStdPred("is_infinite", pos))
	numStdAssert(t, "assert!(!neg.is_infinite());", !numStdPred("is_infinite", neg))
}

func numStdFloats_is_finite[F float32 | float64](t *testing.T) {
	nan := numStdK[F]("NAN")
	_ = nan
	inf := numStdK[F]("INFINITY")
	_ = inf
	neg_inf := numStdK[F]("NEG_INFINITY")
	_ = neg_inf
	pos := F(42.8)
	_ = pos
	neg := -F(109.2)
	_ = neg
	numStdAssert(t, "assert!(!nan.is_finite());", !numStdPred("is_finite", nan))
	numStdAssert(t, "assert!(!inf.is_finite());", !numStdPred("is_finite", inf))
	numStdAssert(t, "assert!(!neg_inf.is_finite());", !numStdPred("is_finite", neg_inf))
	numStdAssert(t, "assert!(Float::ZERO.is_finite());", numStdPred("is_finite", numStdK[F]("ZERO")))
	numStdAssert(t, "assert!(pos.is_finite());", numStdPred("is_finite", pos))
	numStdAssert(t, "assert!(neg.is_finite());", numStdPred("is_finite", neg))
}

func numStdFloats_sqrt_domain[F float32 | float64](t *testing.T) {
	numStdAssert(t, "assert!(Float::NAN.sqrt().is_nan());", numStdPred("is_nan", numStdCall("sqrt", numStdK[F]("NAN"))))
	numStdAssert(t, "assert!(Float::NEG_INFINITY.sqrt().is_nan());", numStdPred("is_nan", numStdCall("sqrt", numStdK[F]("NEG_INFINITY"))))
	numStdAssert(t, "assert!((-Float::ONE).sqrt().is_nan());", numStdPred("is_nan", numStdCall("sqrt", (-numStdK[F]("ONE")))))
	numStdBiteq(t, "assert_biteq!((-Float::ZERO).sqrt(), -Float::ZERO);", numStdCall("sqrt", (-numStdK[F]("ZERO"))), -numStdK[F]("ZERO"))
	numStdBiteq(t, "assert_biteq!(Float::ZERO.sqrt(), Float::ZERO);", numStdCall("sqrt", numStdK[F]("ZERO")), numStdK[F]("ZERO"))
	numStdBiteq(t, "assert_biteq!(Float::ONE.sqrt(), Float::ONE);", numStdCall("sqrt", numStdK[F]("ONE")), numStdK[F]("ONE"))
	numStdBiteq(t, "assert_biteq!(Float::INFINITY.sqrt(), Float::INFINITY);", numStdCall("sqrt", numStdK[F]("INFINITY")), numStdK[F]("INFINITY"))
}

func TestNumFloats_min(t *testing.T) {
	t.Run("f32", numStdFloats_min[float32])
	t.Run("f64", numStdFloats_min[float64])
}

func TestNumFloats_max(t *testing.T) {
	t.Run("f32", numStdFloats_max[float32])
	t.Run("f64", numStdFloats_max[float64])
}

func TestNumFloats_abs(t *testing.T) {
	t.Run("f32", numStdFloats_abs[float32])
	t.Run("f64", numStdFloats_abs[float64])
}

func TestNumFloats_copysign(t *testing.T) {
	t.Run("f32", numStdFloats_copysign[float32])
	t.Run("f64", numStdFloats_copysign[float64])
}

func TestNumFloats_rem_euclid(t *testing.T) {
	t.Run("f32", numStdFloats_rem_euclid[float32])
	t.Run("f64", numStdFloats_rem_euclid[float64])
}

func TestNumFloats_div_euclid(t *testing.T) {
	t.Run("f32", numStdFloats_div_euclid[float32])
	t.Run("f64", numStdFloats_div_euclid[float64])
}

func TestNumFloats_floor(t *testing.T) {
	t.Run("f32", numStdFloats_floor[float32])
	t.Run("f64", numStdFloats_floor[float64])
}

func TestNumFloats_ceil(t *testing.T) {
	t.Run("f32", numStdFloats_ceil[float32])
	t.Run("f64", numStdFloats_ceil[float64])
}

func TestNumFloats_round(t *testing.T) {
	t.Run("f32", numStdFloats_round[float32])
	t.Run("f64", numStdFloats_round[float64])
}

func TestNumFloats_round_ties_even(t *testing.T) {
	t.Run("f32", numStdFloats_round_ties_even[float32])
	t.Run("f64", numStdFloats_round_ties_even[float64])
}

func TestNumFloats_trunc(t *testing.T) {
	t.Run("f32", numStdFloats_trunc[float32])
	t.Run("f64", numStdFloats_trunc[float64])
}

func TestNumFloats_fract(t *testing.T) {
	t.Run("f32", numStdFloats_fract[float32])
	t.Run("f64", numStdFloats_fract[float64])
}

func TestNumFloats_signum(t *testing.T) {
	t.Run("f32", numStdFloats_signum[float32])
	t.Run("f64", numStdFloats_signum[float64])
}

func TestNumFloats_is_sign_positive(t *testing.T) {
	t.Run("f32", numStdFloats_is_sign_positive[float32])
	t.Run("f64", numStdFloats_is_sign_positive[float64])
}

func TestNumFloats_is_sign_negative(t *testing.T) {
	t.Run("f32", numStdFloats_is_sign_negative[float32])
	t.Run("f64", numStdFloats_is_sign_negative[float64])
}

func TestNumFloats_clamp_min_greater_than_max(t *testing.T) {
	t.Run("f32", numStdFloats_clamp_min_greater_than_max[float32])
	t.Run("f64", numStdFloats_clamp_min_greater_than_max[float64])
}

func TestNumFloats_clamp_min_is_nan(t *testing.T) {
	t.Run("f32", numStdFloats_clamp_min_is_nan[float32])
	t.Run("f64", numStdFloats_clamp_min_is_nan[float64])
}

func TestNumFloats_clamp_max_is_nan(t *testing.T) {
	t.Run("f32", numStdFloats_clamp_max_is_nan[float32])
	t.Run("f64", numStdFloats_clamp_max_is_nan[float64])
}

func TestNumFloats_total_cmp(t *testing.T) {
	t.Run("f32", numStdFloats_total_cmp[float32])
	t.Run("f64", numStdFloats_total_cmp[float64])
}

func TestNumFloats_recip(t *testing.T) {
	t.Run("f32", numStdFloats_recip[float32])
	t.Run("f64", numStdFloats_recip[float64])
}

func TestNumFloats_powi(t *testing.T) {
	t.Run("f32", numStdFloats_powi[float32])
	t.Run("f64", numStdFloats_powi[float64])
}

func TestNumFloats_to_degrees(t *testing.T) {
	t.Run("f32", numStdFloats_to_degrees[float32])
	t.Run("f64", numStdFloats_to_degrees[float64])
}

func TestNumFloats_to_radians(t *testing.T) {
	t.Run("f32", numStdFloats_to_radians[float32])
	t.Run("f64", numStdFloats_to_radians[float64])
}

func TestNumFloats_mul_add(t *testing.T) {
	t.Run("f32", numStdFloats_mul_add[float32])
	t.Run("f64", numStdFloats_mul_add[float64])
}

func TestNumFloats_is_nan(t *testing.T) {
	t.Run("f32", numStdFloats_is_nan[float32])
	t.Run("f64", numStdFloats_is_nan[float64])
}

func TestNumFloats_is_infinite(t *testing.T) {
	t.Run("f32", numStdFloats_is_infinite[float32])
	t.Run("f64", numStdFloats_is_infinite[float64])
}

func TestNumFloats_is_finite(t *testing.T) {
	t.Run("f32", numStdFloats_is_finite[float32])
	t.Run("f64", numStdFloats_is_finite[float64])
}

func TestNumFloats_sqrt_domain(t *testing.T) {
	t.Run("f32", numStdFloats_sqrt_domain[float32])
	t.Run("f64", numStdFloats_sqrt_domain[float64])
}
