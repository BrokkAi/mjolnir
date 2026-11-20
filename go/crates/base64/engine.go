// Port of base64 0.22.1 src/engine/mod.rs

package base64

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Provides the [Engine] abstraction and out of the box implementations.

// An `Engine` provides low-level encoding and decoding operations that all other higher-level parts of the API use. Users of the library will generally not need to implement this.
//
// Different implementations offer different characteristics. The library currently ships with
// [GeneralPurpose] that offers good speed and works on any CPU, with more choices
// coming later, like a constant-time one when side channel resistance is called for, and vendor-specific vectorized ones for more speed.
//
// See [general_purpose::STANDARD_NO_PAD] if you just want standard base64. Otherwise, when possible, it's
// recommended to store the engine in a `const` so that references to it won't pose any lifetime
// issues, and to avoid repeating the cost of engine setup.
//
// Since almost nobody will need to implement `Engine`, docs for internal methods are hidden.
//
// The Rust trait has default bodies for the encode and decode methods; they
// are the functions engineEncode, engineEncodeString, ... below, which an
// implementation's methods call. Rust's `T: AsRef<[u8]>` input is a []byte;
// pass `[]byte(s)` for a string.
type Engine interface {
	// This is not meant to be called directly; it is only for `Engine` implementors.
	// See the other `encode*` functions on this trait.
	//
	// Encode the `input` bytes into the `output` buffer based on the mapping in `encode_table`.
	//
	// `output` will be long enough to hold the encoded data.
	//
	// Returns the number of bytes written.
	//
	// No padding should be written; that is handled separately.
	//
	// Must not write any bytes into the output slice other than the encoded data.
	InternalEncode(input []byte, output []byte) int

	// This is not meant to be called directly; it is only for `Engine` implementors.
	//
	// As an optimization to prevent the decoded length from being calculated twice, it is
	// sometimes helpful to have a conservative estimate of the decoded size before doing the
	// decoding, so this calculation is done separately and passed to [Engine::decode()] as needed.
	InternalDecodedLenEstimate(inputLen int) EngineDecodeEstimate

	// This is not meant to be called directly; it is only for `Engine` implementors.
	// See the other `decode*` functions on this trait.
	//
	// Decode `input` base64 bytes into the `output` buffer.
	//
	// `decode_estimate` is the result of [Engine::internal_decoded_len_estimate()], which is passed in to avoid
	// calculating it again (expensive on short inputs).`
	//
	// Each complete 4-byte chunk of encoded data decodes to 3 bytes of decoded data, but this
	// function must also handle the final possibly partial chunk.
	// If the input length is not a multiple of 4, or uses padding bytes to reach a multiple of 4,
	// the trailing 2 or 3 bytes must decode to 1 or 2 bytes, respectively, as per the
	// [RFC](https://tools.ietf.org/html/rfc4648#section-3.5).
	//
	// Decoding must not write any bytes into the output slice other than the decoded data.
	//
	// Non-canonical trailing bits in the final tokens or non-canonical padding must be reported as
	// errors unless the engine is configured otherwise.
	//
	// A nil DecodeSliceError is Ok.
	InternalDecode(input []byte, output []byte, decodeEstimate EngineDecodeEstimate) (EngineDecodeMetadata, DecodeSliceError)

	// Returns the config for this engine.
	Config() EngineConfig

	// Encode arbitrary octets as base64 using the provided `Engine`.
	// Returns a `String`.
	Encode(input []byte) string

	// Encode arbitrary octets as base64 into a supplied `String`.
	// Writes into the supplied `String`, which may allocate if its internal buffer isn't big enough.
	EncodeString(input []byte, outputBuf *string)

	// Encode arbitrary octets as base64 into a supplied slice.
	// Writes into the supplied output buffer.
	//
	// This is useful if you wish to avoid allocation entirely (e.g. encoding into a stack-resident
	// or statically-allocated buffer).
	//
	// The error is an EncodeSliceError.
	EncodeSlice(input []byte, outputBuf []byte) (int, error)

	// Decode the input into a new `Vec`.
	//
	// The error is a DecodeError.
	Decode(input []byte) ([]byte, error)

	// Decode the `input` into the supplied `buffer`.
	//
	// Writes into the supplied `Vec`, which may allocate if its internal buffer isn't big enough.
	// Returns a `Result` containing an empty tuple, aka `()`.
	//
	// The error is a DecodeError.
	DecodeVec(input []byte, buffer *[]byte) error

	// Decode the input into the provided output slice.
	//
	// Returns the number of bytes written to the slice, or an error if `output` is smaller than
	// the estimated decoded length.
	//
	// This will not write any bytes past exactly what is decoded (no stray garbage bytes at the end).
	//
	// See [crate::decoded_len_estimate] for calculating buffer sizes.
	//
	// See [Engine::decode_slice_unchecked] for a version that panics instead of returning an error
	// if the output buffer is too small.
	//
	// The error is a DecodeSliceError.
	DecodeSlice(input []byte, output []byte) (int, error)

	// Decode the input into the provided output slice.
	//
	// Returns the number of bytes written to the slice.
	//
	// This will not write any bytes past exactly what is decoded (no stray garbage bytes at the end).
	//
	// See [crate::decoded_len_estimate] for calculating buffer sizes.
	//
	// See [Engine::decode_slice] for a version that returns an error instead of panicking if the output
	// buffer is too small.
	//
	// # Panics
	//
	// Panics if the provided output buffer is too small for the decoded data.
	//
	// The error is a DecodeError.
	DecodeSliceUnchecked(input []byte, output []byte) (int, error)
}

