// Port of bytesize 2.7.0 src/lib.rs

package bytesize

import (
	"strconv"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// `ByteSize` is a semantic wrapper for byte count representations.
//
// Features:
//
//   - Pre-defined constants for various size units (e.g., B, KB, KiB, MB, MiB, ... EB, EiB).
//   - `ByteSize` type which presents size units convertible to different size units.
//   - Arithmetic operations for `ByteSize`.
//   - `FromStr` impl for `ByteSize`, allowing for parsing string size representations like "1.5KiB"
//     and "521TiB".
//   - Serde support for binary and human-readable deserializers like JSON.

// Number of bytes in 1 kilobyte.
const KB uint64 = 1_000

// Number of bytes in 1 megabyte.
const MB uint64 = 1_000_000

// Number of bytes in 1 gigabyte.
const GB uint64 = 1_000_000_000

// Number of bytes in 1 terabyte.
const TB uint64 = 1_000_000_000_000

// Number of bytes in 1 petabyte.
const PB uint64 = 1_000_000_000_000_000

// Number of bytes in 1 exabyte.
const EB uint64 = 1_000_000_000_000_000_000

// Number of bytes in 1 kibibyte.
const KIB uint64 = 1_024

// Number of bytes in 1 mebibyte.
const MIB uint64 = 1_048_576

// Number of bytes in 1 gibibyte.
const GIB uint64 = 1_073_741_824

// Number of bytes in 1 tebibyte.
const TIB uint64 = 1_099_511_627_776

// Number of bytes in 1 pebibyte.
const PIB uint64 = 1_125_899_906_842_624

// Number of bytes in 1 exbibyte.
const EIB uint64 = 1_152_921_504_606_846_976

// IEC (binary) units.
//
// See <https://en.wikipedia.org/wiki/Kilobyte>.
const libUnitsIec = "KMGTPE"

// SI (decimal) units.
//
// See <https://en.wikipedia.org/wiki/Kilobyte>.
const libUnitsSi = "kMGTPE"

// `ln(1024) ~= 6.931`
const libLnKib float64 = 6.931_471_805_599_453

// `ln(1000) ~= 6.908`
const libLnKb float64 = 6.907_755_278_982_137

// Converts a quantity of kilobytes to bytes.
func Kb(size uint64) uint64 {
	return size * KB
}

// Converts a quantity of kibibytes to bytes.
func Kib(size uint64) uint64 {
	return size * KIB
}

// Converts a quantity of megabytes to bytes.
func Mb(size uint64) uint64 {
	return size * MB
}

// Converts a quantity of mebibytes to bytes.
func Mib(size uint64) uint64 {
	return size * MIB
}

// Converts a quantity of gigabytes to bytes.
func Gb(size uint64) uint64 {
	return size * GB
}

// Converts a quantity of gibibytes to bytes.
func Gib(size uint64) uint64 {
	return size * GIB
}

// Converts a quantity of terabytes to bytes.
func Tb(size uint64) uint64 {
	return size * TB
}

// Converts a quantity of tebibytes to bytes.
func Tib(size uint64) uint64 {
	return size * TIB
}

// Converts a quantity of petabytes to bytes.
func Pb(size uint64) uint64 {
	return size * PB
}

// Converts a quantity of pebibytes to bytes.
func Pib(size uint64) uint64 {
	return size * PIB
}

// Converts a quantity of exabytes to bytes.
func Eb(size uint64) uint64 {
	return size * EB
}

// Converts a quantity of exbibytes to bytes.
func Eib(size uint64) uint64 {
	return size * EIB
}

// Byte size representation.
//
// `struct ByteSize(pub u64)` is a newtype, so it is a defined type over
// uint64: `ByteSize(5)` builds one, `uint64(size)` is `size.0`, and Go's own
// `+`, `-`, `*`, `+=`, `==` and `<` are the crate's arithmetic and
// comparison operators (see doc.go for overflow). The zero value is
// `ByteSize::default()`.
type ByteSize uint64

var _ = rs.RegisterNewtype[ByteSize]("ByteSize")

// Constructs a byte size wrapper from a quantity of bytes.
func ByteSizeB(size uint64) ByteSize {
	return ByteSize(size)
}

// Constructs a byte size wrapper from a quantity of kilobytes.
func ByteSizeKb(size uint64) ByteSize {
	return ByteSize(size * KB)
}

// Constructs a byte size wrapper from a quantity of kibibytes.
func ByteSizeKib(size uint64) ByteSize {
	return ByteSize(size * KIB)
}

// Constructs a byte size wrapper from a quantity of megabytes.
func ByteSizeMb(size uint64) ByteSize {
	return ByteSize(size * MB)
}

// Constructs a byte size wrapper from a quantity of mebibytes.
func ByteSizeMib(size uint64) ByteSize {
	return ByteSize(size * MIB)
}

// Constructs a byte size wrapper from a quantity of gigabytes.
func ByteSizeGb(size uint64) ByteSize {
	return ByteSize(size * GB)
}

// Constructs a byte size wrapper from a quantity of gibibytes.
func ByteSizeGib(size uint64) ByteSize {
	return ByteSize(size * GIB)
}

// Constructs a byte size wrapper from a quantity of terabytes.
func ByteSizeTb(size uint64) ByteSize {
	return ByteSize(size * TB)
}

// Constructs a byte size wrapper from a quantity of tebibytes.
func ByteSizeTib(size uint64) ByteSize {
	return ByteSize(size * TIB)
}

// Constructs a byte size wrapper from a quantity of petabytes.
func ByteSizePb(size uint64) ByteSize {
	return ByteSize(size * PB)
}

// Constructs a byte size wrapper from a quantity of pebibytes.
func ByteSizePib(size uint64) ByteSize {
	return ByteSize(size * PIB)
}

// Constructs a byte size wrapper from a quantity of exabytes.
func ByteSizeEb(size uint64) ByteSize {
	return ByteSize(size * EB)
}

// Constructs a byte size wrapper from a quantity of exbibytes.
func ByteSizeEib(size uint64) ByteSize {
	return ByteSize(size * EIB)
}

// Returns byte count.
func (self ByteSize) AsU64() uint64 {
	return uint64(self)
}

// Returns byte count as kilobytes.
func (self ByteSize) AsKb() float64 {
	return float64(self) / float64(KB)
}

// Returns byte count as kibibytes.
func (self ByteSize) AsKib() float64 {
	return float64(self) / float64(KIB)
}

// Returns byte count as megabytes.
func (self ByteSize) AsMb() float64 {
	return float64(self) / float64(MB)
}

// Returns byte count as mebibytes.
func (self ByteSize) AsMib() float64 {
	return float64(self) / float64(MIB)
}

// Returns byte count as gigabytes.
func (self ByteSize) AsGb() float64 {
	return float64(self) / float64(GB)
}

// Returns byte count as gibibytes.
func (self ByteSize) AsGib() float64 {
	return float64(self) / float64(GIB)
}

// Returns byte count as terabytes.
func (self ByteSize) AsTb() float64 {
	return float64(self) / float64(TB)
}

// Returns byte count as tebibytes.
func (self ByteSize) AsTib() float64 {
	return float64(self) / float64(TIB)
}

// Returns byte count as petabytes.
func (self ByteSize) AsPb() float64 {
	return float64(self) / float64(PB)
}

// Returns byte count as pebibytes.
func (self ByteSize) AsPib() float64 {
	return float64(self) / float64(PIB)
}

// Returns byte count as exabytes.
func (self ByteSize) AsEb() float64 {
	return float64(self) / float64(EB)
}

// Returns byte count as exbibytes.
func (self ByteSize) AsEib() float64 {
	return float64(self) / float64(EIB)
}

// Returns a formatting display wrapper.
func (self ByteSize) Display() *Display {
	return &Display{
		byteSize: self,
		format:   displayFormat_Iec,
	}
}

// String is the `Display` output with no formatting options:
// `self.display().to_string()`, such as `1.8 GiB`.
func (self ByteSize) String() string {
	return self.Display().String()
}

// FmtDisplay is `impl fmt::Display for ByteSize`. Without a width it is the
// IEC display at the formatter's precision (one decimal by default). With a
// width, the text at that precision is padded with the fill character, on
// the right unless the alignment says otherwise, and never truncated.
func (self ByteSize) FmtDisplay(f *rs.Formatter) error {
	display := self.Display()

	width, ok := f.Width().Get()
	if !ok {
		// allocation-free fast path for when no formatting options are specified
		return display.FmtDisplay(f)
	} else {
		// `display.to_string()` renders at the default precision, and `f.pad`
		// reinterprets the formatter's precision as a *maximum* width. Together
		// they drop the requested precision and truncate the value mid-unit
		// (e.g. `{:>12.5}` rendered "1.86328 GiB" as "1.9 G"). Render with the
		// requested precision first, then apply only the width, fill, and align.
		var content string
		if precision, ok := f.Precision().Get(); ok {
			content = display.render(precision)
		} else {
			content = display.String()
		}

		padding := 0
		if chars := utf8.RuneCountInString(content); width > chars {
			padding = width - chars
		}
		if padding == 0 {
			return f.WriteStr(content)
		}

		var left, right int
		switch align := f.Align(); {
		case align == rs.Some(rs.Alignment_Right):
			left, right = padding, 0
		case align == rs.Some(rs.Alignment_Center):
			left, right = padding/2, padding-padding/2
		default:
			// Some(fmt::Alignment::Left) | None
			left, right = 0, padding
		}

		fill := string(f.Fill())
		for range left {
			if err := f.WriteStr(fill); err != nil {
				return err
			}
		}
		if err := f.WriteStr(content); err != nil {
			return err
		}
		for range right {
			if err := f.WriteStr(fill); err != nil {
				return err
			}
		}
		return nil
	}
}

// debugString is the `Debug` output: `1.0 KiB (1024 bytes)`.
func (self ByteSize) debugString() string {
	return self.String() + " (" + strconv.FormatUint(uint64(self), 10) + " bytes)"
}

// FmtDebug is `impl fmt::Debug for ByteSize`: the display, then the byte
// count. Formatting options do not apply.
func (self ByteSize) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(self.debugString())
}
