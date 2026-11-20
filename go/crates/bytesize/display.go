// Port of bytesize 2.7.0 src/display.rs

package bytesize

import (
	"math"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const displayBitsPerByte uint64 = 8

// Format / style to use when displaying a [`ByteSize`].
type displayFormat uint8

const (
	displayFormat_Iec displayFormat = iota
	displayFormat_IecShort
	displayFormat_Si
	displayFormat_SiShort
	displayFormat_IecBits
	displayFormat_SiBits
)

var _ = rs.RegisterEnum[displayFormat]("Format",
	rs.Const(displayFormat_Iec, "Iec"),
	rs.Const(displayFormat_IecShort, "IecShort"),
	rs.Const(displayFormat_Si, "Si"),
	rs.Const(displayFormat_SiShort, "SiShort"),
	rs.Const(displayFormat_IecBits, "IecBits"),
	rs.Const(displayFormat_SiBits, "SiBits"),
)

func (self displayFormat) unit() uint64 {
	switch self {
	case displayFormat_Iec, displayFormat_IecShort:
		return KIB
	case displayFormat_Si, displayFormat_SiShort:
		return KB
	case displayFormat_IecBits:
		return KIB
	case displayFormat_SiBits:
		return KB
	default:
		panic(rs.Unreachable(self))
	}
}

func (self displayFormat) unitBase() float64 {
	switch self {
	case displayFormat_Iec, displayFormat_IecShort:
		return libLnKib
	case displayFormat_Si, displayFormat_SiShort:
		return libLnKb
	case displayFormat_IecBits:
		return libLnKib
	case displayFormat_SiBits:
		return libLnKb
	default:
		panic(rs.Unreachable(self))
	}
}

func (self displayFormat) unitPrefixes() string {
	switch self {
	case displayFormat_Iec, displayFormat_IecShort, displayFormat_IecBits:
		return libUnitsIec
	case displayFormat_Si, displayFormat_SiShort, displayFormat_SiBits:
		return libUnitsSi
	default:
		panic(rs.Unreachable(self))
	}
}

func (self displayFormat) unitSeparator() string {
	switch self {
	case displayFormat_Iec, displayFormat_Si, displayFormat_IecBits, displayFormat_SiBits:
		return " "
	case displayFormat_IecShort, displayFormat_SiShort:
		return ""
	default:
		panic(rs.Unreachable(self))
	}
}

func (self displayFormat) unitSuffix() string {
	switch self {
	case displayFormat_Iec:
		return "iB"
	case displayFormat_Si:
		return "B"
	case displayFormat_IecShort, displayFormat_SiShort:
		return ""
	case displayFormat_IecBits:
		return "ib"
	case displayFormat_SiBits:
		return "b"
	default:
		panic(rs.Unreachable(self))
	}
}

func (self displayFormat) isBits() bool {
	return self == displayFormat_IecBits || self == displayFormat_SiBits
}

// Formatting display wrapper for [`ByteSize`].
//
// Supports various styles, see methods. By default, the [`iec()`](Self::iec()) style is used.
//
// # Examples
//
//	ByteSizeMib(1).Display().Iec().String()    // "1.0 MiB"
//	ByteSizeKb(42).Display().SiShort().String() // "42.0k"
//
// The style methods set the style and return the wrapper they were called on,
// as the Rust methods take the wrapper by value and return it.
type Display struct {
	byteSize ByteSize      `rs:"byte_size"`
	format   displayFormat `rs:"format"`
}

var _ = rs.RegisterStruct[Display]("Display")

// Format using IEC (binary) units.
//
// E.g., `11.8 MiB`.
func (self *Display) Iec() *Display {
	self.format = displayFormat_Iec
	return self
}

// Format using a short style and IEC (binary) units.
//
// E.g., `11.8M`.
//
// Designed to produce output compatible with `sort -h`.
func (self *Display) IecShort() *Display {
	self.format = displayFormat_IecShort
	return self
}

// Format using SI (decimal) units.
//
// E.g., `12.3 MB`.
func (self *Display) Si() *Display {
	self.format = displayFormat_Si
	return self
}

// Format using a short style and SI (decimal) units.
//
// E.g., `12.3M`.
func (self *Display) SiShort() *Display {
	self.format = displayFormat_SiShort
	return self
}

// Format as equivalent number of bits using IEC (binary) units.
//
// E.g., `12.3 Mib`.
func (self *Display) IecBits() *Display {
	self.format = displayFormat_IecBits
	return self
}

// Format as equivalent number of bits using SI (decimal) units.
//
// E.g., `12.3 Mb`.
func (self *Display) SiBits() *Display {
	self.format = displayFormat_SiBits
	return self
}

// String is the `Display` output with no formatting options: one decimal.
func (self *Display) String() string {
	return self.render(1)
}

// FmtDisplay is `impl fmt::Display for Display`. The formatter's precision
// sets the number of decimals (one by default); its width, fill and
// alignment do not apply.
func (self *Display) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.render(f.Precision().UnwrapOr(1)))
}