// engineEncode is the default body of Engine::encode.
func engineEncode(engine Engine, inputBytes []byte) string {
	encodedSize := EncodedLen(len(inputBytes), engine.Config().EncodePadding()).
		Expect("integer overflow when calculating buffer size")

	buf := make([]byte, encodedSize)

	encodeEncodeWithPadding(inputBytes, buf[:], engine, encodedSize)

	return rs.Expect(rs.ResultOf(strs.FromUtf8(buf)), "Invalid UTF8")
}

// engineEncodeString is the default body of Engine::encode_string.
func engineEncodeString(engine Engine, inputBytes []byte, outputBuf *string) {
	sink := chunkedEncoderStringSinkNew(outputBuf)

	rs.ExpectUnit(chunkedEncoderChunkedEncoderNew(engine).
		encode(inputBytes, sink), "Writing to a String shouldn't fail")
}

// engineEncodeSlice is the default body of Engine::encode_slice.
func engineEncodeSlice(engine Engine, inputBytes []byte, outputBuf []byte) (int, error) {
	encodedSize := EncodedLen(len(inputBytes), engine.Config().EncodePadding()).
		Expect("usize overflow when calculating buffer size")

	if len(outputBuf) < encodedSize {
		return 0, EncodeSliceError_OutputSliceTooSmall
	}

	b64Output := outputBuf[0:encodedSize]

	encodeEncodeWithPadding(inputBytes, b64Output, engine, encodedSize)

	return encodedSize, nil
}

// engineDecodeSliceErrorToDecodeError is the `map_err` of decode and
// decode_vec: a Vec is sized so that it is never too small.
func engineDecodeSliceErrorToDecodeError(e DecodeSliceError) DecodeError {
	switch e := e.(type) {
	case *DecodeSliceError_DecodeError:
		return e.F0
	case *DecodeSliceError_OutputSliceTooSmall:
		panic(rs.Unreachable("Vec is sized conservatively"))
	default:
		panic(rs.Unreachable(e))
	}
}

// engineDecode is the default body of Engine::decode.
func engineDecode(engine Engine, inputBytes []byte) ([]byte, error) {
	estimate := engine.InternalDecodedLenEstimate(len(inputBytes))
	buffer := make([]byte, estimate.DecodedLenEstimate())

	metadata, err := engine.InternalDecode(inputBytes, buffer, estimate)
	if err != nil {
		return nil, engineDecodeSliceErrorToDecodeError(err)
	}
	bytesWritten := metadata.decodedLen

	buffer = buffer[:bytesWritten]

	return buffer, nil
}

// engineDecodeVec is the default body of Engine::decode_vec.
func engineDecodeVec(engine Engine, inputBytes []byte, buffer *[]byte) error {
	startingOutputLen := len(*buffer)
	estimate := engine.InternalDecodedLenEstimate(len(inputBytes))

	// checked_add, with the largest Go int as the bound: the total is a Go
	// slice length.
	totalLenEstimate := encodeCheckedAdd(estimate.DecodedLenEstimate(), startingOutputLen).
		Expect("Overflow when calculating output buffer length")

	// buffer.resize(total_len_estimate, 0)
	if totalLenEstimate <= len(*buffer) {
		*buffer = (*buffer)[:totalLenEstimate]
	} else {
		*buffer = append(*buffer, make([]byte, totalLenEstimate-len(*buffer))...)
	}

	bufferSlice := (*buffer)[startingOutputLen:]

	metadata, err := engine.InternalDecode(inputBytes, bufferSlice, estimate)
	if err != nil {
		return engineDecodeSliceErrorToDecodeError(err)
	}
	bytesWritten := metadata.decodedLen

	*buffer = (*buffer)[:startingOutputLen+bytesWritten]

	return nil
}

