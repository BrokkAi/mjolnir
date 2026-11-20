// Port of core 1.99.1 src/fmt/float.rs

package rs

import "math"

// fmtFloatShouldUseExponential is
// GeneralFormat::already_rounded_value_should_use_exponential: it determines
// if a value should use exponential based on its magnitude, given the
// precondition that it will not be rounded any further before it is
// displayed. Rust compares in the float's own type, so an f32 is compared
// with the f32 nearest to each bound.
func fmtFloatShouldUseExponential(v fmtFloat) bool {
	if v.totalBits == 32 {
		abs := math.Float32frombits(uint32(v.bits) &^ (1 << 31))
		return (abs != 0.0 && abs < float32(1e-4)) || abs >= float32(1e+16)
	}
	abs := math.Float64frombits(v.bits &^ (1 << 63))
	return (abs != 0.0 && abs < 1e-4) || abs >= 1e+16
}

func fmtFloatToDecimalCommonExact(f *Formatter, num fmtFloat, sign fmtSign, precision uint16) error {
	formatted := fmtToExactFixedStr(num, sign, int(precision))
	return f.padFormattedParts(&formatted)
}

func fmtFloatToDecimalCommonShortest(f *Formatter, num fmtFloat, sign fmtSign, precision uint16) error {
	formatted := fmtToShortestStr(num, sign, int(precision))
	return f.padFormattedParts(&formatted)
}

// fmtFloatSign picks the sign rule from the `+` flag.
func fmtFloatSign(f *Formatter) fmtSign {
	if f.SignPlus() {
		return fmtSignMinusPlus
	}
	return fmtSignMinus
}

// fmtFloatToDecimalDisplay is float_to_decimal_display: `impl Display` for
// the floats.
func fmtFloatToDecimalDisplay(f *Formatter, num fmtFloat) error {
	sign := fmtFloatSign(f)

	if precision, ok := f.options.getPrecision(); ok {
		return fmtFloatToDecimalCommonExact(f, num, sign, precision)
	}
	minPrecision := uint16(0)
	return fmtFloatToDecimalCommonShortest(f, num, sign, minPrecision)
}

func fmtFloatToExponentialCommonExact(f *Formatter, num fmtFloat, sign fmtSign, precision uint16, upper bool) error {
	formatted := fmtToExactExpStr(num, sign, int(precision), upper)
	return f.padFormattedParts(&formatted)
}

func fmtFloatToExponentialCommonShortest(f *Formatter, num fmtFloat, sign fmtSign, upper bool) error {
	formatted := fmtToShortestExpStr(num, sign, 0, 0, upper)
	return f.padFormattedParts(&formatted)
}

// fmtFloatToExponentialCommon is float_to_exponential_common: the common code
// of floating point LowerExp and UpperExp.
func fmtFloatToExponentialCommon(f *Formatter, num fmtFloat, upper bool) error {
	sign := fmtFloatSign(f)

	if precision, ok := f.options.getPrecision(); ok {
		return fmtFloatToExponentialCommonExact(f, num, sign, precision, upper)
	}
	return fmtFloatToExponentialCommonShortest(f, num, sign, upper)
}

// fmtFloatToGeneralDebug is float_to_general_debug: `impl Debug` for the
// floats.
func fmtFloatToGeneralDebug(f *Formatter, num fmtFloat) error {
	sign := fmtFloatSign(f)

	if precision, ok := f.options.getPrecision(); ok {
		// this behavior of {:.PREC?} predates exponential formatting for {:?}
		return fmtFloatToDecimalCommonExact(f, num, sign, precision)
	}
	// since there is no precision, there will be no rounding
	if fmtFloatShouldUseExponential(num) {
		upper := false
		return fmtFloatToExponentialCommonShortest(f, num, sign, upper)
	}
	minPrecision := uint16(1)
	return fmtFloatToDecimalCommonShortest(f, num, sign, minPrecision)
}
