// Port of base64 0.22.1 src/decode.rs

package base64

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Errors that can occur while decoding.
//
// A DecodeError is returned as a Go error; match the variant with a type
// switch on *DecodeError_InvalidByte and the others.
type DecodeError interface {
	error
	isDecodeError()
}

// An invalid byte was found in the input. The offset and offending byte are provided.
//
// Padding characters (`=`) interspersed in the encoded form are invalid, as they may only
// be present as the last 0-2 bytes of input.
//
// This error may also indicate that extraneous trailing input bytes are present, causing
// otherwise valid padding to no longer be the last bytes of input.
type DecodeError_InvalidByte struct {
	F0 int
	F1 uint8
}

// The length of the input, as measured in valid base64 symbols, is invalid.
// There must be 2-4 symbols in the last input quad.
type DecodeError_InvalidLength struct {
	F0 int
}

// The last non-padding input symbol's encoded 6 bits have nonzero bits that will be discarded.
// This is indicative of corrupted or truncated Base64.
// Unlike [DecodeError::InvalidByte], which reports symbols that aren't in the alphabet,
// this error is for symbols that are in the alphabet but represent nonsensical encodings.
type DecodeError_InvalidLastSymbol struct {
	F0 int
	F1 uint8
}

// The nature of the padding was not as configured: absent or incorrect when it must be
// canonical, or present when it must be absent, etc.
type DecodeError_InvalidPadding struct{}

func (*DecodeError_InvalidByte) isDecodeError()       {}
func (*DecodeError_InvalidLength) isDecodeError()     {}
func (*DecodeError_InvalidLastSymbol) isDecodeError() {}
func (*DecodeError_InvalidPadding) isDecodeError()    {}

var _ = rs.RegisterEnum[DecodeError]("DecodeError",
	rs.Variant[DecodeError_InvalidByte]("InvalidByte", rs.ShapeTuple),
	rs.Variant[DecodeError_InvalidLength]("InvalidLength", rs.ShapeTuple),
	rs.Variant[DecodeError_InvalidLastSymbol]("InvalidLastSymbol", rs.ShapeTuple),
	rs.Variant[DecodeError_InvalidPadding]("InvalidPadding", rs.ShapeUnit),
)

// decodeErrorString is `impl fmt::Display for DecodeError`.
func decodeErrorString(self DecodeError) string {
	switch self := self.(type) {
	case *DecodeError_InvalidByte:
		index, byte_ := self.F0, self.F1
		// write!(f, "Invalid symbol {}, offset {}.", byte, index)
		return "Invalid symbol " + strconv.FormatUint(uint64(byte_), 10) + ", offset " + strconv.Itoa(index) + "."
	case *DecodeError_InvalidLength:
		len_ := self.F0
		// write!(f, "Invalid input length: {}", len)
		return "Invalid input length: " + strconv.Itoa(len_)
	case *DecodeError_InvalidLastSymbol:
		index, byte_ := self.F0, self.F1
		// write!(f, "Invalid last symbol {}, offset {}.", byte, index)
		return "Invalid last symbol " + strconv.FormatUint(uint64(byte_), 10) + ", offset " + strconv.Itoa(index) + "."
	case *DecodeError_InvalidPadding:
		return "Invalid padding"
	default:
		panic(rs.Unreachable(self))
	}
}

// String is the `Display` output.
func (self *DecodeError_InvalidByte) String() string       { return decodeErrorString(self) }
func (self *DecodeError_InvalidLength) String() string     { return decodeErrorString(self) }
func (self *DecodeError_InvalidLastSymbol) String() string { return decodeErrorString(self) }
func (self *DecodeError_InvalidPadding) String() string    { return decodeErrorString(self) }

// Error makes a DecodeError a Go error: the Display text. `impl error::Error
// for DecodeError` adds nothing; the error has no source.
func (self *DecodeError_InvalidByte) Error() string       { return self.String() }
func (self *DecodeError_InvalidLength) Error() string     { return self.String() }
func (self *DecodeError_InvalidLastSymbol) Error() string { return self.String() }
func (self *DecodeError_InvalidPadding) Error() string    { return self.String() }

// FmtDisplay is `impl fmt::Display for DecodeError`, which writes with
// `write!`: formatting options do not apply.
func (self *DecodeError_InvalidByte) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *DecodeError_InvalidLength) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *DecodeError_InvalidLastSymbol) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *DecodeError_InvalidPadding) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// Errors that can occur while decoding into a slice.
type DecodeSliceError interface {
	error
	isDecodeSliceError()
}

// A [DecodeError] occurred
type DecodeSliceError_DecodeError struct {
	F0 DecodeError
}

// The provided slice is too small.
type DecodeSliceError_OutputSliceTooSmall struct{}

func (*DecodeSliceError_DecodeError) isDecodeSliceError()         {}
func (*DecodeSliceError_OutputSliceTooSmall) isDecodeSliceError() {}

var _ = rs.RegisterEnum[DecodeSliceError]("DecodeSliceError",
	rs.Variant[DecodeSliceError_DecodeError]("DecodeError", rs.ShapeTuple),
	rs.Variant[DecodeSliceError_OutputSliceTooSmall]("OutputSliceTooSmall", rs.ShapeUnit),
)

// String is the `Display` output.
func (self *DecodeSliceError_DecodeError) String() string {
	// write!(f, "DecodeError: {}", e)
	return "DecodeError: " + self.F0.Error()
}

// String is the `Display` output.
func (self *DecodeSliceError_OutputSliceTooSmall) String() string {
	return "Output slice too small"
}

func (self *DecodeSliceError_DecodeError) Error() string         { return self.String() }
func (self *DecodeSliceError_OutputSliceTooSmall) Error() string { return self.String() }

// FmtDisplay is `impl fmt::Display for DecodeSliceError`, which writes with
// `write!`: formatting options do not apply.
func (self *DecodeSliceError_DecodeError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
func (self *DecodeSliceError_OutputSliceTooSmall) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}

// Unwrap is `error::Error::source`: the DecodeError.
func (self *DecodeSliceError_DecodeError) Unwrap() error {
	return self.F0
}

// DecodeSliceErrorFromDecodeError is `impl From<DecodeError> for
// DecodeSliceError`.
func DecodeSliceErrorFromDecodeError(e DecodeError) DecodeSliceError {
	return &DecodeSliceError_DecodeError{F0: e}
}

// Returns a conservative estimate of the decoded size of `encoded_len` base64 symbols (rounded up
// to the next group of 3 decoded bytes).
//
// The resulting length will be a safe choice for the size of a decode buffer, but may have up to
// 2 trailing bytes that won't end up being needed.
//
// # Examples
//
//	DecodedLenEstimate(1) // 3
//	DecodedLenEstimate(2) // 3
//	DecodedLenEstimate(3) // 3
//	DecodedLenEstimate(4) // 3
//	// start of the next quad of encoded symbols
//	DecodedLenEstimate(5) // 6
func DecodedLenEstimate(encodedLen int) int {
	return EngineGeneralPurposeStandard.
		InternalDecodedLenEstimate(encodedLen).
		DecodedLenEstimate()
}
