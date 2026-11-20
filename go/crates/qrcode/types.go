// Port of qrcode 0.14.1 src/types.rs

package qrcode

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

//------------------------------------------------------------------------------
//{{{ QrResult

// TypesQrError is qrcode::types::QrError: `QrError` encodes the error
// encountered when generating a QR code.
//
// A QrResult<T> (`Result<T, QrError>`) is `(T, error)`; the error is always a
// TypesQrError.
type TypesQrError uint8

const (
	// TypesQrError_DataTooLong: the data is too long to encode into a QR code
	// for the given version.
	TypesQrError_DataTooLong TypesQrError = iota

	// TypesQrError_InvalidVersion: the provided version / error correction
	// level combination is invalid.
	TypesQrError_InvalidVersion

	// TypesQrError_UnsupportedCharacterSet: some characters in the data cannot
	// be supported by the provided QR code version.
	TypesQrError_UnsupportedCharacterSet

	// TypesQrError_InvalidEciDesignator: the provided ECI designator is
	// invalid. A valid designator should be between 0 and 999999.
	TypesQrError_InvalidEciDesignator

	// TypesQrError_InvalidCharacter: a character not belonging to the
	// character set is found.
	TypesQrError_InvalidCharacter
)

var _ = rs.RegisterEnum[TypesQrError]("QrError",
	rs.Const(TypesQrError_DataTooLong, "DataTooLong"),
	rs.Const(TypesQrError_InvalidVersion, "InvalidVersion"),
	rs.Const(TypesQrError_UnsupportedCharacterSet, "UnsupportedCharacterSet"),
	rs.Const(TypesQrError_InvalidEciDesignator, "InvalidEciDesignator"),
	rs.Const(TypesQrError_InvalidCharacter, "InvalidCharacter"),
)

// String is the Display text of the error.
func (e TypesQrError) String() string {
	switch e {
	case TypesQrError_DataTooLong:
		return "data too long"
	case TypesQrError_InvalidVersion:
		return "invalid version"
	case TypesQrError_UnsupportedCharacterSet:
		return "unsupported character set"
	case TypesQrError_InvalidEciDesignator:
		return "invalid ECI designator"
	case TypesQrError_InvalidCharacter:
		return "invalid character"
	default:
		panic(rs.Unreachable(e))
	}
}

// FmtDisplay is `impl Display for QrError`. It writes with `write_str`, so
// width and precision do not apply.
func (e TypesQrError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.String())
}

// Error is `impl std::error::Error for QrError`.
func (e TypesQrError) Error() string { return e.String() }

// rustName is the variant's name, which is what the derived Debug prints.
func (e TypesQrError) rustName() string {
	switch e {
	case TypesQrError_DataTooLong:
		return "DataTooLong"
	case TypesQrError_InvalidVersion:
		return "InvalidVersion"
	case TypesQrError_UnsupportedCharacterSet:
		return "UnsupportedCharacterSet"
	case TypesQrError_InvalidEciDesignator:
		return "InvalidEciDesignator"
	case TypesQrError_InvalidCharacter:
		return "InvalidCharacter"
	default:
		panic(rs.Unreachable(e))
	}
}

//}}}
//------------------------------------------------------------------------------
//{{{ Color

// Color is qrcode::Color (qrcode::types::Color): the color of a module.
type Color uint8

const (
	// Color_Light: the module is light colored.
	Color_Light Color = iota
	// Color_Dark: the module is dark colored.
	Color_Dark
)

var _ = rs.RegisterEnum[Color]("Color",
	rs.Const(Color_Light, "Light"),
	rs.Const(Color_Dark, "Dark"),
)

// ColorSelect is Color::select: selects a value according to color of the
// module. Equivalent to `if self != Color::Light { dark } else { light }`.
//
//	ColorSelect(Color_Light, 1, 0) == 0
//	ColorSelect(Color_Dark, "black", "white") == "black"
func ColorSelect[T any](self Color, dark T, light T) T {
	switch self {
	case Color_Light:
		return light
	case Color_Dark:
		return dark
	default:
		panic(rs.Unreachable(self))
	}
}

// Not is `impl Not for Color`: `!color`.
func (c Color) Not() Color {
	switch c {
	case Color_Light:
		return Color_Dark
	case Color_Dark:
		return Color_Light
	default:
		panic(rs.Unreachable(c))
	}
}

//}}}
//------------------------------------------------------------------------------
//{{{ Error correction level

// EcLevel is qrcode::EcLevel (qrcode::types::EcLevel): the error correction
// level. It allows the original information be recovered even if parts of
// the code is damaged. It orders L < M < Q < H, as the derived Ord does.
type EcLevel uint8