// engineDecodeSlice is the default body of Engine::decode_slice.
func engineDecodeSlice(engine Engine, inputBytes []byte, output []byte) (int, error) {
	metadata, err := engine.InternalDecode(
		inputBytes,
		output,
		engine.InternalDecodedLenEstimate(len(inputBytes)),
	)
	if err != nil {
		return 0, err
	}
	return metadata.decodedLen, nil
}

// engineDecodeSliceUnchecked is the default body of
// Engine::decode_slice_unchecked.
func engineDecodeSliceUnchecked(engine Engine, inputBytes []byte, output []byte) (int, error) {
	metadata, err := engine.InternalDecode(
		inputBytes,
		output,
		engine.InternalDecodedLenEstimate(len(inputBytes)),
	)
	if err != nil {
		switch e := err.(type) {
		case *DecodeSliceError_DecodeError:
			return 0, e.F0
		case *DecodeSliceError_OutputSliceTooSmall:
			// panic!("Output slice is too small"): a fixed message, which
			// needs no formatting.
			rs.PanicAt(0, "Output slice is too small")
		default:
			panic(rs.Unreachable(e))
		}
	}
	return metadata.decodedLen, nil
}

// The minimal level of configuration that engines must support.
type EngineConfig interface {
	// Returns `true` if padding should be added after the encoded output.
	//
	// Padding is added outside the engine's encode() since the engine may be used
	// to encode only a chunk of the overall output, so it can't always know when
	// the output is "done" and would therefore need padding (if configured).
	EncodePadding() bool
}

// The decode estimate used by an engine implementation. Users do not need to interact with this;
// it is only for engine implementors.
//
// Implementors may store relevant data here when constructing this to avoid having to calculate
// them again during actual decoding.
type EngineDecodeEstimate interface {
	// Returns a conservative (err on the side of too big) estimate of the decoded length to use
	// for pre-allocating buffers, etc.
	//
	// The estimate must be no larger than the next largest complete triple of decoded bytes.
	// That is, the final quad of tokens to decode may be assumed to be complete with no padding.
	DecodedLenEstimate() int
}

// Controls how pad bytes are handled when decoding.
//
// Each [Engine] must support at least the behavior indicated by
// [DecodePaddingMode::RequireCanonical], and may support other modes.
type EngineDecodePaddingMode uint8

const (
	// Canonical padding is allowed, but any fewer padding bytes than that is also allowed.
	EngineDecodePaddingMode_Indifferent EngineDecodePaddingMode = iota
	// Padding must be canonical (0, 1, or 2 `=` as needed to produce a 4 byte suffix).
	EngineDecodePaddingMode_RequireCanonical
	// Padding must be absent -- for when you want predictable padding, without any wasted bytes.
	EngineDecodePaddingMode_RequireNone
)

var _ = rs.RegisterEnum[EngineDecodePaddingMode]("DecodePaddingMode",
	rs.Const(EngineDecodePaddingMode_Indifferent, "Indifferent"),
	rs.Const(EngineDecodePaddingMode_RequireCanonical, "RequireCanonical"),
	rs.Const(EngineDecodePaddingMode_RequireNone, "RequireNone"),
)

// Metadata about the result of a decode operation
type EngineDecodeMetadata struct {
	// Number of decoded bytes output
	decodedLen int `rs:"decoded_len"`
	// Offset of the first padding byte in the input, if any
	paddingOffset rs.Option[int] `rs:"padding_offset"`
}

var _ = rs.RegisterStruct[EngineDecodeMetadata]("DecodeMetadata")

func engineDecodeMetadataNew(decodedBytes int, paddingIndex rs.Option[int]) EngineDecodeMetadata {
	return EngineDecodeMetadata{
		decodedLen:    decodedBytes,
		paddingOffset: paddingIndex,
	}
}
