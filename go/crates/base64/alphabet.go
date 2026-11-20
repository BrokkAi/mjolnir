// Port of base64 0.22.1 src/alphabet.rs

package base64

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Provides [Alphabet] and constants for alphabets commonly used in the wild.

const alphabetAlphabetSize = 64

// An alphabet defines the 64 ASCII characters (symbols) used for base64.
//
// Common alphabets are provided as constants, and custom alphabets
// can be made via `from_str` or the `TryFrom<str>` implementation
// (AlphabetAlphabetNew, AlphabetAlphabetTryFromStr).
//
// # Examples
//
// Building and using a custom Alphabet:
//
//	custom := rs.Unwrap(base64.AlphabetAlphabetNew("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"))
//
//	engine := base64.EngineGeneralPurposeNew(
//		&custom,
//		base64.EngineGeneralPurposePad)
type AlphabetAlphabet struct {
	symbols [alphabetAlphabetSize]byte `rs:"symbols"`
}

var _ = rs.RegisterStruct[AlphabetAlphabet]("Alphabet")

// Performs no checks so that it can be const.
// Used only for known-valid strings.
func alphabetAlphabetFromStrUnchecked(alphabet string) AlphabetAlphabet {
	var symbols [alphabetAlphabetSize]byte
	sourceBytes := alphabet

	// a way to copy that's allowed in const fn
	index := 0
	for index < alphabetAlphabetSize {
		symbols[index] = sourceBytes[index]
		index += 1
	}

	return AlphabetAlphabet{symbols: symbols}
}

// Create an `Alphabet` from a string of 64 unique printable ASCII bytes.
//
// The `=` byte is not allowed as it is used for padding.
//
// The error is a ParseAlphabetError.
func AlphabetAlphabetNew(alphabet string) (AlphabetAlphabet, error) {
	bytes := alphabet
	if len(bytes) != alphabetAlphabetSize {
		return AlphabetAlphabet{}, &AlphabetParseAlphabetError_InvalidLength{}
	}

	{
		index := 0
		for index < alphabetAlphabetSize {
			byte_ := bytes[index]

			// must be ascii printable. 127 (DEL) is commonly considered printable
			// for some reason but clearly unsuitable for base64.
			if !(byte_ >= 32 && byte_ <= 126) {
				return AlphabetAlphabet{}, &AlphabetParseAlphabetError_UnprintableByte{F0: byte_}
			}
			// = is assumed to be padding, so cannot be used as a symbol
			if byte_ == libPadByte {
				return AlphabetAlphabet{}, &AlphabetParseAlphabetError_ReservedByte{F0: byte_}
			}

			// Check for duplicates while staying within what const allows.
			// It's n^2, but only over 64 hot bytes, and only once, so it's likely in the single digit
			// microsecond range.

			probeIndex := 0
			for probeIndex < alphabetAlphabetSize {
				if probeIndex == index {
					probeIndex += 1
					continue
				}

				probeByte := bytes[probeIndex]

				if byte_ == probeByte {
					return AlphabetAlphabet{}, &AlphabetParseAlphabetError_DuplicatedByte{F0: byte_}
				}

				probeIndex += 1
			}

			index += 1
		}
	}

	return alphabetAlphabetFromStrUnchecked(alphabet), nil
}

// Create a `&str` from the symbols in the `Alphabet`
func (self *AlphabetAlphabet) AsStr() string {
	return string(self.symbols[:])
}

// AlphabetAlphabetTryFromStr is `impl TryFrom<&str> for Alphabet`: the same
// as AlphabetAlphabetNew.
func AlphabetAlphabetTryFromStr(value string) (AlphabetAlphabet, error) {
	return AlphabetAlphabetNew(value)
}

// Possible errors when constructing an [Alphabet] from a `str`.
type AlphabetParseAlphabetError interface {
	error
	isAlphabetParseAlphabetError()
}

// Alphabets must be 64 ASCII bytes
type AlphabetParseAlphabetError_InvalidLength struct{}

// All bytes must be unique
type AlphabetParseAlphabetError_DuplicatedByte struct{ F0 uint8 }

// All bytes must be printable (in the range `[32, 126]`).
type AlphabetParseAlphabetError_UnprintableByte struct{ F0 uint8 }

// `=` cannot be used
type AlphabetParseAlphabetError_ReservedByte struct{ F0 uint8 }

func (*AlphabetParseAlphabetError_InvalidLength) isAlphabetParseAlphabetError()   {}
func (*AlphabetParseAlphabetError_DuplicatedByte) isAlphabetParseAlphabetError()  {}
func (*AlphabetParseAlphabetError_UnprintableByte) isAlphabetParseAlphabetError() {}
func (*AlphabetParseAlphabetError_ReservedByte) isAlphabetParseAlphabetError()    {}