const (
	// EcLevel_L: low error correction. Allows up to 7% of wrong blocks.
	EcLevel_L EcLevel = 0

	// EcLevel_M: medium error correction (default). Allows up to 15% of wrong
	// blocks.
	EcLevel_M EcLevel = 1

	// EcLevel_Q: "Quartile" error correction. Allows up to 25% of wrong
	// blocks.
	EcLevel_Q EcLevel = 2

	// EcLevel_H: high error correction. Allows up to 30% of wrong blocks.
	EcLevel_H EcLevel = 3
)

var _ = rs.RegisterEnum[EcLevel]("EcLevel",
	rs.Const(EcLevel_L, "L"),
	rs.Const(EcLevel_M, "M"),
	rs.Const(EcLevel_Q, "Q"),
	rs.Const(EcLevel_H, "H"),
)

//}}}
//------------------------------------------------------------------------------
//{{{ Version

// Version is qrcode::Version (qrcode::types::Version): in QR code
// terminology, `Version` means the size of the generated image. Larger
// version means the size of code is larger, and therefore can carry more
// information.
//
// The smallest version is `Version::Normal(1)` of size 21×21, and the largest
// is `Version::Normal(40)` of size 177×177.
//
// A value is a pointer to one of the variant structs: *Version_Normal or
// *Version_Micro. Compare two with Equal.
type Version interface {
	isVersion()

	// Width is Version::width: get the number of "modules" on each size of
	// the QR code, i.e. the width and height of the code.
	Width() int16

	// ModeBitsCount is Version::mode_bits_count: the number of bits needed
	// to encode the mode indicator.
	ModeBitsCount() int

	// IsMicro is Version::is_micro: checks whether is version refers to a
	// Micro QR code.
	IsMicro() bool

	// Equal is the derived PartialEq.
	Equal(other Version) bool
}

// Version_Normal is Version::Normal: a normal QR code version. The parameter
// should be between 1 and 40.
type Version_Normal struct{ F0 int16 }

// Version_Micro is Version::Micro: a Micro QR code version. The parameter
// should be between 1 and 4.
type Version_Micro struct{ F0 int16 }

var _ = rs.RegisterEnum[Version]("Version",
	rs.Variant[Version_Normal]("Normal", rs.ShapeTuple),
	rs.Variant[Version_Micro]("Micro", rs.ShapeTuple),
)

func (*Version_Normal) isVersion() {}
func (*Version_Micro) isVersion()  {}

func (v *Version_Normal) Width() int16 { return v.F0*4 + 17 }
func (v *Version_Micro) Width() int16  { return v.F0*2 + 9 }

// VersionFetch is Version::fetch: obtains an object from a hard-coded table.
//
// The table must be a 44×4 array. The outer array represents the content for
// each version. The first 40 entry corresponds to QR code versions 1 to 40,
// and the last 4 corresponds to Micro QR code version 1 to 4. The inner array
// represents the content in each error correction level, in the order
// [L, M, Q, H].
//
// # Errors
//
// If the entry compares equal to the default value of `T`, this method
// returns `Err(QrError::InvalidVersion)`.
func VersionFetch[T comparable](self Version, ecLevel EcLevel, table [][4]T) (T, error) {
	var zero T
	switch v := self.(type) {
	case *Version_Normal:
		if 1 <= v.F0 && v.F0 <= 40 {
			return table[asUsizeI16(v.F0-1)][ecLevel], nil
		}
	case *Version_Micro:
		if 1 <= v.F0 && v.F0 <= 4 {
			obj := table[asUsizeI16(v.F0+39)][ecLevel]
			if obj != zero {
				return obj, nil
			}
		}
	default:
		panic(rs.Unreachable(self))
	}
	return zero, TypesQrError_InvalidVersion
}

func (v *Version_Normal) ModeBitsCount() int { return 4 }

func (v *Version_Micro) ModeBitsCount() int { return asUsizeI16(v.F0 - 1) }

func (*Version_Normal) IsMicro() bool { return false }
func (*Version_Micro) IsMicro() bool  { return true }

func (v *Version_Normal) Equal(other Version) bool {
	o, ok := other.(*Version_Normal)
	return ok && v.F0 == o.F0
}

