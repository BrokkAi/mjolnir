// Port of base64 0.22.1 src/encode.rs

package base64

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// The deprecated free functions of this file (encode, encode_engine,
// encode_engine_string, encode_engine_slice) are not ported: they forward to
// the Engine methods.

// B64-encode and pad (if configured).
//
// This helper exists to avoid recalculating encoded_size, which is relatively expensive on short
// inputs.
//
// `encoded_size` is the encoded size calculated for `input`.
//
// `output` must be of size `encoded_size`.
//
// All bytes in `output` will be written to since it is exactly the size of the output.
func encodeEncodeWithPadding(
	input []byte,
	output []byte,
	engine Engine,
	expectedEncodedSize int,
) {
	rs.DebugAssert(expectedEncodedSize == len(output))

	b64BytesWritten := engine.InternalEncode(input, output)

	paddingBytes := 0
	if engine.Config().EncodePadding() {
		paddingBytes = encodeAddPadding(b64BytesWritten, output[b64BytesWritten:])
	}

	// b64_bytes_written.checked_add(padding_bytes): rs.UsizeMax is the bound
	// of a Go int, which is what the sum is stored in.
	encodedBytes := encodeCheckedAdd(b64BytesWritten, paddingBytes).
		Expect("usize overflow when calculating b64 length")

	rs.DebugAssert(expectedEncodedSize == encodedBytes)
}

// Calculate the base64 encoded length for a given input length, optionally including any
// appropriate padding bytes.
//
// Returns `None` if the encoded length can't be represented in `usize`. This will happen for
// input lengths in approximately the top quarter of the range of `usize`.
func EncodedLen(bytesLen int, padding bool) rs.Option[int] {
	rem := bytesLen % 3

	completeInputChunks := bytesLen / 3
	// `?` is disallowed in const, and `let Some(_) = _ else` requires 1.65.0, whereas this
	// messier syntax works on 1.48
	//
	// complete_input_chunks.checked_mul(4), with rs.UsizeMax, the largest Go
	// int, as the bound: the length is a Go int, so the overflow point is
	// the one of the type that holds it (2^63 rather than Rust's 2^64).
	if completeInputChunks > rs.UsizeMax/4 {
		return rs.None[int]()
	}
	completeChunkOutput := completeInputChunks * 4

	if rem > 0 {
		if padding {
			return encodeCheckedAdd(completeChunkOutput, 4)
		} else {
			var encodedRem int
			switch rem {
			case 1:
				encodedRem = 2
			// only other possible remainder is 2
			// can't use a separate _ => unreachable!() in const fns in ancient rust versions
			default:
				encodedRem = 3
			}
			return encodeCheckedAdd(completeChunkOutput, encodedRem)
		}
	} else {
		return rs.Some(completeChunkOutput)
	}
}

// encodeCheckedAdd is usize::checked_add for non-negative a and b, with the
// largest Go int as the bound.
func encodeCheckedAdd(a int, b int) rs.Option[int] {
	return rs.OptionOf(a+b, b <= rs.UsizeMax-a)
}

// Write padding characters.
// `unpadded_output_len` is the size of the unpadded but base64 encoded data.
// `output` is the slice where padding should be written, of length at least 2.
//
// Returns the number of padding bytes written.
func encodeAddPadding(unpaddedOutputLen int, output []byte) int {
	padBytes := (4 - (unpaddedOutputLen % 4)) % 4
	// for just a couple bytes, this has better performance than using
	// .fill(), or iterating over mutable refs, which call memset()
	for i := range padBytes {
		output[i] = libPadByte
	}

	return padBytes
}

// Errors that can occur while encoding into a slice.
type EncodeSliceError uint8

const (
	// The provided slice is too small.
	EncodeSliceError_OutputSliceTooSmall EncodeSliceError = iota
)

var _ = rs.RegisterEnum[EncodeSliceError]("EncodeSliceError",
	rs.Const(EncodeSliceError_OutputSliceTooSmall, "OutputSliceTooSmall"),
)

// String is the `Display` output.
func (self EncodeSliceError) String() string {
	switch self {
	case EncodeSliceError_OutputSliceTooSmall:
		return "Output slice too small"
	default:
		panic(rs.Unreachable(self))
	}
}

// Error makes an EncodeSliceError a Go error: the Display text.
func (self EncodeSliceError) Error() string { return self.String() }

// FmtDisplay is `impl fmt::Display for EncodeSliceError`, which writes with
// `write!`: formatting options do not apply.
func (self EncodeSliceError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(self.String())
}