var _ = rs.RegisterEnum[AlphabetParseAlphabetError]("ParseAlphabetError",
	rs.Variant[AlphabetParseAlphabetError_InvalidLength]("InvalidLength", rs.ShapeUnit),
	rs.Variant[AlphabetParseAlphabetError_DuplicatedByte]("DuplicatedByte", rs.ShapeTuple),
	rs.Variant[AlphabetParseAlphabetError_UnprintableByte]("UnprintableByte", rs.ShapeTuple),
	rs.Variant[AlphabetParseAlphabetError_ReservedByte]("ReservedByte", rs.ShapeTuple),
)

// alphabetHexByte is `format!("{:#04x}", b)` for a u8: `0x` and two
// lower-case hex digits.
func alphabetHexByte(b uint8) string {
	digits := strconv.FormatUint(uint64(b), 16)
	if len(digits) < 2 {
		digits = "0" + digits
	}
	return "0x" + digits
}

// alphabetParseAlphabetErrorString is `impl fmt::Display for
// ParseAlphabetError`.
func alphabetParseAlphabetErrorString(self AlphabetParseAlphabetError) string {
	switch self := self.(type) {
	case *AlphabetParseAlphabetError_InvalidLength:
		return "Invalid length - must be 64 bytes"
	case *AlphabetParseAlphabetError_DuplicatedByte:
		return "Duplicated byte: " + alphabetHexByte(self.F0)
	case *AlphabetParseAlphabetError_UnprintableByte:
		return "Unprintable byte: " + alphabetHexByte(self.F0)
	case *AlphabetParseAlphabetError_ReservedByte:
		return "Reserved byte: " + alphabetHexByte(self.F0)
	default:
		panic(rs.Unreachable(self))
	}
}

func (self *AlphabetParseAlphabetError_InvalidLength) String() string {
	return alphabetParseAlphabetErrorString(self)
}
func (self *AlphabetParseAlphabetError_DuplicatedByte) String() string {
	return alphabetParseAlphabetErrorString(self)
}
func (self *AlphabetParseAlphabetError_UnprintableByte) String() string {
	return alphabetParseAlphabetErrorString(self)
}
func (self *AlphabetParseAlphabetError_ReservedByte) String() string {
	return alphabetParseAlphabetErrorString(self)
}

func (self *AlphabetParseAlphabetError_InvalidLength) Error() string   { return self.String() }
func (self *AlphabetParseAlphabetError_DuplicatedByte) Error() string  { return self.String() }
func (self *AlphabetParseAlphabetError_UnprintableByte) Error() string { return self.String() }
func (self *AlphabetParseAlphabetError_ReservedByte) Error() string    { return self.String() }

// The Display impl writes with `write!`, so formatting options do not apply.
func (self *AlphabetParseAlphabetError_InvalidLength) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *AlphabetParseAlphabetError_DuplicatedByte) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *AlphabetParseAlphabetError_UnprintableByte) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *AlphabetParseAlphabetError_ReservedByte) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// The standard alphabet (with `+` and `/`) specified in [RFC 4648][].
//
// [RFC 4648]: https://datatracker.ietf.org/doc/html/rfc4648#section-4
var AlphabetStandard = alphabetAlphabetFromStrUnchecked(
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
)

// The URL-safe alphabet (with `-` and `_`) specified in [RFC 4648][].
//
// [RFC 4648]: https://datatracker.ietf.org/doc/html/rfc4648#section-5
var AlphabetUrlSafe = alphabetAlphabetFromStrUnchecked(
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
)

// The `crypt(3)` alphabet (with `.` and `/` as the _first_ two characters).
//
// Not standardized, but folk wisdom on the net asserts that this alphabet is what crypt uses.
var AlphabetCrypt = alphabetAlphabetFromStrUnchecked(
	"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
)

// The bcrypt alphabet.
var AlphabetBcrypt = alphabetAlphabetFromStrUnchecked(
	"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
)

// The alphabet used in IMAP-modified UTF-7 (with `+` and `,`).
//
// See [RFC 3501](https://tools.ietf.org/html/rfc3501#section-5.1.3)
var AlphabetImapMutf7 = alphabetAlphabetFromStrUnchecked(
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,",
)

// The alphabet used in BinHex 4.0 files.
//
// See [BinHex 4.0 Definition](http://files.stairways.com/other/binhex-40-specs-info.txt)
var AlphabetBinHex = alphabetAlphabetFromStrUnchecked(
	"!\"#$%&'()*+,-012345689@ABCDEFGHIJKLMNPQRSTUVXYZ[`abcdefhijklmpqr",
)