// render is the body of `impl fmt::Display for Display` with
// `f.precision().unwrap_or(1)` already taken.
func (self *Display) render(precision int) string {
	bytes := self.byteSize.AsU64()

	isBits := self.format.isBits()

	unit := self.format.unit()
	unitBase := self.format.unitBase()

	unitPrefixes := self.format.unitPrefixes()
	unitSeparator := self.format.unitSeparator()
	unitSuffix := self.format.unitSuffix()

	var threshold uint64
	if isBits {
		// unit.div_ceil(BITS_PER_BYTE)
		threshold = (unit + displayBitsPerByte - 1) / displayBitsPerByte
	} else {
		threshold = unit
	}

	if bytes < threshold {
		var value uint64
		if isBits {
			value = bytes * displayBitsPerByte
		} else {
			value = bytes
		}

		if isBits {
			// write!(f, "{value}{unit_separator}b")
			return strconv.FormatUint(value, 10) + unitSeparator + "b"
		} else {
			// write!(f, "{value}{unit_separator}B")
			return strconv.FormatUint(value, 10) + unitSeparator + "B"
		}
	} else {
		// bytes as f64 * if is_bits { BITS_PER_BYTE as f64 } else { 1.0 }
		multiplier := 1.0
		if isBits {
			multiplier = float64(displayBitsPerByte)
		}
		size := float64(bytes) * multiplier

		exp := displayIdealUnitStd(size, unit, unitBase)

		unitPrefix := string(rune(unitPrefixes[exp-1]))

		// write!(f, "{:.precision$}{unit_separator}{unit_prefix}{unit_suffix}",
		//     (size / unit.pow(exp as u32) as f64))
		return strconv.FormatFloat(size/float64(displayPowU64(unit, exp)), 'f', precision, 64) +
			unitSeparator + unitPrefix + unitSuffix
	}
}

// displayPowU64 is `u64::pow`. The exponents reached here keep the result in
// range: the largest size, u64::MAX bytes as bits, is below 1000^7.
func displayPowU64(base uint64, exp int) uint64 {
	result := uint64(1)
	for range exp {
		result *= base
	}
	return result
}

// displayPowi is `f64::powi`: repeated multiplication. The powers of 1000
// and 1024 used here are exact in an f64.
func displayPowi(base float64, exp int) float64 {
	result := 1.0
	for range exp {
		result *= base
	}
	return result
}

func displayIdealUnitNoStd(size float64, unit uint64) int {
	rs.Assert(size >= float64(unit), "only called when bytes >= unit")

	idealPrefix := 0
	idealSize := size

	for {
		idealPrefix += 1
		idealSize /= float64(unit)

		if idealSize < float64(unit) {
			break
		}
	}

	return idealPrefix
}

func displayIdealUnitStd(size float64, unit uint64, unitBase float64) int {
	rs.Assert(size >= float64(unit), "only called when bytes >= unit")

	// `ln()` is a fast approximation, but it's not precise enough to trust at power-of-`unit`
	// boundaries: `f64::ln` can round such that `size.ln() / unit_base` lands one exponent above
	// or below the correct value (see #142), which previously could underflow `exp - 1` and panic,
	// or silently pick the wrong unit prefix. Nudge the approximation to the exact boundary using
	// integer-exact `powi` checks, matching the (slower but exact) loop in `ideal_unit_no_std`.
	//
	// The two loops settle on the same exponent from any starting point, so
	// the last bit of Go's math.Log, which can differ from the C library's
	// log that Rust calls, does not change the result.
	unitF := float64(unit)
	// `as isize` of a value between 1 and 7 here: size is at least `unit`.
	exp := max(int(math.Log(size)/unitBase), 1)

	for exp > 1 && size/displayPowi(unitF, exp-1) < unitF {
		exp -= 1
	}
	for size/displayPowi(unitF, exp) >= unitF {
		exp += 1
	}

	return exp
}