func (v *Version_Micro) Equal(other Version) bool {
	o, ok := other.(*Version_Micro)
	return ok && v.F0 == o.F0
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode indicator

// TypesMode is qrcode::types::Mode: the mode indicator, which specifies the
// character set of the encoded data.
type TypesMode uint8

const (
	// TypesMode_Numeric: the data contains only characters 0 to 9.
	TypesMode_Numeric TypesMode = iota

	// TypesMode_Alphanumeric: the data contains only uppercase letters (A–Z),
	// numbers (0–9) and a few punctuations marks (space, `$`, `%`, `*`, `+`,
	// `-`, `.`, `/`, `:`).
	TypesMode_Alphanumeric

	// TypesMode_Byte: the data contains arbitrary binary data.
	TypesMode_Byte

	// TypesMode_Kanji: the data contains Shift-JIS-encoded double-byte text.
	TypesMode_Kanji
)

var _ = rs.RegisterEnum[TypesMode]("Mode",
	rs.Const(TypesMode_Numeric, "Numeric"),
	rs.Const(TypesMode_Alphanumeric, "Alphanumeric"),
	rs.Const(TypesMode_Byte, "Byte"),
	rs.Const(TypesMode_Kanji, "Kanji"),
)

// LengthBitsCount is Mode::length_bits_count: computes the number of bits
// needed to encode the data length.
//
//	TypesMode_Numeric.LengthBitsCount(&Version_Normal{F0: 1}) == 10
//
// This method will return `Err(QrError::UnsupportedCharacterSet)` if the
// mode is not supported in the given version.
func (m TypesMode) LengthBitsCount(version Version) int {
	switch v := version.(type) {
	case *Version_Micro:
		a := asUsizeI16(v.F0)
		switch m {
		case TypesMode_Numeric:
			return 2 + a
		case TypesMode_Alphanumeric, TypesMode_Byte:
			return 1 + a
		case TypesMode_Kanji:
			return a
		default:
			panic(rs.Unreachable(m))
		}
	case *Version_Normal:
		switch {
		case 1 <= v.F0 && v.F0 <= 9:
			switch m {
			case TypesMode_Numeric:
				return 10
			case TypesMode_Alphanumeric:
				return 9
			case TypesMode_Byte, TypesMode_Kanji:
				return 8
			default:
				panic(rs.Unreachable(m))
			}
		case 10 <= v.F0 && v.F0 <= 26:
			switch m {
			case TypesMode_Numeric:
				return 12
			case TypesMode_Alphanumeric:
				return 11
			case TypesMode_Byte:
				return 16
			case TypesMode_Kanji:
				return 10
			default:
				panic(rs.Unreachable(m))
			}
		default:
			switch m {
			case TypesMode_Numeric:
				return 14
			case TypesMode_Alphanumeric:
				return 13
			case TypesMode_Byte:
				return 16
			case TypesMode_Kanji:
				return 12
			default:
				panic(rs.Unreachable(m))
			}
		}
	default:
		panic(rs.Unreachable(version))
	}
}

// DataBitsCount is Mode::data_bits_count: computes the number of bits
// needed to some data of a given raw length.
//
//	TypesMode_Numeric.DataBitsCount(7) == 24
//
// Note that in Kanji mode, the `rawDataLen` is the number of Kanjis, i.e.
// half the total size of bytes.
func (m TypesMode) DataBitsCount(rawDataLen int) int {
	switch m {
	case TypesMode_Numeric:
		return (rawDataLen*10 + 2) / 3
	case TypesMode_Alphanumeric:
		return (rawDataLen*11 + 1) / 2
	case TypesMode_Byte:
		return rawDataLen * 8
	case TypesMode_Kanji:
		return rawDataLen * 13
	default:
		panic(rs.Unreachable(m))
	}
}

// Max is Mode::max: find the lowest common mode which both modes are
// compatible with.
func (m TypesMode) Max(other TypesMode) TypesMode {
	ordering, ok := m.PartialCmp(other).Get()
	switch {
	case ok && ordering > 0:
		return m
	case ok:
		return other
	default:
		return TypesMode_Byte
	}
}

// PartialCmp is `impl PartialOrd for Mode`: defines a partial ordering
// between modes. If `a <= b`, then `b` contains a superset of all characters
// supported by `a`. The ordering is -1, 0 or 1; None when the two modes are
// not comparable.
func (m TypesMode) PartialCmp(other TypesMode) rs.Option[int] {
	switch {
	case m == other:
		return rs.Some(0)
	case (m == TypesMode_Numeric && other == TypesMode_Alphanumeric) || other == TypesMode_Byte:
		return rs.Some(-1)
	case (m == TypesMode_Alphanumeric && other == TypesMode_Numeric) || m == TypesMode_Byte:
		return rs.Some(1)
	default:
		return rs.None[int]()
	}
}

//}}}
